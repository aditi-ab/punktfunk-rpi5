//! `punktfunk-encode-worker` — the vocabulary both halves speak, and the worker half itself
//! (design: `design/gpu-priority-capability-worker.md` §3; plan §2/WP1). The host half is
//! [`super::pyrowave_remote`].
//!
//! **Why this process exists at all.** PyroWave encodes on the same shader cores a game
//! saturates, and the only lever that preempts it is an elevated `VK_KHR_global_priority` queue —
//! which the driver grants only to a process holding `CAP_SYS_NICE`. `punktfunk-host` may never
//! hold one: KWin identifies a client by `readlink /proc/<pid>/exe`, the kernel refuses that
//! readlink to a reader whose effective set is not a superset of the target's PERMITTED set, and
//! a capped host is therefore unidentifiable — 0.26.0-1 killed every KDE desktop session that
//! way. So the capability lives here, in a leaf that fronts nothing: no Wayland, no D-Bus, no
//! network, one socket to its parent.
//!
//! 🛑 This worker is a **separate executable file**, never a hardlink of the host and never a
//! subcommand of it (unlike the zerocopy worker, which deliberately re-execs the host image). A
//! shared inode shares the file capability, which silently re-creates 0.26.0-1.
//!
//! ## Shape
//!
//! One worker per PyroWave session, spawned at encoder open on the shared [`ipc`] rails (SEQPACKET
//! framing, `SCM_RIGHTS`, fd-3 inheritance, pinned-exe spawn, the zombie sweep). Strict
//! request/response: **every** host→worker message gets exactly one reply, so the two sides can
//! never desync into "whose turn is it".
//!
//! ## Where the bytes go (and why they are not in the JSON)
//!
//! [`ipc::MAX_MSG`] is 64 KiB and the bodies are serde_json, which renders a `Vec<u8>` as one
//! decimal number per byte. A PyroWave AU is `bitrate / (8 × fps)` — 83 KB at 1080p60/40 Mb/s,
//! ~830 KB at 4K — so an inline `bytes` field is not a slow path, it is *unrepresentable*, and
//! base64 would still need ~17 datagrams and ~0.8 ms of codec per frame against a +1.0 ms
//! whole-IPC-hop budget (plan §4 R1). The AU therefore rides a **memfd** the worker creates once
//! and `pwrite`s at offset 0 every frame; the host `pread`s exactly `len` bytes out of it. The fd
//! crosses once, in [`FromWorker::Ready`]. A memfd grows on write, so there is no capacity
//! negotiation and no regrow protocol — a bitrate retarget is invisible to it.
//!
//! Cursor bitmaps take the same route for the same reason (256×256 RGBA = 256 KiB > `MAX_MSG`),
//! except they are rare enough (only when the pointer *image* changes) that a fresh memfd rides
//! along with the frame instead of a persistent one.
//!
//! Frame pixels never cross at all: the dmabuf fd is passed on first sight of its `key` and the
//! worker caches it, so the steady state passes **zero** descriptors (the PipeWire pool recycles a
//! small buffer set).

use anyhow::{Context, Result};
use pf_frame::{CapturedFrame, CursorOverlay, DmabufFrame, FramePayload, PixelFormat};
use pf_zerocopy::ipc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bumped on any wire change. Unlike the zerocopy worker — the same binary as its host by
/// construction — host and worker are **different files** here, so this check is load-bearing:
/// a package that shipped them out of lockstep must degrade to the in-process encoder, never to
/// a dead session.
pub(crate) const PROTO_VERSION: u32 = 1;

/// The workspace version this half was compiled from. A protocol can be unchanged while the
/// *encoder* moves (a vendored-codec bump, a CSC shader change), and the two halves must still be
/// one build — so the handshake compares this too. `env!` resolves at compile time of THIS crate,
/// so a stale worker binary carries its own older string even though both link the same source.
pub(crate) const WORKSPACE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cached dmabuf fds. PipeWire pools are ≤ ~16 buffers; the cap only matters if a producer churns
/// buffers without a renegotiation, and an eviction is recoverable ([`FromWorker::NeedFd`]).
const FD_CACHE_CAP: usize = 64;

/// The largest cursor bitmap that can matter: the encoder clamps to a 256×256 RGBA texture
/// (`pyrowave.rs::CURSOR_MAX`), so uploading more would be bytes the blend cannot read.
const CURSOR_UPLOAD_MAX: usize = 256 * 256 * 4;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// What the `VK_KHR_global_priority` ladder produced — i.e. whether the capability is doing
/// anything. Reported to the host so exactly ONE process logs it: the worker's own
/// `tracing` goes to inherited stderr, but the host is the process with the log pipeline (the
/// ring the web console serves), and the in-process INERT warn's wording ("CAP_SYS_NICE on the
/// host binary") would now actively mislead — the capability belongs on the worker.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorityOutcome {
    /// A class was granted — the lever is live.
    Granted(GrantedClass),
    /// A class was requested, the extension is there, and every class was refused: the lever is
    /// INERT. This is the normal state of an *uncapped* worker.
    Refused,
    /// Nothing was asked for (`PYROWAVE_QUEUE_PRIORITY=off`) or the device advertises no
    /// global-priority extension — not a problem, and never warned about.
    NotRequested,
}

