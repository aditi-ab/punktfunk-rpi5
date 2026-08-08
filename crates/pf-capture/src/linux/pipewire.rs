//! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).

use super::pw_cursor::{composite_cursor, update_cursor_meta, CursorState};
use super::pw_pods::{
    build_cursor_meta_param, build_default_format_obj, build_dmabuf_buffers, build_dmabuf_format,
    build_hdr_dmabuf_format, build_mappable_buffers, build_shm_only_buffers, serialize_pod,
};
use super::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat, ZeroCopyPolicy};
use anyhow::{Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;

/// Map a negotiated SPA video format to a layout the encoder can consume. Returns
/// `None` for formats we don't handle (the frame is then skipped).
fn map_format(f: VideoFormat) -> Option<PixelFormat> {
    Some(match f {
        VideoFormat::BGRx => PixelFormat::Bgrx,
        VideoFormat::RGBx => PixelFormat::Rgbx,
        VideoFormat::BGRA => PixelFormat::Bgra,
        VideoFormat::RGBA => PixelFormat::Rgba,
        VideoFormat::RGB => PixelFormat::Rgb,
        VideoFormat::BGR => PixelFormat::Bgr,
        VideoFormat::NV12 => PixelFormat::Nv12,
        // The GNOME 50+ HDR screencast formats (packed 2:10:10:10; only ever negotiated by
        // the `want_hdr` offer, whose MANDATORY colorimetry props pin them to PQ/BT.2020).
        VideoFormat::xRGB_210LE => PixelFormat::X2Rgb10,
        VideoFormat::xBGR_210LE => PixelFormat::X2Bgr10,
        _ => return None,
    })
}

struct UserData {
    info: VideoInfoRaw,
    /// Negotiated layout (`None` until param_changed, or if unsupported).
    format: Option<PixelFormat>,
    /// Negotiated DRM format modifier (for dmabuf import); 0 = LINEAR.
    modifier: u64,
    /// The one-deep frame mailbox (see [`super::FrameSlot`]) + its wakeup sender. Written
    /// through [`UserData::publish`], never directly.
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    /// Everything this thread publishes to the capturer — see [`super::CaptureSignals`], which also
    /// documents each flag's contract (it used to be restated here, and drifted).
    signals: super::CaptureSignals,
    /// Consecutive tiled-import failures (reset on success); see [`IMPORT_FAIL_POISON`].
    import_fail_streak: u32,
    /// Present when zero-copy is enabled on NVIDIA: imports a dmabuf → CUDA device buffer,
    /// normally via the isolated worker process (`pf_zerocopy::Importer::Remote`).
    importer: Option<pf_zerocopy::Importer>,
    /// VAAPI zero-copy: hand the raw dmabuf to the encoder (which imports + GPU-CSCs it) instead
    /// of a CUDA import. Set when zero-copy is on, the EGL→CUDA importer is unavailable, and the
    /// encoder backend is VAAPI (AMD/Intel).
    vaapi_passthrough: bool,
    /// `PUNKTFUNK_NV12`: on the tiled EGL/GL zero-copy path, convert to NV12 on the GPU and feed
    /// NVENC native YUV (Tier 2A). Off ⇒ the BGRx path is unchanged.
    nv12: bool,
    /// 4:4:4 session: on the tiled EGL/GL zero-copy path, convert to planar YUV444 on the GPU
    /// (`ImportKind::Tiled444`) and feed NVENC native full-chroma YUV — takes precedence over
    /// `nv12` (a 4:4:4 session must never subsample).
    yuv444: bool,
    /// The LINEAR (gamescope) NV12 compute CSC failed once — RGB for the rest of the stream
    /// (T2.5b's per-stream fallback latch; cleared by the next stream's fresh `Ud`).
    linear_nv12_failed: bool,
    /// Rate-limit counter for the latest-frame-only diagnostic log (see `.process`).
    dbg_log_n: u64,
    /// Raw-passthrough frames that silently fell through to the CPU de-pad path, by reason — see
    /// [`PassthroughFallbacks`]. Per-session: a fresh `UserData` is built per pipeline, so a
    /// compositor that starts serving dmabufs again after a rebuild gets a fresh log budget.
    passthrough_fallbacks: PassthroughFallbacks,
    /// Cursor-as-metadata state, composited into the CPU de-pad path (see `consume_frame`).
    cursor: CursorState,
    /// `Some((w, h))` while the producer's negotiated size is a sacrificial birth mode and a
    /// renegotiation to these dims is guaranteed (KWin virtual outputs — kwin.rs `create`):
    /// `.process` skips whole buffers until the negotiated size matches, then clears this
    /// (self-disarming — later legitimate resizes are unaffected). `None` = no gating.
    expect_dims: Option<(u32, u32)>,
    /// Buffers skipped by the `expect_dims` gate (rate-limits its log).
    gate_skips: u64,
    /// When the gate first held a buffer — after [`GATE_DEADLINE`] with no renegotiation the
    /// gate disarms and accepts what the producer serves (degraded dims beat a session wedged
    /// into the first-frame-timeout retry loop; the promised renegotiation normally lands
    /// within a frame or two).
    gate_since: Option<std::time::Instant>,
}

impl UserData {
    /// Hand `frame` to the consumer as THE latest, OVERWRITING any frame it has not taken yet
    /// (drop-oldest — see [`super::FrameSlot`]), then poke the wakeup edge.
    ///
    /// Never blocks the PipeWire loop, which is the hard constraint here: this runs inside
    /// `.process`, so blocking would stall the compositor's stream. Both operations are
    /// best-effort — a full wakeup channel means an edge is already pending (nothing lost, the
    /// slot is the truth), and a poisoned mutex is unreachable in practice (the only critical
    /// sections are a `take` and this store).
    fn publish(&self, frame: CapturedFrame) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(frame);
        }
        let _ = self.wake.try_send(());
    }
}

/// Everything the zero-copy negotiation decision depends on, gathered at ONE point in time.
/// Split out from [`pipewire_thread`]'s prologue so the decision is a pure function of these
/// facts — testable, and consumable by both the thread and `spawn_pipewire` (see
/// [`NegotiationPlan`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct NegotiationInputs {
    /// `allow_zerocopy && pf_zerocopy::enabled()`.
    pub zerocopy: bool,
    /// `PUNKTFUNK_FORCE_SHM` — the race-free download path.
    pub force_shm: bool,
    /// This session offers the 10-bit PQ/BT.2020 formats.
    pub want_hdr: bool,
    /// This session negotiated full-chroma 4:4:4.
    pub want_444: bool,
    /// [`ZeroCopyPolicy::backend_is_vaapi`].
    pub backend_is_vaapi: bool,
    /// [`ZeroCopyPolicy::pyrowave_session`].
    pub pyrowave_session: bool,
    /// [`ZeroCopyPolicy::native_nv12_session`].
    pub native_nv12_session: bool,
    /// `pf_zerocopy::raw_dmabuf_import_disabled()` — the scoped raw-passthrough latch.
    pub raw_dmabuf_import_disabled: bool,
    /// `pf_zerocopy::gpu_import_disabled()` — repeated import-worker deaths.
    pub gpu_import_disabled: bool,
    /// `pf_zerocopy::gpu_dmabuf_negotiation_disabled()` — a previous EGL→CUDA dmabuf-only offer
    /// timed out (the compositor accepts none of the importer's modifiers).
    pub gpu_dmabuf_negotiation_failed: bool,
    /// `PUNKTFUNK_PIPEWIRE_NV12` (default ON) — allow the producer-side NV12 preference.
    pub native_nv12_env_on: bool,
    /// [`ZeroCopyPolicy::hdr_cuda_ok`] — the resolved encoder can ingest a packed 10-bit PQ CUDA
    /// payload. Only the direct-SDK NVENC backend can.
    pub hdr_cuda_ok: bool,
}

/// The resolved zero-copy negotiation decision — **one resolver, consumed by the PipeWire thread
/// AND by `spawn_pipewire`.**
///
/// `spawn_pipewire` used to hand-mirror `vaapi_passthrough` so the capturer's
/// negotiation-timeout branch could tell which offer had failed, and the copy drifted: it omitted
/// `raw_dmabuf_import_disabled`, so after that latch fired the mirror still said "raw
/// passthrough" while the thread had already fallen back — and the timeout branch then latched a
/// downgrade for an offer it had not made. Deriving both consumers from this struct is what makes
/// that class of drift unrepresentable rather than merely fixed (same shape as WP7.6's single
/// Linux encode-backend resolver).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NegotiationPlan {
    /// Build the EGL→CUDA importer for this capture.
    pub build_importer: bool,
    /// Hand raw dmabufs straight to the encoder instead of importing to CUDA.
    pub vaapi_passthrough: bool,
    /// Offer gamescope's producer-side NV12 pod ahead of packed RGB.
    pub prefer_native_nv12: bool,
    /// Carried through so [`want_dmabuf`](Self::want_dmabuf) needs no second copy of it.
    pub force_shm: bool,
    /// Diagnostic: this capture WOULD have taken the raw passthrough, but its scoped latch is
    /// set (the encoder repeatedly failed to import, or a previous negotiation timed out).
    pub raw_dmabuf_latched: bool,
    /// Diagnostic: this capture WOULD have built the EGL→CUDA importer, but a latch fired —
    /// repeated import-worker deaths, or a previous dmabuf-offer negotiation timeout.
    pub gpu_import_latched: bool,
}

/// Resolve the negotiation plan. **Pure** — every environment read is already in `i`.
///
/// The four invariants this encodes were previously prose-only comments spread across the
/// prologue; `negotiation_plan_invariants` in the tests below pins each one:
///   1. HDR never takes the TILED EGL de-tile blit (it renders into an 8-bit `GL_RGBA8` texture
///      → silent depth loss). It may still build the importer, because the HDR pod family
///      advertises LINEAR only ([`build_hdr_dmabuf_format`]) — so an HDR dmabuf necessarily
///      takes the Vulkan-bridge / CUDA-external-memory arm, which is byte-exact for any 4 Bpp
///      packed format. The per-frame gate in `.process` enforces the tiled half.
///   2. 4:4:4 never prefers producer NV12 (a 4:4:4 session must not be subsampled).
///   3. Producer-native NV12 only on a `native_nv12_session` under an active raw passthrough
///      (libav VAAPI would misread the two-plane buffer; the CUDA importer expects packed RGB).
///   4. The raw passthrough is off whenever its own latch has fired.
pub(super) fn negotiation_plan(i: NegotiationInputs) -> NegotiationPlan {
    // The frames' consumer imports raw dmabufs itself: the VAAPI backend (libva import + GPU
    // CSC) or a PyroWave session (the wavelet encoder's own Vulkan device, any vendor).
    let raw_passthrough = i.backend_is_vaapi || i.pyrowave_session;
    // Building the EGL→CUDA importer would waste a CUDA probe under a raw passthrough — or
    // worse, succeed and produce CUDA payloads only NVENC can consume. `gpu_import_disabled` is
    // the repeated-worker-death latch (a wedged GPU stack must not crash-loop);
    // `gpu_dmabuf_negotiation_failed` is the offer's own timeout latch (a compositor that accepts
    // none of the importer's modifiers refuses them identically on every retry, so the next
    // session negotiates the CPU path instead of re-paying the 10 s timeout).
    //
    // HDR is NOT excluded outright (invariant 1): its pods are LINEAR-only, so it lands on the
    // Vulkan-bridge arm, never the 8-bit de-tile blit. But it is excluded where the encoder cannot
    // take a packed 10-bit CUDA payload — the libav fallback's HDR route swscales into a P010
    // hardware frame, so it must keep getting CPU frames. Without that term a
    // `PUNKTFUNK_NVENC_DIRECT=0` host would stream garbage.
    let build_importer = i.zerocopy
        && !raw_passthrough
        && !i.gpu_import_disabled
        && !i.gpu_dmabuf_negotiation_failed
        && (!i.want_hdr || i.hdr_cuda_ok);
    // Note there is no `importer.is_none()` term, unlike the expression this replaces: it was
    // redundant (`build_importer` already excludes `raw_passthrough`, so the importer is
    // necessarily absent here) and it is what made the decision look impure — the reason
    // `spawn_pipewire` "had to" mirror it by hand.
    let vaapi_passthrough =
        i.zerocopy && !i.force_shm && raw_passthrough && !i.raw_dmabuf_import_disabled;
    let prefer_native_nv12 = i.native_nv12_env_on
        && i.native_nv12_session
        && i.backend_is_vaapi
        && vaapi_passthrough
        && !i.pyrowave_session
        && !i.want_444
        && !i.want_hdr;
    NegotiationPlan {
        build_importer,
        vaapi_passthrough,
        prefer_native_nv12,
        force_shm: i.force_shm,
        raw_dmabuf_latched: i.zerocopy
            && !i.force_shm
            && raw_passthrough
            && i.raw_dmabuf_import_disabled,
        // Every `build_importer` term EXCEPT the two latches, and then either latch — i.e. exactly
        // "this capture would have built the importer, but a latch stopped it".
        gpu_import_latched: i.zerocopy
            && !raw_passthrough
            && (!i.want_hdr || i.hdr_cuda_ok)
            && (i.gpu_import_disabled || i.gpu_dmabuf_negotiation_failed),
    }
}

