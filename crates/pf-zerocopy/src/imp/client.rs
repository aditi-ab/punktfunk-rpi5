//! Host side of the isolated zero-copy GPU import (design:
//! `design/zerocopy-worker-isolation.md`): spawns the `zerocopy-worker` subprocess, mirrors the
//! [`super::egl::EglImporter`] entry points over the [`super::proto`] socket, and materializes
//! the worker's pooled CUDA buffers in this process via CUDA IPC (each buffer's handles are
//! opened exactly once and reused as the pool recycles). A worker death — the whole point of the
//! isolation — surfaces as an `Err` with [`RemoteImporter::dead`] set, never as a host fault.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::cuda::{self, CUdeviceptr, DeviceBuffer, CU_IPC_HANDLE_SIZE};
use super::egl::DmabufPlane;
use super::proto::{self, BufferDesc, ImportKind, Reply, Request};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    mappings: Mutex<HashMap<u32, Mapping>>,
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

impl Drop for Shared {
    fn drop(&mut self) {
        // Last reference gone — no DeviceBuffer can still point into these mappings.
        for (_, m) in self.mappings.lock().unwrap().drain() {
            cuda::ipc_close(m.y);
            if let Some((uv, _)) = m.uv {
                cuda::ipc_close(uv);
            }
        }
    }
}

/// Children whose worker hasn't exited yet at `RemoteImporter` drop time (it exits on socket
/// EOF, i.e. after the last in-flight frame drops). Swept on every spawn and every drop so
/// workers don't linger as zombies for more than one capture generation.
static REAPER: Mutex<Vec<(Child, Instant)>> = Mutex::new(Vec::new());

/// How long past `REPLY_TIMEOUT` a parked worker may linger before it is force-killed. A worker
/// wedged INSIDE a driver call never observes socket EOF, so `try_wait` alone would keep it (and
/// its CUcontext + BufferPool — order hundreds of MB of VRAM) forever.
const REAPER_KILL_DEADLINE: Duration = Duration::from_secs(20);

fn sweep_reaper() {
    let mut list = REAPER.lock().unwrap();
    let now = Instant::now();
    list.retain_mut(|(c, parked)| {
        if matches!(c.try_wait(), Ok(Some(_))) {
            return false; // exited on its own → reaped
        }
        if now.duration_since(*parked) > REAPER_KILL_DEADLINE {
            let _ = c.kill();
            let _ = c.wait();
            return false; // wedged past the deadline → force-killed + reaped
        }
        true
    });
}

/// Fd pinned to this process's own executable image, opened (once, lazily) via the
/// `/proc/self/exe` magic link. The link names the running image's *inode*, not its path, so it
/// resolves even after the installed binary was replaced or deleted — and exec'ing the fd (via
/// [`fd_exec_path`]) then still runs byte-for-byte the build this process is. `current_exe()`
/// instead readlinks to a path: after a package upgrade under a running host that path is
/// "<path> (deleted)" and spawning it fails ENOENT — every capture then silently fell back to
/// the CPU copy — and even while the path exists it may hold a newer build whose worker
/// protocol mismatches this process.
static SELF_EXE: OnceLock<Option<File>> = OnceLock::new();

fn self_exe() -> Option<BorrowedFd<'static>> {
    SELF_EXE
        .get_or_init(|| {
            let f = match File::open("/proc/self/exe") {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "cannot pin /proc/self/exe — worker spawns use the current_exe() path, \
                         which breaks if this binary is replaced on disk"
                    );
                    return None;
                }
            };
            if f.as_raw_fd() != 3 {
                return Some(f);
            }
            // Fd 3 is the slot the spawn hands the worker its socket on (the `dup2` in
            // `spawn_exe`) — pinned there, the child would clobber it before exec resolves
            // `/proc/self/fd/3`. Re-number: 3 stays occupied by `f` during the clone, so the
            // duplicate cannot land on it.
            match f.try_clone() {
                Ok(clone) => Some(clone),
                Err(e) => {
                    tracing::warn!(error = %e, "re-numbering the pinned exe fd off fd 3 failed");
                    None
                }
            }
        })
        .as_ref()
        .map(|f| f.as_fd())
}

