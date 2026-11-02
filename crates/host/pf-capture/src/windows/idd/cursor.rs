//! Host side of the hardware-cursor channel.
//!
//! The capturer creates an unnamed [`CursorShm`] section, delivers it to pf-vdisplay
//! (IddCx hardware cursor — DWM then excludes the pointer from consumed frames), and
//! seqlock-reads the driver's publishes at encode-tick pace into the same
//! [`pf_frame::CursorOverlay`] the Linux portal path produces. Downstream (forwarder,
//! wire, client renderer) is shared. IddCx positions are already relative to the monitor.
//! The header also carries the one fact the driver's own blend needs and cannot query in
//! session 0: the HDR desktop's SDR-white scale.

use super::*;
use crate::windows::cursor_raster::ShapeClass;
use pf_driver_proto::cursor::{
    shape_extent, shape_rgba, CursorShm, ShapeRgba, CURSOR_MAGIC, CURSOR_SHAPE_OFFSET,
    CURSOR_SHM_SIZE, CURSOR_TYPE_ALPHA,
};
use std::mem::offset_of;

/// Host end of one monitor's cursor channel. The mapping stays valid for the capturer's
/// life.
pub(super) struct CursorShared {
    section: MappedSection,
    /// Last `shape_id` whose pixels were converted. Position-only updates (the common
    /// case) reuse it — a refcount bump, no pixel work.
    cached_id: u32,
    cached: Option<ConvertedShape>,
}

struct ConvertedShape {
    rgba: std::sync::Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    class: ShapeClass,
}

impl ConvertedShape {
    fn new(hdr: &CursorShm, s: ShapeRgba) -> Self {
        Self {
            rgba: std::sync::Arc::new(s.rgba),
            w: s.w,
            h: s.h,
            hot_x: s.hot_x,
            hot_y: s.hot_y,
            class: if hdr.cursor_type == CURSOR_TYPE_ALPHA {
                ShapeClass::Alpha
            } else {
                ShapeClass::Masked
            },
        }
    }
}

/// Set on every section serial. The client caches shapes by a `u32` serial the poller also
/// numbers from 1; bit 31 keeps the two apart and survives the forwarder's truncation.
pub(super) const SECTION_SERIAL_TAG: u64 = 1 << 31;

/// The header in words: it is all `u32`s and ends where the shape starts.
const HEADER_WORDS: usize = size_of::<CursorShm>() / 4;
const _: () = assert!(HEADER_WORDS * 4 == CURSOR_SHAPE_OFFSET);

/// The header as of now, one atomic load per word. It can be torn against a publish; the
/// caller's sequence check decides.
fn header(section: &MappedSection) -> CursorShm {
    let mut words = [0u32; HEADER_WORDS];
    for (i, word) in words.iter_mut().enumerate() {
        *word = section.word32(i * 4).load(Ordering::Relaxed);
    }
    bytemuck::cast(words)
}

impl CursorShared {
    /// Create the section: zeroed, so seq is 0 (even, nothing published), then the magic the
    /// driver validates at adopt. The origin stays 0: IddCx positions are monitor-relative,
    /// and a driver that still subtracts it must subtract nothing. `self` owns the section;
    /// the caller duplicates its handle into the WUDFHost.
    pub(super) fn create() -> Result<CursorShared> {
        let section = MappedSection::create(CURSOR_SHM_SIZE, None, "cursor")?;
        section
            .word32(offset_of!(CursorShm, magic))
            .store(CURSOR_MAGIC, Ordering::Release);
        Ok(CursorShared {
            section,
            cached_id: 0,
            cached: None,
        })
    }