impl NegotiationPlan {
    /// Whether to request dmabuf buffers. Not part of the plan proper: it depends on whether the
    /// importer actually CONSTRUCTED (`have_importer` — a GPU/driver fact) and on the modifier
    /// list that construction yielded.
    pub(super) fn want_dmabuf(&self, have_importer: bool, modifiers: &[u64]) -> bool {
        (have_importer || self.vaapi_passthrough) && !modifiers.is_empty() && !self.force_shm
    }
}

/// Which capture arm a negotiated pipeline actually resolved to.
///
/// The 2026-08-08 PyroWave triage had to reconstruct this from four files, because no single line
/// ever states it: the arm is the product of a policy, a latch, an importer that may or may not
/// have constructed, and a modifier list. A degraded host and a healthy one logged the same
/// thing. [`resolved_capture_arm`] plus the one INFO line at pipeline build is the whole fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureArm {
    /// Raw dmabufs handed straight to the encoder, which imports them itself — libva (VAAPI) or
    /// the PyroWave encoder's own Vulkan device. No host pixel touch.
    DmabufPassthrough,
    /// dmabufs imported to CUDA device buffers by the EGL→CUDA worker, for NVENC. No host pixel
    /// touch either, but a different failure surface (the worker, the modifier negotiation).
    CudaImport,
    /// CPU frames: an mmap de-pad of every frame, then whatever CSC + upload the encoder needs.
    /// The slow path — always a downgrade when the consumer could have taken a dmabuf.
    Cpu,
}

impl CaptureArm {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            CaptureArm::DmabufPassthrough => "dmabuf-passthrough",
            CaptureArm::CudaImport => "cuda-import",
            CaptureArm::Cpu => "cpu",
        }
    }
}

/// Resolve the arm this pipeline ended up on. **Pure** — `have_importer` and `want_dmabuf` are the
/// two runtime facts `negotiation_plan` cannot know (whether the importer constructed, and what
/// modifier list that yielded); everything else is already in the plan.
pub(super) fn resolved_capture_arm(
    plan: &NegotiationPlan,
    have_importer: bool,
    want_dmabuf: bool,
) -> CaptureArm {
    if !want_dmabuf {
        // No dmabuf offer at all: SHM/CPU frames, whatever the plan wanted.
        CaptureArm::Cpu
    } else if plan.vaapi_passthrough {
        CaptureArm::DmabufPassthrough
    } else if have_importer {
        CaptureArm::CudaImport
    } else {
        // Unreachable via `want_dmabuf` (it requires `have_importer || vaapi_passthrough`), but
        // stated rather than `unreachable!()`: a logging helper must never be the thing that
        // panics a capture thread.
        CaptureArm::Cpu
    }
}

/// Who consumes the captured frames — the fact that decides whether a CPU arm is a *downgrade*
/// worth warning about, and what to call it in the log.
///
/// Derived from the resolved [`ZeroCopyPolicy`](crate::ZeroCopyPolicy), **not** from the encoder
/// pref: `pyrowave_session` is per-session (the negotiated codec), so a PyroWave session on an
/// otherwise-NVENC host reads as PyroWave here. Naming the pref instead is exactly how a PyroWave
/// session's CPU downgrade came to be reported as an NVENC one — or, on an NVIDIA host, not
/// reported at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConsumerKind {
    /// This session encodes PyroWave: the wavelet encoder's own Vulkan device imports dmabufs on
    /// any vendor, so a CPU arm costs it the passthrough it was designed around.
    PyroWave,
    /// The VAAPI backend (AMD/Intel): libva imports the dmabuf and CSCs on the GPU.
    Vaapi,
    /// NVENC, fed by the EGL→CUDA importer.
    Nvenc,
    /// The software encoder — CPU frames are its native input, so a CPU arm is no downgrade.
    Software,
}

impl ConsumerKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ConsumerKind::PyroWave => "pyrowave",
            ConsumerKind::Vaapi => "vaapi",
            ConsumerKind::Nvenc => "nvenc",
            ConsumerKind::Software => "software",
        }
    }

    /// Whether landing on [`CaptureArm::Cpu`] is a performance downgrade for this consumer (i.e.
    /// worth a `warn!`). True for every GPU consumer; false for the software encoder, which wants
    /// CPU frames anyway.
    pub(super) fn cpu_is_downgrade(self) -> bool {
        !matches!(self, ConsumerKind::Software)
    }
}

/// Classify the frames' consumer. **Pure.** `pyrowave_session` wins over `backend_is_vaapi`
/// because it is the per-session truth and the pref is host-global (a PyroWave session also flips
/// `backend_is_vaapi` on, via `linux_zero_copy_is_vaapi`'s `Pyrowave` arm — so testing vaapi first
/// would swallow every PyroWave session).
pub(super) fn consumer_kind(
    pyrowave_session: bool,
    backend_is_vaapi: bool,
    backend_is_gpu: bool,
) -> ConsumerKind {
    if pyrowave_session {
        ConsumerKind::PyroWave
    } else if !backend_is_gpu {
        ConsumerKind::Software
    } else if backend_is_vaapi {
        ConsumerKind::Vaapi
    } else {
        ConsumerKind::Nvenc
    }
}

/// Why a frame on the raw-dmabuf passthrough could not be handed to the encoder and fell through
/// to the CPU de-pad path instead.
///
/// Each variant is a *different* diagnosis with a different fix, and all four were silent: the
/// passthrough block simply fell out of its `if` and the frame took the slow path, so a session
/// that had negotiated zero-copy could pay CPU costs on every frame while logging a healthy
/// "advertising DMA-BUF modifiers" line at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassthroughFallback {
    /// No format negotiated yet — transient, and expected around a renegotiation.
    NoFormat,
    /// The producer delivered an SHM/MemFd buffer for this frame, not a dmabuf.
    NotDmabuf,
    /// The negotiated pixel format has no DRM fourcc, so it cannot be described to the encoder.
    NoFourcc,
    /// `F_DUPFD_CLOEXEC` failed — the fd could not be duplicated to outlive the buffer recycle
    /// (an fd-limit symptom, not a graphics one).
    DupFailed,
}

impl PassthroughFallback {
    fn bit(self) -> u8 {
        match self {
            PassthroughFallback::NoFormat => 1 << 0,
            PassthroughFallback::NotDmabuf => 1 << 1,
            PassthroughFallback::NoFourcc => 1 << 2,
            PassthroughFallback::DupFailed => 1 << 3,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => "no format negotiated yet",
            PassthroughFallback::NotDmabuf => "the producer delivered an SHM/MemFd buffer",
            PassthroughFallback::NoFourcc => "the negotiated format has no DRM fourcc",
            PassthroughFallback::DupFailed => "F_DUPFD_CLOEXEC failed on the dmabuf fd",
        }
    }

    /// What actually happens to the frame. Three of the four reasons downgrade it to the CPU
    /// de-pad path; `NoFormat` does not — the CPU path needs `ud.format` too and returns without
    /// de-padding, so that frame is DROPPED. Worth the distinction: "slower" and "gone" are
    /// different faults, and a diagnostic that conflates them is the thing PW2 exists to remove.
    pub(super) fn falls_back_to_cpu(self) -> bool {
        !matches!(self, PassthroughFallback::NoFormat)
    }

    /// What to do about it — the half a log line is useless without.
    pub(super) fn hint(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => {
                "harmless if it stops: the first buffers can arrive before param_changed"
            }
            PassthroughFallback::NotDmabuf => {
                "the compositor accepted the dmabuf offer and is serving memory anyway — check \
                 PUNKTFUNK_FORCE_SHM and the compositor's allocator"
            }
            PassthroughFallback::NoFourcc => {
                "a capture format the encoder path cannot describe — file it, the negotiation \
                 should not have accepted it"
            }
            PassthroughFallback::DupFailed => "out of file descriptors — raise the host's NOFILE",
        }
    }
}

/// Per-session tally of raw-passthrough frames that fell through to the CPU path, with a one-line
/// budget per distinct reason.
///
/// Rate-limiting is what makes this shippable: `.process` runs per frame, so an unconditional log
/// would flood at the capture rate. Per *reason* rather than per session, because the four reasons
/// diagnose different faults and a transient `NoFormat` at open must not spend the budget a
/// persistent `NotDmabuf` needs.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PassthroughFallbacks {
    frames: u64,
    logged: u8,
}

impl PassthroughFallbacks {
    /// Record one fall-through. Returns `Some(frames_so_far)` the FIRST time each distinct reason
    /// is seen this session — the caller logs then and only then, so at most four lines per
    /// session regardless of frame rate.
    pub(super) fn note(&mut self, reason: PassthroughFallback) -> Option<u64> {
        self.frames += 1;
        let bit = reason.bit();
        (self.logged & bit == 0).then(|| {
            self.logged |= bit;
            self.frames
        })
    }
}

/// Consecutive tiled-import failures (worker alive, e.g. a per-buffer `EGL_BAD_MATCH`) before
/// the stream is poisoned for rebuild. A tiled import failure must NEVER fall through to the
/// CPU mmap path — de-padding tiled bytes as linear produces a scrambled image — so after a
/// short streak of dropped frames the capturer fails loudly and the session renegotiates.
const IMPORT_FAIL_POISON: u32 = 3;

/// Log a frame-drop reason once per process (the process callback runs per frame; a stuck
/// pipeline must say why without flooding).
fn warn_once(msg: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&msg) {
        seen.push(msg);
        tracing::warn!("{msg}");
    }
}

