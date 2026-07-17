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
mod setup;
mod sync_loop;

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