/// The granted `VkQueueGlobalPriorityKHR` class, wire-side.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantedClass {
    Realtime,
    High,
}

/// [`pf_frame::PixelFormat`] on the wire. A hand-written mirror rather than a serde derive on the
/// original: pf-frame carries no serde dependency, and the exhaustive `match` in both directions
/// makes a new capture format a COMPILE error here instead of a silently mis-described frame.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireFormat {
    Bgrx,
    Rgbx,
    Bgra,
    Rgba,
    Rgb,
    Bgr,
    Rgb10a2,
    Nv12,
    P010,
    Yuv444,
    X2Rgb10,
    X2Bgr10,
}

impl From<PixelFormat> for WireFormat {
    fn from(f: PixelFormat) -> WireFormat {
        match f {
            PixelFormat::Bgrx => WireFormat::Bgrx,
            PixelFormat::Rgbx => WireFormat::Rgbx,
            PixelFormat::Bgra => WireFormat::Bgra,
            PixelFormat::Rgba => WireFormat::Rgba,
            PixelFormat::Rgb => WireFormat::Rgb,
            PixelFormat::Bgr => WireFormat::Bgr,
            PixelFormat::Rgb10a2 => WireFormat::Rgb10a2,
            // A Windows-only capture format (the IDD-push 10-bit SDR expansion): the Linux
            // encode worker can never be handed one, and the wire deliberately grows no
            // variant for it.
            PixelFormat::Rgb10a2Sdr => {
                unreachable!(
                    "Rgb10a2Sdr is a Windows capture format — the Linux worker never sees it"
                )
            }
            PixelFormat::Nv12 => WireFormat::Nv12,
            PixelFormat::P010 => WireFormat::P010,
            PixelFormat::Yuv444 => WireFormat::Yuv444,
            PixelFormat::X2Rgb10 => WireFormat::X2Rgb10,
            PixelFormat::X2Bgr10 => WireFormat::X2Bgr10,
        }
    }
}

impl From<WireFormat> for PixelFormat {
    fn from(f: WireFormat) -> PixelFormat {
        match f {
            WireFormat::Bgrx => PixelFormat::Bgrx,
            WireFormat::Rgbx => PixelFormat::Rgbx,
            WireFormat::Bgra => PixelFormat::Bgra,
            WireFormat::Rgba => PixelFormat::Rgba,
            WireFormat::Rgb => PixelFormat::Rgb,
            WireFormat::Bgr => PixelFormat::Bgr,
            WireFormat::Rgb10a2 => PixelFormat::Rgb10a2,
            WireFormat::Nv12 => PixelFormat::Nv12,
            WireFormat::P010 => PixelFormat::P010,
            WireFormat::Yuv444 => PixelFormat::Yuv444,
            WireFormat::X2Rgb10 => PixelFormat::X2Rgb10,
            WireFormat::X2Bgr10 => PixelFormat::X2Bgr10,
        }
    }
}

/// [`pf_frame::CursorOverlay`] minus its pixels — cursor-as-metadata, the way the CSC consumes it.
/// `upload` is the pixel channel: `Some(len)` means a fresh memfd carrying `len` bytes of straight
/// -alpha RGBA rides with this frame (the bitmap `serial` changed); `None` means "reuse the bitmap
/// you cached for `serial`", which is every frame of a pointer that is merely moving.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireCursor {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub serial: u64,
    pub hot_x: u32,
    pub hot_y: u32,
    pub visible: bool,
    pub upload: Option<usize>,
}

