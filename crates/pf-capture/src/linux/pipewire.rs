//! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).

use super::pw_cursor::{composite_cursor, update_cursor_meta, CursorState};
use super::pw_pods::{
    build_cursor_meta_param, build_default_format_obj, build_dmabuf_buffers, build_dmabuf_format,
    build_hdr_dmabuf_format, build_mappable_buffers, build_shm_only_buffers, serialize_pod,
    HDR_FORMAT_ORDER,
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

fn map_format(f: VideoFormat) -> Option<PixelFormat> {
    Some(match f {
        VideoFormat::BGRx => PixelFormat::Bgrx,
        VideoFormat::RGBx => PixelFormat::Rgbx,
        VideoFormat::BGRA => PixelFormat::Bgra,
        VideoFormat::RGBA => PixelFormat::Rgba,
        VideoFormat::RGB => PixelFormat::Rgb,
        VideoFormat::BGR => PixelFormat::Bgr,
        VideoFormat::NV12 => PixelFormat::Nv12,
        // Packed 2:10:10:10; only the `want_hdr` offer negotiates these (MANDATORY PQ/BT.2020).
        VideoFormat::xRGB_210LE => PixelFormat::X2Rgb10,
        VideoFormat::xBGR_210LE => PixelFormat::X2Bgr10,
        _ => return None,
    })
}

struct UserData {
    info: VideoInfoRaw,
    /// `None` until `param_changed`, or if the SPA format is unsupported.
    format: Option<PixelFormat>,
    /// DRM modifier for dmabuf import; 0 = LINEAR.
    modifier: u64,
    /// One-deep mailbox; write only through [`UserData::publish`].
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    signals: super::CaptureSignals,
    /// Consecutive tiled-import failures; reset on success. See [`IMPORT_FAIL_POISON`].
    import_fail_streak: u32,
    /// NVIDIA zero-copy: dmabuf → CUDA, usually via the isolated worker (`Importer::Remote`).
    importer: Option<pf_zerocopy::Importer>,
    /// Raw dmabuf to the encoder instead of a CUDA import (VAAPI).
    vaapi_passthrough: bool,
    /// `PUNKTFUNK_NV12`: tiled EGL/GL path converts to NV12 for native NVENC YUV. Off leaves BGRx.
    nv12: bool,
    /// Tiled EGL/GL path converts to planar YUV444. Wins over `nv12` — 4:4:4 must not subsample.
    yuv444: bool,
    /// LINEAR NV12 compute CSC failed once: RGB for the rest of this stream. Cleared by the next `Ud`.
    linear_nv12_failed: bool,
    /// Rate-limit counter for the latest-frame-only diagnostic (see `.process`).
    dbg_log_n: u64,
    /// Which clock feeds wire `pts_ns`. Delivery stamps sit downstream of compositor jitter.
    pts: crate::pts_provenance::PtsProvenance,
    pts_reported: std::time::Instant,
    /// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns. Re-sampled each 30 s window; clocks drift by µs.
    rt_minus_mono_ns: i64,
    /// `PUNKTFUNK_CAPTURE_HDR_PTS=0` puts the wire back on the delivery stamp unconditionally.
    hdr_pts_enabled: bool,
    /// Producer-fence wait, measured on this loop thread (a block here delays the next recycle).
    fence_wait: FenceWaitStats,
    /// Negotiated pool depth from `add_buffer`/`remove_buffer`. Budget for a deeper encode pipeline.
    pool: PoolCensus,
    /// Raw-passthrough frames that fell through to CPU, by reason. Fresh `UserData` per pipeline.
    passthrough_fallbacks: PassthroughFallbacks,
    cursor: CursorState,
    /// Sacrificial birth-mode size (kwin.rs `create`). `.process` skips until it matches, then clears.
    expect_dims: Option<(u32, u32)>,
    /// Buffers skipped by `expect_dims` (rate-limits its log).
    gate_skips: u64,
    /// When the gate first held a buffer. After [`GATE_DEADLINE`] it disarms: degraded dims beat a retry loop.
    gate_since: Option<std::time::Instant>,
    /// Encode reads the dmabuf after `.process` returns; do not rejoin the pool until [`BufferHold`] drops.
    defer: std::sync::Arc<DeferredRequeue>,
}

impl UserData {
    /// Latest-wins into [`super::FrameSlot`], then a wakeup edge.
    ///
    /// Must not block: this runs inside `.process` and would stall the compositor. A full
    /// wakeup channel already has a pending edge; the slot is the truth.
    fn publish(&self, frame: CapturedFrame) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(frame);
        }
        let _ = self.wake.try_send(());
    }

    /// Withhold this buffer from the producer until the returned hold drops.
    /// `None` (pool too shallow, or `PUNKTFUNK_ZEROCOPY_HOLD=0`) requeues at `.process` return —
    /// the producer may then rewrite the dmabuf while encode still reads it.
    fn try_defer(&mut self, pw_buf: *mut pw::sys::pw_buffer) -> Option<pf_frame::FrameHold> {
        if !zerocopy_hold_enabled() {
            return None;
        }
        let buf = pw_buf as usize;
        let pool_live = self.pool.live;
        let generation = self.defer.book.lock().ok()?.try_hold(buf, pool_live);
        let Some(generation) = generation else {
            if !self.defer.logged_shallow.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    pool_depth = pool_live,
                    reserve = HOLD_POOL_RESERVE,
                    "zero-copy: the producer's buffer pool cannot spare a buffer to hold across \
                     the encode — falling back to the immediate requeue, which the producer may \
                     rewrite mid-encode (torn/discolored frames under load); PUNKTFUNK_FORCE_SHM=1 \
                     trades CPU for a race-free capture if artifacts appear"
                );
            }
            return None;
        };
        if !self.defer.logged_active.swap(true, Ordering::Relaxed) {
            tracing::info!(
                pool_depth = pool_live,
                reserve = HOLD_POOL_RESERVE,
                "zero-copy: withholding each published buffer from the producer until the \
                 encoder releases it (deferred requeue — the producer can no longer rewrite a \
                 frame mid-encode); PUNKTFUNK_ZEROCOPY_HOLD=0 restores the immediate requeue"
            );
        }
        Some(std::sync::Arc::new(BufferHold {
            defer: self.defer.clone(),
            buf,
            generation,
        }))
    }
}

/// Facts the zero-copy decision needs, sampled at one instant so the decision is a pure
/// function — shared by the PipeWire thread and `spawn_pipewire` (see [`NegotiationPlan`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct NegotiationInputs {
    pub zerocopy: bool,
    /// `PUNKTFUNK_FORCE_SHM` — race-free download path.
    pub force_shm: bool,
    pub want_hdr: bool,
    pub want_444: bool,
    pub backend_is_vaapi: bool,
    pub pyrowave_session: bool,
    pub native_nv12_session: bool,
    /// Scoped raw-passthrough latch.
    pub raw_dmabuf_import_disabled: bool,
    /// Repeated import-worker deaths.
    pub gpu_import_disabled: bool,
    /// Previous EGL→CUDA dmabuf-only offer timed out (compositor accepts none of the modifiers).
    pub gpu_dmabuf_negotiation_failed: bool,
    pub native_nv12_env_on: bool,
    /// Encoder can ingest packed 10-bit PQ CUDA. Only direct-SDK NVENC can.
    pub hdr_cuda_ok: bool,
}

/// Zero-copy negotiation, resolved once and consumed by the PipeWire thread and `spawn_pipewire`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NegotiationPlan {
    pub build_importer: bool,
    pub vaapi_passthrough: bool,
    pub prefer_native_nv12: bool,
    /// Carried so [`want_dmabuf`](Self::want_dmabuf) needs no second copy.
    pub force_shm: bool,
    /// Would have taken raw passthrough, but its latch is set.
    pub raw_dmabuf_latched: bool,
    /// Would have built the EGL→CUDA importer, but a latch fired.
    pub gpu_import_latched: bool,
}