/// A read-only mmap of a dmabuf fd, unmapped on drop. Used when MAP_BUFFERS didn't map the
/// buffer (producers don't always flag dmabufs mappable, e.g. gamescope's Vulkan exports).
struct DmabufMap {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

impl DmabufMap {
    fn new(fd: i32, len: usize) -> Option<DmabufMap> {
        // SAFETY: a null `addr` lets the kernel choose the mapping address; `fd` is a caller-owned
        // dmabuf/MemFd fd, valid for the duration of this call, and `len` is the requested map length.
        // `mmap` reads no Rust memory — it installs a fresh PROT_READ/MAP_SHARED page mapping and
        // returns its base (or MAP_FAILED, checked below before `DmabufMap` adopts it). The returned
        // region is a brand-new VMA, so it aliases no live Rust object, and it keeps the underlying
        // object mapped independently of `fd` (which may be closed after this returns).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        (ptr != libc::MAP_FAILED).then_some(DmabufMap { ptr, len })
    }
}

impl Drop for DmabufMap {
    fn drop(&mut self) {
        // SAFETY: `self.ptr`/`self.len` are exactly the base+length of a successful `mmap` in
        // `DmabufMap::new` (constructed only when `ptr != MAP_FAILED`). This `DmabufMap` uniquely owns
        // that mapping and `drop` runs once, so `munmap` releases a live mapping exactly once — no
        // double-unmap. Every `&[u8]` derived from the mapping is bounded by this `DmabufMap`'s
        // lifetime, so no borrow outlives the unmap.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// De-pad / import a single PipeWire buffer and push it to the encoder. Called from the
/// `.process` callback with the NEWEST drained buffer (latest-frame-only). `datas` is sourced
/// via the same transparent cast libspa's `Buffer::datas_mut` performs, so the safe `Data`
/// accessors (`.type_()`, `.chunk()`, `.data()`, `.fd()`, `.as_raw()`) keep working.
fn consume_frame(ud: &mut UserData, spa_buf: *mut spa::sys::spa_buffer) {
    // No active stream: release the buffer without the (expensive at 5K) de-pad.
    if !ud.signals.active.load(Ordering::Relaxed) {
        return;
    }
    // Poisoned (GPU import lost): the capturer is already surfacing an error to the encode
    // loop; skip per-frame work until the rebuild tears this stream down.
    if ud.signals.broken.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: `spa_buf` is the `*mut spa_buffer` of the PipeWire buffer we dequeued and still hold for
    // this `.process` callback (not requeued until after `consume_frame` returns), so it is live. The
    // block null-checks `spa_buf`, requires `n_datas != 0`, and null-checks the `datas` array pointer
    // before forming any slice. `(*spa_buf).datas` points to `n_datas` libspa `spa_data` structs, and
    // `pw::spa::buffer::Data` is `#[repr(transparent)]` over `spa_data` (the same cast
    // `Buffer::datas_mut` performs — see the function doc), so the pointer cast + length describe
    // exactly that array, in bounds. The PipeWire loop is single-threaded and owns the buffer here, so
    // this `&mut` slice is the only reference to it (no aliasing/data race).
    let datas: &mut [pw::spa::buffer::Data] = unsafe {
        if spa_buf.is_null() || (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(
                (*spa_buf).datas as *mut pw::spa::buffer::Data,
                (*spa_buf).n_datas as usize,
            )
        }
    };
    if datas.is_empty() {
        return;
    }
    let sz = ud.info.size();
    let (w, h) = (sz.width as usize, sz.height as usize);
    if w == 0 || h == 0 {
        return; // format not negotiated yet
    }

    // Implicit-fence wait: Mutter renders into the dmabuf and hands it over at
    // GPU-submit time; with no producer explicit sync (Mutter+NVIDIA can't) we snapshot
    // the buffer's implicit fence and wait the producer's render before sampling —
    // closing the stale/old-frame race on NVIDIA. No-op for shm buffers or drivers that
    // attach no fence. Covers both the GPU import and the CPU mmap read below.
    if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
        match pf_zerocopy::dmabuf_fence::wait_read_ready(datas[0].fd(), 100) {
            Ok(outcome) => {
                static F1: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if F1.swap(false, Ordering::Relaxed) {
                    tracing::info!(
                        ?outcome,
                        "dmabuf implicit-fence sync active (Signaled → driver fences the \
                         render, race closed; NoFence → no implicit fence, zero-copy may \
                         still show stale frames; TimedOut → fence pending past 100ms, \
                         proceeded anyway)"
                    );
                }
            }
            Err(e) => {
                static F2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if F2.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        error = %e,
                        "dmabuf EXPORT_SYNC_FILE failed — no implicit-fence sync; NVIDIA \
                         zero-copy may show stale frames (no producer explicit sync)"
                    );
                }
            }
        }
    }