/// host → worker. Every variant has exactly one reply.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) enum ToWorker {
    /// Open the encoder. Answered with [`FromWorker::Ready`] (which carries the AU memfd) or
    /// [`FromWorker::InitErr`].
    ///
    /// `priority_intent` is the raw `PYROWAVE_QUEUE_PRIORITY` value as the HOST resolved it
    /// (`None` = unset ⇒ the default REALTIME→HIGH ladder). Forwarded **explicitly** rather than
    /// read from the worker's environment, and the worker strips the variable from its own env
    /// before opening, so one knob cannot mean two things across the process boundary.
    Hello {
        proto: u32,
        workspace_version: String,
        /// The host's `PUNKTFUNK_RENDER_NODE` (`None` = unset). Log-only, exactly as it is
        /// in-process: the device selection deliberately ignores every node anchor (see
        /// `pyrowave.rs::select_physical_device` — two "fixes" were withdrawn). Carried so a
        /// wrong-device field report shows the host's anchor beside the worker's pick.
        drm_node: Option<String>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        priority_intent: Option<String>,
    },
    /// Encode one frame. The dmabuf fd rides as `SCM_RIGHTS` only on first sight of `key`
    /// (`has_fd`); a cursor upload, when present, is the fd AFTER it. Answered with
    /// [`FromWorker::Au`], [`FromWorker::NeedFd`] or [`FromWorker::EncodeErr`].
    Frame {
        key: u64,
        has_fd: bool,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
        plane1: Option<(u32, u32)>,
        width: u32,
        height: u32,
        pts_ns: u64,
        format: WireFormat,
        cursor: Option<WireCursor>,
    },
    /// `Encoder::set_wire_chunking` — the datagram-aligned packetization boundary (plan §4.4).
    /// This has to cross: it changes the AU BYTES (the windowed `build_au` framing) and the rate
    /// budget, not just how the host hands them out. The streamed-AU *cutting* stays host-side.
    SetWireChunking { shard_payload: usize },
    /// `Encoder::reconfigure_bitrate` — an in-place rate retarget.
    Reconfigure { bitrate_bps: u64 },
    /// `Encoder::reset` — the stall watchdog's in-place rebuild, run INSIDE the worker so the
    /// priority-elevated device survives it (see [`super::pyrowave_remote::RemotePyroWave::reset`]
    /// for why this is a message and not a respawn).
    Reset,
}

/// worker → host.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) enum FromWorker {
    /// The encoder is open. Carries the AU memfd as its single `SCM_RIGHTS` descriptor.
    ///
    /// ⚠ `proto` and `workspace_version` are the first two fields and must never be renamed: they
    /// are how a version-skewed pair diagnoses itself instead of failing obscurely.
    Ready {
        proto: u32,
        workspace_version: String,
        priority: PriorityOutcome,
        device: String,
        /// The chroma the encoder REALLY opened, and whether it blends the cursor — i.e.
        /// `EncoderCaps` as only the opened encoder knows it. The proxy must not guess: a
        /// hardcoded default mis-reports a 4:4:4 open and fires the session glue's spurious
        /// "chroma disagrees with the negotiated Welcome" warn.
        chroma444: bool,
        blends_cursor: bool,
    },
    /// The open failed (no Vulkan 1.3 device, missing features, …) — an ANSWER, not a crash: the
    /// host falls back to the in-process encoder, which will fail the same way if the cause is
    /// real and succeed if the cause was the worker's own environment.
    InitErr { message: String },
    /// One access unit, complete, at offset 0 of the AU memfd. **Doubles as the buffer-release
    /// signal**: it maps 1:1 onto `Encoder::submit`'s lifetime contract (the caller already holds
    /// the frame alive until its AU comes back from `poll`), so the host loop needs no change.
    Au {
        key: u64,
        len: usize,
        pts_ns: u64,
        keyframe: bool,
        chunk_aligned: bool,
        encode_us: u32,
    },
    /// No cached fd for this `key` (evicted, or the caches diverged) — the host forgets its
    /// "already sent" note and retries the frame once, with the fd.
    NeedFd,
    /// This frame failed but the worker is alive.
    EncodeErr { message: String },
    /// Reply to [`ToWorker::SetWireChunking`] / [`ToWorker::Reconfigure`] / [`ToWorker::Reset`].
    Ack { ok: bool },
}

// ---------------------------------------------------------------------------
// Framing helpers — EINTR, and the deadline it must not defeat
// ---------------------------------------------------------------------------

