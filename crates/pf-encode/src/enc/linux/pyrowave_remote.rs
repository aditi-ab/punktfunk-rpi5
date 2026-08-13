//! Host half of `punktfunk-encode-worker` (design: `design/gpu-priority-capability-worker.md` §3;
//! plan §2/WP2). The worker half and the vocabulary they share are [`super::worker`].
//!
//! [`RemotePyroWave`] is an `Encoder` that forwards a PyroWave session to the capability-carrying
//! worker process, and it sits **under** `TrackedEncoder` — session accounting, the encode-stall
//! watchdog, encoder recovery and the forwarding rot-guard all apply to it unchanged.
//!
//! ## The ladder — no rung may kill a negotiated session
//!
//! A PyroWave open is only ever reached by a session that already negotiated PyroWave, so a hard
//! error here is a dead stream, not a fallback to another codec. Every rung therefore ends at the
//! **in-process encoder exactly as today**, at default GPU priority, with one warning:
//!
//! | rung | where | outcome |
//! |---|---|---|
//! | `PUNKTFUNK_ENCODE_WORKER=off` | [`resolve_worker_path`] | in-process, one info line |
//! | binary not found | [`resolve_worker_path`] | in-process, one warn |
//! | spawn failed | [`open_preferring_worker`] | in-process, one warn |
//! | handshake timed out / worker died starting | [`spawn_link`] | in-process, one warn |
//! | proto or workspace-version skew | [`spawn_link`] | in-process, one warn |
//! | the worker could not open its encoder | [`spawn_link`] | in-process, one warn |
//! | a non-dmabuf frame arrived | [`RemotePyroWave::submit`] | in-process for the session, one warn |
//! | the worker failed a frame | [`RemotePyroWave::submit`] | in-process for the session, one warn |
//! | socket EOF mid-session | [`RemotePyroWave::reset`] | one respawn, then in-process |
//!
//! The first six happen before any frame and return the bare in-process encoder — no proxy, no
//! per-frame cost on the path that is still the overwhelming majority of hosts. The last three
//! happen inside a live proxy, which keeps its own in-process encoder from then on.
//!
//! Every fallback line ends with the same clause — *"encoding in-process at default GPU
//! priority"* — so one `grep` finds them all, whichever rung fired.

use super::worker::{self, FromWorker, PriorityOutcome, ToWorker, WireCursor};
use crate::pyrowave_wire::{stream_chunk_step, AuChunker};
use crate::{AuChunk, ChromaFormat, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pf_zerocopy::ipc;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

/// The installed name every packaging channel writes — and the ONLY file that may carry
/// `cap_sys_nice=ep`. Never a hardlink of `punktfunk-host` and never a subcommand of it: a shared
/// inode shares the file capability, which makes the host unidentifiable to KWin and kills every
/// KDE desktop session (0.26.0-1).
const WORKER_BIN: &str = "punktfunk-encode-worker";

/// Handshake budget — the same one the zerocopy worker's GPU bring-up gets. It covers a Vulkan
/// instance + device create and the pyrowave object build; a cold driver load can take seconds.
/// Blowing it means the driver is wedged, in which case the in-process encoder would wedge too,
/// so a generous budget costs nothing that the fallback would have saved.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-request budget. A PyroWave encode is 2–5 ms and the encoder's own fence wait is capped at
/// 5 s, so a worker silent for twice that is wedged inside a driver call, not slow.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the worker binary is, or why we are not using one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkerPath {
    /// `PUNKTFUNK_ENCODE_WORKER=off` — the debug escape hatch that makes the worker/in-process
    /// A/B a one-line change.
    Off,
    Found(PathBuf),
    /// Not beside the host binary and not on `PATH` (a source build, or a partial install).
    Missing,
}

/// Resolve the worker binary: `PUNKTFUNK_ENCODE_WORKER` → alongside `/proc/self/exe` → `PATH`.
///
/// The env override is **load-bearing on NixOS**, not a convenience: a file capability cannot live
/// on a read-only store path, so the module exposes the worker through `security.wrappers` and
/// points this variable at the wrapper (an ambient grant is fine *there* — the worker is not a KWin
/// client). Pure and fully injected so the table can be tested without mutating process env, which
/// races `getenv` in parallel tests.
fn resolve_worker_path_in(
    env: Option<&str>,
    exe_dir: Option<&Path>,
    path_var: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> WorkerPath {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        if v.eq_ignore_ascii_case("off") {
            return WorkerPath::Off;
        }
        // Deliberately NOT existence-checked. An operator who names a path is entitled to a
        // failure that names it back — the spawn rung's warn carries the path, where a silent
        // fall-through to "beside the host binary" would hide the typo behind a working stream.
        return WorkerPath::Found(PathBuf::from(v));
    }
    if let Some(p) = exe_dir.map(|d| d.join(WORKER_BIN)).filter(|p| exists(p)) {
        return WorkerPath::Found(p);
    }
    if let Some(p) = path_var
        .into_iter()
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(WORKER_BIN))
        .find(|p| exists(p))
    {
        return WorkerPath::Found(p);
    }
    WorkerPath::Missing
}

/// [`resolve_worker_path_in`] against the real process.
pub(crate) fn resolve_worker_path() -> WorkerPath {
    // `current_exe()` readlinks `/proc/self/exe`, which reads `"<path> (deleted)"` after the
    // binary was replaced under a running host — harmless HERE, because only the parent directory
    // is used and the suffix lands on the file name. (The worker we then resolve is pinned by fd
    // before it is exec'd, which is where that trap actually bites.)
    let exe_dir = std::env::current_exe().ok();
    let path_var = std::env::var("PATH").ok();
    resolve_worker_path_in(
        std::env::var("PUNKTFUNK_ENCODE_WORKER").ok().as_deref(),
        exe_dir.as_deref().and_then(Path::parent),
        path_var.as_deref(),
        &|p| p.is_file(),
    )
}

/// The negotiated session's parameters, kept so the in-process fallback can be opened at ANY point
/// mid-session with exactly what the worker was opened with.
#[derive(Clone, Copy)]
struct Params {
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaFormat,
}

/// A per-frame failure and what it means for the session.
enum Fail {
    /// The transport broke: the worker is gone or wedged. Surfaces as an `Err` from `submit` so
    /// the host's existing encoder-rebuild path runs — [`RemotePyroWave::reset`] is where the
    /// single respawn attempt lives.
    Dead(anyhow::Error),
    /// The worker is alive and said no to this frame (an unimportable dmabuf, a frame that is not
    /// the session's mode). Pins the session in-process, because the recovery machinery for those
    /// causes lives there: the raw-dmabuf degrade latch is a process-wide static in the HOST, and
    /// an import failure noted inside the worker dies with it.
    Encode(String),
}

