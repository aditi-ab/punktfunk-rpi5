//! Host side of the isolated zero-copy GPU import (design:
//! `design/zerocopy-worker-isolation.md`): spawns the `zerocopy-worker` subprocess on the shared
//! [`super::ipc`] rails, mirrors the [`super::egl::EglImporter`] entry points over the
//! [`super::proto`] vocabulary, and materializes the worker's pooled CUDA buffers in this process
//! via CUDA IPC (each buffer's handles are opened exactly once and reused as the pool recycles).
//! A worker death — the whole point of the isolation — surfaces as an `Err` with
//! [`RemoteImporter::dead`] set, never as a host fault.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::cuda::{self, CUdeviceptr, DeviceBuffer, CU_IPC_HANDLE_SIZE};
use super::egl::DmabufPlane;
use super::ipc;
use super::proto::{BufferDesc, ImportKind, Reply, Request, PROTO_VERSION};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Handshake budget: EGL + CUDA bring-up is ~200 ms; a cold driver load can take seconds.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Per-request budget. An import is a few ms of GPU work; if the worker can't answer in this
/// window it is wedged (GPU fault in progress) and gets treated as dead.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// State shared with in-flight frames: the socket (their release messages) and the CUDA IPC
/// mappings (their device pointers). Lives until the LAST in-flight [`DeviceBuffer`] drops, so a
/// mapping is never closed under a frame the encoder still reads — and only then does the socket
/// close, which is what tells an idle worker to exit.
struct Shared {
    sock: OwnedFd,
    mappings: Mutex<HashMap<u32, MapEntry>>,
    dead: AtomicBool,
}

/// One pooled worker buffer, opened in this process.
#[derive(Clone, Copy)]
struct Mapping {
    y: CUdeviceptr,
    y_pitch: usize,
    uv: Option<(CUdeviceptr, usize)>,
    width: u32,
    height: u32,
}

/// A [`Mapping`] plus its lifecycle: how many in-flight [`DeviceBuffer`]s still point into it,
/// and whether its pool generation was retired by a renegotiation ([`RemoteImporter::clear_cache`]).
/// Retired-but-referenced entries linger as a graveyard and close when their last frame releases
/// — without this, every renegotiation (mode change, HDR toggle, client reconnect) permanently
/// pinned a pool's worth of host VA reservations to peer memory the worker had already freed.
/// Worker buffer ids are never reused (its `next_id` only counts up), so retired entries can
/// share the map with the next generation's.
struct MapEntry {
    m: Mapping,
    refs: u32,
    retired: bool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Last reference gone — no DeviceBuffer can still point into these mappings (current
        // generation or graveyard alike).
        for (_, e) in self.mappings.lock().unwrap().drain() {
            close_mapping(&e.m);
        }
    }
}

fn close_mapping(m: &Mapping) {
    cuda::ipc_close(m.y);
    if let Some((uv, _)) = m.uv {
        cuda::ipc_close(uv);
    }
}

/// The remote (isolated) importer — one per capture. Method-for-method mirror of the in-process
/// [`super::egl::EglImporter`] surface the capture thread uses.
pub struct RemoteImporter {
    shared: Arc<Shared>,
    child: Option<Child>,
    /// Reused receive scratch buffer (all replies are read by the single capture thread).
    rbuf: Vec<u8>,
    /// Dmabuf keys (`st_ino`) whose fd the worker already holds — the fd is passed only once.
    sent_keys: HashSet<u64>,
}

impl RemoteImporter {
    /// Spawn the worker from this host binary and complete the readiness handshake. The worker
    /// is exec'd through the pinned [`ipc::self_exe`] fd, so it is always the exact image this
    /// process runs — even after the installed binary was replaced mid-flight. An `Err` here
    /// means "no isolated zero-copy available" — callers fall back to the CPU path, exactly like
    /// an in-process `EglImporter::new()` failure.
    ///
    /// Self-exec is right *here* — host and worker are the same build by construction, so the
    /// version check is a formality. It is the one thing the capability-carrying encode worker
    /// must NOT copy: a shared inode shares the file capability.
    pub fn spawn() -> Result<RemoteImporter> {
        match ipc::self_exe() {
            Some(exe) => Self::spawn_exe(&exe.exec_path()),
            None => Self::spawn_exe(
                &std::env::current_exe().context("resolve /proc/self/exe for the worker")?,
            ),
        }
    }