    pub(super) fn section_handle(&self) -> BorrowedHandle<'_> {
        self.section.handle.as_handle()
    }

    /// Tell the driver where this HDR desktop puts SDR white (1.0 = 80 nits) for its blend
    /// onto an FP16 frame; `0` = SDR. One word, read by the driver per publish.
    pub(super) fn set_sdr_white_scale(&self, scale: f32) {
        self.section
            .word32(offset_of!(CursorShm, sdr_white_scale))
            .store(scale.to_bits(), Ordering::Relaxed);
    }

    /// Class of the shape the last [`Self::read`] returned; `None` before any shape.
    pub(super) fn shape_class(&self) -> Option<ShapeClass> {
        self.cached.as_ref().map(|s| s.class)
    }

    /// Seqlock-read the latest publish as a frame-relative [`pf_frame::CursorOverlay`], its
    /// serial tagged [`SECTION_SERIAL_TAG`]. `None` until the first publish. Hidden pointer →
    /// `Some` with `visible: false` — the forwarder turns that into the client's
    /// relative-mode hint, as on Linux.
    pub(super) fn read(&mut self) -> Option<pf_frame::CursorOverlay> {
        let seq = self.section.word32(offset_of!(CursorShm, seq));
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None; // seq 0: no publish yet (even, but not a valid snapshot)
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue; // odd seq: writer mid-update
            }
            let hdr = header(&self.section);
            if hdr.visible != 0 && hdr.shape_id != self.cached_id {
                // `shape_extent` clamps `rows * pitch` to the shape region.
                let (_, rows, pitch) = shape_extent(&hdr);
                let mut raw = vec![0u8; rows * pitch];
                self.section.read_bytes(CURSOR_SHAPE_OFFSET, &mut raw);
                // Writer raced mid-shape (seq moved) — retry without caching. The fence is what
                // keeps the payload reads above from being reordered after this check: an
                // acquire LOAD only orders what follows it. Free on x86, load-load ordering on
                // aarch64, where the reads could otherwise straddle a writer's update.
                std::sync::atomic::fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) != s1 {
                    continue;
                }
                self.cached = Some(ConvertedShape::new(&hdr, shape_rgba(&hdr, &raw)));
                self.cached_id = hdr.shape_id;
            } else {
                std::sync::atomic::fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) != s1 {
                    continue;
                }
            }
            // Hidden before any shape was seen is still a hide the client must hear.
            let Some(shape) = self.cached.as_ref() else {
                return (hdr.visible == 0).then(|| pf_frame::CursorOverlay {
                    x: hdr.x,
                    y: hdr.y,
                    w: 0,
                    h: 0,
                    rgba: std::sync::Arc::new(Vec::new()),
                    serial: 0,
                    hot_x: 0,
                    hot_y: 0,
                    visible: false,
                });
            };
            return Some(pf_frame::CursorOverlay {
                x: hdr.x,
                y: hdr.y,
                w: shape.w,
                h: shape.h,
                rgba: shape.rgba.clone(),
                serial: u64::from(hdr.shape_id) | SECTION_SERIAL_TAG,
                hot_x: shape.hot_x,
                hot_y: shape.hot_y,
                visible: hdr.visible != 0,
            });
        }
        None // writer wedged mid-seq; skip this tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand in for the driver: the header and the shape's pixels, then `seq`.
    fn publish(shared: &CursorShared, seq: u32, hdr: CursorShm, bgra: &[[u8; 4]]) {
        let words: [u32; HEADER_WORDS] = bytemuck::cast(hdr);
        for (i, word) in words.iter().enumerate().skip(2) {
            shared.section.word32(i * 4).store(*word, Ordering::Relaxed);
        }
        for (i, px) in bgra.iter().enumerate() {
            shared
                .section
                .word32(CURSOR_SHAPE_OFFSET + i * 4)
                .store(u32::from_le_bytes(*px), Ordering::Relaxed);
        }
        shared
            .section
            .word32(offset_of!(CursorShm, seq))
            .store(seq, Ordering::Release);
    }

    #[test]
    fn a_publish_reads_back_and_a_writer_mid_update_does_not() {
        let mut shared = CursorShared::create().expect("cursor section");
        assert!(shared.read().is_none(), "nothing published yet");

        let hdr = CursorShm {
            visible: 1,
            cursor_type: CURSOR_TYPE_ALPHA,
            x: 5,
            y: -7,
            shape_id: 1,
            width: 2,
            height: 1,
            pitch: 8,
            hot_x: 1,
            ..bytemuck::Zeroable::zeroed()
        };
        publish(&shared, 2, hdr, &[[1, 2, 3, 4], [5, 6, 7, 8]]);
        let got = shared.read().expect("a published cursor");
        assert_eq!((got.x, got.y, got.w, got.h, got.hot_x), (5, -7, 2, 1, 1));
        assert_eq!(got.serial, 1 | SECTION_SERIAL_TAG);
        assert_eq!(shared.shape_class(), Some(ShapeClass::Alpha));
        assert!(got.visible);
        // ALPHA shapes are BGRA in the section and RGBA on the way out.
        assert_eq!(got.rgba[..], [3, 2, 1, 4, 7, 6, 5, 8]);

        publish(&shared, 3, hdr, &[]);
        assert!(shared.read().is_none(), "an odd seq is a writer mid-update");

        publish(&shared, 4, CursorShm { visible: 0, ..hdr }, &[]);
        assert!(!shared.read().expect("a hide is still a publish").visible);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn a_word_past_the_section_is_refused() {
        let shared = CursorShared::create().expect("cursor section");
        shared.section.word32(CURSOR_SHM_SIZE);
    }
}