/// A live worker: its socket, its child, and the two caches that keep the steady state free of
/// descriptors.
#[derive(Debug)]
struct Link {
    sock: OwnedFd,
    child: Option<Child>,
    /// The worker's AU return buffer (a memfd), received once in `Ready`. Every AU is `pwrite`n at
    /// offset 0 there and `pread` back here — see [`super::worker`] for why the bytes cannot ride
    /// in the message body. `None` only between the spawn and the handshake, because the `Link`
    /// exists that early so its `Drop` reaps the child on every failure path.
    au_buf: Option<File>,
    rbuf: Vec<u8>,
    /// Dmabuf keys whose fd the worker already holds; the fd crosses only on first sight.
    sent_keys: HashSet<u64>,
    /// The cursor bitmap `serial` the worker has pixels for. A moving pointer re-sends position
    /// only, exactly as the in-process blend re-uses its uploaded texture.
    cursor_serial: Option<u64>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // The worker exits on socket EOF, which is `self.sock` dropping right after this. Hand the
        // child to the shared reaper rather than waiting on it: a worker wedged inside a driver
        // ioctl sits in D state and ignores SIGKILL, and session teardown must never block behind
        // a process that may never die (plan §4 R3).
        if let Some(child) = self.child.take() {
            ipc::park_child(child);
        }
    }
}

impl Link {
    /// Send one request and take its single reply. Every host→worker message has exactly one
    /// reply, so the two sides cannot desync into "whose turn is it".
    fn request(&mut self, msg: &ToWorker, fds: &[BorrowedFd]) -> Result<FromWorker> {
        worker::send_eintr(self.sock.as_fd(), msg, fds).context("send to the encode worker")?;
        let (reply, _) = worker::recv_eintr::<FromWorker>(
            self.sock.as_fd(),
            &mut self.rbuf,
            Some(REPLY_TIMEOUT),
        )
        .context("no reply from the encode worker")?;
        Ok(reply)
    }

    /// Encode one frame across the socket. The reply doubles as the buffer-release signal, which
    /// is exactly `Encoder::submit`'s existing lifetime contract — the caller already holds the
    /// frame alive until its AU comes back.
    fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame, Fail> {
        let FramePayload::Dmabuf(d) = &frame.payload else {
            // Unreachable: `submit` routes every non-dmabuf payload in-process before reaching here.
            return Err(Fail::Encode("not a dmabuf payload".into()));
        };
        let key = dmabuf_key(d.fd.as_fd()).map_err(|e| Fail::Dead(e.into()))?;
        // The upload is built ONCE and re-sent on a `NeedFd` retry: the worker drops every
        // descriptor of a frame it refuses, so a retry that dropped the cursor would blend a stale
        // pointer for the rest of that bitmap's life.
        // An EMPTY bitmap uploads too, deliberately: the invariant the worker checks is "the host
        // has sent pixels for every serial it asks me to blend", and an empty overlay is a real,
        // handled state (`prep_cursor` takes its no-cursor arm on one). Special-casing it here
        // would make "no upload" mean two different things — new-and-empty, or already-sent — and
        // the worker could no longer tell a desync from a blank pointer.
        let upload = match frame.cursor.as_ref() {
            Some(c) if self.cursor_serial != Some(c.serial) => Some(
                worker::cursor_upload(&c.rgba)
                    .map_err(|e| Fail::Dead(anyhow::Error::from(e).context("stage the cursor")))?,
            ),
            _ => None,
        };
        let cursor = frame.cursor.as_ref().map(|c| WireCursor {
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            serial: c.serial,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            visible: c.visible,
            upload: upload.as_ref().map(|(_, n)| *n),
        });

        let mut attempts = 0;
        let reply = loop {
            attempts += 1;
            let has_fd = self.sent_keys.insert(key);
            let msg = ToWorker::Frame {
                key,
                has_fd,
                fourcc: d.fourcc,
                modifier: d.modifier,
                offset: d.offset,
                stride: d.stride,
                plane1: d.plane1,
                width: frame.width,
                height: frame.height,
                pts_ns: frame.pts_ns,
                format: frame.format.into(),
                cursor: cursor.clone(),
            };
            // Descriptor ORDER is the protocol: dmabuf first (iff first sight), cursor second.
            let mut fds: Vec<BorrowedFd> = Vec::new();
            if has_fd {
                fds.push(d.fd.as_fd());
            }
            if let Some((f, _)) = upload.as_ref() {
                fds.push(f.as_fd());
            }
            match self.request(&msg, &fds).map_err(Fail::Dead)? {
                // The worker's fd cache evicted this key (or the two diverged): forget our
                // "already sent" note and retry ONCE, with the fd.
                FromWorker::NeedFd if attempts == 1 => {
                    self.sent_keys.remove(&key);
                    continue;
                }
                FromWorker::NeedFd => {
                    return Err(Fail::Dead(anyhow::anyhow!(
                        "the encode worker still lacks the dmabuf fd after a resend (desync)"
                    )))
                }
                other => break other,
            }
        };
        match reply {
            FromWorker::Au {
                len,
                pts_ns,
                keyframe,
                chunk_aligned,
                encode_us,
                ..
            } => {
                let mut data = vec![0u8; len];
                self.au_buf
                    .as_ref()
                    .ok_or_else(|| {
                        Fail::Dead(anyhow::anyhow!("no AU return buffer (no handshake)"))
                    })?
                    .read_exact_at(&mut data, 0)
                    .map_err(|e| {
                        Fail::Dead(anyhow::Error::from(e).context("read the AU return buffer"))
                    })?;
                if let Some(c) = frame.cursor.as_ref() {
                    if upload.is_some() {
                        self.cursor_serial = Some(c.serial);
                    }
                }
                tracing::trace!(len, encode_us, "pyrowave: AU from the encode worker");
                Ok(EncodedFrame {
                    data,
                    pts_ns,
                    keyframe,
                    recovery_anchor: false,
                    chunk_aligned,
                })
            }
            FromWorker::EncodeErr { message } => Err(Fail::Encode(message)),
            other => Err(Fail::Dead(anyhow::anyhow!(
                "unexpected encode worker reply to a frame: {other:?}"
            ))),
        }
    }
}

