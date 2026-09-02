//! Side-plane queue depths, the `RumbleUpdate` alias, and the public `AudioPacket`.

/// Audio packets for the embedder. 64 × 5 ms = 320 ms of slack at the Opus
/// frame; overflow drops the newest (the renderer conceals the gap).
///
/// Depth is packets, not ms, and lossless `0xD3` shares it: a 2 ms PCM frame
/// then holds ~128 ms, still above the 15–90 ms de-jitter target. A
/// format-dependent depth would make overflow session-dependent.
pub(crate) const AUDIO_QUEUE: usize = 64;

/// Rumble updates for the embedder. Overflow drops the newest; the host
/// renews (v2) or re-sends (v1), so a dropped transition heals in one period.
pub(crate) const RUMBLE_QUEUE: usize = 16;

/// Embedder rumble: `(pad, low, high, ttl_ms)`. `Some(ms)` is a v2 envelope
/// (render at most that long); `None` is legacy v1 (renderer staleness).
/// The v2 seq is consumed by the datagram reorder gate and is not forwarded.
pub(crate) type RumbleUpdate = (u16, u16, u16, Option<u16>);

/// HID-output (DualSense lightbar / LEDs / triggers). Overflow drops newest;
/// the host re-sends on the next feedback change.
pub(crate) const HIDOUT_QUEUE: usize = 32;

/// Pad-audio (`0xD1` voice-coil + speaker), all pads on one queue. Same 64
/// and newest-drop as [`AUDIO_QUEUE`]; the embedder fans out by `pad`/`kind`.
pub(crate) const PAD_AUDIO_QUEUE: usize = 64;

/// Static HDR metadata (ST.2086 + CLL). One on start, re-sent on mastering
/// changes / keyframes; 8 is ample.
pub(crate) const HDR_META_QUEUE: usize = 8;

/// Host-timing (`0xCF`, one datagram per AU). 512 holds a 240 fps stream
/// drained once per second with headroom. Overflow drops newest: observability,
/// not state.
pub(crate) const HOST_TIMING_QUEUE: usize = 512;

/// Clipboard events. Human-paced; 32 is ample. Overflow drops newest: a
/// dropped offer heals on the next copy, a dropped fetch-request times out.
pub(crate) const CLIP_EVENT_QUEUE: usize = 32;

/// Cursor-shape ([`crate::quic::CursorShape`]). Human-paced but bursty.
/// Overflow drops newest and the host does not re-send — only a serial
/// change emits again — so embedders must keep the last shape when
/// `hostCursors[serial]` misses rather than hiding the pointer.
pub(crate) const CURSOR_SHAPE_QUEUE: usize = 8;

/// Cursor-state (`0xD0`, one datagram per captured frame). Latest-wins; a
/// tiny ring only bridges scheduling jitter. Overflow heals next frame.
pub(crate) const CURSOR_STATE_QUEUE: usize = 8;

/// One packet from the host audio datagram: Opus off `0xC9`/`0xD2`
/// (48 kHz, 5 ms) or lossless PCM off `0xD3` at the negotiated rate/depth
/// and one rung of [`crate::audio::pcm::FRAME_US_LADDER`].
///
/// The planes share this type because they share `seq` / `pts_ns`. They do
/// not share how `data` is read — the session
/// [`NativeClient::audio_codec`](crate::client::NativeClient::audio_codec)
/// says which, once, for the whole session.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub pts_ns: u64,
    /// Opus: one decoder frame. PCM: interleaved LE integers for
    /// [`crate::audio::pcm::to_f32`]. Empty is a DTX silence marker (Opus).
    pub data: Vec<u8>,
}