    // Raw DMA-BUF passthrough: packed RGB is imported for GPU CSC; producer-native NV12 can
    // be consumed by the Vulkan Video encoder without another color conversion.
    //
    // The block below either publishes a dmabuf and RETURNS, or breaks with the reason it could
    // not — so every non-success exit is named and counted instead of silently falling out of
    // three nested `if`s into the CPU path, which is how a session that had negotiated zero-copy
    // could pay a full CPU pixel touch per frame while logging nothing but a healthy open.
    if ud.vaapi_passthrough {
        let reason = 'passthrough: {
            let Some(fmt) = ud.format else {
                break 'passthrough PassthroughFallback::NoFormat;
            };
            if datas[0].type_() != pw::spa::buffer::DataType::DmaBuf {
                break 'passthrough PassthroughFallback::NotDmabuf;
            }
            let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
                break 'passthrough PassthroughFallback::NoFourcc;
            };
            let chunk = datas[0].chunk();
            let offset = chunk.offset();
            let stride = chunk.stride().max(0) as u32;
            // Native NV12 usually arrives as a two-plane SPA buffer over ONE buffer
            // object; plane 1's chunk carries the REAL UV offset/stride (compositors
            // may align the Y plane before UV). Pass it through instead of assuming
            // contiguity. Each spa_data holds its own (dup'd) fd, so BO identity is
            // by inode, not fd number; a genuinely two-BO frame cannot travel through
            // the single-fd import — drop it with a diagnosis instead of streaming
            // garbage chroma.
            let plane1 = if fmt == PixelFormat::Nv12 && datas.len() >= 2 && datas[1].fd() > 0 {
                // SAFETY: zeroed `libc::stat` is a valid POD initializer; both fds are
                // owned by the live PipeWire buffer for this callback, and `fstat`
                // only writes the out-param structs, whose fields are read only after
                // the `== 0` success checks.
                let same_bo = unsafe {
                    let mut s0: libc::stat = std::mem::zeroed();
                    let mut s1: libc::stat = std::mem::zeroed();
                    libc::fstat(datas[0].fd() as i32, &mut s0) == 0
                        && libc::fstat(datas[1].fd() as i32, &mut s1) == 0
                        && (s0.st_dev, s0.st_ino) == (s1.st_dev, s1.st_ino)
                };
                if !same_bo {
                    warn_once(
                        "NV12 planes live in different buffer objects — frames \
                                 dropped (single-fd import only)",
                    );
                    // Not a fall-through: this frame is DROPPED, not downgraded (de-padding it as
                    // linear would stream scrambled chroma), so it is not counted below.
                    return;
                }
                let c1 = datas[1].chunk();
                Some((c1.offset(), c1.stride().max(0) as u32))
            } else {
                None
            };
            // dup the fd so it survives the SPA buffer recycle — the encode thread
            // imports it. Content stability across the brief import/encode window relies
            // on the compositor's buffer-pool depth, like any zero-copy capture.
            // SAFETY: `datas[0].fd()` is the dmabuf fd owned by the live PipeWire buffer (valid
            // for this callback). `fcntl(fd, F_DUPFD_CLOEXEC, 0)` reads only the integer fd,
            // touches no Rust memory, and returns a fresh independent CLOEXEC duplicate (or -1).
            // The original stays owned by PipeWire; the dup is a new fd we own (checked >= 0).
            let dup = unsafe { libc::fcntl(datas[0].fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                break 'passthrough PassthroughFallback::DupFailed;
            }
            let pts_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            ud.publish(CapturedFrame {
                width: w as u32,
                height: h as u32,
                pts_ns,
                format: fmt,
                payload: FramePayload::Dmabuf(DmabufFrame {
                    // SAFETY: `dup` is the fresh fd `fcntl(F_DUPFD_CLOEXEC)` just returned
                    // (checked `dup >= 0`); nothing else owns it, so `OwnedFd` takes sole
                    // ownership and closes it exactly once on drop — no alias, no
                    // double-close.
                    fd: unsafe { OwnedFd::from_raw_fd(dup) },
                    fourcc,
                    modifier: ud.modifier,
                    offset,
                    stride,
                    plane1,
                }),
                // Cursor-as-metadata is blended only by RGB→NV12 backends. Gamescope
                // embeds its pointer in the produced pixels, so native NV12 has none.
                cursor: ud.cursor.overlay(),
            });
            static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
            if ONCE.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    w,
                    h,
                    modifier = ud.modifier,
                    fourcc = format_args!("{:#010x}", fourcc),
                    source = if fmt == PixelFormat::Nv12 {
                        "producer-native NV12"
                    } else {
                        "packed RGB (encoder GPU CSC)"
                    },
                    "zero-copy: handing the raw DMA-BUF to the encoder"
                );
            }
            return;
        };
        // The passthrough declined this frame. Say so ONCE per distinct reason (`.process` runs
        // per frame — see `PassthroughFallbacks`), carrying the running count so a persistent
        // downgrade is distinguishable from a one-frame hiccup at renegotiation.
        if let Some(frames) = ud.passthrough_fallbacks.note(reason) {
            tracing::warn!(
                frames,
                "zero-copy raw-dmabuf passthrough did not take this frame: {} — {} ({})",
                reason.as_str(),
                if reason.falls_back_to_cpu() {
                    "it falls back to the CPU capture path, costing a full-resolution mmap \
                     de-pad plus the encoder's own upload on every such frame"
                } else {
                    "the frame is DROPPED — the CPU de-pad path needs the negotiated format \
                     too, so nothing streams while this persists"
                },
                reason.hint()
            );
        }
    }

    // Zero-copy path: if the buffer is a dmabuf and we have an importer, import it
    // into a CUDA device buffer (no CPU touch) and deliver that. Otherwise fall
    // through to the shm de-pad copy below.
    let mut gpu_import_broken = false;
    if let (Some(importer), Some(fmt)) = (ud.importer.as_mut(), ud.format) {
        // Invariant 1's teeth: a 10-bit PQ frame may take the LINEAR (Vulkan-bridge → CUDA
        // external memory) arm, which moves 4 Bpp words verbatim, but must NEVER take the TILED
        // EGL de-tile blit — that renders into an 8-bit `GL_RGBA8` texture and would crush the
        // depth silently. The HDR pods advertise LINEAR only, so a tiled modifier here means the
        // producer ignored the offer; drop to the CPU path rather than trust it.
        let hdr_tiled = fmt.is_hdr_rgb10() && ud.modifier != 0;
        if hdr_tiled {
            warn_once(
                "HDR frame arrived with a tiled modifier — the GPU de-tile blit is 8-bit, so \
                 this stream falls back to the CPU path (the producer ignored our LINEAR-only \
                 HDR offer)",
            );
        }
        if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf && !hdr_tiled {
            let plane = pf_zerocopy::DmabufPlane {
                fd: datas[0].fd(),
                offset: datas[0].chunk().offset(),
                stride: datas[0].chunk().stride().max(0) as u32,
            };
            // Tiled modifier → EGL/GL de-tile import; LINEAR (0/unset, e.g.
            // gamescope) → direct CUDA external-memory import (NVIDIA EGL can't
            // sample LINEAR).
            let modifier = (ud.modifier != 0).then_some(ud.modifier);
            if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                // GPU converts: a 4:4:4 session gets the planar-YUV444 convert on the tiled
                // EGL/GL path (full chroma, takes precedence over NV12 — 4:4:4 must never
                // subsample), otherwise `PUNKTFUNK_NV12` gets NV12 — tiled via the EGL/GL
                // blit, LINEAR/gamescope via the Vulkan bridge's compute CSC (latency plan
                // T2.5b) — so NVENC encodes native YUV and skips its internal RGB→YUV CSC on
                // the contended SM. A 4:4:4 session on LINEAR frames has no convert and
                // stays RGB, falling to the encoder's clear-error path (`want_444` with an
                // RGB CUDA payload) rather than silently subsampling. A LINEAR NV12 convert
                // failure latches RGB for the stream (mid-frame fallback, no drop).
                // A 10-bit frame takes NEITHER convert: both the GL and the Vulkan compute CSCs
                // write 8-bit planes, and NVENC ingests the packed 10-bit RGB natively
                // (`ARGB10`/`ABGR10`) with its own BT.2020 CSC. So HDR stays packed RGB all the
                // way to the encoder — no depth loss, no extra pass.
                let ten_bit = fmt.is_hdr_rgb10();
                let yuv444 = ud.yuv444 && modifier.is_some() && !ten_bit;
                let mut nv12 = ud.nv12 && !ud.yuv444 && !ten_bit;
                let imported = if let Some(m) = modifier {
                    if yuv444 {
                        importer.import_yuv444(&plane, w as u32, h as u32, fourcc, Some(m))
                    } else if nv12 {
                        importer.import_nv12(&plane, w as u32, h as u32, fourcc, Some(m))
                    } else {
                        importer.import(&plane, w as u32, h as u32, fourcc, Some(m))
                    }
                } else if nv12 && !ud.linear_nv12_failed {
                    match importer.import_linear_nv12(&plane, w as u32, h as u32) {
                        Ok(buf) => Ok(buf),
                        Err(e) => {
                            ud.linear_nv12_failed = true;
                            nv12 = false;
                            tracing::warn!(error = %format!("{e:#}"),
                                "LINEAR NV12 compute CSC failed — RGB for the rest of this \
                                 stream (NVENC does the CSC internally)");
                            importer.import_linear(&plane, w as u32, h as u32)
                        }
                    }
                } else {
                    nv12 = false;
                    importer.import_linear(&plane, w as u32, h as u32)
                };
                match imported {
                    Ok(devbuf) => {
                        ud.import_fail_streak = 0;
                        pf_zerocopy::note_gpu_import_ok();
                        static ONCE: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(true);
                        if ONCE.swap(false, Ordering::Relaxed) {
                            tracing::info!(
                                w,
                                h,
                                modifier = ud.modifier,
                                nv12,
                                yuv444,
                                "zero-copy: dmabuf imported to CUDA (no CPU copy)"
                            );
                        }
                        let pts_ns = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        ud.publish(CapturedFrame {
                            width: w as u32,
                            height: h as u32,
                            pts_ns,
                            format: if yuv444 {
                                PixelFormat::Yuv444
                            } else if nv12 {
                                PixelFormat::Nv12
                            } else {
                                fmt
                            },
                            payload: FramePayload::Cuda(devbuf),
                            // Cursor-as-metadata: blended by the CUDA encoder into its owned
                            // device surface. (RGB LINEAR-import case; YUV sessions blend planes.)
                            cursor: ud.cursor.overlay(),
                        });
                        return;
                    }
                    Err(e) => {
                        let dead = importer.dead();
                        if dead {
                            pf_zerocopy::note_gpu_import_death();
                        }
                        if modifier.is_some() {
                            // Tiled buffer: the CPU fallback below would mmap TILED bytes
                            // and de-pad them as linear — a scrambled image, worse than no
                            // frame. Drop the frame instead; on a dead worker (it absorbed a
                            // driver fault) or a short failure streak, poison the stream so
                            // the session's capture-loss rebuild renegotiates cleanly.
                            ud.import_fail_streak += 1;
                            if dead || ud.import_fail_streak >= IMPORT_FAIL_POISON {
                                tracing::error!(error = %format!("{e:#}"), dead,
                                    "tiled GPU import lost — failing this capture for rebuild");
                                ud.signals.broken.store(true, Ordering::Relaxed);
                            } else {
                                tracing::warn!(error = %format!("{e:#}"),
                                    streak = ud.import_fail_streak,
                                    "tiled dmabuf GPU import failed — frame dropped");
                            }
                            return;
                        }
                        // LINEAR dmabuf: CPU-mappable, so disable the importer and fall
                        // through to the CPU mmap path — degraded, not dead.
                        tracing::warn!(error = %format!("{e:#}"),
                            "LINEAR dmabuf GPU import failed — falling back to the CPU copy path");
                        gpu_import_broken = true;
                    }
                }
            } else {
                return; // format has no DRM fourcc mapping — skip the frame
            }
        }
    }
    if gpu_import_broken {
        ud.importer = None;
    }

    let d = &mut datas[0];
    // CPU path may also receive LINEAR dmabufs (gamescope offers only those once its
    // modifier-bearing format pod wins); capture the fd before `data()` borrows `d`.
    let data_type = d.type_();
    // fd-backed buffer (MemFd SHM, or DmaBuf)? Capture the fd before `data()` borrows `d`.
    let raw_fd = d.fd();
    // `mapoffset` is where THIS spa_data's region begins inside the fd — non-zero for a pooled
    // producer that carves every buffer out of one shared fd. PipeWire's own MAP_BUFFERS slice
    // already starts there, but our self-mmap below maps the fd from 0, so that path must add it
    // (see `region_off`). Reading it without it made the self-mmap path index the WRONG buffer,
    // and the `needed > avail` guard could not catch that: `avail` came from the whole-fd
    // mapping, so it was large enough for any single buffer's span.
    let map_off = d.as_raw().mapoffset as usize;
    let (size, chunk_off, stride) = {
        let c = d.chunk();
        (
            c.size() as usize,
            c.offset() as usize,
            c.stride().max(0) as usize,
        )
    };
    let Some(fmt) = ud.format else { return }; // unsupported/not negotiated
                                               // The de-pad below assumes ONE packed plane of `bytes_per_pixel` bytes. `bytes_per_pixel`'s
                                               // catch-all answers 4 for NV12, so a producer-native NV12 buffer (stride ≈ w, two planes)
                                               // computed `row = 4w` and always tripped the `stride < row` guard below — blaming the
                                               // PRODUCER's stride for a host limitation. NV12 cannot be de-padded on this path at all
                                               // (the second plane is not even in `datas[0]`'s span), so say so honestly. It only arrives
                                               // here if a native-NV12 negotiation happens without the zero-copy passthrough that is
                                               // supposed to consume it, which is a host bug, not a producer one.
    if matches!(fmt, PixelFormat::Nv12) {
        warn_once(
            "negotiated producer-native NV12 but this capture fell back to the CPU de-pad path, \
             which handles single-plane packed formats only — frames dropped (the NV12 offer is \
             only valid under the raw-dmabuf passthrough that imports it directly)",
        );
        return;
    }
    let bpp = fmt.bytes_per_pixel();
    let row = w * bpp;
    let stride = if stride == 0 { row } else { stride };
    if stride < row {
        warn_once("chunk stride < row — frames dropped");
        return;
    }
    let needed = stride * (h - 1) + row;
    // dmabuf chunks commonly report size 0; fall back to the computed span.
    let size = if size == 0 { needed } else { size };
    // For fd-backed buffers (MemFd SHM, DmaBuf) mmap the fd OURSELVES, sized to the fd's real
    // length (fstat), rather than trusting PipeWire's MAP_BUFFERS slice: xdg-desktop-portal-wlr
    // hands MemFd buffers whose reported `data.maxsize` exceeds the bytes actually mapped into
    // our process, so reading to maxsize segfaults (it also covers the original case — MAP_BUFFERS
    // not mapping Vulkan dmabufs, e.g. gamescope). The `needed > avail` guard below then drops
    // cleanly if the real buffer is genuinely too small. MemPtr buffers (no fd) are same-process —
    // trust `d.data()`.
    let fd_len = if raw_fd > 0 {
        // SAFETY: `libc::stat` is a C plain-old-data struct for which all-zero is a valid value, so
        // `mem::zeroed()` is a sound initializer. `raw_fd` is the buffer's fd (`> 0` checked here) and
        // valid for this callback; `fstat` writes metadata into `&mut st`, a live, aligned,
        // correctly-sized stack `stat` that outlives the synchronous call. `st.st_size` is read only
        // after the return value is confirmed `== 0`. `st` is a fresh local, so nothing aliases it.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(raw_fd as i32, &mut st) == 0 && st.st_size > 0)
                .then_some(st.st_size as usize)
        }
    } else {
        None
    };
    let _mapping; // keeps a manual mmap alive for the copy below
                  // Prefer our own fstat-sized mmap of the fd; fall back to PipeWire's MAP_BUFFERS slice
                  // (and finally drop) so an fd PipeWire could map but we can't never silently over-reads.
                  //
                  // `fd_len` is REQUIRED, not preferred: it used to fall back to
                  // `offset + needed`, i.e. a length invented from producer-controlled geometry.
                  // That defeated the whole point of the fstat (the "buffer smaller than the
                  // frame span" guard compares against exactly the number the producer just
                  // supplied) and could map — and then read — past the end of the object, which
                  // is a SIGBUS, not an `Err`. Without a real length we decline to self-map and
                  // let PipeWire's own slice serve, which is bounded by construction.
    let self_mapped: Option<&[u8]> = if raw_fd > 0 {
        match fd_len.and_then(|map_len| DmabufMap::new(raw_fd as i32, map_len)) {
            Some(m) => {
                _mapping = m;
                // SAFETY: `_mapping` is the `DmabufMap` just stored; its `ptr`/`len` come from a
                // successful `mmap` of `map_len` PROT_READ bytes, so `ptr` is non-null, page-aligned,
                // and the VMA is one allocated object of `len` bytes valid for reads. In the common
                // path `map_len == fd_len` (the fd's real size from `fstat`), so the mapping spans the
                // whole object; the de-pad copy below is further bounded by the `offset <= buf.len()`
                // and `needed > avail` guards. The `&[u8]` borrows `_mapping`, which lives to the end
                // of `consume_frame`, so the slice never outlives the mapping, and the memory is only
                // read here, so there is no aliasing/mutation.
                Some(unsafe { std::slice::from_raw_parts(_mapping.ptr as *const u8, _mapping.len) })
            }
            None => None,
        }
    } else {
        None
    };
    // Which base `chunk.offset` is relative to differs by path: our self-mmap starts at fd
    // offset 0, so this spa_data's region begins at `mapoffset`; PipeWire's MAP_BUFFERS slice
    // already begins there. Checked add — both halves are producer-controlled.
    let (buf, region_off): (&[u8], usize) = if let Some(b) = self_mapped {
        match map_off.checked_add(chunk_off) {
            Some(off) => (b, off),
            None => {
                warn_once("mapoffset + chunk offset overflows — frames dropped");
                return;
            }
        }
    } else if let Some(data) = d.data() {
        (data, chunk_off)
    } else {
        warn_once("buffer has no mappable data — frames dropped");
        return;
    };
    // Need stride*(h-1)+row valid bytes within [region_off, region_off+size).
    if region_off > buf.len() {
        return;
    }
    let avail = buf.len() - region_off;
    {
        // One-time geometry dump — makes a new compositor/GPU's buffer layout visible in the
        // logs (the kind of mismatch that crashed xdpw MemFd capture before the self-mmap fix).
        use std::sync::atomic::{AtomicBool, Ordering};
        static ONCE: AtomicBool = AtomicBool::new(true);
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                stride, size, chunk_off, map_off, region_off, buf_len = buf.len(), needed,
                data_type = ?data_type, fd_len = ?fd_len, self_mapped = self_mapped.is_some(),
                "capture CPU de-pad geometry (first frame)"
            );
        }
    }
    if needed > avail || needed > size {
        warn_once("buffer smaller than frame span — frames dropped");
        return;
    }
    let region = &buf[region_off..region_off + size.min(avail)];
    // De-pad into a tightly-packed buffer (chunk stride may exceed w*bpp).
    let mut tight = vec![0u8; row * h];
    for y in 0..h {
        tight[y * row..y * row + row].copy_from_slice(&region[y * stride..y * stride + row]);
    }
    // Cursor-as-metadata: blit the latched pointer into the frame (no-op when hidden or when
    // the layout isn't packed RGB). This is the CPU path's counterpart to the producer's
    // hardware cursor plane, which stays out of the captured buffer.
    composite_cursor(&mut tight, w, h, fmt, &ud.cursor);
    let pts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let frame = CapturedFrame {
        width: w as u32,
        height: h as u32,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(tight),
        // Already composited inline into `tight` above — nothing for the encoder to blend.
        cursor: None,
    };
    // Overwrite whatever the consumer has not taken yet — never block the pipewire loop.
    ud.publish(frame);
}