/// The dmabuf's identity across frames: its inode. dma-buf objects live on one anonymous inode
/// filesystem and the number is unique per object, which is what makes the fd-identity cache
/// possible — the same key the zerocopy worker's importer uses.
fn dmabuf_key(fd: BorrowedFd) -> io::Result<u64> {
    // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value; `fstat` writes
    // into the live, correctly-sized `&mut st` and only reads `fd`, which the caller keeps open
    // for the duration. `st_ino` is read only after the return value is checked.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st.st_ino as u64)
    }
}

/// What a completed handshake told us about the worker.
#[derive(Debug)]
struct Handshake {
    link: Link,
    caps: EncoderCaps,
    priority: PriorityOutcome,
    device: String,
}

/// Spawn a worker on `exe` and complete the handshake. Any `Err` is a ladder rung: the caller
/// warns once and encodes in-process.
fn spawn_link(exe: &Path, p: &Params, bitrate_bps: u64) -> Result<Handshake> {
    // Pin the WORKER's inode, never `self_exe()`: this binary is a different file from the host by
    // construction (it carries the capability the host must never have). Pinning also means a
    // package upgrade landing between here and the exec still runs the build we resolved.
    let pinned =
        ipc::PinnedExe::open(exe).with_context(|| format!("open {} for exec", exe.display()))?;
    let (sock, child) = ipc::spawn_worker(&pinned.exec_path(), WORKER_BIN, &["--fd", "3"])
        .with_context(|| format!("spawn {}", exe.display()))?;
    // Built before the handshake ON PURPOSE: its `Drop` is what hands the child to the reaper, so
    // every `?` in `handshake` reaps rather than leaving a worker behind.
    handshake(
        Link {
            sock,
            child: Some(child),
            au_buf: None,
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
            cursor_serial: None,
        },
        p,
        bitrate_bps,
    )
}

/// The handshake itself, split from the spawn so the ladder's rungs are testable against a plain
/// socket — and, more to the point, so the tests exercise THIS code rather than a copy of it that
/// can drift from it.
fn handshake(mut link: Link, p: &Params, bitrate_bps: u64) -> Result<Handshake> {
    let hello = ToWorker::Hello {
        proto: worker::PROTO_VERSION,
        workspace_version: worker::WORKSPACE_VERSION.to_string(),
        drm_node: std::env::var("PUNKTFUNK_RENDER_NODE").ok(),
        width: p.width,
        height: p.height,
        fps: p.fps,
        bitrate_bps,
        chroma444: p.chroma.is_444(),
        // Resolved HERE and forwarded explicitly. The worker strips this variable from its own
        // environment, so the operator's knob cannot silently mean something different across
        // the process boundary.
        priority_intent: std::env::var("PYROWAVE_QUEUE_PRIORITY").ok(),
    };
    worker::send_eintr(link.sock.as_fd(), &hello, &[]).context("send Hello")?;
    let (ready, fds) = worker::recv_eintr::<FromWorker>(
        link.sock.as_fd(),
        &mut link.rbuf,
        Some(HANDSHAKE_TIMEOUT),
    )
    .context("encode worker handshake (died on startup?)")?;
    match ready {
        FromWorker::Ready {
            proto,
            workspace_version,
            priority,
            device,
            chroma444,
            blends_cursor,
        } => {
            // Load-bearing, unlike the zerocopy worker's formality of a check: host and worker are
            // different FILES here, so a channel that shipped them out of lockstep is a real
            // deployment state — and it must degrade to the in-process encoder, not to a session
            // that cannot decode its own peer.
            if proto != worker::PROTO_VERSION || workspace_version != worker::WORKSPACE_VERSION {
                bail!(
                    "encode worker version skew: worker proto {proto} v{workspace_version}, \
                     host proto {} v{} — host and worker must ship lockstep",
                    worker::PROTO_VERSION,
                    worker::WORKSPACE_VERSION
                );
            }
            let au_buf = fds
                .into_iter()
                .next()
                .context("Ready carried no AU return buffer")?;
            link.au_buf = Some(File::from(au_buf));
            Ok(Handshake {
                link,
                caps: EncoderCaps {
                    // The REAL opened values, not a guess: a hardcoded default mis-reports a
                    // 4:4:4 open and fires the session glue's spurious "chroma disagrees with the
                    // negotiated Welcome" warn.
                    blends_cursor,
                    chroma_444: chroma444,
                    ..EncoderCaps::default()
                },
                priority,
                device,
            })
        }
        FromWorker::InitErr { message } => {
            bail!("encode worker could not open its encoder: {message}")
        }
        other => bail!("unexpected encode worker handshake: {other:?}"),
    }
}

/// Open a PyroWave session, preferring the capability-carrying worker.
///
/// This is the ONE seam the Linux `open_video` PyroWave arms go through. It is deliberately not
/// wired into the Windows arm: that platform has no such worker, and pointing it at a Linux-only
/// binary would be a fallback rung firing on every Windows session.
pub(crate) fn open_preferring_worker(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    chroma: ChromaFormat,
) -> Result<Box<dyn Encoder>> {
    let params = Params {
        width,
        height,
        fps,
        chroma,
    };
    // Every rung below ends here — the in-process encoder, exactly as it opened before this
    // worker existed.
    let inline = || -> Result<Box<dyn Encoder>> {
        super::pyrowave::PyroWaveEncoder::open(width, height, fps, bitrate_bps, chroma)
            .map(|e| Box::new(e) as Box<dyn Encoder>)
    };
    let path = match resolve_worker_path() {
        WorkerPath::Off => {
            tracing::info!(
                "pyrowave: PUNKTFUNK_ENCODE_WORKER=off — encoding in-process at default GPU priority"
            );
            return inline();
        }
        WorkerPath::Missing => {
            tracing::warn!(
                worker = WORKER_BIN,
                "pyrowave: the encode worker was not found beside the host binary or on PATH — \
                 encoding in-process at default GPU priority (the GPU-preemption lever needs the \
                 capability-carrying worker; set PUNKTFUNK_ENCODE_WORKER to its path)"
            );
            return inline();
        }
        WorkerPath::Found(p) => p,
    };
    let hs = match spawn_link(&path, &params, bitrate_bps) {
        Ok(hs) => hs,
        Err(e) => {
            tracing::warn!(
                worker = %path.display(),
                error = %format!("{e:#}"),
                "pyrowave: the encode worker did not come up — encoding in-process at default \
                 GPU priority"
            );
            return inline();
        }
    };
    match hs.priority {
        PriorityOutcome::Granted(class) => tracing::info!(
            priority = ?class,
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: encoding in the capability-carrying worker at an elevated global queue \
             priority (the encode dispatch preempts a GPU-bound game where the driver honors it)"
        ),
        PriorityOutcome::Refused => tracing::warn!(
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: every global queue priority class was refused — encoding at default \
             priority. The GPU-preemption lever is INERT without CAP_SYS_NICE on the encode \
             WORKER binary (never on punktfunk-host — a capability there makes the host \
             unidentifiable to KWin and kills desktop streaming); PYROWAVE_QUEUE_PRIORITY=off \
             silences this"
        ),
        PriorityOutcome::NotRequested => tracing::info!(
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: encoding in the capability-carrying worker, no queue priority requested"
        ),
    }
    Ok(Box::new(RemotePyroWave {
        link: Some(hs.link),
        inline: None,
        params,
        bitrate_bps,
        worker_path: path,
        caps: hs.caps,
        wire_chunk: None,
        pending: VecDeque::new(),
        chunker: None,
        respawn_used: false,
    }))
}

