//! The isolated zero-copy GPU-import worker (`punktfunk-host zerocopy-worker`; design:
//! `design/zerocopy-worker-isolation.md`). It owns the fragile driver stack — the headless
//! EGLDisplay + GL context, the CUDA context, and the Vulkan bridge — so that a driver fault on a
//! producer-invalidated dmabuf (the `cuGraphicsMapResources` SIGSEGV the F44 Game→Desktop switch
//! reproduced) kills THIS process, not the streaming host. The host observes the dead socket,
//! fails the frame cleanly, and its existing capture-loss rebuild takes over.
//!
//! One worker serves one capture (spawned per `pipewire_thread`). It exits on socket EOF — which
//! only happens after the capturer AND every in-flight frame on the host side are gone, so pooled
//! device memory is never freed under a frame the host still reads.

use super::cuda::{self, CUdeviceptr, DeviceBuffer};
use super::egl::{DmabufPlane, EglImporter};
use super::ipc;
use super::proto::{BufferDesc, ImportKind, Reply, Request, PROTO_VERSION};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

/// Cap on cached per-key dmabuf fds. PipeWire buffer pools are ≤ ~16 buffers; the cap only
/// matters if a misbehaving producer churns buffers without a renegotiation.
const FD_CACHE_CAP: usize = 64;

/// Entry point for the hidden `zerocopy-worker` subcommand. `args` are the subcommand's own
/// arguments (`--fd N`, default 3 — the socket end the spawning host `dup2`'d in).
pub fn run_from_args(args: &[String]) -> Result<()> {
    // The host execs this worker through its pinned exe fd (`ipc::self_exe`), so the kernel
    // derives our comm from the exec path's basename — a meaningless fd number. Rename so
    // `top`/`pkill` see the worker.
    // SAFETY: `PR_SET_NAME` copies at most 16 bytes from the given pointer; the C-string literal
    // is valid, NUL-terminated, and short enough. No pointer is retained past the call.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"pf-zerocopy".as_ptr());
    }
    let fd: i32 = args
        .iter()
        .skip_while(|a| *a != "--fd")
        .nth(1)
        .map(|s| s.parse())
        .transpose()
        .context("parse --fd")?
        .unwrap_or(3);
    // Refuse to adopt anything that cannot be the spawning host's socket: a negative fd is
    // outright UB inside `OwnedFd` (its niche), and 0–2 would make this worker close one of its
    // own stdio streams on exit. Then confirm the number actually holds an open socket — the
    // subcommand is hidden but runnable by hand, and adopting an arbitrary inherited fd would
    // close it behind its real owner.
    anyhow::ensure!(fd >= 3, "--fd must be >= 3 (got {fd})");
    // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value, so
    // `mem::zeroed()` is a sound initializer; `fstat` writes into the live, correctly-sized
    // `&mut st` and only reads `fd`. `st_mode` is read only after the return value is checked.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    anyhow::ensure!(is_socket, "--fd {fd} is not an open socket");
    // SAFETY: the spawning host `dup2`'d its socketpair end onto exactly this fd number before
    // exec (the subcommand's contract, just verified to be an open socket ≥ 3) and nothing else
    // in this fresh process owns it, so `OwnedFd` takes sole ownership and closes it exactly
    // once at exit.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    run(sock)
}

/// Bring up the GPU stack, report readiness, and serve until the host goes away.
fn run(sock: OwnedFd) -> Result<()> {
    let importer = match EglImporter::new() {
        Ok(i) => i,
        Err(e) => {
            // Init failure is an ANSWER, not a crash: the host falls back to the CPU path,
            // exactly like an in-process `EglImporter::new()` failure.
            let _ = ipc::send(
                sock.as_fd(),
                &Reply::InitErr {
                    message: format!("{e:#}"),
                },
                None,
            );
            return Ok(());
        }
    };
    ipc::send(
        sock.as_fd(),
        &Reply::Ready {
            version: PROTO_VERSION,
        },
        None,
    )
    .context("send Ready")?;
    tracing::info!(pid = std::process::id(), "zerocopy import worker ready");
    let mut backend = EglBackend::new(importer);
    serve(&sock, &mut backend)
}

/// What [`serve`] needs from an import implementation — split out so the dispatch loop is
/// unit-testable without a GPU.
pub(crate) trait ImportBackend {
    fn modifiers(&mut self, fourcc: u32) -> Vec<u64>;
    /// Answers with [`Reply::Frame`] (buffer id + [`BufferDesc`] iff first delivery of that id),
    /// [`Reply::NeedFd`] (this side lacks the key's fd — host resends it once), or [`Reply::Err`].
    fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply;
    fn release(&mut self, id: u32);
    fn clear_cache(&mut self);
}

