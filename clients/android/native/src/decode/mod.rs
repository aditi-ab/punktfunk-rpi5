//! Android video decode (android-only): pull HEVC access units from the connector and render them
//! to the SurfaceView via NDK `AMediaCodec` — hardware decode, zero per-frame JNI.
//!
//! One-in/one-out: the host opens every stream with an IDR carrying VPS/SPS/PPS **in-band**, so the
//! decoder needs no out-of-band codec-specific data — we configure with mime + the negotiated
//! WxH (from [`NativeClient::mode`]) and feed each access unit as it arrives. The decode thread owns
//! the codec + window for its whole life; [`crate::session`] signals it to stop via the shared flag.

mod async_loop;
mod display;
mod latency;
mod presenter;
mod setup;
mod sync_loop;
mod vsync;

use async_loop::run_async;
pub(crate) use setup::{codec_label, codec_mime};
use sync_loop::run_sync;

use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Cap on AUs parked in the async loop awaiting a free codec input slot. Matches the connector's
/// own frame-channel depth; on sustained overflow the oldest is dropped and a keyframe requested
/// (same recovery as a reassembler drop). In steady state this stays near-empty.
const FRAME_PARK_CAP: usize = 16;

/// Cap on the pts→received-timestamp map below: MediaCodec holds only a handful of frames in
/// flight, so anything beyond this is stale (codec flushed / HUD toggled) and gets evicted.
const IN_FLIGHT_CAP: usize = 64;

/// Cap on received AUs awaiting their 0xCF host timing (Phase 2 host/network split): the timing
/// datagram trails its AU by at most the wire, so a match lands within a frame or two — anything
/// this deep is a lost datagram (or an old host that never sends any) and gets evicted.
const PENDING_SPLIT_CAP: usize = 256;

/// Cap on rendered frames parked in [`DisplayTracker`] awaiting their `OnFrameRendered` render
/// timestamp: the callback trails its release by at most a vsync or two, so anything this deep
/// means the platform stopped delivering render callbacks (allowed under load, per the docs) and
/// gets evicted.
const RENDERED_CAP: usize = 64;

/// How long the decoder may be FED while producing nothing before we treat it as un-anchored and
/// ask the host for a fresh IDR.
///
/// The shared gate's per-AU streak ([`punktfunk_core::reanchor::ReanchorGate::on_no_output`]) can't
/// be used verbatim here: it counts one-in/one-out decodes (the desktop clients' `LOW_DELAY`
/// libavcodec path and Apple's VideoToolbox), while MediaCodec is pipelined — inputs and outputs
/// don't pair up, so "this AU produced no output" isn't a thing this loop can observe. A wall-clock
/// silence window is the same signal in the shape Android can measure.
///
/// Why it matters: the host opens a stream with an IDR and, under infinite GOP, sends no other one
/// unless asked. Miss that one — the decode thread only starts at `surfaceCreated`, so a slow TV box
/// can be handed the stream mid-GOP — and every later AU references a picture the decoder never had.
/// A hardware decoder doesn't error on that; it simply emits nothing. Without this backstop the
/// session sat there forever: AUs arriving, a healthy HUD, and a black surface, because nothing in
/// the Android loops ever asked for the keyframe that would re-anchor it.
///
/// 500 ms because it must never fire on a decoder that is merely slow to spin up: even the pokiest
/// hardware decoder emits its first frame within a couple of frame periods, and a wedge that only
/// costs half a second before it self-heals is not a bug the user reports.
const NO_OUTPUT_PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a session may deliver NO access unit at all before we ask for a keyframe and say so.
///
/// [`NO_OUTPUT_PATIENCE`] covers "fed but silent", and it deliberately requires `fed` to have moved
/// so an idle stream never asks for anything. That leaves its mirror image uncovered: a session that
/// receives nothing whatsoever. A decoder cannot be starved of output when it was handed no input,
/// so no signal in either loop fires, and the session sits connected — audio, input and the control
/// plane all alive — behind a black surface with a HUD reading `0 fps · 0.0 Mb/s`, which is exactly
/// how it comes back in reports (2026-07-30).
///
/// Asking costs one small control message, and it is the right ask in the case we can actually fix:
/// the host is encoding, but under infinite GOP every picture it sends references an IDR this client
/// never saw. When the host is sending nothing at all, the request changes nothing — but the log line
/// beside it is what separates that from "we received AUs and lost them", which no previous black
/// screen report could tell us.
const NO_VIDEO_PATIENCE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Re-ask cadence once [`NO_VIDEO_PATIENCE`] has elapsed with still nothing received. Slow, because
/// this state is either self-healing on the first ask or not ours to heal — and each pass logs.
const NO_VIDEO_RETRY: std::time::Duration = std::time::Duration::from_millis(2000);