/// Resolve the negotiation plan. **Pure** — every environment read is already in `i`.
///
/// Invariants (pinned by `negotiation_plan_invariants`):
/// 1. HDR never takes the tiled EGL de-tile blit (8-bit `GL_RGBA8`). It may still build the
///    importer: HDR pods advertise LINEAR only ([`build_hdr_dmabuf_format`]), so the frame
///    takes the Vulkan-bridge arm. The per-frame gate in `.process` enforces the tiled half.
/// 2. 4:4:4 never prefers producer NV12 (must not subsample).
/// 3. Producer-native NV12 only on a `native_nv12_session` under active raw passthrough
///    (libav VAAPI misreads two-plane; the CUDA importer expects packed RGB).
/// 4. Raw passthrough is off once its latch has fired.
pub(super) fn negotiation_plan(i: NegotiationInputs) -> NegotiationPlan {
    // Consumer imports raw dmabufs: VAAPI (libva + GPU CSC) or PyroWave (its Vulkan device).
    let raw_passthrough = i.backend_is_vaapi || i.pyrowave_session;
    // Skip under raw passthrough (payloads only NVENC consumes) and both GPU latches
    // (worker-death crash-loop; compositor that rejects our modifiers would re-pay 10 s).
    // HDR is allowed: pods are LINEAR-only, so it never hits the 8-bit de-tile blit. Exclude
    // HDR when the encoder cannot take packed 10-bit CUDA (`PUNKTFUNK_NVENC_DIRECT=0` is garbage).
    let build_importer = i.zerocopy
        && !raw_passthrough
        && !i.gpu_import_disabled
        && !i.gpu_dmabuf_negotiation_failed
        && (!i.want_hdr || i.hdr_cuda_ok);
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
        // Every `build_importer` term except the two latches, then either latch.
        gpu_import_latched: i.zerocopy
            && !raw_passthrough
            && (!i.want_hdr || i.hdr_cuda_ok)
            && (i.gpu_import_disabled || i.gpu_dmabuf_negotiation_failed),
    }
}

impl NegotiationPlan {
    /// Request dmabufs only if the importer actually constructed and returned modifiers.
    pub(super) fn want_dmabuf(&self, have_importer: bool, modifiers: &[u64]) -> bool {
        (have_importer || self.vaapi_passthrough) && !modifiers.is_empty() && !self.force_shm
    }
}

/// Which capture arm a negotiated pipeline resolved to.
///
/// Product of a policy, a latch, whether the importer constructed, and the modifier list.
/// [`resolved_capture_arm`] plus the INFO line at pipeline build is the one place this is stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureArm {
    /// Raw dmabufs to the encoder (libva or PyroWave Vulkan). No host pixel touch.
    DmabufPassthrough,
    /// dmabufs imported to CUDA by the EGL→CUDA worker, for NVENC.
    CudaImport,
    /// CPU mmap de-pad. A downgrade when the consumer could have taken a dmabuf.
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

/// Resolve the arm this pipeline ended up on. **Pure.**
///
/// `have_importer` and `want_dmabuf` are the two runtime facts `negotiation_plan` cannot know.
pub(super) fn resolved_capture_arm(
    plan: &NegotiationPlan,
    have_importer: bool,
    want_dmabuf: bool,
) -> CaptureArm {
    if !want_dmabuf {
        CaptureArm::Cpu
    } else if plan.vaapi_passthrough {
        CaptureArm::DmabufPassthrough
    } else if have_importer {
        CaptureArm::CudaImport
    } else {
        // `want_dmabuf` requires `have_importer || vaapi_passthrough`. Fallback so a logging
        // helper can never panic the capture thread.
        CaptureArm::Cpu
    }
}

/// Who consumes captured frames — whether a CPU arm is a downgrade, and what to call it.
///
/// From the resolved [`ZeroCopyPolicy`](crate::ZeroCopyPolicy), not the encoder pref:
/// `pyrowave_session` is per-session, so a PyroWave session on an NVENC host is PyroWave here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConsumerKind {
    /// Wavelet encoder's Vulkan device imports dmabufs on any vendor; CPU costs the passthrough.
    PyroWave,
    /// libva imports the dmabuf and CSCs on the GPU.
    Vaapi,
    Nvenc,
    /// Software encoder — CPU frames are native input, so a CPU arm is not a downgrade.
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

    /// True for every GPU consumer; false for software, which wants CPU frames.
    pub(super) fn cpu_is_downgrade(self) -> bool {
        !matches!(self, ConsumerKind::Software)
    }
}

/// Classify the frames' consumer. **Pure.** `pyrowave_session` wins over `backend_is_vaapi`
/// because it is per-session and the pref is host-global (a PyroWave session also sets
/// `backend_is_vaapi` via `linux_zero_copy_is_vaapi`'s `Pyrowave` arm).
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

/// Why a raw-dmabuf passthrough frame fell through to the CPU de-pad path.
///
/// Each variant is a different diagnosis. Without this, the `if` fell out silently and a
/// zero-copy session could pay CPU on every frame while logging a healthy open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassthroughFallback {
    /// No format yet — transient around renegotiation.
    NoFormat,
    /// Producer delivered SHM/MemFd, not a dmabuf.
    NotDmabuf,
    /// Negotiated format has no DRM fourcc, so the encoder cannot describe it.
    NoFourcc,
    /// `F_DUPFD_CLOEXEC` failed (fd-limit, not graphics).
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

    /// Three reasons downgrade to CPU; `NoFormat` drops the frame (CPU path needs `ud.format` too).
    pub(super) fn falls_back_to_cpu(self) -> bool {
        !matches!(self, PassthroughFallback::NoFormat)
    }

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

/// Upper bounds (µs) of the fence-wait histogram; last bucket is overflow.
///
/// ≤100 µs is noise; >1 ms is a stall on a 60 Hz 16.6 ms budget. Coarse on purpose: the
/// question is whether the tail is ~0 or milliseconds.
const FENCE_WAIT_BUCKETS_US: [u64; 6] = [100, 500, 1_000, 2_000, 5_000, 10_000];

/// Producer implicit-fence wait, measured on the PipeWire loop thread.
///
/// That thread is the compositor's consumer, so a block here delays recycling for the next
/// frame. A `NoFence` majority means the wait is structurally free, not merely short.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct FenceWaitStats {
    samples: u64,
    total_us: u64,
    max_us: u64,
    /// Overflow bucket: one past the last bound.
    buckets: [u64; FENCE_WAIT_BUCKETS_US.len() + 1],
    signaled: u64,
    no_fence: u64,
    timed_out: u64,
    failed: u64,
}

impl FenceWaitStats {
    pub(super) fn record(&mut self, us: u64) {
        self.samples += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        let idx = FENCE_WAIT_BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(FENCE_WAIT_BUCKETS_US.len());
        self.buckets[idx] += 1;
    }

    /// Bucket upper bound for the `q`-quantile, µs; inner `None` is overflow.
    /// Counts up to the quantile rather than interpolating.
    pub(super) fn quantile_bucket_us(&self, q: f64) -> Option<Option<u64>> {
        if self.samples == 0 {
            return None;
        }
        // 0-based index of the sample at `q`; q=1.0 is the last sample.
        let target = ((self.samples as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                return Some(FENCE_WAIT_BUCKETS_US.get(i).copied());
            }
        }
        Some(None)
    }

    pub(super) fn mean_us(&self) -> u64 {
        self.total_us.checked_div(self.samples).unwrap_or(0)
    }

    /// 100 frames ≈ 1.7 s at 60 fps — past the point one outlier dominates p99.
    pub(super) fn is_meaningful(&self) -> bool {
        self.samples >= 100
    }
}

/// How many buffers the producer allocated for this stream.
///
/// `live` comes from `add_buffer`/`remove_buffer` on the loop thread. There is no "pool
/// complete" event, so the count is published from `.process` (first dequeue ⇒ allocation
/// finished). Depth is the budget [`HoldBook::try_hold`] spends; a pool of ≤ [`HOLD_POOL_RESERVE`]
/// cannot defer and the producer may rewrite a buffer mid-encode.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PoolCensus {
    live: u32,
    /// Deepest `live` this session. A depth decision must not follow a renegotiation down to zero.
    high_water: u32,
    /// Last logged `live`, so a stable pool logs once per distinct depth.
    logged: Option<u32>,
}