/// The [`Request::Import`] fields, destructured for [`ImportBackend::import`].
pub(crate) struct ImportReq {
    pub key: u64,
    pub kind: ImportKind,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: Option<u64>,
    pub offset: u32,
    pub stride: u32,
    pub has_fd: bool,
}

/// The request loop. Returns `Ok(())` on host EOF (normal end-of-life); any other socket error
/// propagates (the process exits — the host treats it like a death, which it is).
pub(crate) fn serve(sock: &OwnedFd, backend: &mut dyn ImportBackend) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        let (req, fd) = match ipc::recv::<Request>(sock.as_fd(), &mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("worker recv"),
        };
        match req {
            Request::Modifiers { fourcc } => {
                let reply = Reply::Modifiers {
                    modifiers: backend.modifiers(fourcc),
                };
                if send_or_eof(sock, &reply)? {
                    return Ok(());
                }
            }
            Request::Import {
                key,
                kind,
                width,
                height,
                fourcc,
                modifier,
                offset,
                stride,
                has_fd,
            } => {
                let req = ImportReq {
                    key,
                    kind,
                    width,
                    height,
                    fourcc,
                    modifier,
                    offset,
                    stride,
                    has_fd,
                };
                let reply = backend.import(&req, fd);
                if send_or_eof(sock, &reply)? {
                    return Ok(());
                }
            }
            Request::Release { id } => backend.release(id),
            Request::ClearCache => backend.clear_cache(),
        }
    }
}

/// Send a reply; `Ok(true)` means the host is gone (EPIPE) and the loop should end quietly.
fn send_or_eof(sock: &OwnedFd, reply: &Reply) -> Result<bool> {
    match ipc::send(sock.as_fd(), reply, None) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e).context("worker send"),
    }
}

/// The real backend: the in-process [`EglImporter`] plus the cross-process bookkeeping —
/// per-key dmabuf fds, in-flight frames (held until `Release`), and stable buffer ids.
struct EglBackend {
    importer: EglImporter,
    /// The dmabuf fd for each host key (`st_ino`), kept because the tiled path re-imports the fd
    /// every frame (`eglCreateImage`) and the LINEAR path caches per fd inside the Vulkan bridge.
    fds: HashMap<u64, OwnedFd>,
    /// Insertion order of `fds` keys for the LRU cap.
    fd_lru: VecDeque<u64>,
    /// Frames delivered to the host and not yet released — holding the `DeviceBuffer` is what
    /// keeps its device memory alive (pool `Arc`s) while the host encodes from it.
    inflight: HashMap<u32, DeviceBuffer>,
    /// Buffer id per device allocation. Valid only within one pool generation: pools never free
    /// allocations while alive, so a device VA can't repeat until a size change replaces the pool
    /// — at which point [`Self::note_dims`] clears this map, as does `ClearCache` (the host
    /// retires its IPC mappings on that boundary). Ids themselves are never reused; `next_id`
    /// only counts up.
    ids: HashMap<CUdeviceptr, u32>,
    next_id: u32,
    /// The (kind, width, height) of the last import — a change means the importer replaced its
    /// pool, invalidating the VA→id map (see [`Self::ids`]).
    last_shape: Option<(ImportKind, u32, u32)>,
}

impl EglBackend {
    fn new(importer: EglImporter) -> EglBackend {
        EglBackend {
            importer,
            fds: HashMap::new(),
            fd_lru: VecDeque::new(),
            inflight: HashMap::new(),
            ids: HashMap::new(),
            next_id: 0,
            last_shape: None,
        }
    }

    /// Store (or replace) the cached fd for `key`, evicting beyond the cap. A replaced or
    /// evicted fd is first forgotten by the Vulkan bridge so its per-fd import can't go stale.
    fn store_fd(&mut self, key: u64, fd: OwnedFd) {
        if let Some(old) = self.fds.insert(key, fd) {
            self.importer.forget_linear_fd(old.as_raw_fd());
            self.fd_lru.retain(|k| *k != key);
        }
        self.fd_lru.push_back(key);
        while self.fds.len() > FD_CACHE_CAP {
            let Some(oldest) = self.fd_lru.pop_front() else {
                break;
            };
            if let Some(old) = self.fds.remove(&oldest) {
                self.importer.forget_linear_fd(old.as_raw_fd());
            }
        }
    }

    /// Clear the VA→id map when the importer is about to replace its per-size pool (see
    /// [`Self::ids`]).
    fn note_dims(&mut self, kind: ImportKind, width: u32, height: u32) {
        if self.last_shape != Some((kind, width, height)) {
            self.last_shape = Some((kind, width, height));
            self.ids.clear();
        }
    }
}