    /// [`Self::spawn`] with an explicit executable (separated for tests).
    fn spawn_exe(exe: &Path) -> Result<RemoteImporter> {
        // `exe` is normally an opaque `/proc/self/fd/<n>` — the argv[0] keeps `ps` meaningful.
        let (host_end, child) =
            ipc::spawn_worker(exe, "punktfunk-host", &["zerocopy-worker", "--fd", "3"])
                .context("spawn zerocopy-worker")?;
        Self::from_socket(host_end, Some(child))
    }

    /// Complete the handshake on an already-connected socket (the unit tests drive this against
    /// a mock server thread instead of a real subprocess).
    fn from_socket(sock: OwnedFd, child: Option<Child>) -> Result<RemoteImporter> {
        let mut importer = RemoteImporter {
            shared: Arc::new(Shared {
                sock,
                mappings: Mutex::new(HashMap::new()),
                dead: AtomicBool::new(false),
            }),
            child,
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
        };
        ipc::set_recv_timeout(importer.shared.sock.as_fd(), Some(HANDSHAKE_TIMEOUT))?;
        let ready = ipc::recv::<Reply>(importer.shared.sock.as_fd(), &mut importer.rbuf);
        ipc::set_recv_timeout(importer.shared.sock.as_fd(), Some(REPLY_TIMEOUT))?;
        match ready {
            Ok((Reply::Ready { version }, _)) if version == PROTO_VERSION => {
                tracing::info!(
                    pid = importer.child.as_ref().map(|c| c.id()),
                    "zero-copy GPU import isolated in a worker process"
                );
                Ok(importer)
            }
            Ok((Reply::Ready { version }, _)) => {
                importer.mark_dead();
                bail!(
                    "zerocopy worker protocol mismatch (worker v{version}, host v{})",
                    PROTO_VERSION
                )
            }
            Ok((Reply::InitErr { message }, _)) => {
                // The worker exits by itself after reporting; not a death, just "no GPU here".
                bail!("zerocopy worker init failed: {message}")
            }
            Ok((other, _)) => {
                importer.mark_dead();
                bail!("unexpected zerocopy worker handshake: {other:?}")
            }
            Err(e) => {
                importer.mark_dead();
                Err(e).context("zerocopy worker handshake (died on startup?)")
            }
        }
    }

    /// True once any exchange failed at the transport level — the worker is gone (or wedged) and
    /// every further call fails fast. The capture layer poisons its stream on this.
    pub fn dead(&self) -> bool {
        self.shared.dead.load(Ordering::Relaxed)
    }

    fn mark_dead(&self) {
        self.shared.dead.store(true, Ordering::Relaxed);
    }

