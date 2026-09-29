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
/// Overflow evicts the OLDEST ([`shape_queue`]): the host sends a shape once
/// per change and its state names the newest, so losing that one would leave
/// the embedder on a stale pointer until the next change.
pub(crate) const CURSOR_SHAPE_QUEUE: usize = 8;

type ShapeSlot = (
    std::collections::VecDeque<crate::quic::CursorShape>,
    // The sender is gone: the session ended.
    bool,
);

#[derive(Default)]
struct ShapeShared {
    slot: std::sync::Mutex<ShapeSlot>,
    ready: std::sync::Condvar,
}

/// A [`CURSOR_SHAPE_QUEUE`]-deep queue that evicts the oldest shape when full. Dropping the
/// sender closes it, as a channel's disconnect does.
pub(crate) fn shape_queue() -> (ShapeSender, ShapeReceiver) {
    let shared = std::sync::Arc::new(ShapeShared::default());
    (ShapeSender(shared.clone()), ShapeReceiver(shared))
}

pub(crate) struct ShapeSender(std::sync::Arc<ShapeShared>);

impl ShapeSender {
    pub(crate) fn send(&self, shape: crate::quic::CursorShape) {
        let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.0.len() == CURSOR_SHAPE_QUEUE {
            slot.0.pop_front();
        }
        slot.0.push_back(shape);
        drop(slot);
        self.0.ready.notify_one();
    }
}

impl Drop for ShapeSender {
    fn drop(&mut self) {
        self.0.slot.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.0.ready.notify_all();
    }
}

pub(crate) struct ShapeReceiver(std::sync::Arc<ShapeShared>);

impl ShapeReceiver {
    /// The oldest queued shape, waiting up to `timeout`; `Disconnected` once the sender is
    /// gone and nothing is left.
    pub(crate) fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<crate::quic::CursorShape, std::sync::mpsc::RecvTimeoutError> {
        let slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        let (mut slot, _) = self
            .0
            .ready
            .wait_timeout_while(slot, timeout, |s| s.0.is_empty() && !s.1)
            .unwrap_or_else(|e| e.into_inner());
        match slot.0.pop_front() {
            Some(shape) => Ok(shape),
            None if slot.1 => Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            None => Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    fn shape(serial: u32) -> crate::quic::CursorShape {
        crate::quic::CursorShape {
            serial,
            w: 1,
            h: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0; 4],
        }
    }

    #[test]
    fn a_full_shape_queue_keeps_the_newest() {
        let (tx, rx) = shape_queue();
        for serial in 0..CURSOR_SHAPE_QUEUE as u32 + 3 {
            tx.send(shape(serial));
        }
        let got: Vec<u32> = std::iter::from_fn(|| rx.recv_timeout(Duration::ZERO).ok())
            .map(|s| s.serial)
            .collect();
        assert_eq!(got.len(), CURSOR_SHAPE_QUEUE);
        assert_eq!(got.last(), Some(&(CURSOR_SHAPE_QUEUE as u32 + 2)));
        assert!(matches!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }
}