impl ImportBackend for EglBackend {
    fn modifiers(&mut self, fourcc: u32) -> Vec<u64> {
        self.importer.supported_modifiers(fourcc)
    }

    fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply {
        if let Some(fd) = fd {
            self.store_fd(req.key, fd);
        } else if req.has_fd {
            return Reply::Err {
                message: "Import said has_fd but no fd arrived".into(),
            };
        }
        let Some(raw) = self.fds.get(&req.key).map(|f| f.as_raw_fd()) else {
            // We no longer hold this buffer's fd (LRU eviction / cache desync) — ask the host to
            // resend it rather than failing the frame.
            return Reply::NeedFd;
        };
        match self.import_inner(req, raw) {
            Ok((id, desc)) => Reply::Frame { id, desc },
            Err(e) => Reply::Err {
                message: format!("{e:#}"),
            },
        }
    }

    fn release(&mut self, id: u32) {
        if self.inflight.remove(&id).is_none() {
            tracing::warn!(id, "release for a frame not in flight (host/worker desync)");
        }
    }

    fn clear_cache(&mut self) {
        for (_, fd) in self.fds.drain() {
            self.importer.forget_linear_fd(fd.as_raw_fd());
        }
        self.fd_lru.clear();
        self.importer.clear_linear_cache();
        // ClearCache is the generation boundary the HOST retires its CUDA IPC mappings on: it
        // closes every mapping the renegotiated-away generation opened (keeping only ones still
        // under an in-flight frame, until they release). Forget the VA→id map so anything
        // delivered after this point gets a fresh id WITH its descriptor — re-delivering an old
        // id would reference a mapping the host just closed. Ids never repeat (`next_id` only
        // counts up), so fresh ids can't collide with the host's graveyard.
        self.ids.clear();
    }
}