/// A PyroWave session encoded in `punktfunk-encode-worker`, with the in-process encoder underneath
/// it as the floor. See the module docs for the ladder.
pub(crate) struct RemotePyroWave {
    /// The live worker, or `None` once this session is pinned in-process.
    link: Option<Link>,
    /// The in-process encoder, opened lazily on any mid-session rung. Once open it serves the rest
    /// of the session — the worker is not re-attempted per frame.
    inline: Option<super::pyrowave::PyroWaveEncoder>,
    params: Params,
    /// The live rate, so a fallback opens the in-process encoder at the bitrate ABR last set
    /// rather than the one the session started with.
    bitrate_bps: u64,
    worker_path: PathBuf,
    caps: EncoderCaps,
    /// The datagram-aligned boundary, mirrored HERE as well as forwarded. It has to cross (it
    /// changes the AU bytes), and it has to be kept (it decides this proxy's own chunked-poll
    /// answers) — see [`Self::poll_chunk`].
    wire_chunk: Option<usize>,
    /// AUs the worker returned and the caller has not polled yet. Empty in in-process mode, where
    /// the inline encoder owns its own queue — except for whatever was still here when the
    /// fallback fired, which [`Self::poll_whole`] drains first.
    pending: VecDeque<EncodedFrame>,
    /// The AU currently being handed out in streamed chunks — the proxy's, in BOTH modes, so
    /// exactly one chunker can ever be open (see [`Self::poll_chunk`]).
    chunker: Option<AuChunker>,
    /// One respawn per session. After that the in-process encoder is the answer: a worker that
    /// dies twice is a worker that will keep dying, and burning the host's five-reset budget on it
    /// costs the session.
    respawn_used: bool,
}

impl RemotePyroWave {
    /// Drop the worker and encode in-process for the rest of the session, with the one warn every
    /// mid-session rung owes.
    fn pin_inline(&mut self, reason: &str) {
        if self.link.take().is_some() {
            tracing::warn!(
                worker = %self.worker_path.display(),
                reason,
                "pyrowave: leaving the encode worker — encoding in-process at default GPU \
                 priority for the rest of this session"
            );
        }
    }

    /// The in-process encoder, opened on demand at the session's CURRENT parameters and with the
    /// state the caller set through the trait replayed onto it.
    fn inline_mut(&mut self) -> Result<&mut super::pyrowave::PyroWaveEncoder> {
        if self.inline.is_none() {
            let mut e = super::pyrowave::PyroWaveEncoder::open(
                self.params.width,
                self.params.height,
                self.params.fps,
                self.bitrate_bps,
                self.params.chroma,
            )
            .context("open the in-process PyroWave encoder after leaving the worker")?;
            // Replay: the boundary changes the AU BYTES, so a fallback that forgot it would ship
            // dense AUs flagged as datagram-aligned. (The bitrate needs no replay — it is an open
            // parameter above.)
            if let Some(shard) = self.wire_chunk {
                e.set_wire_chunking(shard);
            }
            self.inline = Some(e);
        }
        Ok(self.inline.as_mut().expect("just opened"))
    }

    /// One whole AU from whichever half is live, worker leftovers first.
    fn poll_whole(&mut self) -> Result<Option<EncodedFrame>> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        match self.inline.as_mut() {
            Some(e) => e.poll(),
            None => Ok(None),
        }
    }
}