    /// Mirror of [`super::egl::EglImporter::supported_modifiers`] (worker round-trip; empty on
    /// any failure, which makes the capture fall back like an importless negotiation).
    pub fn supported_modifiers(&mut self, fourcc: u32) -> Vec<u64> {
        if self.dead() {
            return Vec::new();
        }
        if let Err(e) = ipc::send(
            self.shared.sock.as_fd(),
            &Request::Modifiers { fourcc },
            None,
        ) {
            tracing::warn!(error = %e, "zerocopy worker modifier query failed");
            self.mark_dead();
            return Vec::new();
        }
        match ipc::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
            Ok((Reply::Modifiers { modifiers }, _)) => modifiers,
            Ok((other, _)) => {
                tracing::warn!(?other, "unexpected zerocopy worker reply to Modifiers");
                self.mark_dead();
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "zerocopy worker modifier reply failed");
                self.mark_dead();
                Vec::new()
            }
        }
    }

    /// Mirror of [`super::egl::EglImporter::import`] (tiled dmabuf → BGRx CUDA buffer).
    pub fn import(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_impl(plane, ImportKind::Tiled, width, height, fourcc, modifier)
    }

    /// Mirror of [`super::egl::EglImporter::import_nv12`].
    pub fn import_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_impl(
            plane,
            ImportKind::TiledNv12,
            width,
            height,
            fourcc,
            modifier,
        )
    }

    /// Mirror of [`super::egl::EglImporter::import_yuv444`] (tiled dmabuf → stacked 3-plane
    /// YUV444 CUDA buffer — the 4:4:4 zero-copy path).
    pub fn import_yuv444(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_impl(plane, ImportKind::Tiled444, width, height, fourcc, modifier)
    }

    /// Mirror of [`super::egl::EglImporter::import_linear`] (LINEAR dmabuf → Vulkan bridge).
    pub fn import_linear(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        self.import_impl(plane, ImportKind::Linear, width, height, 0, None)
    }

    /// Mirror of [`super::egl::EglImporter::import_linear_nv12`] (LINEAR dmabuf → Vulkan-bridge
    /// compute CSC → two-plane NV12 buffer, latency plan T2.5b).
    pub fn import_linear_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        self.import_impl(plane, ImportKind::LinearNv12, width, height, 0, None)
    }

    fn import_impl(
        &mut self,
        plane: &DmabufPlane,
        kind: ImportKind,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        if self.dead() {
            bail!("zerocopy worker is dead");
        }
        let key = dmabuf_key(plane.fd)?;
        // One retry: a `NeedFd` reply (the worker's fd cache evicted this key) clears our
        // "already sent" note so the second attempt carries the fd again.
        let mut attempts = 0;
        let reply = loop {
            attempts += 1;
            let has_fd = self.sent_keys.insert(key);
            // SAFETY: `plane.fd` is the dmabuf fd of the PipeWire buffer the capture thread still
            // holds for this callback (`consume_frame`'s contract), so it is open and stays open
            // for this synchronous call; the `BorrowedFd` never outlives it (used only for the
            // `send`).
            let pass = has_fd.then(|| unsafe { BorrowedFd::borrow_raw(plane.fd) });
            let req = Request::Import {
                key,
                kind,
                width,
                height,
                fourcc,
                modifier,
                offset: plane.offset,
                stride: plane.stride,
                has_fd,
            };
            if let Err(e) = ipc::send(self.shared.sock.as_fd(), &req, pass) {
                self.mark_dead();
                return Err(e).context("zerocopy worker died (send)");
            }
            let reply = match ipc::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
                Ok((reply, _)) => reply,
                Err(e) => {
                    self.mark_dead();
                    return Err(e).context("zerocopy worker died (no reply)");
                }
            };
            match reply {
                Reply::NeedFd if attempts == 1 => {
                    self.sent_keys.remove(&key);
                    continue;
                }
                Reply::NeedFd => {
                    self.mark_dead();
                    bail!("zerocopy worker still lacks the fd after a resend (desync)");
                }
                other => break other,
            }
        };
        match reply {
            Reply::Frame { id, desc } => {
                if let Some(desc) = desc {
                    let mapping = open_mapping(&desc).with_context(|| {
                        // An unopenable mapping poisons every future frame in this buffer —
                        // treat it as a dead worker so the capture rebuilds cleanly.
                        self.mark_dead();
                        format!("open CUDA IPC mapping for worker buffer {id}")
                    })?;
                    self.shared.mappings.lock().unwrap().insert(
                        id,
                        MapEntry {
                            m: mapping,
                            refs: 0,
                            retired: false,
                        },
                    );
                }
                let m = {
                    let mut g = self.shared.mappings.lock().unwrap();
                    let entry = g.get_mut(&id).ok_or_else(|| {
                        self.mark_dead();
                        anyhow::anyhow!("worker delivered unknown buffer id {id} (desync)")
                    })?;
                    entry.refs += 1;
                    entry.m
                };
                let shared = self.shared.clone();
                Ok(DeviceBuffer::remote(
                    m.y,
                    m.y_pitch,
                    m.width,
                    m.height,
                    m.uv,
                    // The wire carries no plane format — the buffer's layout is what WE requested.
                    kind == ImportKind::Tiled444,
                    Box::new(move || {
                        // Fire-and-forget recycle; a dead worker just means EPIPE, ignored. The
                        // captured `shared` Arc is what keeps the mapping + socket alive until
                        // the last frame drops. A retired mapping (its generation renegotiated
                        // away) closes here with its last reference.
                        let _ = ipc::send(shared.sock.as_fd(), &Request::Release { id }, None);
                        let mut g = shared.mappings.lock().unwrap();
                        if let Some(entry) = g.get_mut(&id) {
                            entry.refs = entry.refs.saturating_sub(1);
                            if entry.retired && entry.refs == 0 {
                                let entry = g.remove(&id).expect("entry exists");
                                close_mapping(&entry.m);
                            }
                        }
                    }),
                ))
            }
            Reply::Err { message } => bail!("zerocopy worker import failed: {message}"),
            other => {
                self.mark_dead();
                bail!("unexpected zerocopy worker reply: {other:?}")
            }
        }
    }

    /// The PipeWire stream renegotiated — reset both sides' per-buffer caches, and retire the
    /// outgoing generation's CUDA IPC mappings: the worker replaces its pool, so these host-side
    /// mappings pin VA reservations to peer memory that is about to be (or already was) freed.
    /// Unreferenced ones close now; ones still under an in-flight frame close with its release.
    pub fn clear_cache(&mut self) {
        self.sent_keys.clear();
        {
            let mut g = self.shared.mappings.lock().unwrap();
            g.retain(|_, entry| {
                if entry.refs == 0 {
                    close_mapping(&entry.m);
                    false
                } else {
                    entry.retired = true;
                    true
                }
            });
        }
        if !self.dead() {
            if let Err(e) = ipc::send(self.shared.sock.as_fd(), &Request::ClearCache, None) {
                tracing::warn!(error = %e, "zerocopy worker ClearCache failed");
                self.mark_dead();
            }
        }
    }
}