impl EglBackend {
    /// The fallible core of [`ImportBackend::import`], once the fd for `req.key` is resolved.
    fn import_inner(&mut self, req: &ImportReq, raw: i32) -> Result<(u32, Option<BufferDesc>)> {
        let plane = DmabufPlane {
            fd: raw,
            offset: req.offset,
            stride: req.stride,
        };
        self.note_dims(req.kind, req.width, req.height);
        let buf = match req.kind {
            ImportKind::Tiled => {
                self.importer
                    .import(&plane, req.width, req.height, req.fourcc, req.modifier)?
            }
            ImportKind::TiledNv12 => self.importer.import_nv12(
                &plane,
                req.width,
                req.height,
                req.fourcc,
                req.modifier,
            )?,
            ImportKind::Tiled444 => self.importer.import_yuv444(
                &plane,
                req.width,
                req.height,
                req.fourcc,
                req.modifier,
            )?,
            ImportKind::Linear => self.importer.import_linear(&plane, req.width, req.height)?,
            ImportKind::LinearNv12 => self
                .importer
                .import_linear_nv12(&plane, req.width, req.height)?,
        };
        // Assign / look up the buffer's id and export its CUDA IPC identity on first delivery.
        cuda::make_current()?;
        let (id, desc) = match self.ids.get(&buf.ptr) {
            Some(&id) => (id, None),
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                let y_handle = cuda::ipc_export(buf.ptr)?.to_vec();
                let uv = match buf.uv {
                    Some((uv_ptr, uv_pitch)) => {
                        Some((cuda::ipc_export(uv_ptr)?.to_vec(), uv_pitch))
                    }
                    None => None,
                };
                self.ids.insert(buf.ptr, id);
                (
                    id,
                    Some(BufferDesc {
                        width: buf.width,
                        height: buf.height,
                        y_handle,
                        y_pitch: buf.pitch,
                        uv,
                    }),
                )
            }
        };
        if self.inflight.insert(id, buf).is_some() {
            // A pool never hands out a buffer that hasn't been recycled, so a duplicate id means
            // corrupted bookkeeping — fail the import rather than alias two frames.
            bail!("buffer id {id} already in flight");
        }
        Ok((id, desc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Identity (`st_ino`) of an open fd — what SCM_RIGHTS preserves across the socket while
    /// re-numbering the descriptor. Lets the dispatch test assert the fd that ARRIVED is the one
    /// the host sent, not merely that the JSON body claimed one.
    fn fd_ino(fd: impl AsRawFd) -> u64 {
        // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value, so
        // `mem::zeroed()` is a sound initializer. `fd` is a live descriptor owned by the caller;
        // `fstat` writes into the live, correctly-sized `&mut st`, and `st_ino` is read only
        // after the return value is checked.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0, "fstat");
            st.st_ino
        }
    }

    /// Records calls; import behavior is scripted per key.
    struct MockBackend {
        calls: mpsc::Sender<String>,
        next: u32,
    }

    impl ImportBackend for MockBackend {
        fn modifiers(&mut self, fourcc: u32) -> Vec<u64> {
            let _ = self.calls.send(format!("modifiers:{fourcc}"));
            vec![7, 8, 9]
        }
        fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply {
            let received = match &fd {
                Some(f) => format!("ino:{}", fd_ino(f.as_raw_fd())),
                None => "none".into(),
            };
            let _ = self.calls.send(format!(
                "import:key={} kind={:?} fd={received}",
                req.key, req.kind,
            ));
            if req.key == 0xbad {
                return Reply::Err {
                    message: "scripted failure".into(),
                };
            }
            if req.key == 0xfeed && !req.has_fd {
                return Reply::NeedFd;
            }
            let id = self.next;
            self.next += 1;
            let desc = (id == 0).then(|| BufferDesc {
                width: req.width,
                height: req.height,
                y_handle: vec![0u8; 64],
                y_pitch: 256,
                uv: None,
            });
            Reply::Frame { id, desc }
        }
        fn release(&mut self, id: u32) {
            let _ = self.calls.send(format!("release:{id}"));
        }
        fn clear_cache(&mut self) {
            let _ = self.calls.send("clear".into());
        }
    }

    fn start_server() -> (
        OwnedFd,
        mpsc::Receiver<String>,
        std::thread::JoinHandle<Result<()>>,
    ) {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let mut backend = MockBackend { calls: tx, next: 0 };
            serve(&worker, &mut backend)
        });
        (host, rx, join)
    }

    fn import_req(key: u64, has_fd: bool) -> Request {
        Request::Import {
            key,
            kind: ImportKind::Tiled,
            width: 64,
            height: 64,
            fourcc: 1,
            modifier: None,
            offset: 0,
            stride: 256,
            has_fd,
        }
    }

    #[test]
    fn dispatch_and_eof() {
        let (host, rx, join) = start_server();
        let mut buf = Vec::new();

        ipc::send(host.as_fd(), &Request::Modifiers { fourcc: 42 }, None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(
            reply,
            Reply::Modifiers {
                modifiers: vec![7, 8, 9]
            }
        );

        // First import delivers the desc; the second (same mock id sequence continues) doesn't.
        ipc::send(host.as_fd(), &import_req(1, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        match reply {
            Reply::Frame {
                id: 0,
                desc: Some(_),
            } => {}
            other => panic!("unexpected reply {other:?}"),
        }
        ipc::send(host.as_fd(), &import_req(1, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::Frame { id: 1, desc: None });

        // The descriptor itself must cross the socket: an import WITH an fd rides SCM_RIGHTS and
        // the backend receives a live descriptor with the sender's identity — `serve` dropping the
        // received fd (e.g. `backend.import(&req, None)`) would only be caught here.
        let (pr, _pw) = std::io::pipe().unwrap();
        let sent_ino = fd_ino(pr.as_fd().as_raw_fd());
        ipc::send(host.as_fd(), &import_req(3, true), Some(pr.as_fd())).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::Frame { id: 2, desc: None });

        // A missing worker-side fd is a NeedFd reply (host resends), not a failure.
        ipc::send(host.as_fd(), &import_req(0xfeed, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::NeedFd);

        // A failed import is an Err reply, not a dead worker.
        ipc::send(host.as_fd(), &import_req(0xbad, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        match reply {
            Reply::Err { message } => assert!(message.contains("scripted failure")),
            other => panic!("unexpected reply {other:?}"),
        }

        // Fire-and-forget ops reach the backend without replies.
        ipc::send(host.as_fd(), &Request::Release { id: 0 }, None).unwrap();
        ipc::send(host.as_fd(), &Request::ClearCache, None).unwrap();

        // Closing the host end terminates serve() cleanly.
        drop(host);
        join.join().unwrap().unwrap();

        let calls: Vec<String> = rx.iter().collect();
        assert_eq!(
            calls,
            vec![
                "modifiers:42".to_string(),
                "import:key=1 kind=Tiled fd=none".to_string(),
                "import:key=1 kind=Tiled fd=none".to_string(),
                format!("import:key=3 kind=Tiled fd=ino:{sent_ino}"),
                "import:key=65261 kind=Tiled fd=none".to_string(), // 0xfeed
                "import:key=2989 kind=Tiled fd=none".to_string(),  // 0xbad
                "release:0".to_string(),
                "clear".to_string(),
            ]
        );
    }
}