/// [`ipc::recv_fds`] that survives a signal.
///
/// With `SO_RCVTIMEO` armed the kernel returns **EINTR**, not `ERESTARTSYS`, so `SA_RESTART` does
/// not save the caller — any signal delivered to a thread blocked in `recv` surfaces as an error.
/// pf-zerocopy's importer maps *any* recv error to "the worker died", which is right for a
/// once-per-capture handshake and wrong for a per-frame AU: one stray signal would drop a healthy
/// session to the in-process fallback. So retry here.
///
/// The retry re-arms with the REMAINING budget rather than the full one — a signal arriving every
/// 100 ms would otherwise reset the clock forever and a real hang would never time out.
/// `budget = None` means "block until the host speaks or closes" (the worker's own serve loop).
pub(crate) fn recv_eintr<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
    budget: Option<Duration>,
) -> io::Result<(T, Vec<OwnedFd>)> {
    let deadline = budget.map(|d| Instant::now() + d);
    loop {
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "encode worker did not answer within its budget",
                ));
            }
            ipc::set_recv_timeout(sock, Some(left))?;
        }
        match ipc::recv_fds::<T>(sock, buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// [`ipc::send_fds`] that survives a signal. A small body on a socket whose peer is actively
/// reading does not block, so this normally retries never; it exists so that "normally" is not
/// load-bearing.
pub(crate) fn send_eintr<T: Serialize>(
    sock: BorrowedFd,
    msg: &T,
    fds: &[BorrowedFd],
) -> io::Result<()> {
    loop {
        match ipc::send_fds(sock, msg, fds) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// An anonymous RAM-backed file for the bulk channels. Grows on `pwrite`, so callers never size it.
fn memfd(name: &CStr) -> io::Result<File> {
    // SAFETY: `memfd_create` reads a NUL-terminated name (a live `CStr` for the duration of the
    // call) and returns a fresh descriptor or -1; it retains no pointer. The result is checked
    // before use, and the returned fd is owned by nobody else, so `File::from_raw_fd` takes sole
    // ownership and closes it exactly once.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is the fresh, valid descriptor just created and checked above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Build the memfd carrying one cursor bitmap, clamped to what the blend can actually sample.
pub(crate) fn cursor_upload(rgba: &[u8]) -> io::Result<(File, usize)> {
    let n = rgba.len().min(CURSOR_UPLOAD_MAX);
    let f = memfd(c"pf-encode-cursor")?;
    f.write_all_at(&rgba[..n], 0)?;
    Ok((f, n))
}

// ---------------------------------------------------------------------------
// The worker half
// ---------------------------------------------------------------------------

/// `punktfunk-encode-worker` entry point. `args` are the process's own arguments after argv[0]
/// (`--fd N`, default 3 — the socket end the spawning host `dup2`'d in).
pub fn run_from_args(args: &[String]) -> Result<()> {
    // Core dumps ON, and FIRST — the opposite of the host's posture, deliberately. `PR_SET_DUMPABLE`
    // is cleared by the kernel whenever a process gains a file capability, which also suppresses
    // core dumps and makes `/proc/<pid>/environ` unreadable. This process fronts nothing (no
    // Wayland, no D-Bus, no network), so nothing is protected by that suppression and a crash in a
    // GPU driver is exactly what we want a core for. It does NOT make us identifiable to KWin —
    // the 0.26.0-1 matrix measured that dumpable is not the gate, the PERMITTED set is — and it
    // does not need to: this process never speaks Wayland.
    // SAFETY: `prctl(PR_SET_DUMPABLE, 1)` takes integers by value, touches no Rust memory and
    // affects only this process.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 1);
    }
    // The host execs us via a pinned `/proc/self/fd/<n>`, so the kernel derives our comm from a
    // meaningless fd number. Rename so `top`/`pkill`/a coredump path see the worker.
    // SAFETY: `PR_SET_NAME` copies at most 16 bytes from the given pointer; the C-string literal is
    // valid, NUL-terminated and short enough, and no pointer is retained past the call.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"pf-encode-wk".as_ptr());
    }
    sanitize_env();
    // Real teeth, and the second half of what the capability buys: `setpriority` is a silent no-op
    // without `CAP_SYS_NICE`/`RLIMIT_NICE`, which is why the in-host encode thread's nice(-10) has
    // never actually applied on a packaged Linux host. Here it applies. The worker is
    // single-threaded, so this IS the encode thread.
    pf_frame::thread_qos::boost_thread_priority(true);

    let fd: i32 = args
        .iter()
        .skip_while(|a| *a != "--fd")
        .nth(1)
        .map(|s| s.parse())
        .transpose()
        .context("parse --fd")?
        .unwrap_or(3);
    // Refuse anything that cannot be the spawning host's socket: a negative fd is UB inside
    // `OwnedFd` (its niche), and 0–2 would make the worker close one of its own stdio streams on
    // exit. Then confirm the number really holds a socket — this binary is installed and runnable
    // by hand, and adopting an arbitrary inherited fd would close it behind its real owner.
    anyhow::ensure!(fd >= 3, "--fd must be >= 3 (got {fd})");
    // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value, so `mem::zeroed`
    // is a sound initializer; `fstat` writes into the live, correctly-sized `&mut st` and only
    // reads `fd`. `st_mode` is read only after the return value is checked.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    anyhow::ensure!(
        is_socket,
        "--fd {fd} is not an open socket (this binary is spawned by punktfunk-host, not run by hand)"
    );
    // SAFETY: the spawning host `dup2`'d its socketpair end onto exactly this fd number before
    // exec (the worker's contract, just verified to be an open socket ≥ 3) and nothing else in
    // this fresh process owns it, so `OwnedFd` takes sole ownership and closes it once at exit.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    run(sock)
}

/// Drop the environment variables this process must not act on.
///
/// Deliberately a DENYLIST, not an allowlist. The obvious "clear everything but a handful of
/// names" is wrong here: the Vulkan loader discovers its ICDs through the environment
/// (`VK_ICD_FILENAMES`/`VK_DRIVER_FILES`/`XDG_DATA_DIRS`), so a strict allowlist would leave the
/// worker with no GPU exactly on NixOS — the one channel where this worker's env override is
/// load-bearing. What must go is the punktfunk state that would make one knob mean two things:
/// the priority intent (it arrives explicitly in `Hello`) and the worker path itself (nothing here
/// spawns a worker, and a stale value in a core dump is just noise).
fn sanitize_env() {
    for k in ["PYROWAVE_QUEUE_PRIORITY", "PUNKTFUNK_ENCODE_WORKER"] {
        // SAFETY: single-threaded — this runs before anything in this process creates a thread,
        // which is the one situation where mutating the environment is sound (the `getenv` race
        // `remove_var`'s contract is about needs a second thread).
        unsafe { std::env::remove_var(k) };
    }
}

/// Handshake, then serve until the host goes away.
fn run(sock: OwnedFd) -> Result<()> {
    let mut buf = Vec::new();
    // No timeout on the worker's own receives: the host owns the clock (it arms `SO_RCVTIMEO` on
    // its end), and a worker that gave up on its own would look exactly like a crash.
    let (hello, _) = recv_eintr::<ToWorker>(sock.as_fd(), &mut buf, None).context("recv Hello")?;
    let ToWorker::Hello {
        proto,
        workspace_version,
        drm_node,
        width,
        height,
        fps,
        bitrate_bps,
        chroma444,
        priority_intent,
    } = hello
    else {
        anyhow::bail!("first message was not Hello");
    };
    if proto != PROTO_VERSION || workspace_version != WORKSPACE_VERSION {
        // Answer, don't crash: the host prints one warn naming both builds and encodes in-process.
        let _ = send_eintr(
            sock.as_fd(),
            &FromWorker::InitErr {
                message: format!(
                    "version skew: worker proto {PROTO_VERSION} v{WORKSPACE_VERSION}, \
                     host proto {proto} v{workspace_version}"
                ),
            },
            &[],
        );
        return Ok(());
    }

    let enc = match super::pyrowave::PyroWaveEncoder::open_in_worker(
        width,
        height,
        fps,
        bitrate_bps,
        chroma444,
        priority_intent.as_deref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            let _ = send_eintr(
                sock.as_fd(),
                &FromWorker::InitErr {
                    message: format!("{e:#}"),
                },
                &[],
            );
            return Ok(());
        }
    };
    let au_buf = memfd(c"pf-encode-au").context("create the AU return buffer")?;
    let caps = crate::Encoder::caps(&enc);
    let ready = FromWorker::Ready {
        proto: PROTO_VERSION,
        workspace_version: WORKSPACE_VERSION.to_string(),
        priority: enc.priority_outcome(),
        device: enc.device_name().to_string(),
        chroma444: caps.chroma_444,
        blends_cursor: caps.blends_cursor,
    };
    send_eintr(sock.as_fd(), &ready, &[au_buf.as_fd()]).context("send Ready")?;
    tracing::info!(
        pid = std::process::id(),
        device = %enc.device_name(),
        priority = ?enc.priority_outcome(),
        host_render_node = ?drm_node,
        "punktfunk-encode-worker ready"
    );
    serve(&sock, enc, &au_buf)
}