impl PoolCensus {
    fn add(&mut self) {
        self.live += 1;
        self.high_water = self.high_water.max(self.live);
    }

    fn remove(&mut self) {
        self.live = self.live.saturating_sub(1);
    }

    /// `Some(live)` the first time each distinct depth is seen.
    fn note_frame(&mut self) -> Option<u32> {
        (self.logged != Some(self.live)).then(|| {
            self.logged = Some(self.live);
            self.live
        })
    }
}

/// Per-session tally of raw-passthrough fall-throughs, one log line per distinct reason.
///
/// `.process` runs per frame; per-reason so a transient `NoFormat` at open does not spend
/// the budget a persistent `NotDmabuf` needs.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PassthroughFallbacks {
    frames: u64,
    logged: u8,
}

impl PassthroughFallbacks {
    /// `Some(frames_so_far)` the first time this reason is seen this session.
    pub(super) fn note(&mut self, reason: PassthroughFallback) -> Option<u64> {
        self.frames += 1;
        let bit = reason.bit();
        (self.logged & bit == 0).then(|| {
            self.logged |= bit;
            self.frames
        })
    }
}

/// Tiled-import failures (worker alive) before the stream is poisoned for rebuild.
/// Never fall through to CPU mmap: de-padding tiled bytes as linear is a scrambled image.
const IMPORT_FAIL_POISON: u32 = 3;

/// Buffers left in the producer's pool: one it is rendering, one in transit.
/// Withholding past that skips frames when holds peak (host frame + up to two encoder slots).
const HOLD_POOL_RESERVE: u32 = 2;

/// `PUNKTFUNK_ZEROCOPY_HOLD=0` restores immediate requeue (racy). Use `env_on`; a bare
/// `== "0"` is the trap `PUNKTFUNK_FORCE_SHM` already hit.
fn zerocopy_hold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_ZEROCOPY_HOLD").unwrap_or(true))
}

/// Which buffers are withheld, each under a per-hold generation.
///
/// A pointer-value reuse across pool renegotiation must not satisfy a stale hold (`complete`).
/// Insert/remove run only on the loop thread; a dropping [`BufferHold`] only sends. Between
/// hold creation and the loop servicing release, `contains` is stable — what `.process` uses
/// to choose "requeue now" vs "the hold owns the requeue".
#[derive(Default)]
struct HoldBook {
    /// `*mut pw_buffer` as usize → generation that owns it.
    out: std::collections::HashMap<usize, u64>,
    /// Last issued hold generation (monotonic per stream).
    last_gen: u64,
}

impl HoldBook {
    /// Withhold `buf` if the pool can spare it (`pool_live - HOLD_POOL_RESERVE` out at once).
    fn try_hold(&mut self, buf: usize, pool_live: u32) -> Option<u64> {
        let cap = pool_live.saturating_sub(HOLD_POOL_RESERVE) as usize;
        if self.out.len() >= cap || self.out.contains_key(&buf) {
            return None;
        }
        self.last_gen += 1;
        self.out.insert(buf, self.last_gen);
        Some(self.last_gen)
    }

    /// Release `buf` iff this generation still owns it. `true` ⇒ caller must requeue;
    /// `false` ⇒ purged (pool renegotiated — pointer may be a new buffer) so do not touch it.
    fn complete(&mut self, buf: usize, generation: u64) -> bool {
        match self.out.get(&buf) {
            Some(&g) if g == generation => {
                self.out.remove(&buf);
                true
            }
            _ => false,
        }
    }

    /// `remove_buffer`: the buffer is being freed under us. Later release of its hold is a no-op.
    fn purge(&mut self, buf: usize) {
        self.out.remove(&buf);
    }

    fn contains(&self, buf: usize) -> bool {
        self.out.contains_key(&buf)
    }
}

/// Shared by the loop thread ([`HoldBook`] ops) and [`BufferHold`] guards on the encode thread.
struct DeferredRequeue {
    book: std::sync::Mutex<HoldBook>,
    /// Wakes the loop to requeue `(buffer, generation)`. Send failure = the loop is gone.
    tx: pw::channel::Sender<(usize, u64)>,
    logged_active: std::sync::atomic::AtomicBool,
    logged_shallow: std::sync::atomic::AtomicBool,
}

/// Releases its buffer to the producer when the last clone drops. Send-only from the dropping
/// thread; `pw_stream_queue_buffer` runs in the requeue channel's loop-thread callback.
struct BufferHold {
    defer: std::sync::Arc<DeferredRequeue>,
    buf: usize,
    generation: u64,
}

impl Drop for BufferHold {
    fn drop(&mut self) {
        let _ = self.defer.tx.send((self.buf, self.generation));
    }
}

/// Log a frame-drop reason once per process (`.process` runs per frame).
fn warn_once(msg: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&msg) {
        seen.push(msg);
        tracing::warn!("{msg}");
    }
}

/// Read-only mmap of a dmabuf fd, unmapped on drop. Used when MAP_BUFFERS left the buffer unmapped
/// (producers do not always flag dmabufs mappable — gamescope Vulkan exports).
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

fn supported_data_plane_count(count: u32) -> Option<usize> {
    (1..=2).contains(&count).then_some(count as usize)
}

/// How often the wire-pts provenance line is emitted. Matches the audio plane's stats cadence.
const PTS_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns.
///
/// PipeWire stamps `spa_meta_header.pts` in `CLOCK_MONOTONIC`; the wire speaks realtime-since-epoch.
/// A failed read reports 0, which puts every rebased stamp outside the 50 ms plausibility window
/// and falls the stream back to delivery stamps — the safe direction.
fn realtime_minus_monotonic_ns() -> i64 {
    let rt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer and touches nothing else;
    // `ts` is a live, properly aligned local. A non-zero return leaves it untouched.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    rt - (ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
}

fn packed_frame_geometry(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    reported_stride: usize,
) -> Option<(usize, usize, usize, usize)> {
    if height == 0 {
        return None;
    }
    let row = width.checked_mul(bytes_per_pixel)?;
    let stride = if reported_stride == 0 {
        row
    } else {
        reported_stride
    };
    if stride < row {
        return None;
    }
    let needed = stride.checked_mul(height - 1)?.checked_add(row)?;
    let tight = row.checked_mul(height)?;
    Some((row, stride, needed, tight))
}

