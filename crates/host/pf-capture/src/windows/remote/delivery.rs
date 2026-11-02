//! What the two pixel-less capturers share: IDD-push, where the driver encodes, and the
//! capture worker's, where the worker does.
//!
//! Neither hands the stream loop pixels. A delivery is the news that the encoder's pool took
//! a new frame, read off the encoder's own counter, and the loop paces and sizes itself from
//! it. [`DeliveryGate`] decides when there is news; [`frame`] is what a delivery looks like.
//!
//! Pure, so the gate's rule has tests on every target.

use pf_frame::{CapturedFrame, FramePayload, PixelFormat, Provenance};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// When a pixel-less capturer has a delivery for the loop. `G` is the geometry the loop
/// sizes the encoder from.
pub(crate) struct DeliveryGate<G> {
    /// The encoder's open time and pool counter at the last delivery.
    source: Option<(Instant, u64)>,
    /// The geometry last delivered. A change must reach the loop even over a desktop composing
    /// nothing: it rebuilds the encoder off the frame.
    delivered: Option<G>,
}

impl<G: PartialEq> DeliveryGate<G> {
    pub(crate) fn new() -> Self {
        Self {
            source: None,
            delivered: None,
        }
    }

    /// Whether this tick delivers. `source` is the encoder's open time and pool counter, the
    /// only source clock this side has, and `None` until the encoder opens: the loop needs a
    /// frame to open it. A count is news against its own encoder's last delivery: a re-opened
    /// encoder's first frame may carry the number its predecessor delivered last. A fresh
    /// section reads 0 until the pool takes a frame, which is no news.
    pub(crate) fn admit(&mut self, source: Option<(Instant, u64)>, geometry: G) -> bool {
        let news = match source {
            Some((_, 0)) => false,
            Some(_) => source != self.source,
            None => true,
        };
        if !news && self.delivered.as_ref() == Some(&geometry) {
            return false;
        }
        self.source = source;
        self.delivered = Some(geometry);
        true
    }
}

/// The `seq`th delivery of a pixel-less capturer, stamped now. No pixels cross the boundary:
/// the loop sees [`FramePayload::Pixelless`] and owes wire indexes for what `Encoder::ready_aus`
/// counts instead of submitting this frame. The encoder stamps each access unit with the
/// frame's own present time, so this side reports the sequence only.
pub(crate) fn frame(seq: u64, width: u32, height: u32, format: PixelFormat) -> CapturedFrame {
    CapturedFrame {
        provenance: Provenance::source(seq, 0),
        width,
        height,
        pts_ns: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        format,
        payload: FramePayload::Pixelless,
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: (u32, u32) = (1920, 1080);
    const QHD: (u32, u32) = (2560, 1440);

    /// One gate through a session: the encoder and pool counter and the geometry of each tick,
    /// and whether the tick delivers. `a`, `b`, `c` and `d` are four encoder opens.
    #[test]
    fn a_tick_delivers_only_news() {
        let a = Instant::now();
        let [b, c, d] = [1, 2, 3].map(|s| a + std::time::Duration::from_secs(s));
        let ticks = [
            (None, HD, true, "the loop needs a frame to open the encoder"),
            (None, HD, true, "and needs one until it is open"),
            (Some((a, 0)), HD, false, "a fresh section is no news"),
            (Some((a, 1)), HD, true, "the pool took a frame"),
            (Some((a, 1)), HD, false, "the desktop composed nothing"),
            (Some((a, 2)), HD, true, "the pool took another"),
            (Some((a, 2)), QHD, true, "a new size over a still desktop"),
            (Some((a, 2)), QHD, false, "delivered once"),
            (Some((b, 0)), HD, true, "a new size over a fresh section"),
            (Some((b, 0)), HD, false, "its fresh section is no news"),
            (Some((b, 1)), HD, true, "until it takes a frame"),
            (Some((c, 1)), HD, true, "same size re-open, b's last number"),
            (Some((c, 1)), HD, false, "delivered once"),
            (Some((d, 0)), HD, false, "its fresh section is no news"),
            (Some((d, 1)), HD, true, "news against its own encoder"),
        ];
        let mut gate = DeliveryGate::new();
        for (i, (source, geometry, delivers, why)) in ticks.into_iter().enumerate() {
            assert_eq!(gate.admit(source, geometry), delivers, "tick {i}: {why}");
        }
    }

    #[test]
    fn a_delivery_carries_no_pixels() {
        let f = frame(3, 1920, 1080, PixelFormat::Nv12);
        assert!(matches!(f.payload, FramePayload::Pixelless));
        assert_eq!(f.provenance, Provenance::source(3, 0));
        assert_eq!(
            (f.width, f.height, f.format),
            (1920, 1080, PixelFormat::Nv12)
        );
        assert!(f.cursor.is_none());
    }
}