#[allow(clippy::too_many_arguments)]
pub fn pipewire_thread(
    fd: Option<OwnedFd>,
    node_id: u32,
    // The frame mailbox + its wakeup sender (see `super::FrameSlot`): publishing OVERWRITES,
    // so a stalled consumer costs the intermediate frames, never the freshest one.
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    // Every flag/slot this thread PUBLISHES, in one struct (see `super::CaptureSignals`). It used to
    // be seven separate parameters here, seven `Arc` clones at the call site, and the same seven
    // fields restated on `UserData`.
    signals: super::CaptureSignals,
    // THE zero-copy negotiation decision, resolved once by `spawn_pipewire` (which consumes the
    // same struct for the capturer's timeout diagnosis) — never re-derived here.
    plan: NegotiationPlan,
    // The session's capture options (see `super::CaptureOpts`) — `want_444`/`want_hdr` pick the pod
    // family, `expect_exact_dims` arms the sacrificial-mode gate. `allow_zerocopy` is already folded
    // into `plan`.
    opts: super::CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    quit_rx: pw::channel::Receiver<()>,
    // Encode-backend facts resolved by the facade (never re-derived here) — the one-way
    // capture→encode edge (plan §W6).
    policy: ZeroCopyPolicy,
) -> Result<()> {
    let super::CaptureOpts {
        want_444,
        want_hdr,
        expect_exact_dims,
        cursor_id0_hides,
        ..
    } = opts;
    crate::pwinit::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    // A quit signal (capturer `Drop`) lands here on the loop thread and stops `run()` so the
    // thread unwinds instead of blocking to process exit. Hold the attachment for the loop's
    // life; the cloned loop handle is the one the callback quits.
    let quit_loop = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), move |()| {
        tracing::debug!("pipewire: quit signal received — stopping capture loop");
        quit_loop.quit();
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    // A portal source hands us an fd to a (sandboxed) PipeWire remote; the KWin
    // virtual-output source has no fd — its node lives on the user's default daemon.
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?,
        None => context
            .connect_rc(None)
            .context("pw connect (default daemon)")?,
    };

    // The negotiation decision arrives pre-resolved in `plan` (see `negotiation_plan`): what to
    // build, which offer to make, and whether to prefer producer NV12. Nothing here re-derives
    // any of it — that duplication is what F1/L3 was.
    let backend_is_vaapi = policy.backend_is_vaapi;
    let force_shm = plan.force_shm;
    let vaapi_passthrough = plan.vaapi_passthrough;
    let prefer_native_nv12 = plan.prefer_native_nv12;
    // Build the GPU importer — normally the ISOLATED worker process
    // (design/zerocopy-worker-isolation.md), so a driver fault on a dying compositor's dmabuf
    // kills the worker, not this host. If construction fails, log and fall back to the CPU path
    // (we simply won't request dmabuf below); `plan.build_importer` already encodes WHEN to try
    // at all (not under a raw passthrough, not once repeated worker deaths latched the import
    // off — see `negotiation_plan`).
    if plan.gpu_import_latched {
        tracing::warn!(
            "zero-copy GPU import disabled for this host process (repeated import-worker deaths, \
             or a previous dmabuf negotiation timeout) — using CPU path"
        );
    }
    let mut importer = if plan.build_importer {
        match pf_zerocopy::Importer::new_for_capture() {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                None
            }
        }
    } else {
        None
    };
    if prefer_native_nv12 {
        tracing::info!(
            "zero-copy: preferring gamescope producer-side NV12 LINEAR DMA-BUF (no host \
             RGB CSC; PUNKTFUNK_PIPEWIRE_NV12=0 restores the packed-RGB negotiation)"
        );
    }
    // Modifiers our import stack handles for BGRx: the EGL-importable (tiled) set, plus LINEAR
    // (0) — NVIDIA's EGL won't list it, but LINEAR dmabufs (gamescope's only offer) import via
    // CUDA external memory instead. For the VAAPI passthrough path we advertise LINEAR only:
    // radeonsi/iHD import it and any compositor can allocate it.
    let mut modifiers = importer
        .as_mut()
        .map(|i| i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgrx).unwrap()))
        .unwrap_or_default();
    if (importer.is_some() || vaapi_passthrough) && !modifiers.contains(&0) {
        modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
    }
    // PyroWave passthrough: the encoder imports through Vulkan, not libva — extend the
    // advertisement with every modifier its device samples from, so compositors that
    // never allocate LINEAR (Mutter+NVIDIA) still negotiate zero-copy dmabufs. The modifiers
    // were resolved by the facade (`ZeroCopyPolicy::pyrowave_modifiers`) — non-empty only when
    // the host's `pyrowave` feature is on AND the session (or the global encoder pref) is
    // PyroWave — so capture never calls back into `encode` and needs no feature gate of its
    // own (the emptiness check gates it).
    if vaapi_passthrough && !policy.pyrowave_modifiers.is_empty() {
        for &m in &policy.pyrowave_modifiers {
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    // The one runtime-dependent half of the decision (see `NegotiationPlan::want_dmabuf`): it
    // needs the modifier list the importer's construction actually yielded.
    let want_dmabuf = plan.want_dmabuf(importer.is_some(), &modifiers);
    // Record whether THIS capture really advertises the EGL→CUDA dmabuf-only offer, for the
    // capturer's negotiation-timeout diagnosis (its latch must fire only for an offer that was
    // actually made — `plan.build_importer` alone can't know the importer constructed).
    signals.gpu_dmabuf_offer.store(
        want_dmabuf && !vaapi_passthrough && !want_hdr,
        Ordering::Relaxed,
    );
    // The ONE line that says which arm this session actually got, and who consumes it. Everything
    // below explains a particular arm; nothing below states the answer. Reconstructing it from the
    // detail lines cost the 2026-08-08 PyroWave triage four files, and on the one arm that matters
    // most — a PyroWave session downgraded to CPU on an NVIDIA host — there was no detail line to
    // reconstruct it from at all (see the final `else if` of this chain).
    let consumer = consumer_kind(
        policy.pyrowave_session,
        backend_is_vaapi,
        policy.backend_is_gpu,
    );
    let arm = resolved_capture_arm(&plan, importer.is_some(), want_dmabuf);
    tracing::info!(
        capture_arm = arm.as_str(),
        consumer = consumer.as_str(),
        modifier_count = modifiers.len(),
        "capture pipeline resolved: {} → {}",
        arm.as_str(),
        consumer.as_str()
    );
    if force_shm {
        tracing::info!(
            "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
        );
    } else if plan.raw_dmabuf_latched {
        tracing::warn!(
            "zero-copy raw-dmabuf passthrough disabled for this host process (repeated encoder \
             import failures, or a previous dmabuf negotiation timeout) — capturing CPU frames \
             instead"
        );
    } else if !want_dmabuf && (plan.build_importer || plan.vaapi_passthrough) {
        tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
    } else if vaapi_passthrough {
        // The raw-passthrough advertisement. Covers the PyroWave case too: its extra
        // Vulkan-importable modifiers were appended (and logged) just above, so this arm must
        // NOT be gated on `pyrowave_modifiers.is_empty()` — that gate is what dropped a fully
        // zero-copy PyroWave session through to the CPU-path warning below (L11).
        tracing::info!(
            native_nv12_preferred = prefer_native_nv12,
            modifier_count = modifiers.len(),
            pyrowave_extended = !policy.pyrowave_modifiers.is_empty(),
            "zero-copy: advertising DMA-BUF modifiers for direct encoder import (LINEAR \
             always; native NV12 first when enabled, packed RGB fallback)"
        );
    } else if want_dmabuf {
        tracing::info!(
            count = modifiers.len(),
            sample = ?&modifiers[..modifiers.len().min(6)],
            "zero-copy: advertising EGL-importable dmabuf modifiers"
        );
    } else if consumer.cpu_is_downgrade() {
        // Reached only when no dmabuf is advertised at all (every arm above rules out a
        // zero-copy path), so this genuinely IS the CPU capture path: the consumer then pays
        // full-frame CPU touches (mmap de-pad + whatever CSC/upload it needs) —
        // make the silent fallback visible.
        // The `raw_dmabuf_latched` arm above catches the latched downgrade, so by here zero-copy
        // is off at the source: the env var, or the session's own output format.
        //
        // The gate used to be `backend_is_vaapi && backend_is_gpu`, which is why a PyroWave
        // session's CPU downgrade was invisible on an NVIDIA/auto host: `backend_is_vaapi` reads
        // the host-global encoder pref, so a per-session PyroWave negotiation there is `false` and
        // fell out of the chain logging NOTHING. `consumer_kind` asks the per-session question
        // instead, and excludes only the software encoder (which wants CPU frames).
        tracing::warn!(
            consumer = consumer.as_str(),
            "{} encode with the CPU capture path (per-frame de-pad + CSC + upload) — \
             zero-copy is off for this capture ({}); set PUNKTFUNK_ZEROCOPY=1 to restore the \
             dmabuf default",
            consumer.as_str(),
            if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                "PUNKTFUNK_ZEROCOPY is set falsy"
            } else if want_hdr && !policy.hdr_cuda_ok {
                // Reachable and NOT the output format's doing: `build_importer` drops an HDR
                // capture whose encoder cannot take a packed 10-bit CUDA payload (libav's HDR
                // route swscales into a P010 hardware frame). Naming the output format here
                // would send the reader hunting the wrong knob.
                "this HDR session's encoder cannot ingest a 10-bit CUDA payload, so the capture \
                 stays on CPU frames"
            } else {
                "this session's output format asked for CPU frames"
            }
        );
    }
    if want_dmabuf && !vaapi_passthrough && want_444 {
        tracing::info!(
            "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
             NVENC fed native full-chroma YUV, no CPU pixel path"
        );
    } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
        tracing::info!(
            "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709 limited) on the GPU — NVENC \
             fed native YUV (no internal RGB→YUV CSC)"
        );
    }

    let data = UserData {
        info: VideoInfoRaw::default(),
        format: None,
        modifier: 0,
        slot,
        wake,
        signals,
        import_fail_streak: 0,
        importer,
        vaapi_passthrough,
        nv12: pf_zerocopy::nv12_enabled(),
        yuv444: want_444,
        linear_nv12_failed: false,
        dbg_log_n: 0,
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(cursor_id0_hides),
        expect_dims: if expect_exact_dims {
            preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "punktfunk-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE     => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE     => "Screen",
            // Never let the session manager re-target this stream to a different node when
            // its target goes away: an orphaned stream auto-linked to a fresh Video/Source
            // wedges that node — and a stuck link head-blocks the PipeWire daemon's shared
            // work queue, stalling ALL new link negotiation system-wide.
            "node.dont-reconnect"     => "true",
        },
    )
    .context("pw Stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_stream, ud, old, new| {
            tracing::info!(?old, ?new, "pipewire stream state");
            // Track whether the node is actively producing. A live source sits in `Streaming`
            // (a static desktop just sends no buffers); anything else — `Paused`/`Unconnected`/
            // `Error` — means the source went away (compositor died, virtual output removed on a
            // Gaming↔Desktop switch). `try_latest` turns a sustained non-Streaming state into a
            // capture-loss so the encode loop rebuilds instead of freezing on the last frame.
            ud.signals.streaming.store(
                matches!(new, pw::stream::StreamState::Streaming),
                Ordering::Relaxed,
            );
        })
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) =
                pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if ud.info.parse(param).is_ok() {
                ud.signals.negotiated.store(true, Ordering::Relaxed);
                // A (re)negotiation replaces the buffer pool: every cached per-buffer import
                // (stored fds in the worker, the Vulkan bridge's per-fd sources) keys on
                // buffers that no longer exist — and a recycled fd number/inode must never
                // resolve to a stale import. No-op on the first negotiation (empty caches).
                if let Some(imp) = ud.importer.as_mut() {
                    imp.clear_cache();
                }
                let sz = ud.info.size();
                // Publish the negotiated size for the gamescope cursor source's root→frame
                // scaling (`xfixes_cursor::scale_to_frame`); a renegotiation updates it.
                ud.signals.frame_size.store(
                    (u64::from(sz.width) << 32) | u64::from(sz.height),
                    Ordering::Relaxed,
                );
                ud.format = map_format(ud.info.format());
                ud.modifier = ud.info.modifier();
                // HDR: the 10-bit PQ formats are only ever offered with MANDATORY BT.2020/PQ
                // colorimetry props, so a 10-bit negotiation IS an HDR negotiation — but log
                // what the producer actually fixated for diagnosis.
                let hdr = ud.format.is_some_and(|f| f.is_hdr_rgb10());
                ud.signals.hdr_negotiated.store(hdr, Ordering::Relaxed);
                tracing::info!(
                    width = sz.width,
                    height = sz.height,
                    spa_format = ?ud.info.format(),
                    mapped = ?ud.format,
                    modifier = ud.modifier,
                    hdr,
                    transfer_function = ud.info.transfer_function(),
                    color_primaries = ud.info.color_primaries(),
                    "pipewire format negotiated"
                );
                if ud.format.is_none() {
                    tracing::error!(
                        spa_format = ?ud.info.format(),
                        "negotiated a pixel format the encoder cannot consume — frames will be skipped"
                    );
                }
            }
        })
        .process(|stream, ud| {
            // Latest-frame-only (OBS pattern): Mutter delivers buffers in bursts and recycles its
            // pool; an older queued buffer carries a STALE frame. Drain all queued buffers, requeue
            // the older ones, keep only the newest. This dequeue/requeue runs OUTSIDE the
            // `catch_unwind` below — they are non-panicking C FFI pointer ops, and `newest` is
            // requeued exactly once AFTER the panic-containing region. Previously the whole thing was
            // inside the catch, so a caught panic (in `update_cursor_meta`/`consume_frame`) stranded
            // `newest` forever, permanently shrinking the stream's fixed pool until capture wedged.
            // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on the
            // loop thread; `dequeue_raw_buffer` returns a stream-owned `*mut pw_buffer` or null
            // (null-checked), single-threaded so no concurrent access.
            let mut newest = unsafe { stream.dequeue_raw_buffer() };
            if newest.is_null() {
                return;
            }
            let mut drained = 1u32;
            loop {
                // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
                let next = unsafe { stream.dequeue_raw_buffer() };
                if next.is_null() {
                    break;
                }
                // SAFETY: `newest` was dequeued from this stream and not yet requeued; we immediately
                // overwrite it, so the requeued pointer is never touched again.
                unsafe { stream.queue_raw_buffer(newest) };
                newest = next;
                drained += 1;
            }
            // Sacrificial-mode gate (kwin.rs `create`): until the producer renegotiates to the
            // expected dims, every buffer — frame AND cursor meta, whose positions are in the
            // doomed mode's space — belongs to the birth mode; consuming one would build the
            // pipeline at the wrong size. Self-disarms on the first matching negotiation, or
            // after `GATE_DEADLINE` without one — degraded dims beat wedging the session into
            // the first-frame-timeout retry loop when the promised renegotiation never comes.
            if let Some((ew, eh)) = ud.expect_dims {
                /// The renegotiation normally lands within a frame or two of recording; well
                /// past that, the producer is not going to deliver it (the on-glass case: the
                /// real mode never actually applied) — stop starving the pipeline.
                const GATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
                let sz = ud.info.size();
                if sz.width == ew && sz.height == eh {
                    tracing::info!(
                        skipped = ud.gate_skips,
                        width = ew,
                        height = eh,
                        "producer renegotiated to the expected mode — frames flow"
                    );
                    ud.expect_dims = None;
                } else if ud
                    .gate_since
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed()
                    > GATE_DEADLINE
                {
                    tracing::warn!(
                        negotiated_w = sz.width,
                        negotiated_h = sz.height,
                        expected_w = ew,
                        expected_h = eh,
                        skipped = ud.gate_skips,
                        "producer never renegotiated to the expected mode — accepting its \
                         dims (session runs degraded rather than wedged)"
                    );
                    ud.expect_dims = None;
                } else {
                    ud.gate_skips += 1;
                    if ud.gate_skips == 1 || ud.gate_skips.is_power_of_two() {
                        tracing::info!(
                            negotiated_w = sz.width,
                            negotiated_h = sz.height,
                            expected_w = ew,
                            expected_h = eh,
                            n = ud.gate_skips,
                            "holding frames until the producer renegotiates to the expected mode"
                        );
                    }
                    // SAFETY: `newest` was dequeued from this stream and not yet requeued;
                    // requeued exactly once here, then never touched (mirrors the null path).
                    unsafe { stream.queue_raw_buffer(newest) };
                    return;
                }
            }
            // PipeWire dispatches from a C trampoline with no catch_unwind; a panic crossing that FFI
            // boundary would abort the whole host. Contain the inspect/consume work — the only Rust
            // code here that can panic — and requeue `newest` unconditionally after it.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                // load through a valid pointer — no mutation or aliasing.
                let spa_buf = unsafe { (*newest).buffer };

                // Refresh cursor-as-metadata BEFORE the stale-frame skip below: Mutter delivers
                // pointer-only movements as metadata-only "corrupted" buffers we drop for their
                // frame, but their cursor meta is fresh and must still move our overlay.
                update_cursor_meta(&mut ud.cursor, spa_buf);
                // Publish the LIVE overlay (frames or not) so the encode loop's forwarder
                // tracks pointer-only motion on a static desktop — the frame-attached overlay
                // alone stales between damage frames. ONLY when we actually have one: a
                // gamescope node carries no `SPA_META_Cursor`, so `overlay()` is always `None`
                // here, and writing that would clobber — at frame rate — the `Some` the
                // attached XFixes source publishes into this SAME slot, strobing the
                // composited pointer on/off. Portal cursors are `None` only before the first
                // bitmap (nothing to drop), and a HIDDEN pointer is still `Some(visible:false)`.
                if let Some(overlay) = ud.cursor.overlay() {
                    if let Ok(mut slot) = ud.signals.cursor_live.lock() {
                        *slot = Some(overlay);
                    }
                }

                // Inspect the newest buffer's header + first chunk for the diagnostic and the
                // CORRUPTED skip. SPA_META_Header is optional — `hdr` may be null.
                // SAFETY: `spa_buf` is the `*mut spa_buffer` of the buffer we still hold.
                // `spa_buffer_find_meta_data` scans that buffer's metadata array for a `SPA_META_Header`
                // of at least `size_of::<spa_meta_header>()` bytes and returns a pointer into the held
                // buffer's metadata (or null). The size argument matches the struct the result is cast
                // to, and the pointer stays valid as long as the buffer is held (until requeue). Null is
                // handled below.
                let hdr = unsafe {
                    spa::sys::spa_buffer_find_meta_data(
                        spa_buf,
                        spa::sys::SPA_META_Header,
                        std::mem::size_of::<spa::sys::spa_meta_header>(),
                    ) as *const spa::sys::spa_meta_header
                };
                let hdr_flags = if hdr.is_null() {
                    0u32
                } else {
                    // SAFETY: reached only when `hdr` is non-null; it points to a `spa_meta_header`
                    // inside the live buffer's metadata (returned for a size >=
                    // `size_of::<spa_meta_header>()`, so `.flags` is in bounds). A single field read
                    // while the buffer is still held.
                    unsafe { (*hdr).flags }
                };
                // First data chunk's size + flags (used for the diagnostic + CORRUPTED check)
                // and its data type (a dmabuf legitimately reports chunk size 0, so the size-0
                // stale skip only applies to mappable SHM buffers).
                // SAFETY: every dereference is guarded in order before any field read — `spa_buf`
                // non-null, `n_datas > 0`, the `datas` (`*mut spa_data`) array non-null, and the first
                // element's `chunk` (`*mut spa_chunk`) non-null. `d0` is that first `spa_data` and `c`
                // its chunk; reading `(*d0).type_`, `(*c).size`, `(*c).flags` are in-bounds field loads
                // of libspa structs inside the buffer we still hold. Single-threaded loop, no mutation.
                let (chunk_size, chunk_flags, is_dmabuf) = unsafe {
                    if !spa_buf.is_null()
                        && (*spa_buf).n_datas > 0
                        && !(*spa_buf).datas.is_null()
                        && !(*(*spa_buf).datas).chunk.is_null()
                    {
                        let d0 = (*spa_buf).datas;
                        let c = (*d0).chunk;
                        let is_dmabuf =
                            (*d0).type_ == spa::sys::SPA_DATA_DmaBuf;
                        ((*c).size, (*c).flags, is_dmabuf)
                    } else {
                        (0u32, 0i32, false)
                    }
                };

                let corrupted = (hdr_flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED) != 0
                    || (chunk_flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32) != 0;

                // THE GNOME FLASH FIX: skip Mutter's CORRUPTED / size-0 cursor-update buffers.
                // When the pointer moves (e.g. dragging a window) Mutter sends metadata-only
                // buffers flagged CORRUPTED (chunk size 0) that still reference a RECYCLED old
                // frame; consuming them encodes "the window at its old position" — the flash.
                // Confirmed live on worker-3 (chunk_flags=CORRUPTED, size 0) for both the zero-copy
                // and SHM paths. The size-0 half is SHM-only (a real dmabuf legitimately reports
                // chunk size 0). `drained` is the latest-frame-only depth — a cheap extra defense
                // against bursty delivery, though here Mutter sends one buffer per callback.
                if corrupted || (chunk_size == 0 && !is_dmabuf) {
                    ud.dbg_log_n += 1;
                    if ud.dbg_log_n.is_power_of_two() {
                        tracing::debug!(
                            skipped = ud.dbg_log_n,
                            drained,
                            "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                        );
                    }
                    // Skip this stale/cursor buffer — `newest` is requeued unconditionally below.
                    return;
                }

                consume_frame(ud, spa_buf);
            }));
            // Hand `newest` back to the stream exactly once, on EVERY path — normal, corrupted-skip,
            // or a caught panic in the closure above. This single requeue is what keeps the fixed
            // buffer pool from draining.
            // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame) completed
            // inside the closure above; `newest` was dequeued from this stream and not yet requeued.
            unsafe { stream.queue_raw_buffer(newest) };
            if outcome.is_err() {
                // In the per-frame `.process` callback: a deterministic panic (e.g. a bad
                // format) would fire this every frame, so power-of-two throttle it — enough to
                // surface the fault without evicting the whole log ring.
                static PANICS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::error!(count = n, "panic in pipewire process callback — frame dropped");
                }
            }
        })
        .register()
        .context("register stream listener")?;

    // Debug knob: offer a single fixed format (PUNKTFUNK_PW_FIXED_POD="WxH") to bisect
    // negotiation failures against a producer's exact EnumFormat (e.g. gamescope).
    let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));

    // Request raw video in any encoder-mappable layout, any size/framerate.
    let obj = if let Some((fw, fh)) = fixed_pod {
        tracing::info!(
            fw,
            fh,
            "pipewire: offering a fixed BGRx format pod (PUNKTFUNK_PW_FIXED_POD)"
        );
        pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Id,
                VideoFormat::BGRx
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: fw,
                    height: fh
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Fraction,
                pw::spa::utils::Fraction { num: 0, denom: 1 }
            ),
        )
    } else {
        build_default_format_obj(preferred)
    };

    // gamescope trap — the Steam overlay's presence in the stream is decided HERE by omission:
    // gamescope's `paint_pipewire()` composites the overlay (Shift+Tab / Quick Access Menu) into
    // the node it hands us ONLY when the consumer-negotiated `gamescope_focus_appid` is 0 — the
    // default, and the "mirror the focused window + overlay" branch (gamescope ≥ 3.16.23; see
    // `MIN_GAMESCOPE_OVERLAY`). None of the EnumFormat pods below advertise the
    // `SPA_FORMAT_VIDEO_gamescope_focus_appid` property, so gamescope reads 0 and paints the
    // overlay for us for free. DO NOT add a non-zero focus-appid (e.g. to "isolate the game" in a
    // dedicated session) — that flips gamescope into the Remote-Play branch that deliberately
    // drops the overlay (and all host chrome) back out of the capture. The cursor, external
    // overlay (MangoHUD), and notifications are excluded from the node on EVERY gamescope
    // version and are composited host-side instead (see `xfixes_cursor.rs`).
    //
    // When zero-copy is on, offer ONLY a BGRx dmabuf format with our EGL-importable modifiers
    // (offering shm too makes the compositor pick shm). The modifier list goes out as a plain
    // MANDATORY `ChoiceEnum::Enum` and the producer fixates one of the alternatives directly —
    // this is NOT the two-step DONT_FIXATE handshake (libspa 0.9's `ChoiceFlags` cannot express
    // `SPA_POD_PROP_FLAG_DONT_FIXATE`, and `param_changed` only READS the fixated format, it
    // re-emits nothing). Worth revisiting if a multi-modifier offer is ever seen to fail
    // negotiation on a compositor that needs the allocator round-trip. Otherwise offer the
    // multi-format shm pod and let MAP_BUFFERS map it. An HDR session replaces ALL of this with the two 10-bit
    // PQ pods (LINEAR dmabuf, MANDATORY colorimetry — see `build_hdr_dmabuf_format`): offering
    // SDR alongside would make the producer pick its earlier-listed SDR format, and the
    // negotiation-timeout path latches the process-wide SDR downgrade if nothing matches.
    let format_pods: Vec<Vec<u8>> = if want_hdr {
        tracing::info!(
            "HDR capture: offering xRGB_210LE/xBGR_210LE LINEAR dmabufs with MANDATORY \
             BT.2020 + SMPTE-2084 (PQ) colorimetry (GNOME 50+ monitor stream)"
        );
        vec![
            build_hdr_dmabuf_format(VideoFormat::xRGB_210LE, preferred)?,
            build_hdr_dmabuf_format(VideoFormat::xBGR_210LE, preferred)?,
        ]
    } else if want_dmabuf {
        let mut pods = Vec::with_capacity(if prefer_native_nv12 { 2 } else { 1 });
        if prefer_native_nv12 {
            // First compatible consumer pod wins. Gamescope advertises NV12 and BGRx; pinning
            // BT.709 limited here selects its RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(VideoFormat::NV12, &[0], preferred)?);
        }
        pods.push(build_dmabuf_format(
            VideoFormat::BGRx,
            &modifiers,
            preferred,
        )?);
        pods
    } else {
        vec![serialize_pod(obj)?]
    };
    let buffers_values = if want_hdr || want_dmabuf {
        // Dmabuf-only. For HDR this is load-bearing beyond zero-copy: Mutter's SHM record
        // path paints 8-bit ARGB32 regardless of the negotiated format, so a MemFd buffer
        // under a 10-bit format would carry mislabeled bytes.
        Some(build_dmabuf_buffers()?)
    } else if force_shm {
        // True SHM: exclude DmaBuf so Mutter MUST download (glReadPixels orders against render).
        Some(build_shm_only_buffers()?)
    } else {
        // CPU path still accepts mappable dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins the intersection).
        Some(build_mappable_buffers()?)
    };

    // Ask for cursor-as-metadata on every path (harmless if the producer can't supply it): the
    // pointer rides as SPA_META_Cursor rather than being burned into the frame, so the
    // compositor keeps its cheap hardware cursor plane (see `choose_cursor_mode`).
    let cursor_meta = build_cursor_meta_param()?;
    let mut byte_slices: Vec<&[u8]> = Vec::new();
    for pod in &format_pods {
        byte_slices.push(pod);
    }
    if let Some(b) = &buffers_values {
        byte_slices.push(b);
    }
    byte_slices.push(&cursor_meta);
    let mut params: Vec<&Pod> = byte_slices
        .iter()
        .map(|&b| Pod::from_bytes(b).context("pod from bytes"))
        .collect::<Result<_>>()?;

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw stream connect")?;

    // Blocks this thread, pumping frame callbacks until the capturer's `Drop` fires the quit
    // channel attached above (`_quit_attach` → `quit_loop.quit()`), at which point `run()`
    // returns and the thread unwinds — releasing the importer / CUDA context deterministically.
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{negotiation_plan, NegotiationInputs};

    /// A healthy NVENC session: zero-copy on, no latches, SDR 4:2:0, non-VAAPI backend.
    fn nvenc() -> NegotiationInputs {
        NegotiationInputs {
            zerocopy: true,
            force_shm: false,
            want_hdr: false,
            want_444: false,
            backend_is_vaapi: false,
            pyrowave_session: false,
            native_nv12_session: false,
            raw_dmabuf_import_disabled: false,
            gpu_import_disabled: false,
            gpu_dmabuf_negotiation_failed: false,
            native_nv12_env_on: true,
            hdr_cuda_ok: true,
        }
    }

    /// A gamescope-style VAAPI session that CAN take producer-native NV12.
    fn vaapi_native_nv12() -> NegotiationInputs {
        NegotiationInputs {
            backend_is_vaapi: true,
            native_nv12_session: true,
            ..nvenc()
        }
    }

    /// The four invariants that were prose-only comments in `pipewire_thread`'s prologue.
    #[test]
    fn negotiation_plan_invariants() {
        // 1. HDR builds the importer on the NVENC path — its pods are LINEAR-only, so the frames
        //    take the Vulkan-bridge/CUDA arm (byte-exact for 4 Bpp), never the 8-bit de-tile
        //    blit. (The tiled half of the invariant is enforced per frame in `.process`, which
        //    sees the negotiated modifier this plan cannot.)
        for want_444 in [false, true] {
            let p = negotiation_plan(NegotiationInputs {
                want_hdr: true,
                want_444,
                ..nvenc()
            });
            assert!(p.build_importer, "HDR on NVENC keeps zero-copy");
        }
        // …but never under a raw passthrough (VAAPI/PyroWave import the dmabuf themselves).
        assert!(
            !negotiation_plan(NegotiationInputs {
                want_hdr: true,
                ..vaapi_native_nv12()
            })
            .build_importer
        );
        // …and never when the resolved encoder can't take a packed 10-bit CUDA payload (libav
        // NVENC: its HDR route swscales into a P010 hardware frame). SDR is unaffected — the term
        // is HDR-only, so a libav host keeps its 8-bit zero-copy.
        assert!(
            !negotiation_plan(NegotiationInputs {
                want_hdr: true,
                hdr_cuda_ok: false,
                ..nvenc()
            })
            .build_importer,
            "HDR must stay on the CPU path where the encoder can't ingest 10-bit CUDA"
        );
        assert!(
            negotiation_plan(NegotiationInputs {
                hdr_cuda_ok: false,
                ..nvenc()
            })
            .build_importer,
            "the HDR-only guard must not touch an SDR session"
        );

        // 2. 4:4:4 never prefers producer NV12 (a 4:4:4 session must not be subsampled).
        let p = negotiation_plan(NegotiationInputs {
            want_444: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.prefer_native_nv12, "4:4:4 must not take NV12");
        // Nor does HDR (no 10-bit NV12 path).
        assert!(
            !negotiation_plan(NegotiationInputs {
                want_hdr: true,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12
        );

        // 3. Producer-native NV12 needs a `native_nv12_session` AND an active raw passthrough:
        //    libav VAAPI would misread the two-plane buffer, and the CUDA importer expects
        //    packed RGB.
        assert!(negotiation_plan(vaapi_native_nv12()).prefer_native_nv12);
        assert!(
            !negotiation_plan(NegotiationInputs {
                native_nv12_session: false,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12,
            "a session whose encoder can't ingest NV12 must never be offered it"
        );
        assert!(
            !negotiation_plan(NegotiationInputs {
                force_shm: true,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12,
            "no passthrough (force_shm) ⇒ no native NV12"
        );
        // A PyroWave session takes the passthrough but its CSC ingests packed RGB only.
        assert!(
            !negotiation_plan(NegotiationInputs {
                pyrowave_session: true,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12
        );

        // 4. The pyrowave-modifier extension (and the passthrough generally) is off whenever
        //    the raw-dmabuf latch has fired.
        let p = negotiation_plan(NegotiationInputs {
            raw_dmabuf_import_disabled: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.vaapi_passthrough, "latched ⇒ no raw passthrough");
        assert!(!p.prefer_native_nv12);
        assert!(p.raw_dmabuf_latched, "…and the operator gets told why");
    }

    /// The drift F1 was about: `spawn_pipewire` derived `vaapi_passthrough` by hand and its copy
    /// omitted the raw-dmabuf latch, so after that latch fired the capturer believed it had made
    /// the passthrough offer while the thread had already fallen back — and a timeout then
    /// latched a downgrade for an offer nobody made. With one resolver the two cannot disagree,
    /// so this pins the property that made the mirror wrong: the latch MUST move the decision.
    #[test]
    fn the_raw_dmabuf_latch_moves_the_passthrough_decision() {
        for pyrowave in [false, true] {
            let base = NegotiationInputs {
                backend_is_vaapi: !pyrowave,
                pyrowave_session: pyrowave,
                ..nvenc()
            };
            assert!(negotiation_plan(base).vaapi_passthrough);
            assert!(
                !negotiation_plan(NegotiationInputs {
                    raw_dmabuf_import_disabled: true,
                    ..base
                })
                .vaapi_passthrough
            );
        }
    }

    /// The EGL→CUDA offer's own negotiation-timeout latch gates `build_importer` — the twin of
    /// the raw passthrough's latch, so a compositor that accepts none of the importer's modifiers
    /// stops being asked (previously it re-ran the same 10 s timeout on every session, forever).
    /// The raw passthrough is untouched by it.
    #[test]
    fn gpu_dmabuf_negotiation_latch_gates_only_the_importer() {
        let p = negotiation_plan(NegotiationInputs {
            gpu_dmabuf_negotiation_failed: true,
            ..nvenc()
        });
        assert!(!p.build_importer, "latched offer must not be re-made");
        assert!(p.gpu_import_latched, "the downgrade must be diagnosable");
        let p = negotiation_plan(NegotiationInputs {
            gpu_dmabuf_negotiation_failed: true,
            ..vaapi_native_nv12()
        });
        assert!(
            p.vaapi_passthrough,
            "the raw passthrough has its own latch — this one must not touch it"
        );
        assert!(!p.gpu_import_latched, "no importer was ever wanted here");
    }

    /// A PyroWave session on an NVIDIA box takes the raw passthrough (the wavelet encoder's own
    /// Vulkan device imports dmabufs on any vendor) and therefore must NOT also build the
    /// EGL→CUDA importer — whose payloads only NVENC can consume.
    #[test]
    fn a_pyrowave_session_passes_through_without_a_cuda_importer() {
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert!(p.vaapi_passthrough);
        assert!(!p.build_importer);
    }

    /// `force_shm` is the race-free download path: no passthrough, and `want_dmabuf` stays false
    /// even with an importer and a full modifier list.
    #[test]
    fn force_shm_wins_over_every_dmabuf_path() {
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.vaapi_passthrough);
        assert!(!p.want_dmabuf(true, &[0, 1, 2]));
        // …and an SHM-forced NVENC session may still build the importer (it just won't be fed
        // dmabufs), which is why `want_dmabuf` — not `build_importer` — is the gate.
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..nvenc()
        });
        assert!(p.build_importer);
        assert!(!p.want_dmabuf(true, &[0]));
    }

    /// `want_dmabuf` needs a real modifier list: an importer that constructed but advertised
    /// nothing importable falls back to the CPU path.
    #[test]
    fn want_dmabuf_needs_both_a_consumer_and_a_modifier() {
        let p = negotiation_plan(nvenc());
        assert!(p.want_dmabuf(true, &[0]));
        assert!(!p.want_dmabuf(true, &[]), "no modifiers ⇒ CPU path");
        assert!(
            !p.want_dmabuf(false, &[0]),
            "importer failed to construct and no passthrough ⇒ CPU path"
        );
        // The passthrough needs no importer at all.
        let p = negotiation_plan(vaapi_native_nv12());
        assert!(p.want_dmabuf(false, &[0]));
    }

    /// The GPU-import death latch stops the importer being rebuilt, and says so.
    #[test]
    fn the_gpu_import_death_latch_skips_the_importer() {
        let p = negotiation_plan(NegotiationInputs {
            gpu_import_disabled: true,
            ..nvenc()
        });
        assert!(!p.build_importer);
        assert!(p.gpu_import_latched);
        // Reported for an HDR capture too — HDR takes the same importer (LINEAR/Vulkan-bridge
        // arm), so the latch really did cost it zero-copy.
        assert!(
            negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                want_hdr: true,
                ..nvenc()
            })
            .gpu_import_latched
        );
        // It is NOT reported for a capture that would never have built one anyway (a raw
        // passthrough) — that would misdirect the operator.
        assert!(
            !negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                ..vaapi_native_nv12()
            })
            .gpu_import_latched
        );
    }

    /// Zero-copy off ⇒ nothing at all: no importer, no passthrough, no NV12 preference.
    #[test]
    fn zerocopy_off_disables_every_branch() {
        for i in [
            NegotiationInputs {
                zerocopy: false,
                ..nvenc()
            },
            NegotiationInputs {
                zerocopy: false,
                ..vaapi_native_nv12()
            },
        ] {
            let p = negotiation_plan(i);
            assert!(!p.build_importer);
            assert!(!p.vaapi_passthrough);
            assert!(!p.prefer_native_nv12);
            assert!(!p.want_dmabuf(false, &[0]));
        }
    }

    // ---- PW2: capture-arm observability (pure halves) -------------------------------------
    //
    // Env-var reads race under a shared test process, so these assert against the pure functions
    // the logging sites call — the same rule `negotiation_plan_invariants` follows.

    use super::{
        consumer_kind, resolved_capture_arm, CaptureArm, ConsumerKind, PassthroughFallback,
        PassthroughFallbacks,
    };

    /// A PyroWave session is PyroWave even though it also flips `backend_is_vaapi` on (the
    /// `linux_zero_copy_is_vaapi` `Pyrowave` arm). Getting this precedence backwards is the exact
    /// shape of the bug PW2 fixes: the session gets reported as somebody else's backend.
    #[test]
    fn pyrowave_outranks_the_host_global_backend_pref() {
        assert_eq!(consumer_kind(true, true, true), ConsumerKind::PyroWave);
        // ...and on an NVIDIA/auto host, where `backend_is_vaapi` is false, it is still PyroWave —
        // the case that previously logged nothing at all.
        assert_eq!(consumer_kind(true, false, true), ConsumerKind::PyroWave);
    }

    /// The non-PyroWave consumers, and the one that must NOT warn on a CPU arm.
    #[test]
    fn consumer_kinds_and_which_ones_a_cpu_arm_degrades() {
        assert_eq!(consumer_kind(false, true, true), ConsumerKind::Vaapi);
        assert_eq!(consumer_kind(false, false, true), ConsumerKind::Nvenc);
        // No GPU backend ⇒ the software encoder, whose native input IS CPU frames.
        assert_eq!(consumer_kind(false, false, false), ConsumerKind::Software);
        assert!(ConsumerKind::PyroWave.cpu_is_downgrade());
        assert!(ConsumerKind::Vaapi.cpu_is_downgrade());
        assert!(ConsumerKind::Nvenc.cpu_is_downgrade());
        assert!(!ConsumerKind::Software.cpu_is_downgrade());
    }

    /// The arm is a function of the plan plus the two runtime facts. Pinned against every plan the
    /// resolver can produce, so the headline line can never claim an arm the session did not take.
    #[test]
    fn resolved_arm_matches_the_plan_that_produced_it() {
        // PyroWave/VAAPI raw passthrough.
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert!(p.vaapi_passthrough);
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[0])),
            CaptureArm::DmabufPassthrough
        );
        // NVENC via the EGL→CUDA importer.
        let p = negotiation_plan(nvenc());
        assert!(p.build_importer);
        assert_eq!(
            resolved_capture_arm(&p, true, p.want_dmabuf(true, &[0])),
            CaptureArm::CudaImport
        );
        // The importer was meant to be built but did not construct (no driver): CPU, not a
        // cuda-import the session never got.
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[0])),
            CaptureArm::Cpu
        );
        // An empty modifier list is a CPU arm even under a live passthrough plan.
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[])),
            CaptureArm::Cpu
        );
        // Forced SHM: CPU regardless of everything else.
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..nvenc()
        });
        assert_eq!(
            resolved_capture_arm(&p, true, p.want_dmabuf(true, &[0])),
            CaptureArm::Cpu
        );
    }

    /// The rate limiter: ONE line per distinct reason per session, counting every fall-through.
    /// `.process` runs per frame, so an off-by-one here is a log flood at the capture rate.
    #[test]
    fn fallback_log_budget_is_one_line_per_reason() {
        let mut f = PassthroughFallbacks::default();
        // First of a reason logs, and reports the running total (not a per-reason count).
        assert_eq!(f.note(PassthroughFallback::NotDmabuf), Some(1));
        // Repeats of the SAME reason never log again, but are still counted.
        for _ in 0..1_000 {
            assert_eq!(f.note(PassthroughFallback::NotDmabuf), None);
        }
        // A DIFFERENT reason is a different diagnosis and gets its own line, carrying the
        // now-large total — which is what distinguishes a persistent downgrade from a hiccup.
        assert_eq!(f.note(PassthroughFallback::DupFailed), Some(1002));
        assert_eq!(f.note(PassthroughFallback::DupFailed), None);
        // All four reasons fit the budget independently; the tally counts every frame.
        assert_eq!(f.note(PassthroughFallback::NoFormat), Some(1004));
        assert_eq!(f.note(PassthroughFallback::NoFourcc), Some(1005));
        // Budget spent: every reason has logged once, so nothing logs again however long the
        // session runs.
        for r in [
            PassthroughFallback::NoFormat,
            PassthroughFallback::NotDmabuf,
            PassthroughFallback::NoFourcc,
            PassthroughFallback::DupFailed,
        ] {
            assert_eq!(f.note(r), None);
        }
    }

    /// Every reason is distinguishable (a shared bit would silence one of them) and carries an
    /// actionable hint — a reason with no fix is a line the reader cannot use.
    #[test]
    fn every_fallback_reason_is_distinct_and_actionable() {
        let all = [
            PassthroughFallback::NoFormat,
            PassthroughFallback::NotDmabuf,
            PassthroughFallback::NoFourcc,
            PassthroughFallback::DupFailed,
        ];
        let mut f = PassthroughFallbacks::default();
        for r in all {
            assert!(
                f.note(r).is_some(),
                "{r:?} shares a bit with an earlier reason"
            );
            assert!(!r.as_str().is_empty());
            assert!(!r.hint().is_empty());
        }
        // Only `NoFormat` drops the frame; the other three downgrade it. The log line picks its
        // consequence clause off this, so an inverted answer would print the opposite of the truth.
        assert!(!PassthroughFallback::NoFormat.falls_back_to_cpu());
        assert!(PassthroughFallback::NotDmabuf.falls_back_to_cpu());
        assert!(PassthroughFallback::NoFourcc.falls_back_to_cpu());
        assert!(PassthroughFallback::DupFailed.falls_back_to_cpu());
    }
}