/// De-pad / import one PipeWire buffer and push it to the encoder.
///
/// Called from `.process` with the newest drained buffer. `datas` uses the same transparent
/// cast as libspa's `Buffer::datas_mut`, so the safe `Data` accessors keep working. `pw_buf`
/// is identity for [`UserData::try_defer`] only — never dereferenced here.
fn consume_frame(
    ud: &mut UserData,
    spa_buf: *mut spa::sys::spa_buffer,
    pw_buf: *mut pw::sys::pw_buffer,
    hdr_pts_ns: Option<i64>,
) {
    // Inactive: skip the de-pad (expensive at 5K).
    if !ud.signals.active.load(Ordering::Relaxed) {
        return;
    }
    // GPU import lost: skip per-frame work until the rebuild tears this stream down.
    if ud.signals.broken.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: the dequeued buffer stays held for this callback. We reject counts outside the
    // one/two-plane formats this function supports before using PipeWire's array pointer.
    let datas: &mut [pw::spa::buffer::Data] = unsafe {
        if spa_buf.is_null() || (*spa_buf).datas.is_null() {
            &mut []
        } else if let Some(len) = supported_data_plane_count((*spa_buf).n_datas) {
            std::slice::from_raw_parts_mut((*spa_buf).datas as *mut pw::spa::buffer::Data, len)
        } else {
            &mut []
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

    // One stamp for every publish path, taken before de-pad/import. Sampling at publish put
    // CPU work inside the timestamp and let the three paths drift apart.
    let delivery_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let hdr_pts_ns = hdr_pts_ns.filter(|&p| p > 0);
    // Observe even if we ship the delivery stamp: which clock is cleaner on this host is the
    // question; a diagnostic that runs only after you trust the answer cannot inform it.
    ud.pts.observe(hdr_pts_ns, delivery_ns);
    let stamp = crate::pts_provenance::wire_pts(
        ud.hdr_pts_enabled.then_some(hdr_pts_ns).flatten(),
        delivery_ns,
        ud.rt_minus_mono_ns,
    );
    let pts_ns = stamp.pts_ns;
    if ud.hdr_pts_enabled && hdr_pts_ns.is_some() && !stamp.from_header {
        ud.pts.implausible += 1;
    }
    if ud.pts_reported.elapsed() >= PTS_REPORT_EVERY {
        if let Some(r) = ud.pts.report() {
            tracing::info!(
                frames = r.frames,
                with_hdr = r.with_hdr,
                samples = r.samples,
                period_us = r.period_us,
                // Tighter hdr_mad than delivery_mad ⇒ compositor stamp is worth shipping;
                // equally ragged ⇒ the producer composes irregularly and no stamp fixes it.
                hdr_mad_us = r.hdr_mad_us,
                delivery_mad_us = r.delivery_mad_us,
                offset_p50_ms = r.offset_p50_ms,
                implausible = r.implausible,
                hdr_pts_used = ud.hdr_pts_enabled,
                "capture wire-pts provenance"
            );
        }
        ud.pts.reset_window();
        ud.pts_reported = std::time::Instant::now();
        // Clocks drift by µs over a window; re-pair here so a multi-hour session stays honest.
        ud.rt_minus_mono_ns = realtime_minus_monotonic_ns();
    }

    // Mutter hands the dmabuf at GPU-submit; without producer explicit sync (Mutter+NVIDIA)
    // wait the implicit fence before sampling, or the CPU/GPU path reads a stale frame.
    // No-op when the driver attaches no fence. 100 ms is a guard for a producer that does fence.
    if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
        let t0 = std::time::Instant::now();
        let waited = pf_zerocopy::dmabuf_fence::wait_read_ready(datas[0].fd(), 100);
        ud.fence_wait.record(t0.elapsed().as_micros() as u64);
        match waited {
            Ok(outcome) => {
                use pf_zerocopy::dmabuf_fence::WaitOutcome;
                match outcome {
                    WaitOutcome::Signaled => ud.fence_wait.signaled += 1,
                    WaitOutcome::NoFence => ud.fence_wait.no_fence += 1,
                    WaitOutcome::TimedOut => ud.fence_wait.timed_out += 1,
                }
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
                ud.fence_wait.failed += 1;
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
        // One line per ~5 s at 60 fps, under PUNKTFUNK_PERF only — same gate as encode submit splits.
        if pf_host_config::config().perf
            && ud.fence_wait.is_meaningful()
            && ud.fence_wait.samples % 300 == 0
        {
            let q = |p: f64| match ud.fence_wait.quantile_bucket_us(p) {
                Some(Some(us)) => format!("<={us}us"),
                Some(None) => format!(
                    ">{}us",
                    FENCE_WAIT_BUCKETS_US[FENCE_WAIT_BUCKETS_US.len() - 1]
                ),
                None => "n/a".to_string(),
            };
            tracing::info!(
                samples = ud.fence_wait.samples,
                mean_us = ud.fence_wait.mean_us(),
                max_us = ud.fence_wait.max_us,
                p50 = %q(0.50),
                p99 = %q(0.99),
                signaled = ud.fence_wait.signaled,
                no_fence = ud.fence_wait.no_fence,
                timed_out = ud.fence_wait.timed_out,
                failed = ud.fence_wait.failed,
                "dmabuf implicit-fence wait on the PipeWire loop thread (PW4: a p99 in the first \
                 bucket means this wait is already free and moving it off-thread buys nothing)"
            );
        }
    }

    // Raw DMA-BUF passthrough: packed RGB for GPU CSC, or producer NV12 without another convert.
    // Publishes and returns, or breaks with a named reason. Silent fall-through CPU-touches
    // every frame on a session that had negotiated zero-copy.
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
            // NV12 is usually two SPA planes on one BO; plane 1's chunk has the real UV
            // offset/stride. BO identity is inode, not fd number. A two-BO frame cannot
            // travel the single-fd import — drop it rather than stream garbage chroma.
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
                    // Dropped, not downgraded: de-padding as linear would scramble chroma.
                    return;
                }
                let c1 = datas[1].chunk();
                Some((c1.offset(), c1.stride().max(0) as u32))
            } else {
                None
            };
            // Dup so the fd outlives SPA recycle. Content stability is `try_defer`; without a
            // hold (shallow pool / PUNKTFUNK_ZEROCOPY_HOLD=0) the pool depth must outrun encode.

            // SAFETY: `datas[0].fd()` is the dmabuf fd owned by the live PipeWire buffer (valid
            // for this callback). `fcntl(fd, F_DUPFD_CLOEXEC, 0)` reads only the integer fd,
            // touches no Rust memory, and returns a fresh independent CLOEXEC duplicate (or -1).
            // The original stays owned by PipeWire; the dup is a new fd we own (checked >= 0).
            let dup = unsafe { libc::fcntl(datas[0].fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                break 'passthrough PassthroughFallback::DupFailed;
            }
            let hold = ud.try_defer(pw_buf);
            ud.publish(CapturedFrame {
                provenance: Default::default(),
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
                    hold,
                }),
                // RGB→NV12 backends blend cursor-as-metadata. Gamescope burns the pointer in;
                // native NV12 has none.
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
        // Once per distinct reason (`.process` is per-frame). The running count separates a
        // persistent downgrade from a one-frame hiccup at renegotiation.
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

    // dmabuf + importer → CUDA, no CPU touch. Else fall through to the shm de-pad copy.
    let mut gpu_import_broken = false;
    if let (Some(importer), Some(fmt)) = (ud.importer.as_mut(), ud.format) {
        // 10-bit PQ may take LINEAR (Vulkan-bridge, 4 Bpp verbatim) but never the tiled EGL
        // de-tile blit (`GL_RGBA8`, silent depth loss). A tiled modifier here means the
        // producer ignored the LINEAR-only HDR offer — drop to CPU rather than trust it.
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
            // Tiled → EGL/GL de-tile; LINEAR (gamescope) → CUDA external-memory (NVIDIA EGL
            // cannot sample LINEAR).
            let modifier = (ud.modifier != 0).then_some(ud.modifier);
            if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                // 4:4:4 planar YUV444 on tiled EGL (never subsample; wins over NV12). Else
                // `PUNKTFUNK_NV12` → NV12 (tiled blit or LINEAR compute CSC). HDR stays packed
                // RGB: both CSCs write 8-bit planes, and NVENC ingests ARGB10/ABGR10 natively.
                // LINEAR NV12 failure latches RGB for the stream (no drop).
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
                        ud.publish(CapturedFrame {
                            provenance: Default::default(),
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
                            // CUDA encoder blends cursor-as-metadata into its owned device surface.
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
                            // Tiled: CPU mmap would de-pad tiled bytes as linear — scrambled.
                            // Drop the frame; on a dead worker or a short streak, poison so
                            // capture-loss rebuild renegotiates.
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
                        // LINEAR dmabuf is CPU-mappable: disable the importer and fall through.
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
    // LINEAR dmabufs also land here (gamescope). Capture the fd before `data()` borrows `d`.
    let data_type = d.type_();
    let raw_fd = d.fd();
    // `mapoffset` is this spa_data's start in the fd — non-zero when one fd is pooled.
    // PipeWire's MAP_BUFFERS slice already starts there; our self-mmap maps from 0, so
    // add it (`region_off`). Skip it and we index the wrong buffer; `needed > avail` cannot
    // catch that because `avail` is the whole-fd mapping.
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
    // This de-pad assumes one packed plane. `bytes_per_pixel` reports 4 for NV12, so a
    // native NV12 buffer (`stride ≈ w`, two planes) always trips `stride < row` and blames
    // the producer. The second plane is not in `datas[0]`; arriving here is a host bug
    // (NV12 offer without the raw-dmabuf passthrough that consumes it).
    if matches!(fmt, PixelFormat::Nv12) {
        warn_once(
            "negotiated producer-native NV12 but this capture fell back to the CPU de-pad path, \
             which handles single-plane packed formats only — frames dropped (the NV12 offer is \
             only valid under the raw-dmabuf passthrough that imports it directly)",
        );
        return;
    }
    let bpp = fmt.bytes_per_pixel();
    let Some((row, stride, needed, tight_len)) = packed_frame_geometry(w, h, bpp, stride) else {
        warn_once("invalid or overflowing packed-frame geometry — frames dropped");
        return;
    };
    // dmabuf chunks commonly report size 0; fall back to the computed span.
    let size = if size == 0 { needed } else { size };
    // mmap the fd ourselves at fstat length. xdg-desktop-portal-wlr MemFd reports
    // `data.maxsize` past the mapped bytes — reading to maxsize segfaults. Also covers
    // MAP_BUFFERS skipping Vulkan dmabufs. MemPtr (no fd) is same-process: trust `d.data()`.
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
    // Prefer our fstat-sized mmap; else PipeWire's MAP_BUFFERS slice. `fd_len` is required:
    // falling back to `offset + needed` maps a producer-invented length and can SIGBUS past
    // the object. Without a real length, decline to self-map.
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
    // Self-mmap starts at fd offset 0, so this spa_data begins at `mapoffset`; MAP_BUFFERS
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
        // First-frame geometry — a compositor/GPU layout mismatch is otherwise silent here.
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
    let mut tight = vec![0u8; tight_len];
    for y in 0..h {
        tight[y * row..y * row + row].copy_from_slice(&region[y * stride..y * stride + row]);
    }
    // Blit the latched pointer (no-op when hidden or not packed RGB). The producer's hardware
    // cursor plane stays out of the captured buffer.
    composite_cursor(&mut tight, w, h, fmt, &ud.cursor);
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: w as u32,
        height: h as u32,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(tight),
        // Already composited into `tight` — nothing for the encoder to blend.
        cursor: None,
    };
    ud.publish(frame);
}

#[allow(clippy::too_many_arguments)]
pub fn pipewire_thread(
    fd: Option<OwnedFd>,
    node_id: u32,
    // One-deep mailbox: publish overwrites, so a stalled consumer loses intermediates, never the latest.
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    signals: super::CaptureSignals,
    // Zero-copy decision, resolved once by `spawn_pipewire` — never re-derived here.
    plan: NegotiationPlan,
    // `want_444`/`want_hdr` pick the pod family; `expect_exact_dims` arms the birth-mode gate.
    opts: super::CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    quit_rx: pw::channel::Receiver<()>,
    // Encode-backend facts from the facade — never re-derived here.
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
    // Capturer `Drop` lands here on the loop thread and stops `run()` so the thread unwinds
    // instead of blocking to process exit. Hold the attachment for the loop's life.
    let quit_loop = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), move |()| {
        tracing::debug!("pipewire: quit signal received — stopping capture loop");
        quit_loop.quit();
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    // Portal source: fd to a sandboxed PipeWire remote. KWin virtual-output: no fd, default daemon.
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?,
        None => context
            .connect_rc(None)
            .context("pw connect (default daemon)")?,
    };

    let backend_is_vaapi = policy.backend_is_vaapi;
    let force_shm = plan.force_shm;
    let vaapi_passthrough = plan.vaapi_passthrough;
    let prefer_native_nv12 = plan.prefer_native_nv12;
    // Isolated worker (design/zerocopy-worker-isolation.md): a driver fault kills the worker,
    // not this host. Construction failure → CPU path (no dmabuf request). `plan.build_importer`
    // already encodes when to try.
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
    // Per-fourcc: EGL/libva answer per format; XR24 (BGRx) and AR24 (BGRA) are asked
    // separately. Append LINEAR (0): NVIDIA EGL will not list it, but gamescope LINEAR
    // dmabufs import via CUDA external memory. VAAPI passthrough has no importer, so
    // LINEAR is all we advertise — radeonsi/iHD import it and any compositor can allocate it.
    let mut modifiers = Vec::new();
    let mut modifiers_bgra = Vec::new();
    if let Some(i) = importer.as_mut() {
        modifiers = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgrx).unwrap());
        modifiers_bgra = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgra).unwrap());
    }
    // PyroWave imports through Vulkan, not libva. Extra modifiers come from the facade
    // (`ZeroCopyPolicy::pyrowave_modifiers`) so capture never calls `encode`. Empty unless
    // the `pyrowave` feature is on and this session (or the global pref) is PyroWave.
    let extend_pyrowave = vaapi_passthrough && !policy.pyrowave_modifiers.is_empty();
    for list in [&mut modifiers, &mut modifiers_bgra] {
        if (importer.is_some() || vaapi_passthrough) && !list.contains(&0) {
            list.push(0); // DRM_FORMAT_MOD_LINEAR
        }
        if extend_pyrowave {
            for &m in &policy.pyrowave_modifiers {
                if !list.contains(&m) {
                    list.push(m);
                }
            }
        }
    }
    if extend_pyrowave {
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    let want_dmabuf = plan.want_dmabuf(importer.is_some(), &modifiers);
    // Latch must fire only for an offer actually made — `plan.build_importer` cannot know
    // the importer constructed.
    signals.gpu_dmabuf_offer.store(
        want_dmabuf && !vaapi_passthrough && !want_hdr,
        Ordering::Relaxed,
    );
    // One line for the resolved arm and its consumer. Detail lines below explain an arm;
    // they do not state which one this session took.
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
        // Latch state belongs on the same line as the arm: `cpu` is either "never dmabuf"
        // or "a prior failure we are still living with" — only the second is a bug.
        raw_dmabuf_latch = pf_zerocopy::raw_dmabuf_latch_state(),
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
        // Covers PyroWave: its extra Vulkan modifiers were appended above. Do not gate this
        // on `pyrowave_modifiers.is_empty()` — that dropped a zero-copy PyroWave session to CPU.
        tracing::info!(
            native_nv12_preferred = prefer_native_nv12,
            modifier_count = modifiers.len(),
            pyrowave_extended = !policy.pyrowave_modifiers.is_empty(),
            "zero-copy: advertising DMA-BUF modifiers for direct encoder import (LINEAR \
             always; native NV12 first when enabled, packed RGB fallback)"
        );
    } else if want_dmabuf {
        tracing::info!(
            bgrx_count = modifiers.len(),
            bgra_count = modifiers_bgra.len(),
            // Sample is truncated to 6, LINEAR pushed last — reading the sample as the whole
            // list makes a good offer look tiled-only.
            linear_offered = modifiers.contains(&0),
            sample = ?&modifiers[..modifiers.len().min(6)],
            "zero-copy: advertising EGL-importable dmabuf modifiers (BGRx + BGRA pods)"
        );
    } else if consumer.cpu_is_downgrade() {
        // No dmabuf advertised: this is the CPU path. `raw_dmabuf_latched` already caught a
        // latched downgrade. Warn for every GPU consumer; software wants CPU frames.
        // `consumer_kind` is per-session so a PyroWave session on an NVIDIA host still warns
        // (the host-global encoder pref would have called it NVENC and logged nothing).
        tracing::warn!(
            consumer = consumer.as_str(),
            "{} encode with the CPU capture path (per-frame de-pad + CSC + upload) — \
             zero-copy is off for this capture ({}); set PUNKTFUNK_ZEROCOPY=1 to restore the \
             dmabuf default",
            consumer.as_str(),
            if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                "PUNKTFUNK_ZEROCOPY is set falsy"
            } else if want_hdr && !policy.hdr_cuda_ok {
                // `build_importer` drops HDR when the encoder cannot take packed 10-bit CUDA
                // (libav swscales into P010). Naming the output format would send the reader
                // to the wrong knob.
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

    // Holds on published frames release through this channel from whichever thread drops last;
    // the receiver (attached after the stream exists) is the only place a withheld buffer rejoins.
    let (requeue_tx, requeue_rx) = pw::channel::channel::<(usize, u64)>();
    let defer = std::sync::Arc::new(DeferredRequeue {
        book: std::sync::Mutex::new(HoldBook::default()),
        tx: requeue_tx,
        logged_active: std::sync::atomic::AtomicBool::new(false),
        logged_shallow: std::sync::atomic::AtomicBool::new(false),
    });

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
        pts: crate::pts_provenance::PtsProvenance::new(),
        pts_reported: std::time::Instant::now(),
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
        hdr_pts_enabled: std::env::var("PUNKTFUNK_CAPTURE_HDR_PTS").as_deref() != Ok("0"),
        fence_wait: FenceWaitStats::default(),
        pool: PoolCensus::default(),
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(cursor_id0_hides),
        expect_dims: if expect_exact_dims {
            preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
        defer: defer.clone(),
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "punktfunk-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE     => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE     => "Screen",
            // Do not let the session manager re-target this stream: an orphaned auto-link to
            // a fresh Video/Source wedges that node and head-blocks the daemon work queue,
            // stalling all new link negotiation system-wide.
            "node.dont-reconnect"     => "true",
        },
    )
    .context("pw Stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_stream, ud, old, new| {
            tracing::info!(?old, ?new, "pipewire stream state");
            // `Streaming` with no buffers is a static desktop. Anything else means the source
            // went away; `try_latest` turns a sustained non-Streaming state into capture-loss
            // so the encode loop rebuilds instead of freezing on the last frame.
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
            // Parse once (`parse` takes `&mut self`) and report failure. On `Err`, `negotiated`
            // stays false so the timeout looks like "no accepted format" — a malformed pod we
            // accepted, not a format mismatch.
            let parsed = ud.info.parse(param);
            if let Err(e) = &parsed {
                tracing::error!(
                    error = %e,
                    "pipewire: failed to parse the negotiated Format pod — capture will time out \
                     with no usable format"
                );
            }
            if parsed.is_ok() {
                ud.signals.negotiated.store(true, Ordering::Relaxed);
                // Renegotiation replaces the pool: cached per-buffer imports key on buffers
                // that no longer exist, and a recycled fd/inode must not resolve to a stale import.
                if let Some(imp) = ud.importer.as_mut() {
                    imp.clear_cache();
                }
                let sz = ud.info.size();
                // Gamescope cursor source scales root→frame (`xfixes_cursor::scale_to_frame`).
                ud.signals.frame_size.store(
                    (u64::from(sz.width) << 32) | u64::from(sz.height),
                    Ordering::Relaxed,
                );
                ud.format = map_format(ud.info.format());
                ud.modifier = ud.info.modifier();
                // 10-bit PQ is only offered with MANDATORY BT.2020/PQ, so a 10-bit negotiation
                // is HDR — still log the producer's fixated transfer/primaries.
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
        // Pool census. `remove_buffer` also purges the deferred-requeue book: the buffer is
        // being freed under any hold, so that hold's later release must be a no-op (generation
        // in `HoldBook::complete` also covers the address being reused by a new pool).
        .add_buffer(|_stream, ud, _buf| ud.pool.add())
        .remove_buffer(|_stream, ud, buf| {
            ud.pool.remove();
            if let Ok(mut book) = ud.defer.book.lock() {
                book.purge(buf as usize);
            }
        })
        .process(|stream, ud| {
            // Latest-frame-only: Mutter bursts, older queued buffers are stale. Drain, requeue
            // older, keep newest. Dequeue/requeue stay outside `catch_unwind` — a panic inside
            // would strand `newest` and shrink the fixed pool until capture wedged.

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
            // Producer's actual pool depth, once per distinct value. `build_dmabuf_buffers`
            // asks for a range; the producer picks. Depth is the deferred-requeue budget:
            // ≤ HOLD_POOL_RESERVE cannot defer, and a requeued buffer may be rewritten mid-encode.
            if let Some(depth) = ud.pool.note_frame() {
                tracing::info!(
                    pool_depth = depth,
                    high_water = ud.pool.high_water,
                    drained,
                    "pipewire buffer pool negotiated — the producer's ACTUAL count \
                     (add_buffer/remove_buffer): the deferred-requeue budget, and the rewrite \
                     window for any frame published without a hold"
                );
            }
            // Sacrificial birth mode (kwin.rs `create`): frame and cursor meta are in the doomed
            // size until renegotiation. Self-disarms on match, or after `GATE_DEADLINE` — degraded
            // dims beat a first-frame-timeout retry loop if the promised renegotiation never comes.
            if let Some((ew, eh)) = ud.expect_dims {
                /// Renegotiation normally lands within a frame or two; past this, stop starving
                /// the pipeline (the real mode never applied).
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
            // PipeWire dispatches from a C trampoline with no catch_unwind; a panic across that
            // FFI aborts the host. Contain inspect/consume — the only Rust here that can panic.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                // load through a valid pointer — no mutation or aliasing.
                let spa_buf = unsafe { (*newest).buffer };

                // Cursor meta before the stale-frame skip: Mutter pointer-only moves arrive as
                // metadata-only CORRUPTED buffers we drop for pixels, but the cursor is fresh.
                update_cursor_meta(&mut ud.cursor, spa_buf);
                // Publish the live overlay so pointer-only motion on a static desktop still
                // moves. Skip when `overlay()` is `None`: gamescope has no `SPA_META_Cursor`,
                // and writing `None` at frame rate would clobber the XFixes `Some` in this
                // same slot (pointer strobes). Hidden is still `Some(visible:false)`.
                if let Some(overlay) = ud.cursor.overlay() {
                    if let Ok(mut slot) = ud.signals.cursor_live.lock() {
                        *slot = Some(overlay);
                    }
                }

                // Header + first chunk for the CORRUPTED skip. SPA_META_Header is optional.

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
                // Compositor stamp, upstream of delivery jitter `SystemTime::now()` cannot see.
                // Whether it is worth shipping is what the provenance line measures.
                let hdr_pts = if hdr.is_null() {
                    None
                } else {
                    // SAFETY: as for `.flags` — non-null, from a lookup that demanded at least
                    // `size_of::<spa_meta_header>()` bytes (so `.pts` is in bounds), read while
                    // the buffer is still held.
                    Some(unsafe { (*hdr).pts })
                };
                // Size + flags for the CORRUPTED skip. dmabuf legitimately reports chunk size
                // 0, so the size-0 stale skip is SHM-only.

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

                // Skip Mutter CORRUPTED / size-0 cursor-update buffers. Pointer motion sends
                // metadata-only buffers flagged CORRUPTED (chunk size 0) that still reference
                // a recycled old frame — encoding that is the flash. Size-0 skip is SHM-only.
                if corrupted || (chunk_size == 0 && !is_dmabuf) {
                    ud.dbg_log_n += 1;
                    if ud.dbg_log_n.is_power_of_two() {
                        tracing::debug!(
                            skipped = ud.dbg_log_n,
                            drained,
                            "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                        );
                    }
                    return;
                }

                consume_frame(ud, spa_buf, newest, hdr_pts);
            }));
            // Requeue `newest` exactly once on every path unless `try_defer` withheld it —
            // then `BufferHold` owns the requeue; doing both hands the producer the buffer
            // twice. The book is stable here: only this thread removes entries, and neither
            // callback runs inside `.process`. A panic after publish still leaves the hold live.
            let withheld = ud
                .defer
                .book
                .lock()
                .map(|b| b.contains(newest as usize))
                .unwrap_or(false);
            if !withheld {
                // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame)
                // completed inside the closure above; `newest` was dequeued from this stream,
                // not yet requeued, and — per the `withheld` check — carries no hold that would
                // requeue it a second time.
                unsafe { stream.queue_raw_buffer(newest) };
            }
            if outcome.is_err() {
                // `.process` is per-frame; a deterministic panic would flood. Power-of-two throttle.
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

    // A `BufferHold` dropping on any thread only sends; this loop-thread callback is where a
    // withheld buffer rejoins. `HoldBook::complete` no-ops a renegotiated-away (or reused)
    // address, so a stale hold can never queue somebody else's buffer.
    let defer_cb = defer.clone();
    let stream_ptr = stream.as_raw_ptr() as usize;
    let _requeue_attach = requeue_rx.attach(mainloop.loop_(), move |(buf, generation)| {
        let requeue = defer_cb
            .book
            .lock()
            .map(|mut b| b.complete(buf, generation))
            .unwrap_or(false);
        if requeue {
            // SAFETY: `complete` returned true ⇒ this buffer was withheld by exactly this hold
            // and no `remove_buffer` has freed it since (that purges the book), so the pointer
            // is a live buffer of this stream that we own (dequeued, never requeued). The
            // stream outlives this attached receiver (declared after it, dropped before it),
            // and the loop stops dispatching once `run()` returns.
            let _ = unsafe {
                pw::sys::pw_stream_queue_buffer(
                    stream_ptr as *mut pw::sys::pw_stream,
                    buf as *mut pw::sys::pw_buffer,
                )
            };
        }
    });

    // `PUNKTFUNK_PW_FIXED_POD="WxH"`: one fixed format, to bisect against a producer's EnumFormat.
    let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));

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

    // gamescope paints the Steam overlay into this node only when negotiated
    // `gamescope_focus_appid` is 0 (the default). Do not advertise a non-zero focus-appid —
    // that is the Remote-Play branch, which drops the overlay.

    // Zero-copy: offer only BGRx dmabuf with our EGL-importable modifiers (offering shm
    // makes the compositor pick shm). Modifiers go out as MANDATORY `ChoiceEnum::Enum`;
    // this is not the two-step DONT_FIXATE handshake (`ChoiceFlags` cannot express it).
    let format_pods: Vec<Vec<u8>> = if want_hdr {
        tracing::info!(
            "HDR capture: offering xBGR_210LE/xRGB_210LE LINEAR dmabufs with MANDATORY \
             BT.2020 + SMPTE-2084 (PQ) colorimetry (GNOME 50+ monitor stream)"
        );
        // Offering SDR alongside lets the producer pick it, and a timeout latches SDR downgrade.
        // Order is the fix — see the NVIDIA note on `HDR_FORMAT_ORDER`. First compatible pod wins.
        HDR_FORMAT_ORDER
            .iter()
            .map(|fmt| build_hdr_dmabuf_format(*fmt, preferred))
            .collect::<Result<Vec<_>>>()?
    } else if want_dmabuf {
        let mut pods = Vec::with_capacity(if prefer_native_nv12 { 3 } else { 2 });
        if prefer_native_nv12 {
            // First compatible consumer pod wins. Pinning BT.709 limited selects gamescope's
            // RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(VideoFormat::NV12, &[0], preferred)?);
        }
        if !modifiers.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &modifiers,
                preferred,
            )?);
        }
        // xdph (Hyprland/sway) lists only BGRA on its dmabuf EnumFormat (BGRA+BGRx on SHM).
        // A BGRx-only dmabuf offer intersects nothing and the link fails as if modifiers
        // mismatched. Same 32-bit layout; listed after BGRx so a producer offering both
        // still takes the existing path (first compatible consumer pod wins).
        if !modifiers_bgra.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRA,
                &modifiers_bgra,
                preferred,
            )?);
        }
        pods
    } else {
        vec![serialize_pod(obj)?]
    };
    let buffers_values = if want_hdr || want_dmabuf {
        // Dmabuf-only. HDR: Mutter's SHM path paints 8-bit ARGB32 regardless of format, so a
        // MemFd buffer under a 10-bit format would carry mislabeled bytes.
        Some(build_dmabuf_buffers()?)
    } else if force_shm {
        // Exclude DmaBuf so Mutter must download (glReadPixels orders against render).
        Some(build_shm_only_buffers()?)
    } else {
        // CPU path still accepts mappable dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins).
        Some(build_mappable_buffers()?)
    };

    // Cursor-as-metadata on every path (harmless if the producer cannot supply it): the pointer
    // rides as SPA_META_Cursor so the compositor keeps its hardware cursor plane.
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

    // Blocks until capturer `Drop` fires the quit channel; `run()` returns and the thread
    // unwinds, releasing the importer / CUDA context.
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        negotiation_plan, packed_frame_geometry, supported_data_plane_count, NegotiationInputs,
    };

    #[test]
    fn only_supported_pipewire_plane_counts_become_slice_lengths() {
        assert_eq!(supported_data_plane_count(0), None);
        assert_eq!(supported_data_plane_count(1), Some(1));
        assert_eq!(supported_data_plane_count(2), Some(2));
        assert_eq!(supported_data_plane_count(3), None);
        assert_eq!(supported_data_plane_count(u32::MAX), None);
    }

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

    #[test]
    fn packed_frame_geometry_rejects_overflow_and_short_stride() {
        assert_eq!(packed_frame_geometry(4, 3, 4, 0), Some((16, 16, 48, 48)));
        assert_eq!(packed_frame_geometry(4, 3, 4, 15), None);
        assert_eq!(packed_frame_geometry(1, 0, 4, 4), None);
        assert_eq!(packed_frame_geometry(usize::MAX, 2, 4, 0), None);
        assert_eq!(
            packed_frame_geometry(usize::MAX / 2, 3, 1, usize::MAX / 2),
            None
        );
    }

    /// Pins the four invariants documented on [`negotiation_plan`].
    #[test]
    fn negotiation_plan_invariants() {
        // HDR on NVENC builds the importer: pods are LINEAR-only, so frames take the
        // Vulkan-bridge arm, never the 8-bit de-tile blit. Tiled half is enforced in `.process`.
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
        // Never when the encoder cannot take packed 10-bit CUDA (libav HDR swscales to P010).
        // SDR is unaffected — the term is HDR-only.
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

        // Producer-native NV12 needs a `native_nv12_session` and an active raw passthrough:
        // libav VAAPI misreads two-plane; the CUDA importer expects packed RGB.
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

        // Passthrough (and the pyrowave-modifier extension) is off once the raw-dmabuf latch fires.
        let p = negotiation_plan(NegotiationInputs {
            raw_dmabuf_import_disabled: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.vaapi_passthrough, "latched ⇒ no raw passthrough");
        assert!(!p.prefer_native_nv12);
        assert!(p.raw_dmabuf_latched, "…and the operator gets told why");
    }

    /// The latch must move `vaapi_passthrough`. One resolver is shared by the thread and
    /// `spawn_pipewire`; a timeout must not latch a downgrade for an offer nobody made.
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

    /// EGL→CUDA negotiation-timeout latch gates `build_importer` only, so a compositor that
    /// accepts none of the importer's modifiers is not re-asked (10 s) every session.
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

    /// PyroWave takes raw passthrough (its Vulkan device imports on any vendor) and must not
    /// also build the EGL→CUDA importer — those payloads only NVENC can consume.
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
        // SHM-forced NVENC may still build the importer (it will not be fed dmabufs), so
        // `want_dmabuf` — not `build_importer` — is the gate.
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

    #[test]
    fn the_gpu_import_death_latch_skips_the_importer() {
        let p = negotiation_plan(NegotiationInputs {
            gpu_import_disabled: true,
            ..nvenc()
        });
        assert!(!p.build_importer);
        assert!(p.gpu_import_latched);
        // HDR takes the same importer (LINEAR/Vulkan-bridge), so the latch costs it zero-copy.
        assert!(
            negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                want_hdr: true,
                ..nvenc()
            })
            .gpu_import_latched
        );
        // Not reported for a raw passthrough that would never have built an importer.
        assert!(
            !negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                ..vaapi_native_nv12()
            })
            .gpu_import_latched
        );
    }

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

    // Env-var reads race under a shared test process, so these assert against the pure
    // functions the logging sites call.

    use super::{
        consumer_kind, resolved_capture_arm, CaptureArm, ConsumerKind, FenceWaitStats,
        PassthroughFallback, PassthroughFallbacks, PoolCensus, FENCE_WAIT_BUCKETS_US,
    };

    /// PyroWave wins even when it also flips `backend_is_vaapi` on (`linux_zero_copy_is_vaapi`
    /// `Pyrowave` arm). The other order reports the session as somebody else's backend.
    #[test]
    fn pyrowave_outranks_the_host_global_backend_pref() {
        assert_eq!(consumer_kind(true, true, true), ConsumerKind::PyroWave);
        // NVIDIA/auto host (`backend_is_vaapi` false) is still PyroWave — that case logged nothing.
        assert_eq!(consumer_kind(true, false, true), ConsumerKind::PyroWave);
    }

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
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert!(p.vaapi_passthrough);
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[0])),
            CaptureArm::DmabufPassthrough
        );
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
        assert_eq!(f.note(PassthroughFallback::NotDmabuf), Some(1));
        for _ in 0..1_000 {
            assert_eq!(f.note(PassthroughFallback::NotDmabuf), None);
        }
        // A different reason is a different diagnosis and gets its own line, carrying the
        // running total — which distinguishes a persistent downgrade from a hiccup.
        assert_eq!(f.note(PassthroughFallback::DupFailed), Some(1002));
        assert_eq!(f.note(PassthroughFallback::DupFailed), None);
        assert_eq!(f.note(PassthroughFallback::NoFormat), Some(1004));
        assert_eq!(f.note(PassthroughFallback::NoFourcc), Some(1005));
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
        // Only `NoFormat` drops the frame; the other three downgrade it.
        assert!(!PassthroughFallback::NoFormat.falls_back_to_cpu());
        assert!(PassthroughFallback::NotDmabuf.falls_back_to_cpu());
        assert!(PassthroughFallback::NoFourcc.falls_back_to_cpu());
        assert!(PassthroughFallback::DupFailed.falls_back_to_cpu());
    }

    // A p99 one bucket low would call the wait free; one bucket high would justify moving
    // load-bearing sync off the loop thread for nothing.

    /// An empty histogram must say "no answer", not "zero" — the second looks like p99 ≈ 0.
    #[test]
    fn an_empty_histogram_has_no_quantile() {
        let s = FenceWaitStats::default();
        assert_eq!(s.quantile_bucket_us(0.99), None);
        assert_eq!(s.mean_us(), 0);
        assert!(!s.is_meaningful(), "it must not be trusted yet either");
    }

    /// Every sample in the first bucket must put p99 there too.
    #[test]
    fn an_all_fast_distribution_puts_p99_in_the_first_bucket() {
        let mut s = FenceWaitStats::default();
        for _ in 0..1000 {
            s.record(3);
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(s.mean_us(), 3);
    }

    /// Fast median, heavy tail: p50 stays low and p99 finds the tail. A smear cannot tell them apart.
    #[test]
    fn a_heavy_tail_moves_p99_without_moving_p50() {
        let mut s = FenceWaitStats::default();
        for _ in 0..980 {
            s.record(10); // fast majority
        }
        for _ in 0..20 {
            s.record(6_000); // 2 % of frames stall milliseconds
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0])),
            "the median is still free"
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(10_000)),
            "...but the p99 must land in the 5-10ms bucket, not with the median"
        );
        assert_eq!(s.max_us, 6_000);
    }

    /// Anything past the last edge reports as overflow rather than being clamped into the last
    /// bucket — "worse than 10 ms" is a distinct finding and must not read as "10 ms".
    #[test]
    fn waits_past_the_last_edge_report_as_overflow() {
        let mut s = FenceWaitStats::default();
        s.record(99_000);
        assert_eq!(s.quantile_bucket_us(0.99), Some(None));
    }

    /// Bucket edges are inclusive upper bounds, so a sample exactly ON an edge belongs to that
    /// bucket and not the next one up.
    #[test]
    fn bucket_edges_are_inclusive() {
        for (i, &edge) in FENCE_WAIT_BUCKETS_US.iter().enumerate() {
            let mut s = FenceWaitStats::default();
            s.record(edge);
            assert_eq!(
                s.quantile_bucket_us(1.0),
                Some(Some(edge)),
                "a sample of exactly {edge}us belongs in bucket {i}"
            );
        }
    }

    /// A stable pool logs one line, not one per frame. `.process` runs at capture rate.
    #[test]
    fn a_stable_pool_is_logged_once() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..100 {
            assert_eq!(p.note_frame(), None, "the same depth must not re-log");
        }
    }

    /// A renegotiation frees the pool and re-allocates it. The LIVE count therefore dips (and the
    /// new depth is worth a second line), but `high_water` — the number a pipeline-depth decision
    /// keys on — must not follow the dip down.
    #[test]
    fn a_renegotiated_pool_relogs_but_the_high_water_holds() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..8 {
            p.remove();
        }
        for _ in 0..4 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(4), "a changed depth is worth a line");
        assert_eq!(p.high_water, 8, "the deepest pool seen this session");
    }

    /// `remove_buffer` without a matching `add_buffer` must not wrap the count to `u32::MAX` —
    /// a depth gate reading that would happily pipeline against a pool of zero.
    #[test]
    fn unmatched_removes_saturate_at_zero() {
        let mut p = PoolCensus::default();
        p.remove();
        p.remove();
        assert_eq!(p.note_frame(), Some(0));
        assert_eq!(p.high_water, 0);
    }

    use super::{HoldBook, HOLD_POOL_RESERVE};

    /// The book must always leave [`HOLD_POOL_RESERVE`] buffers with the producer: an 8-pool
    /// spares 6, and the pools at or below the reserve spare NOTHING — those sessions must fall
    /// back to the immediate requeue rather than starve the compositor of render targets.
    #[test]
    fn hold_book_spends_at_most_pool_minus_reserve() {
        let mut b = HoldBook::default();
        for i in 0..6 {
            assert!(
                b.try_hold(0x1000 + i, 8).is_some(),
                "hold {i} within budget"
            );
        }
        assert!(
            b.try_hold(0x2000, 8).is_none(),
            "7th of 8 exceeds the budget"
        );
        assert!(
            HoldBook::default()
                .try_hold(0x1000, HOLD_POOL_RESERVE)
                .is_none(),
            "a pool of exactly the reserve cannot spare a buffer"
        );
        assert!(
            HoldBook::default()
                .try_hold(0x1000, HOLD_POOL_RESERVE + 1)
                .is_some(),
            "one past the reserve spares exactly one"
        );
    }

    /// One hold ⇒ one requeue: the first `complete` releases, a duplicate release (a bug shape,
    /// but also the benign stale-message case) must NOT requeue a second time — handing the
    /// producer the same buffer twice corrupts its pool.
    #[test]
    fn hold_book_releases_exactly_once() {
        let mut b = HoldBook::default();
        let g = b.try_hold(0x1000, 8).unwrap();
        assert!(b.complete(0x1000, g), "first release requeues");
        assert!(!b.complete(0x1000, g), "second release is a no-op");
        assert!(!b.contains(0x1000));
    }

    /// Pool replaced (`remove_buffer` purges), a new buffer lands on the same address, then the
    /// old hold's release arrives. Matching by pointer alone would requeue the new tenant while
    /// its own hold is still out.
    #[test]
    fn hold_book_generation_outlives_an_address_reuse() {
        let mut b = HoldBook::default();
        let old = b.try_hold(0x1000, 8).unwrap();
        b.purge(0x1000); // `remove_buffer`: pool renegotiated away under the hold
        assert!(!b.complete(0x1000, old), "purged hold releases nothing");
        let new = b.try_hold(0x1000, 8).unwrap(); // new pool, same address
        assert!(
            !b.complete(0x1000, old),
            "the OLD hold cannot release the NEW tenant"
        );
        assert!(b.contains(0x1000), "new tenant still withheld");
        assert!(b.complete(0x1000, new), "its own hold releases it");
    }

    /// A buffer already out cannot be withheld again (one requeue duty per buffer): `.process`
    /// can only re-see an address after its requeue, so a duplicate try_hold means state
    /// confusion — refuse it and let the epilogue requeue immediately.
    #[test]
    fn hold_book_refuses_a_buffer_already_out() {
        let mut b = HoldBook::default();
        b.try_hold(0x1000, 8).unwrap();
        assert!(b.try_hold(0x1000, 8).is_none());
    }
}