/// `/proc/self/fd/<n>` — an exec'able path to `fd`'s inode. The kernel resolves it at exec time
/// inside the forked child, whose fd table is a copy of ours (close-on-exec applies only once
/// the exec succeeds), so it names the pinned inode no matter what sits at the file's original
/// path by then.
fn fd_exec_path(fd: BorrowedFd<'_>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
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
    /// is exec'd through the pinned `SELF_EXE` fd, so it is always the exact image this
    /// process runs — even after the installed binary was replaced mid-flight. An `Err` here
    /// means "no isolated zero-copy available" — callers fall back to the CPU path, exactly like
    /// an in-process `EglImporter::new()` failure.
    pub fn spawn() -> Result<RemoteImporter> {
        match self_exe() {
            Some(fd) => Self::spawn_exe(&fd_exec_path(fd)),
            None => Self::spawn_exe(
                &std::env::current_exe().context("resolve /proc/self/exe for the worker")?,
            ),
        }
    }

    /// [`Self::spawn`] with an explicit executable (separated for tests).
    fn spawn_exe(exe: &Path) -> Result<RemoteImporter> {
        sweep_reaper();
        let (host_end, worker_end) = proto::socketpair_seqpacket().context("worker socketpair")?;
        let mut cmd = Command::new(exe);
        // `exe` is normally an opaque `/proc/self/fd/<n>` — keep `ps` output meaningful.
        cmd.arg0("punktfunk-host");
        cmd.arg("zerocopy-worker").arg("--fd").arg("3");
        let raw = worker_end.as_raw_fd();
        // SAFETY: `pre_exec` runs between fork and exec, so only async-signal-safe calls are
        // allowed — `dup2` and `fcntl` both are, and the closure captures only the `Copy` int
        // `raw` (no allocation, no locks). `dup2(raw, 3)` installs the socket at the fd number
        // the subcommand expects and clears CLOEXEC on the copy; if the parent's fd already IS 3,
        // `dup2(3,3)` would preserve CLOEXEC, so that case clears the flag explicitly instead.
        unsafe {
            cmd.pre_exec(move || {
                if raw == 3 {
                    let flags = libc::fcntl(3, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(raw, 3) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("spawn zerocopy-worker")?;
        drop(worker_end); // the child holds its own copy now
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
        proto::set_recv_timeout(importer.shared.sock.as_fd(), Some(HANDSHAKE_TIMEOUT))?;
        let ready = proto::recv::<Reply>(importer.shared.sock.as_fd(), &mut importer.rbuf);
        proto::set_recv_timeout(importer.shared.sock.as_fd(), Some(REPLY_TIMEOUT))?;
        match ready {
            Ok((Reply::Ready { version }, _)) if version == proto::PROTO_VERSION => {
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
                    proto::PROTO_VERSION
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
        if let Err(e) = proto::send(
            self.shared.sock.as_fd(),
            &Request::Modifiers { fourcc },
            None,
        ) {
            tracing::warn!(error = %e, "zerocopy worker modifier query failed");
            self.mark_dead();
            return Vec::new();
        }
        match proto::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
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
            if let Err(e) = proto::send(self.shared.sock.as_fd(), &req, pass) {
                self.mark_dead();
                return Err(e).context("zerocopy worker died (send)");
            }
            let reply = match proto::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
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
                    self.shared.mappings.lock().unwrap().insert(id, mapping);
                }
                let m = self
                    .shared
                    .mappings
                    .lock()
                    .unwrap()
                    .get(&id)
                    .copied()
                    .ok_or_else(|| {
                        self.mark_dead();
                        anyhow::anyhow!("worker delivered unknown buffer id {id} (desync)")
                    })?;
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
                        // the last frame drops.
                        let _ = proto::send(shared.sock.as_fd(), &Request::Release { id }, None);
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

    /// The PipeWire stream renegotiated — reset both sides' per-buffer caches.
    pub fn clear_cache(&mut self) {
        self.sent_keys.clear();
        if !self.dead() {
            if let Err(e) = proto::send(self.shared.sock.as_fd(), &Request::ClearCache, None) {
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
                REAPER.lock().unwrap().push((child, Instant::now()));
            }
        }
        sweep_reaper();
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
    use std::thread;

    fn handshake_server(reply: Reply) -> OwnedFd {
        let (host, worker) = proto::socketpair_seqpacket().unwrap();
        proto::send(worker.as_fd(), &reply, None).unwrap();
        // Keep the worker end alive alongside the host end for the test's duration by leaking it
        // into the reply thread below? Not needed: the handshake reply is already queued in the
        // socket buffer, so the worker end may drop — recv still delivers queued data first.
        drop(worker);
        host
    }

    #[test]
    fn handshake_ready_and_version_gate() {
        let host = handshake_server(Reply::Ready {
            version: proto::PROTO_VERSION,
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
        let (host, worker) = proto::socketpair_seqpacket().unwrap();
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

    #[test]
    fn pinned_fd_exec_survives_on_disk_replacement() {
        // The 2026-07-10 canary regression: a package upgrade replaced the installed binary and
        // every worker spawn ENOENT'd (`current_exe()` readlinked to "<path> (deleted)"). The
        // pinned-fd mechanism must keep exec'ing the original image after the file is gone: pin
        // a copy of /bin/sh, delete it, then run it through the fd path.
        let copy = std::env::temp_dir().join(format!("pf-zerocopy-exe-pin-{}", std::process::id()));
        std::fs::copy("/bin/sh", &copy).unwrap();
        let pinned = File::open(&copy).unwrap();
        std::fs::remove_file(&copy).unwrap();
        // Retry ETXTBSY: `fs::copy`'s write fd leaks into other tests' concurrently-forked
        // children until their execs clear it (CLOEXEC applies only at exec), and exec'ing a
        // file someone holds open for writing is refused. A harness artifact of copy-then-exec,
        // not the mechanism under test — production pins a read-only fd on a binary nobody
        // write-opens.
        let status = loop {
            match Command::new(fd_exec_path(pinned.as_fd()))
                .arg("-c")
                .arg("exit 42")
                .status()
            {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => break other.expect("exec via /proc/self/fd of a deleted file"),
            }
        };
        assert_eq!(status.code(), Some(42));
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
        let (host, worker) = proto::socketpair_seqpacket().unwrap();
        proto::send(
            worker.as_fd(),
            &Reply::Ready {
                version: proto::PROTO_VERSION,
            },
            None,
        )
        .unwrap();
        let join = thread::spawn(move || {
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            let mut replies = replies.into_iter();
            while let Ok((req, fd)) = proto::recv::<Request>(worker.as_fd(), &mut buf) {
                let needs_reply = matches!(req, Request::Modifiers { .. } | Request::Import { .. });
                let ino = fd
                    .as_ref()
                    .map(|f| dmabuf_key(f.as_raw_fd()).expect("fstat received fd"));
                seen.push((req, ino));
                if needs_reply {
                    match replies.next() {
                        Some(r) => proto::send(worker.as_fd(), &r, None).unwrap(),
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