/// The request loop. `Ok(())` on host EOF (normal end-of-life — the host dropped its proxy);
/// any other socket error propagates and the process exits, which the host reads as a death,
/// because it is one.
fn serve(sock: &OwnedFd, mut enc: super::pyrowave::PyroWaveEncoder, au_buf: &File) -> Result<()> {
    use crate::Encoder as _;
    let mut buf = Vec::new();
    let mut fds: HashMap<u64, OwnedFd> = HashMap::new();
    // Insertion order, for the eviction the cap implies.
    let mut fd_order: VecDeque<u64> = VecDeque::new();
    // The cursor bitmap the host last uploaded, by `serial` — a moving pointer re-sends only its
    // position, exactly like the in-process path re-uses its uploaded texture.
    let mut cursor_rgba: Option<(u64, Arc<Vec<u8>>)> = None;
    loop {
        let (msg, got) = match recv_eintr::<ToWorker>(sock.as_fd(), &mut buf, None) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("worker recv"),
        };
        let reply = match msg {
            ToWorker::Hello { .. } => FromWorker::EncodeErr {
                message: "duplicate Hello".into(),
            },
            ToWorker::SetWireChunking { shard_payload } => {
                enc.set_wire_chunking(shard_payload);
                FromWorker::Ack { ok: true }
            }
            ToWorker::Reconfigure { bitrate_bps } => FromWorker::Ack {
                ok: enc.reconfigure_bitrate(bitrate_bps),
            },
            ToWorker::Reset => FromWorker::Ack { ok: enc.reset() },
            ToWorker::Frame {
                key,
                has_fd,
                fourcc,
                modifier,
                offset,
                stride,
                plane1,
                width,
                height,
                pts_ns,
                format,
                cursor,
            } => {
                // Descriptor order is the sender's: the dmabuf (iff `has_fd`), then the cursor
                // upload (iff the bitmap changed). Taken before any early return so an unexpected
                // extra descriptor is closed with the `Vec` rather than leaked.
                let mut got = got.into_iter();
                let dmabuf = if has_fd { got.next() } else { None };
                let cursor_fd = cursor.as_ref().and_then(|c| c.upload.map(|_| got.next()));
                if let Some(fd) = dmabuf {
                    if fds.insert(key, fd).is_none() {
                        fd_order.push_back(key);
                    }
                    while fd_order.len() > FD_CACHE_CAP {
                        if let Some(old) = fd_order.pop_front() {
                            fds.remove(&old);
                        }
                    }
                }
                match encode_one(
                    &mut enc,
                    au_buf,
                    &fds,
                    &mut cursor_rgba,
                    FrameReq {
                        key,
                        fourcc,
                        modifier,
                        offset,
                        stride,
                        plane1,
                        width,
                        height,
                        pts_ns,
                        format,
                        cursor,
                    },
                    cursor_fd.flatten(),
                ) {
                    Ok(reply) => reply,
                    Err(e) => FromWorker::EncodeErr {
                        message: format!("{e:#}"),
                    },
                }
            }
        };
        match send_eintr(sock.as_fd(), &reply, &[]) {
            Ok(()) => {}
            // The host vanished between our recv and our send — the same end-of-life as EOF.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e).context("worker send"),
        }
    }
}

