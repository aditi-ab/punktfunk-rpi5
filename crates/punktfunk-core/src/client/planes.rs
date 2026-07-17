//! The side-plane queue depths, the `RumbleUpdate` alias, and the public `AudioPacket`.

/// Audio packets buffered for the embedder: 64 × 5 ms = 320 ms of slack. A lagging
/// embedder drops the newest packet (the audio renderer conceals the gap).
pub(crate) const AUDIO_QUEUE: usize = 64;

/// Rumble updates buffered for the embedder. Overflow drops the NEWEST update (same
/// `try_send` discipline as the other planes) — the host renews rumble state periodically
/// (v2 envelopes) or re-sends it (legacy v1), so a dropped transition (including a stop) heals
/// within one renewal/refresh period.
pub(crate) const RUMBLE_QUEUE: usize = 16;

/// A rumble update handed to the embedder: `(pad, low, high, ttl_ms)`. `ttl_ms` is `Some(ms)` for
/// a self-terminating v2 envelope (render for at most that long) and `None` for a legacy v1
/// datagram (an old host — the renderer applies its own staleness policy). The seq from a v2
/// envelope is consumed by the reorder gate in the datagram demux and is NOT forwarded.
pub(crate) type RumbleUpdate = (u16, u16, u16, Option<u16>);

/// HID-output (DualSense lightbar / player LEDs / adaptive triggers) buffered for the embedder.
/// Same overflow discipline as rumble; the host re-sends on the next feedback change.
pub(crate) const HIDOUT_QUEUE: usize = 32;

/// Static HDR metadata (ST.2086 mastering + content light level) buffered for the embedder. Tiny
/// and low-rate (one on start, re-sent on mastering changes / keyframes); a small ring is ample.
pub(crate) const HDR_META_QUEUE: usize = 8;

/// Host-timing plane depth (0xCF, one datagram per AU). Sized for a 240 fps stream whose stats
/// consumer drains once per second with headroom; overflow drops the newest sample (try_send) —
/// harmless, it's per-frame observability, not state.
pub(crate) const HOST_TIMING_QUEUE: usize = 512;

/// One Opus packet from the host's audio datagram stream (48 kHz stereo, 5 ms frames).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub pts_ns: u64,
    /// The raw Opus payload — feed it to an Opus decoder as one frame.
    pub data: Vec<u8>,
}