impl Drop for RemoteImporter {
    fn drop(&mut self) {
        // The worker exits on socket EOF, which happens when the last `Shared` reference (this
        // importer, or the final in-flight frame on the encode side) drops. Reap what's already
        // gone; park the rest for the next sweep.
        if let Some(mut child) = self.child.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                ipc::park_child(child);
            }
        }
        ipc::sweep_reaper();
    }
}

/// Identity of the dma-buf behind `fd`, stable across frames and across `SCM_RIGHTS` re-numbering:
/// every dma-buf gets a unique inode on the kernel's dmabuf pseudo-fs for its lifetime. Used as
/// the worker's fd-cache key so the fd itself is only passed once.
fn dmabuf_key(fd: i32) -> Result<u64> {
    // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value, so
    // `mem::zeroed()` is a sound initializer. `fd` is the caller's live dmabuf fd; `fstat` writes
    // into `&mut st`, a live, correctly-sized stack struct that outlives the synchronous call,
    // and `st_ino` is read only after the return value is checked.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            bail!("fstat(dmabuf fd): {}", io::Error::last_os_error());
        }
        Ok(st.st_ino)
    }
}

/// Open a worker buffer's CUDA IPC handles in this process.
fn open_mapping(desc: &BufferDesc) -> Result<Mapping> {
    cuda::make_current()?;
    let y_handle: [u8; CU_IPC_HANDLE_SIZE] = desc
        .y_handle
        .as_slice()
        .try_into()
        .context("worker sent a malformed Y IPC handle")?;
    let y = cuda::ipc_open(&y_handle).context("open Y plane IPC handle")?;
    let uv = match &desc.uv {
        Some((handle, pitch)) => {
            let handle: [u8; CU_IPC_HANDLE_SIZE] = handle
                .as_slice()
                .try_into()
                .context("worker sent a malformed UV IPC handle")?;
            match cuda::ipc_open(&handle) {
                Ok(ptr) => Some((ptr, *pitch)),
                Err(e) => {
                    // Don't leak the Y mapping on a half-open failure.
                    cuda::ipc_close(y);
                    return Err(e).context("open UV plane IPC handle");
                }
            }
        }
        None => None,
    };
    Ok(Mapping {
        y,
        y_pitch: desc.y_pitch,
        uv,
        width: desc.width,
        height: desc.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::thread;

    fn handshake_server(reply: Reply) -> OwnedFd {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        ipc::send(worker.as_fd(), &reply, None).unwrap();
        // Keep the worker end alive alongside the host end for the test's duration by leaking it
        // into the reply thread below? Not needed: the handshake reply is already queued in the
        // socket buffer, so the worker end may drop — recv still delivers queued data first.
        drop(worker);
        host
    }

    #[test]
    fn handshake_ready_and_version_gate() {
        let host = handshake_server(Reply::Ready {
            version: PROTO_VERSION,
        });
        let imp = RemoteImporter::from_socket(host, None).unwrap();
        assert!(!imp.dead());

        let host = handshake_server(Reply::Ready { version: 999 });
        assert!(RemoteImporter::from_socket(host, None).is_err());
    }

    #[test]
    fn handshake_init_err() {
        let host = handshake_server(Reply::InitErr {
            message: "no GPU".into(),
        });
        let Err(err) = RemoteImporter::from_socket(host, None) else {
            panic!("InitErr handshake must fail")
        };
        assert!(format!("{err:#}").contains("no GPU"), "{err:#}");
    }

    #[test]
    fn handshake_eof_is_an_error() {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        drop(worker);
        assert!(RemoteImporter::from_socket(host, None).is_err());
    }

    #[test]
    fn spawning_a_non_worker_fails_cleanly() {
        // `true` exits immediately without a handshake → EOF → clean spawn error, the same
        // fallback path a GPU-less box takes.
        let Err(err) = RemoteImporter::spawn_exe(Path::new("true")) else {
            panic!("spawning a non-worker must fail")
        };
        assert!(format!("{err:#}").contains("handshake"), "{err:#}");
    }

    #[test]
    fn spawn_execs_the_pinned_self_exe() {
        // `spawn()` execs this very process's image via the pinned `/proc/self/fd/…` path. Here
        // that image is the libtest harness, which rejects `--fd` and exits without a handshake
        // — so a "handshake" error proves the exec itself succeeded (an exec failure would read
        // "spawn zerocopy-worker" instead).
        let Err(err) = RemoteImporter::spawn() else {
            panic!("the test harness is not a worker; spawn must fail at the handshake")
        };
        assert!(format!("{err:#}").contains("handshake"), "{err:#}");
    }

    /// A request as the scripted peer saw it, paired with the identity (`st_ino`) of the
    /// descriptor that actually arrived via SCM_RIGHTS — the `has_fd` boolean in the JSON body is
    /// a *claim*; the received fd is the mechanism the whole worker design rests on, so tests
    /// assert on it directly.
    type SeenRequest = (Request, Option<u64>);

    /// A scripted peer: answers the handshake, then serves canned replies per request.
    fn scripted_server(
        replies: Vec<Reply>,
    ) -> (RemoteImporter, thread::JoinHandle<Vec<SeenRequest>>) {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        ipc::send(
            worker.as_fd(),
            &Reply::Ready {
                version: PROTO_VERSION,
            },
            None,
        )
        .unwrap();
        let join = thread::spawn(move || {
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            let mut replies = replies.into_iter();
            while let Ok((req, fd)) = ipc::recv::<Request>(worker.as_fd(), &mut buf) {
                let needs_reply = matches!(req, Request::Modifiers { .. } | Request::Import { .. });
                let ino = fd
                    .as_ref()
                    .map(|f| dmabuf_key(f.as_raw_fd()).expect("fstat received fd"));
                seen.push((req, ino));
                if needs_reply {
                    match replies.next() {
                        Some(r) => ipc::send(worker.as_fd(), &r, None).unwrap(),
                        None => break, // close → client sees a dead worker
                    }
                }
            }
            seen
        });
        let imp = RemoteImporter::from_socket(host, None).unwrap();
        (imp, join)
    }

    #[test]
    fn modifiers_round_trip() {
        let (mut imp, join) = scripted_server(vec![Reply::Modifiers {
            modifiers: vec![1, 2, 3],
        }]);
        assert_eq!(imp.supported_modifiers(0x3432_5258), vec![1, 2, 3]);
        assert!(!imp.dead());
        drop(imp);
        let seen = join.join().unwrap();
        assert_eq!(
            seen,
            vec![(
                Request::Modifiers {
                    fourcc: 0x3432_5258
                },
                None
            )]
        );
    }

    #[test]
    fn need_fd_triggers_one_resend_with_the_fd() {
        let (mut imp, join) = scripted_server(vec![
            Reply::Err {
                message: "one".into(),
            },
            Reply::NeedFd,
            Reply::Err {
                message: "two".into(),
            },
        ]);
        let (pr, _pw) = std::io::pipe().unwrap();
        let plane = DmabufPlane {
            fd: pr.as_fd().as_raw_fd(),
            offset: 0,
            stride: 256,
        };
        // First import: first sight of the key → fd rides along; the Err reply keeps the key
        // marked as sent (the worker cached the fd before failing).
        assert!(imp.import(&plane, 64, 64, 1, Some(2)).is_err());
        // Second import: no fd (already sent) → worker answers NeedFd → one retry WITH the fd.
        assert!(imp.import(&plane, 64, 64, 1, Some(2)).is_err());
        assert!(!imp.dead(), "NeedFd handling must not mark the worker dead");
        // The identity the passed descriptor must carry — SCM_RIGHTS re-numbers the fd but
        // preserves the open file description, so st_ino survives the crossing.
        let key = dmabuf_key(plane.fd).unwrap();
        drop(imp);
        let fd_sends: Vec<(bool, Option<u64>)> = join
            .join()
            .unwrap()
            .iter()
            .map(|(r, ino)| match r {
                Request::Import { has_fd, .. } => (*has_fd, *ino),
                other => panic!("unexpected request {other:?}"),
            })
            .collect();
        // Not just the has_fd *claim* — the descriptor itself must have crossed, with the same
        // identity the worker will key its cache on (`pass = None` at the send site would leave
        // has_fd=true with no actual fd, which only this assertion catches).
        assert_eq!(
            fd_sends,
            vec![(true, Some(key)), (false, None), (true, Some(key))]
        );
    }

    #[test]
    fn import_error_reply_keeps_worker_alive_and_death_is_detected() {
        let (mut imp, join) = scripted_server(vec![Reply::Err {
            message: "EGL_BAD_MATCH".into(),
        }]);
        // Any pipe works as a stand-in fd for key derivation.
        let (pr, _pw) = std::io::pipe().unwrap();
        let plane = DmabufPlane {
            fd: pr.as_fd().as_raw_fd(),
            offset: 0,
            stride: 256,
        };
        let Err(err) = imp.import(&plane, 64, 64, 1, Some(2)) else {
            panic!("scripted Err reply must fail the import")
        };
        assert!(format!("{err:#}").contains("EGL_BAD_MATCH"));
        assert!(!imp.dead(), "an Err reply must not mark the worker dead");

        // The scripted replies are exhausted → the server closes → the next import dies.
        let Err(err) = imp.import(&plane, 64, 64, 1, Some(2)) else {
            panic!("a closed worker must fail the import")
        };
        assert!(format!("{err:#}").contains("died"), "{err:#}");
        assert!(imp.dead());
        let key = dmabuf_key(plane.fd).unwrap();
        drop(imp);
        let seen = join.join().unwrap();
        // First import carried the fd (first sight of the key — and the DESCRIPTOR arrived, with
        // the sender's identity); the retry didn't re-send it.
        match (&seen[0], &seen[1]) {
            (
                (
                    Request::Import {
                        has_fd: true,
                        kind: ImportKind::Tiled,
                        ..
                    },
                    Some(ino),
                ),
                (Request::Import { has_fd: false, .. }, None),
            ) => assert_eq!(*ino, key),
            other => panic!("unexpected requests {other:?}"),
        }
    }
}