/// Whether low-latency mode uses the event-driven async decode loop (default) or the synchronous
/// poll loop. Flip to `false` to A/B the two on the HUD (`design/…`); the async loop presents a
/// decoded frame the instant it's ready instead of waiting out a poll interval. Only consulted when
/// the user's "Low-latency mode" toggle is ON (now the default) — off, the sync loop always runs (the
/// original pipeline, kept as the per-device escape hatch).
const USE_ASYNC_DECODE: bool = true;

/// Per-session decode configuration, resolved by the JNI layer (`nativeStartVideo`) and passed to
/// the decode loop. Bundled so the loop entry points don't sprout a wide argument list.
pub(crate) struct DecodeOptions {
    /// The decoder Kotlin ranked from `MediaCodecList` (`VideoDecoders.pickDecoder`). `None`/empty ⇒
    /// let the platform resolve the default decoder for the MIME.
    pub decoder_name: Option<String>,
    /// Whether Kotlin found the chosen decoder advertises `FEATURE_LowLatency` (queryable only via
    /// the Java `CodecCapabilities` API) — surfaced on the HUD next to the decoder name.
    pub ll_feature: bool,
    /// The user's "Low-latency mode" master toggle. On (default) ⇒ the full fast pipeline: async
    /// decode loop, per-SoC vendor keys, pipeline thread boosts, ADPF max-performance, forced TV
    /// mode switch. Off ⇒ the original synchronous pre-overhaul pipeline, kept as the per-device
    /// escape hatch.
    pub low_latency_mode: bool,
    /// TV form factor (Kotlin's `UiModeManager`): actively drive the HDMI output into the stream's
    /// refresh mode, vs. the softer seamless hint on a phone/tablet.
    pub is_tv: bool,
    /// The user's presentation intent (`present_priority` setting): 0 = lowest latency
    /// (newest-wins), 1 = smoothness (a small FIFO). Resolved by
    /// [`presenter::PresentPriority::resolve`]; anything else = latency.
    pub present_priority: i32,
    /// The smoothness buffer depth (`smooth_buffer` setting): 0 = automatic (2), else 1..=3.
    /// Only meaningful with `present_priority` = smooth.
    pub smooth_buffer: i32,
    /// SEED for the panel's refresh period — the latch grid the presenter subdivides onto when
    /// the app's choreographer stream is down-rated below the panel (see `vsync.rs`). Kotlin
    /// resolves it from the display mode TABLE (`MainActivity.streamPanelFps`), not
    /// `display.refreshRate`, which reports a per-uid override rather than the panel. 0 = unknown.
    ///
    /// ⚠ Only a seed: `preferredDisplayModeId` is a REQUEST the system may refuse, so the mode
    /// named here is not necessarily the one the panel ends up in. The measured timeline spacing
    /// corrects it in both directions ([`punktfunk_core::phase::PanelGrid`]).
    pub panel_hz: i32,
}

/// The decode entry point on the `pf-decode` thread: dispatches to the async or synchronous loop.
/// Both run until `shutdown` is set or the session closes.
pub fn run(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    if opts.low_latency_mode && USE_ASYNC_DECODE {
        run_async(client, window, shutdown, stats, opts);
    } else {
        run_sync(client, window, shutdown, stats, opts);
    }
}