impl Encoder for RemotePyroWave {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // A CPU-backed frame can genuinely reach this encoder — a 4:4:4 PyroWave session with
        // zero-copy off takes the host's `force_cpu_for_nvenc_444` arm, and the process-wide
        // raw-dmabuf degrade latch flips later sessions to CPU delivery — and it must never cross
        // the socket: 1080p BGRA is ~8 MB, i.e. ~480 MB/s at 60 fps. The in-process encoder
        // uploads it straight into its own device, which is what the CPU arm of `submit_frame`
        // has always done, so this is just one more rung of the same ladder.
        if self.link.is_some() && !matches!(frame.payload, FramePayload::Dmabuf(_)) {
            self.pin_inline(
                "capture delivered a non-dmabuf frame, which the worker path cannot take",
            );
        }
        if self.link.is_some() {
            match self
                .link
                .as_mut()
                .expect("checked just above")
                .encode(frame)
            {
                Ok(au) => {
                    self.pending.push_back(au);
                    return Ok(());
                }
                Err(Fail::Dead(e)) => {
                    // The worker is gone. Surface it: the host's encoder-rebuild path runs, and
                    // `reset` below is where the single respawn attempt lives.
                    self.link = None;
                    return Err(e.context("pyrowave encode worker"));
                }
                Err(Fail::Encode(message)) => {
                    // The worker is alive but refused this frame. Re-run it in-process — the
                    // recovery machinery for every cause of this lives on THIS side (the
                    // raw-dmabuf degrade latch is a host-process static), so the caller gets the
                    // in-process outcome, latch included, instead of a frame dropped in a
                    // subprocess.
                    self.pin_inline(&format!("the worker failed a frame: {message}"));
                }
            }
        }
        self.inline_mut()?.submit(frame)
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        // Mirrors the in-process impl, which takes the trait default: every PyroWave AU is a
        // keyframe, so there is no per-frame reference bookkeeping to pin to the wire index.
        let _ = wire_index;
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        // Once in-process, the live encoder is authoritative; before that, the values the worker
        // reported for the encoder it really opened.
        match self.inline.as_ref() {
            Some(e) => e.caps(),
            None => self.caps,
        }
    }

    fn request_keyframe(&mut self) {
        // Intra-only: every AU is already a keyframe (mirrors the in-process impl's default).
    }

    fn set_hdr_meta(&mut self, _meta: Option<punktfunk_core::quic::HdrMeta>) {
        // PyroWave carries no VUI/SEI grade — the colour contract is the CSC shader's.
    }

    fn invalidate_ref_frames(&mut self, _first_frame: i64, _last_frame: i64) -> bool {
        // No references to invalidate.
        false
    }

    fn set_pipelined(&mut self, _on: bool) -> bool {
        // No pipelined-retrieve mode; the encode is synchronous by design.
        false
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Trait contract (PW6): each AU is drained through ONE method. Same wording as the
        // in-process impl, because it is the same caller bug.
        if self.chunker.is_some() {
            bail!("pyrowave: poll() on an AU already being drained through poll_chunk");
        }
        self.poll_whole()
    }

    fn supports_chunked_poll(&self) -> bool {
        // Exactly the in-process answer: the cut rule is a pure function of the boundary, and the
        // boundary is mirrored here.
        stream_chunk_step(self.wire_chunk).is_some()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // The streamed-AU cut needs NO protocol. `submit` is synchronous on both sides of the
        // socket, so an AU that reached `pending` is complete by construction and the identical
        // `AuChunker` runs here — sub-frame send latency survives at zero wire cost. The chunker
        // is the proxy's in BOTH modes (the fallback's `poll_chunk` is never called), so exactly
        // one cursor can ever be open.
        if let Some(c) = self.chunker.as_mut() {
            if let Some(chunk) = c.next() {
                return Ok(Some(chunk));
            }
            self.chunker = None;
        }
        let Some(f) = self.poll_whole()? else {
            return Ok(None);
        };
        match stream_chunk_step(self.wire_chunk) {
            Some(step) => Ok(self.chunker.insert(AuChunker::new(f, step)).next()),
            None => Ok(Some(AuChunk::whole(f))),
        }
    }

    fn reset(&mut self) -> bool {
        // A rebuild forfeits every in-flight frame, including an AU only half-handed-out — drop
        // the cursor first so the next `poll_chunk` cannot splice the tail of a dead AU onto a
        // fresh one.
        self.chunker = None;
        self.pending.clear();
        if let Some(link) = self.link.as_mut() {
            // A message, not a respawn, and deliberately: the expensive, capability-dependent
            // thing is the priority-elevated DEVICE, and an in-worker reset keeps it. A respawn
            // would re-run the whole global-priority ladder mid-session and could silently land on
            // a different class than the one this session has been measured at — changing the very
            // quantity the worker exists to protect — and it would pay the full Vulkan (and, until
            // WP-D, FFmpeg) load inside the stall watchdog's recovery window. The in-process
            // `reset` it forwards to is already the bounded, wedge-aware rebuild: it re-waits the
            // in-flight fences under a 5 s cap and reports failure rather than destroying a
            // pyrowave encoder under live GPU work. And if the worker is itself wedged, this
            // request times out and falls through to the rung below — so respawn stays reachable
            // as the failure path without being the policy.
            match link.request(&ToWorker::Reset, &[]) {
                Ok(FromWorker::Ack { ok }) => return ok,
                Ok(other) => {
                    tracing::warn!(?other, "pyrowave: unexpected encode worker reply to Reset");
                    self.link = None;
                }
                Err(e) => {
                    tracing::warn!(
                        worker = %self.worker_path.display(),
                        error = %format!("{e:#}"),
                        "pyrowave: the encode worker died mid-session — rebuilding the encoder"
                    );
                    self.link = None;
                }
            }
        }
        // One respawn, and only while nothing has fallen back yet.
        if self.link.is_none() && self.inline.is_none() && !self.respawn_used {
            self.respawn_used = true;
            match spawn_link(&self.worker_path, &self.params, self.bitrate_bps) {
                Ok(hs) => {
                    self.caps = hs.caps;
                    self.link = Some(hs.link);
                    if let Some(shard) = self.wire_chunk {
                        // Same replay the in-process fallback does, for the same reason.
                        self.set_wire_chunking(shard);
                    }
                    tracing::info!(
                        worker = %self.worker_path.display(),
                        priority = ?hs.priority,
                        "pyrowave: respawned the encode worker after a mid-session death"
                    );
                    return true;
                }
                Err(e) => tracing::warn!(
                    worker = %self.worker_path.display(),
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker would not respawn — encoding in-process at \
                     default GPU priority for the rest of this session"
                ),
            }
        }
        let already_open = self.inline.is_some();
        match self.inline_mut() {
            // A freshly opened in-process encoder IS the rebuild the caller asked for.
            Ok(e) => {
                if already_open {
                    e.reset()
                } else {
                    true
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "pyrowave: no encoder left after the worker went away — the session cannot \
                     recover"
                );
                false
            }
        }
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Kept regardless of which half is live: a later in-process fallback opens at the rate ABR
        // actually settled on, not the one the session started with.
        self.bitrate_bps = bps;
        if let Some(link) = self.link.as_mut() {
            match link.request(&ToWorker::Reconfigure { bitrate_bps: bps }, &[]) {
                Ok(FromWorker::Ack { ok }) => return ok,
                Ok(other) => {
                    tracing::warn!(
                        ?other,
                        "pyrowave: unexpected encode worker reply to Reconfigure"
                    )
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker did not accept a bitrate retarget"
                ),
            }
            // Not a lie to the ABR controller: report failure and let it use its rebuild path,
            // which lands on `reset` above.
            return false;
        }
        match self.inline.as_mut() {
            Some(e) => e.reconfigure_bitrate(bps),
            // Nothing is open yet, and the new rate is now the one the open will use.
            None => true,
        }
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // Mirrors the in-process impl (the trait default): PyroWave applies the requested rate as
        // a per-frame byte budget with no internal clamp to report.
        None
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        // The same sanity floor as the in-process impl, applied HERE so the mirrored state and the
        // worker's can never disagree about whether chunking is on.
        if shard_payload < 64 {
            return;
        }
        self.wire_chunk = Some(shard_payload);
        if let Some(link) = self.link.as_mut() {
            // This one really does have to cross: it changes the packetize boundary and the rate
            // budget, i.e. the AU BYTES. Only the streamed-AU CUT stays host-side.
            if let Err(e) = link.request(&ToWorker::SetWireChunking { shard_payload }, &[]) {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker did not accept the datagram-aligned boundary"
                );
            }
            // Deliberately NOT word-for-word the in-process line: the worker emits that one
            // itself (to inherited stderr), and two identical sentences from two processes read
            // as a bug. This is the host-ring copy — the web console's Logs tab only ever sees
            // this process — and it says what actually happened here.
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization forwarded to the encode worker \
                 (partial-frame loss mode)"
            );
        }
        if let Some(e) = self.inline.as_mut() {
            e.set_wire_chunking(shard_payload);
        }
    }

    fn set_send_spread_us(&mut self, _us: u32) {
        // Only the direct-NVENC split arbitration consumes this; PyroWave never splits.
    }

    fn set_input_ring_depth(&mut self, _depth: usize) {
        // The encoder imports the capture dmabuf and CSCs it into its own images, so the
        // capturer's ring depth constrains nothing here.
    }

    fn flush(&mut self) -> Result<()> {
        // Nothing is ever in flight ACROSS the socket: `submit` returns only once the AU is in
        // `pending`, so there is no worker-side backlog a flush could drain — unlike the
        // in-process encoder, whose `submit`/`poll` split really does leave a fence unwaited.
        match self.inline.as_mut() {
            Some(e) => e.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::worker::GrantedClass;
    use super::*;
    use pf_frame::{CursorOverlay, DmabufFrame, PixelFormat};
    use std::os::fd::AsFd;
    use std::sync::Arc;

    fn params() -> Params {
        Params {
            width: 1920,
            height: 1080,
            fps: 60,
            chroma: ChromaFormat::Yuv420,
        }
    }

    /// A `Link` wired to a socket the test drives itself — the shape pf-zerocopy's importer tests
    /// use, and the only way to exercise the per-frame path without a GPU.
    fn mock_link(sock: OwnedFd, au_buf: File) -> Link {
        Link {
            sock,
            child: None,
            au_buf: Some(au_buf),
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
            cursor_serial: None,
        }
    }

    /// [`handshake`] driven against a socket instead of a spawned child — the process rungs get
    /// their own tests; this one exercises what the worker SAYS.
    fn handshake_on(sock: OwnedFd) -> Result<Handshake> {
        handshake(mock_link(sock, File::from(memfd())), &params(), 40_000_000)
    }

    /// A dmabuf-shaped frame whose "dmabuf" is a memfd: the host half only fstats the descriptor
    /// and passes it, so this exercises the real key/cache/`SCM_RIGHTS` path.
    fn frame(fd: OwnedFd, cursor: Option<CursorOverlay>) -> CapturedFrame {
        CapturedFrame {
            width: 1920,
            height: 1080,
            pts_ns: 42,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Dmabuf(DmabufFrame {
                fd,
                fourcc: 0x3432_5258,
                modifier: 0,
                plane1: None,
                offset: 0,
                stride: 1920 * 4,
                hold: None,
            }),
            cursor,
        }
    }

    fn memfd() -> OwnedFd {
        let (f, _) = worker::cursor_upload(&[]).unwrap();
        OwnedFd::from(f)
    }

    #[test]
    fn path_resolution_table() {
        let here = Path::new("/opt/punktfunk/bin");
        let installed = here.join(WORKER_BIN);
        let on_path = Path::new("/usr/bin").join(WORKER_BIN);
        let exists = {
            let (a, b) = (installed.clone(), on_path.clone());
            move |p: &Path| p == a || p == b
        };

        // `off`, in every spelling the row promises — the debug escape hatch.
        for v in ["off", "OFF", " Off "] {
            assert_eq!(
                resolve_worker_path_in(Some(v), Some(here), Some("/usr/bin"), &exists),
                WorkerPath::Off
            );
        }
        // An explicit path wins over both discoveries — the NixOS wrapper case…
        assert_eq!(
            resolve_worker_path_in(
                Some("/run/wrappers/bin/punktfunk-encode-worker"),
                Some(here),
                Some("/usr/bin"),
                &exists
            ),
            WorkerPath::Found("/run/wrappers/bin/punktfunk-encode-worker".into())
        );
        // …and it is NOT existence-checked, so a typo surfaces as a spawn failure naming the path
        // instead of silently falling through to a worker that happens to be installed.
        assert_eq!(
            resolve_worker_path_in(
                Some("/nope/pf-worker"),
                Some(here),
                Some("/usr/bin"),
                &exists
            ),
            WorkerPath::Found("/nope/pf-worker".into())
        );
        // Empty/whitespace reads as unset, not as a path.
        assert_eq!(
            resolve_worker_path_in(Some("  "), Some(here), Some("/usr/bin"), &exists),
            WorkerPath::Found(installed.clone())
        );
        // Beside the host binary beats PATH.
        assert_eq!(
            resolve_worker_path_in(None, Some(here), Some("/usr/bin"), &exists),
            WorkerPath::Found(installed)
        );
        // Then PATH, entry by entry, skipping empties.
        assert_eq!(
            resolve_worker_path_in(
                None,
                Some(Path::new("/nowhere")),
                Some(":/nope:/usr/bin"),
                &exists
            ),
            WorkerPath::Found(on_path)
        );
        // Nothing anywhere: the "missing binary" rung.
        assert_eq!(
            resolve_worker_path_in(None, Some(Path::new("/nowhere")), Some("/nope"), &exists),
            WorkerPath::Missing
        );
        assert_eq!(
            resolve_worker_path_in(None, None, None, &exists),
            WorkerPath::Missing
        );
    }

    /// A binary that exists, execs, and exits at once. Resolved off `PATH` rather than hardcoded
    /// to `/bin/false`: NixOS ships only `/bin/sh` in `/bin`, and `PinnedExe::open` needs a real
    /// path (so a bare name cannot stand in for one — it would fail the OPEN and take the
    /// spawn-failure rung instead of the handshake rung this exercises).
    fn a_binary_that_exits_immediately() -> PathBuf {
        std::env::var_os("PATH")
            .as_deref()
            .map(std::env::split_paths)
            .into_iter()
            .flatten()
            .map(|d| d.join("false"))
            .find(|p| p.is_file())
            .expect("a `false` binary on PATH")
    }

    /// Ladder rung: the binary exists and runs but is not a worker. It exits at once, so the
    /// handshake reads EOF — the same rung a worker that dies during Vulkan bring-up takes.
    #[test]
    fn a_worker_that_exits_immediately_is_a_handshake_failure() {
        let err =
            spawn_link(&a_binary_that_exits_immediately(), &params(), 40_000_000).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("handshake"),
            "the rung must name the handshake: {text}"
        );
    }

    /// Ladder rung: a binary that does not exist at all — an operator-set `PUNKTFUNK_ENCODE_WORKER`
    /// typo, or a half-installed package.
    #[test]
    fn a_missing_binary_is_a_spawn_failure() {
        let err = spawn_link(
            Path::new("/nonexistent/punktfunk-encode-worker"),
            &params(),
            40_000_000,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("punktfunk-encode-worker"));
    }

    /// Ladder rung: the worker started, spoke, and could not open its encoder (no Vulkan 1.3
    /// device, a missing feature). Not a death — an ANSWER, and the host encodes in-process,
    /// where the same cause will either reproduce or turn out to have been the worker's own
    /// environment.
    #[test]
    fn an_init_error_fails_the_handshake_without_looking_like_a_death() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None);
            worker::send_eintr(
                peer.as_fd(),
                &FromWorker::InitErr {
                    message: "GPU lacks pyrowave-required Vulkan features".into(),
                },
                &[],
            )
            .unwrap();
        });
        let err = handshake_on(host).unwrap_err();
        server.join().unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("could not open its encoder") && text.contains("Vulkan features"),
            "the rung must carry the worker's own diagnosis: {text}"
        );
    }

    /// Ladder rung: a dead worker gets **one** respawn from `reset`, then the session is
    /// in-process for good. Driven with a dead socket and `/bin/false` as the worker binary, so
    /// the respawn attempt is real and fails; what is pinned here is the budget, which is the part
    /// that must not drift — a proxy that retried every `reset` would burn the host's five-reset
    /// recovery budget on a worker that is not coming back and end the session.
    #[test]
    fn reset_respawns_the_worker_at_most_once() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        drop(peer); // the worker is already gone
        let mut proxy = RemotePyroWave {
            link: Some(mock_link(host, File::from(memfd()))),
            inline: None,
            params: params(),
            bitrate_bps: 40_000_000,
            // Spawns, then exits without a handshake — the respawn attempt runs for real and
            // fails, which is the case this budget exists for.
            worker_path: PathBuf::from("/bin/false"),
            caps: EncoderCaps::default(),
            wire_chunk: None,
            pending: VecDeque::new(),
            chunker: None,
            respawn_used: false,
        };
        // The reply never comes: the link dies, the one respawn is spent and fails, and the
        // session is in-process from here.
        let _ = proxy.reset();
        assert!(proxy.respawn_used, "the one respawn must have been spent");
        assert!(proxy.link.is_none(), "/bin/false cannot become a worker");
        // Every later reset stays in-process — the budget is spent, not renewed.
        let _ = proxy.reset();
        assert!(proxy.respawn_used);
        assert!(proxy.link.is_none());
    }

    /// Ladder rung: a CPU-backed frame. It genuinely reaches this encoder (a 4:4:4 session with
    /// zero-copy off, or after the raw-dmabuf degrade latch fires), and ~8 MB per frame must never
    /// cross the socket — so the session leaves the worker for good. Asserts the CLASSIFICATION
    /// (the link is dropped) and not the submit result, which depends on whether the box running
    /// the test has a Vulkan device to open the in-process encoder on.
    #[test]
    fn a_cpu_frame_leaves_the_worker_for_the_rest_of_the_session() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let mut proxy = RemotePyroWave {
            link: Some(mock_link(host, File::from(memfd()))),
            inline: None,
            params: Params {
                width: 64,
                height: 64,
                fps: 60,
                chroma: ChromaFormat::Yuv420,
            },
            bitrate_bps: 5_000_000,
            worker_path: PathBuf::from("/usr/bin/punktfunk-encode-worker"),
            caps: EncoderCaps::default(),
            wire_chunk: None,
            pending: VecDeque::new(),
            chunker: None,
            respawn_used: false,
        };
        let cpu = CapturedFrame {
            width: 64,
            height: 64,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0u8; 64 * 64 * 4]),
            cursor: None,
        };
        let _ = proxy.submit(&cpu);
        assert!(
            proxy.link.is_none(),
            "a non-dmabuf payload must pin the session in-process"
        );
        // Nothing was ever asked of the worker: the rung fires before the socket is touched.
        drop(proxy);
        let mut buf = Vec::new();
        assert_eq!(
            worker::recv_eintr::<ToWorker>(
                peer.as_fd(),
                &mut buf,
                Some(Duration::from_millis(200))
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    /// Ladder rung: proto/workspace skew. Host and worker are different files, so this is a real
    /// deployment state and must land on the in-process encoder rather than a broken session.
    #[test]
    fn version_skew_fails_the_handshake() {
        for (proto, version) in [
            (
                worker::PROTO_VERSION + 1,
                worker::WORKSPACE_VERSION.to_string(),
            ),
            (worker::PROTO_VERSION, "0.0.1-stale".to_string()),
        ] {
            let (host, peer) = ipc::socketpair_seqpacket().unwrap();
            let server = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None);
                worker::send_eintr(
                    peer.as_fd(),
                    &FromWorker::Ready {
                        proto,
                        workspace_version: version,
                        priority: PriorityOutcome::Granted(GrantedClass::Realtime),
                        device: "mock".into(),
                        chroma444: false,
                        blends_cursor: true,
                    },
                    &[],
                )
                .unwrap();
            });
            let err = handshake_on(host).unwrap_err();
            server.join().unwrap();
            assert!(
                format!("{err:#}").contains("version skew"),
                "unexpected error: {err:#}"
            );
        }
    }

    /// The per-frame path end to end, host side: the fd crosses ONCE, the cursor bitmap crosses
    /// only when its serial changes, and the AU comes back through the memfd rather than the
    /// message body.
    #[test]
    fn frames_pass_the_fd_once_and_the_au_comes_back_through_the_buffer() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let au_buf = File::from(memfd());
        let au = vec![0x7Eu8; 300_000];
        let expect = au.clone();
        let server_buf = au_buf.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut seen: Vec<(bool, Option<usize>)> = Vec::new();
            for _ in 0..3 {
                let (msg, fds) =
                    worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
                let ToWorker::Frame {
                    key,
                    has_fd,
                    cursor,
                    ..
                } = msg
                else {
                    panic!("expected a frame");
                };
                let want = usize::from(has_fd)
                    + usize::from(cursor.as_ref().is_some_and(|c| c.upload.is_some()));
                assert_eq!(fds.len(), want, "descriptor count must match the flags");
                seen.push((has_fd, cursor.and_then(|c| c.upload)));
                server_buf.write_all_at(&expect, 0).unwrap();
                worker::send_eintr(
                    peer.as_fd(),
                    &FromWorker::Au {
                        key,
                        len: expect.len(),
                        pts_ns: 42,
                        keyframe: true,
                        chunk_aligned: false,
                        encode_us: 4400,
                    },
                    &[],
                )
                .unwrap();
            }
            seen
        });

        let mut link = mock_link(host, au_buf);
        let cursor = |serial: u64| {
            Some(CursorOverlay {
                x: 1,
                y: 2,
                w: 32,
                h: 32,
                rgba: Arc::new(vec![9u8; 32 * 32 * 4]),
                serial,
                hot_x: 0,
                hot_y: 0,
                visible: true,
            })
        };
        // The SAME buffer three times (one memfd, re-passed), with the cursor bitmap changing once.
        let dmabuf = memfd();
        for c in [cursor(1), cursor(1), cursor(2)] {
            let f = frame(dmabuf.try_clone().unwrap(), c);
            let got = match link.encode(&f) {
                Ok(au) => au,
                Err(Fail::Dead(e)) => panic!("worker died: {e:#}"),
                Err(Fail::Encode(m)) => panic!("encode error: {m}"),
            };
            assert_eq!(got.data, au, "the AU must arrive byte-for-byte");
            assert_eq!(got.pts_ns, 42);
            assert!(got.keyframe);
        }
        let seen = server.join().unwrap();
        assert_eq!(
            seen,
            vec![
                (true, Some(32 * 32 * 4)),
                (false, None),
                (false, Some(32 * 32 * 4))
            ],
            "the dmabuf fd crosses once; the cursor crosses only on a serial change"
        );
    }

    /// A `NeedFd` (the worker evicted the key) is answered by ONE retry carrying the fd again.
    #[test]
    fn need_fd_resends_the_descriptor_once() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let au_buf = File::from(memfd());
        let server_buf = au_buf.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut flags = Vec::new();
            for i in 0..2 {
                let (msg, fds) =
                    worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
                let ToWorker::Frame { key, has_fd, .. } = msg else {
                    panic!("expected a frame");
                };
                flags.push((has_fd, fds.len()));
                let reply = if i == 0 {
                    FromWorker::NeedFd
                } else {
                    server_buf.write_all_at(&[1, 2, 3], 0).unwrap();
                    FromWorker::Au {
                        key,
                        len: 3,
                        pts_ns: 0,
                        keyframe: true,
                        chunk_aligned: false,
                        encode_us: 1,
                    }
                };
                worker::send_eintr(peer.as_fd(), &reply, &[]).unwrap();
            }
            flags
        });
        let mut link = mock_link(host, au_buf);
        // Pretend the key already crossed, so the first attempt sends no descriptor.
        let dmabuf = memfd();
        link.sent_keys.insert(dmabuf_key(dmabuf.as_fd()).unwrap());
        let f = frame(dmabuf, None);
        let au = match link.encode(&f) {
            Ok(au) => au,
            Err(Fail::Dead(e)) => panic!("worker died: {e:#}"),
            Err(Fail::Encode(m)) => panic!("encode error: {m}"),
        };
        assert_eq!(au.data, vec![1, 2, 3]);
        assert_eq!(server.join().unwrap(), vec![(false, 0), (true, 1)]);
    }

    /// Ladder rung: the worker dies mid-stream. The transport error must classify as `Dead` (the
    /// host's rebuild path, then one respawn) and never as a per-frame encode error.
    #[test]
    fn a_worker_that_dies_mid_stream_is_a_transport_death() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        drop(peer);
        let mut link = mock_link(host, File::from(memfd()));
        let f = frame(memfd(), None);
        match link.encode(&f) {
            Err(Fail::Dead(_)) => {}
            Err(Fail::Encode(m)) => panic!("a dead socket must not read as an encode error: {m}"),
            Ok(_) => panic!("a dead socket must not produce an AU"),
        }
    }

    /// …and a worker that is alive and refuses the frame classifies as `Encode`, which pins the
    /// session in-process so the raw-dmabuf degrade latch (a HOST-process static) still fires.
    #[test]
    fn a_refused_frame_is_an_encode_error() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
            worker::send_eintr(
                peer.as_fd(),
                &FromWorker::EncodeErr {
                    message: "unsupported dmabuf fourcc".into(),
                },
                &[],
            )
            .unwrap();
        });
        let mut link = mock_link(host, File::from(memfd()));
        let f = frame(memfd(), None);
        match link.encode(&f) {
            Err(Fail::Encode(m)) => assert!(m.contains("fourcc")),
            Err(Fail::Dead(e)) => panic!("a live worker's refusal must not read as death: {e:#}"),
            Ok(_) => panic!("a refusal must not produce an AU"),
        }
        server.join().unwrap();
    }

    /// WP7.7 guard, applied to the proxy: every `Encoder` trait method must be explicitly written
    /// here. The proxy has TWO backends under it, so an unforwarded default is worse than the
    /// `TrackedEncoder` case it copies — it would silently disable a feature only in the
    /// worker-backed half of the ladder, i.e. on exactly the hosts that got the new code path.
    /// Source-text parse, same as `tracked_encoder_forwards_every_trait_method`.
    #[test]
    fn the_proxy_writes_every_trait_method() {
        fn item_block<'a>(src: &'a str, marker: &str) -> &'a str {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("marker {marker:?} not found — update this guard"));
            let body = &src[start..];
            let end = body
                .find("\n}")
                .unwrap_or_else(|| panic!("no column-0 close brace after {marker:?}"));
            &body[..end]
        }
        fn fn_names(block: &str) -> std::collections::BTreeSet<&str> {
            block
                .lines()
                .map(str::trim_start)
                .filter(|l| !l.starts_with("//"))
                .filter_map(|l| l.strip_prefix("fn "))
                .map(|rest| {
                    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .next()
                        .expect("split yields at least one item")
                })
                .collect()
        }
        let trait_fns = fn_names(item_block(
            include_str!("../codec.rs"),
            "pub trait Encoder: Send {",
        ));
        let impl_fns = fn_names(item_block(
            include_str!("pyrowave_remote.rs"),
            "impl Encoder for RemotePyroWave {",
        ));
        assert!(
            trait_fns.len() >= 12,
            "only {} trait methods parsed — the extraction markers have rotted",
            trait_fns.len()
        );
        let missing: Vec<_> = trait_fns.difference(&impl_fns).collect();
        assert!(
            missing.is_empty(),
            "Encoder methods NOT written by RemotePyroWave: {missing:?} — an unforwarded default \
             silently disables the feature for every worker-backed session."
        );
        assert_eq!(trait_fns, impl_fns);
    }
}