/// [`ToWorker::Frame`] minus the descriptors, so [`encode_one`] takes one argument per concept.
struct FrameReq {
    key: u64,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    stride: u32,
    plane1: Option<(u32, u32)>,
    width: u32,
    height: u32,
    pts_ns: u64,
    format: WireFormat,
    cursor: Option<WireCursor>,
}

/// Rebuild the `CapturedFrame`, encode it synchronously, and write the AU into `au_buf`.
fn encode_one(
    enc: &mut super::pyrowave::PyroWaveEncoder,
    au_buf: &File,
    fds: &HashMap<u64, OwnedFd>,
    cursor_rgba: &mut Option<(u64, Arc<Vec<u8>>)>,
    req: FrameReq,
    cursor_fd: Option<OwnedFd>,
) -> Result<FromWorker> {
    use crate::Encoder as _;
    let Some(cached) = fds.get(&req.key) else {
        return Ok(FromWorker::NeedFd);
    };
    // A dup per frame, not a borrow: `DmabufFrame` owns its fd (the encoder's import path dups it
    // again for Vulkan and drops the rest), while the cache must keep holding the original so the
    // steady state passes no descriptors at all. One `dup`/`close` pair per frame is µs.
    let fd = cached.try_clone().context("dup the cached dmabuf fd")?;

    let cursor = match req.cursor {
        Some(c) => {
            match (c.upload, cursor_fd) {
                (Some(len), Some(f)) => {
                    let mut px = vec![0u8; len];
                    File::from(f)
                        .read_exact_at(&mut px, 0)
                        .context("read the cursor upload")?;
                    *cursor_rgba = Some((c.serial, Arc::new(px)));
                }
                // An announced upload whose descriptor did not arrive. Rare (it takes a kernel
                // refusal of the `SCM_RIGHTS`), and the reason it is an ERROR rather than a
                // shrug: the host marks the serial "sent" on a successful AU, so blending
                // nothing here would leave the pointer INVISIBLE for the rest of that bitmap's
                // life, silently. Failing the frame drops the session onto the in-process
                // encoder instead, which is a rung with a warning attached.
                (Some(_), None) => anyhow::bail!("cursor upload announced but no descriptor came"),
                (None, _) => {}
            }
            // Likewise a serial we hold no pixels for: the host only omits the upload for a serial
            // it has seen acknowledged, so a miss is a desync, not a frame to guess at.
            let Some(rgba) = cursor_rgba
                .as_ref()
                .filter(|(serial, _)| *serial == c.serial)
                .map(|(_, px)| px.clone())
            else {
                anyhow::bail!("no cursor bitmap cached for serial {}", c.serial);
            };
            Some(CursorOverlay {
                x: c.x,
                y: c.y,
                w: c.w,
                h: c.h,
                rgba,
                serial: c.serial,
                hot_x: c.hot_x,
                hot_y: c.hot_y,
                visible: c.visible,
            })
        }
        None => None,
    };
    let frame = CapturedFrame {
        width: req.width,
        height: req.height,
        pts_ns: req.pts_ns,
        format: req.format.into(),
        payload: FramePayload::Dmabuf(DmabufFrame {
            fd,
            fourcc: req.fourcc,
            modifier: req.modifier,
            plane1: req.plane1,
            offset: req.offset,
            stride: req.stride,
            // The deferred-requeue hold stays host-side: this backend is synchronous at depth 1
            // (see below), so the host's frame — hold and all — outlives the whole encode.
            hold: None,
        }),
        cursor,
    };
    // submit→poll in one breath: this backend's encode is synchronous at depth 1, so the AU is
    // ready when `poll` returns and `frame` (with its fd) is alive across both halves — the
    // trait's lifetime contract, honored on this side of the socket too.
    let t0 = Instant::now();
    enc.submit(&frame)?;
    let Some(au) = enc.poll()? else {
        anyhow::bail!("encoder returned no AU for a submitted frame");
    };
    let encode_us = t0.elapsed().as_micros() as u32;
    au_buf
        .write_all_at(&au.data, 0)
        .context("write the AU into the return buffer")?;
    Ok(FromWorker::Au {
        key: req.key,
        len: au.data.len(),
        pts_ns: au.pts_ns,
        keyframe: au.keyframe,
        chunk_aligned: au.chunk_aligned,
        encode_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn hello() -> ToWorker {
        ToWorker::Hello {
            proto: PROTO_VERSION,
            workspace_version: WORKSPACE_VERSION.to_string(),
            drm_node: Some("/dev/dri/renderD128".into()),
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_bps: 400_000_000,
            chroma444: true,
            priority_intent: Some("realtime".into()),
        }
    }

    /// The vocabulary survives the wire in both directions, descriptors included. (The framing —
    /// EOF, timeouts, the descriptor cap — is pf-zerocopy's `ipc` tests' job; this pins the
    /// message types and the fd ORDER the frame path depends on.)
    #[test]
    fn proto_round_trip_both_directions() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        ipc::send(a.as_fd(), &hello(), None).unwrap();
        let (got, fds) = ipc::recv_fds::<ToWorker>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, hello());
        assert!(fds.is_empty());

        let frame = ToWorker::Frame {
            key: 0xdead_beef,
            has_fd: true,
            fourcc: 0x3432_5258,
            modifier: 0x0300_0000_0000_1234,
            offset: 0,
            stride: 3840 * 4,
            plane1: None,
            width: 3840,
            height: 2160,
            pts_ns: 1_234_567_890,
            format: WireFormat::Bgrx,
            cursor: Some(WireCursor {
                x: 10,
                y: 20,
                w: 32,
                h: 32,
                serial: 7,
                hot_x: 1,
                hot_y: 2,
                visible: true,
                upload: Some(32 * 32 * 4),
            }),
        };
        // Two descriptors, in the order the receiver destructures them: dmabuf, then cursor.
        let (dma, cur) = (memfd(c"t-dma").unwrap(), memfd(c"t-cur").unwrap());
        ipc::send_fds(a.as_fd(), &frame, &[dma.as_fd(), cur.as_fd()]).unwrap();
        let (got, fds) = ipc::recv_fds::<ToWorker>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, frame);
        assert_eq!(fds.len(), 2);

        let ready = FromWorker::Ready {
            proto: PROTO_VERSION,
            workspace_version: WORKSPACE_VERSION.to_string(),
            priority: PriorityOutcome::Granted(GrantedClass::Realtime),
            device: "NVIDIA GeForce RTX 5070 Ti".into(),
            chroma444: true,
            blends_cursor: true,
        };
        ipc::send(b.as_fd(), &ready, Some(dma.as_fd())).unwrap();
        let (got, fd) = ipc::recv::<FromWorker>(a.as_fd(), &mut buf).unwrap();
        assert_eq!(got, ready);
        assert!(fd.is_some(), "Ready carries the AU return buffer");

        for reply in [
            FromWorker::Au {
                key: 1,
                len: 830_000,
                pts_ns: 5,
                keyframe: true,
                chunk_aligned: true,
                encode_us: 4400,
            },
            FromWorker::NeedFd,
            FromWorker::Ack { ok: true },
            FromWorker::EncodeErr {
                message: "boom".into(),
            },
        ] {
            ipc::send(b.as_fd(), &reply, None).unwrap();
            let (got, _) = ipc::recv::<FromWorker>(a.as_fd(), &mut buf).unwrap();
            assert_eq!(got, reply);
        }
    }

    /// An AU never rides in the JSON body, and this is why: the smallest per-frame budget the
    /// encoder will ever use is already `MAX_MSG`, and serde_json renders a byte as up to four
    /// characters. Pinned as a test so nobody "simplifies" the memfd away.
    #[test]
    fn an_inline_au_would_not_fit_a_message() {
        // 1080p60 at a modest 40 Mb/s — well inside the shipped range.
        let au = vec![0xABu8; 40_000_000 / (8 * 60)];
        let body = serde_json::to_vec(&au).unwrap();
        assert!(
            body.len() > ipc::MAX_MSG,
            "a {}-byte AU serialized to {} bytes, which would (wrongly) fit MAX_MSG {}",
            au.len(),
            body.len(),
            ipc::MAX_MSG
        );
    }

    /// A body over [`ipc::MAX_MSG`] is refused at the sender rather than truncated on the wire —
    /// the property the memfd channel exists to respect.
    #[test]
    fn oversized_messages_are_refused_not_truncated() {
        let (a, _b) = ipc::socketpair_seqpacket().unwrap();
        let ToWorker::Hello {
            proto,
            drm_node,
            width,
            height,
            fps,
            bitrate_bps,
            chroma444,
            priority_intent,
            ..
        } = hello()
        else {
            unreachable!("hello() builds a Hello");
        };
        let huge = ToWorker::Hello {
            proto,
            // Over `MAX_MSG` on its own — enum variants take no functional-update syntax, so the
            // rest is destructured above rather than `..hello()`.
            workspace_version: "x".repeat(ipc::MAX_MSG),
            drm_node,
            width,
            height,
            fps,
            bitrate_bps,
            chroma444,
            priority_intent,
        };
        let err = ipc::send(a.as_fd(), &huge, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Every `PixelFormat` maps to a wire tag and back unchanged. The `match`es are exhaustive, so
    /// a new capture format is a compile error; this catches a mis-typed ARM in either direction.
    #[test]
    fn pixel_formats_round_trip() {
        for f in [
            PixelFormat::Bgrx,
            PixelFormat::Rgbx,
            PixelFormat::Bgra,
            PixelFormat::Rgba,
            PixelFormat::Rgb,
            PixelFormat::Bgr,
            PixelFormat::Rgb10a2,
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Yuv444,
            PixelFormat::X2Rgb10,
            PixelFormat::X2Bgr10,
        ] {
            assert_eq!(PixelFormat::from(WireFormat::from(f)), f);
        }
    }

    /// The bulk channel: a memfd written by one holder of the descriptor is readable at offset 0
    /// by another, and it grows on write with no explicit sizing. That is the whole mechanism the
    /// AU return depends on.
    #[test]
    fn memfd_round_trips_bytes_across_a_descriptor() {
        let f = memfd(c"pf-encode-test").unwrap();
        let au = vec![0x5Au8; 900_000];
        f.write_all_at(&au, 0).unwrap();
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        ipc::send(a.as_fd(), &FromWorker::NeedFd, Some(f.as_fd())).unwrap();
        let (_, fd) = ipc::recv::<FromWorker>(b.as_fd(), &mut buf).unwrap();
        let mut back = vec![0u8; au.len()];
        File::from(fd.unwrap()).read_exact_at(&mut back, 0).unwrap();
        assert_eq!(back, au);
    }

    /// A cursor bitmap is clamped to what the 256×256 blend texture can sample — a larger one is
    /// truncated, exactly as `prep_cursor`'s `bytes.min(rgba.len())` copy already truncates it.
    #[test]
    fn cursor_upload_clamps_to_the_blend_texture() {
        let (_, n) = cursor_upload(&vec![0u8; CURSOR_UPLOAD_MAX * 4]).unwrap();
        assert_eq!(n, CURSOR_UPLOAD_MAX);
        let (_, n) = cursor_upload(&vec![0u8; 64 * 64 * 4]).unwrap();
        assert_eq!(n, 64 * 64 * 4);
    }

    /// EINTR must not read as a dead worker. A `SIGURG` (default-ignored, so the test process
    /// survives it) delivered to a thread parked in `recv` with `SO_RCVTIMEO` armed returns EINTR
    /// — `SA_RESTART` does not apply to a timeout-armed socket — and the retry must swallow it and
    /// still deliver the message that arrives afterwards.
    #[test]
    fn recv_survives_a_signal() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let b = std::sync::Arc::new(b);
        let waiter = {
            let b = b.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                recv_eintr::<FromWorker>(b.as_fd(), &mut buf, Some(Duration::from_secs(10)))
            })
        };
        // Give the thread time to park in `recvmsg`, then interrupt it repeatedly while the
        // message is still not there.
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..5 {
            // SAFETY: `pthread_kill` takes the live thread's id by value and a signal number;
            // SIGURG's default disposition is "ignore", so delivery cannot kill the process.
            unsafe {
                libc::pthread_kill(
                    std::os::unix::thread::JoinHandleExt::as_pthread_t(&waiter),
                    libc::SIGURG,
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ipc::send(a.as_fd(), &FromWorker::Ack { ok: true }, None).unwrap();
        let (got, _) = waiter.join().unwrap().expect("EINTR must not surface");
        assert_eq!(got, FromWorker::Ack { ok: true });
    }

    /// …and the retry must not defeat the deadline: a socket nobody ever writes to still times
    /// out, signals or no signals.
    #[test]
    fn recv_still_times_out() {
        let (a, _b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let err = recv_eintr::<FromWorker>(a.as_fd(), &mut buf, Some(Duration::from_millis(80)))
            .unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {err:?}"
        );
    }
}
