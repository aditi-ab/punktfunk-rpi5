//! Host side of the hardware-cursor channel.
//!
//! The capturer creates an unnamed [`CursorShm`] section, delivers it to pf-vdisplay
//! (IddCx hardware cursor — DWM then excludes the pointer from consumed frames), and
//! seqlock-reads the driver's publishes at encode-tick pace into the same
//! [`pf_frame::CursorOverlay`] the Linux portal path produces. Downstream (forwarder,
//! wire, client renderer) is shared.

use super::*;
use pf_driver_proto::cursor::{
    CursorShm, CURSOR_MAGIC, CURSOR_SHAPE_BYTES, CURSOR_SHAPE_MAX, CURSOR_SHAPE_OFFSET,
    CURSOR_SHM_SIZE, CURSOR_TYPE_MASKED_COLOR,
};
use std::sync::atomic::AtomicU32;

/// Host end of one monitor's cursor channel. The mapping stays valid for the capturer's
/// life.
pub(super) struct CursorShared {
    section: MappedSection,
    /// Monitor desktop origin. IddCx reports desktop coordinates; the overlay wants
    /// frame-relative. Placement is stable for the session; a topology change recreates
    /// the pipeline.
    origin: (i32, i32),
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
}

impl CursorShared {
    /// Create + initialize the section (magic stamped, seq even/zero). The returned handle is
    /// the section itself (owned by `self`); the caller duplicates it into the WUDFHost.
    pub(super) fn create(ccd: pf_win_display::win_display::CcdTargetKey) -> Result<CursorShared> {
        // SAFETY: plain FFI. Unnamed pagefile-backed section, host-lifetime owned; the view is
        // mapped once here and unmapped exactly once by `MappedSection::drop` (which unmaps before
        // closing the mapping handle). No borrow into the view outlives the `MappedSection`: every
        // access goes through `&self` accessors on the owner.
        let section = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                CURSOR_SHM_SIZE as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(cursor)")?;
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                CURSOR_SHM_SIZE,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for the cursor section");
            }
            let shm = view.Value.cast::<CursorShm>();
            std::ptr::write_bytes(view.Value.cast::<u8>(), 0, CURSOR_SHM_SIZE);
            // Magic last: the driver validates it at adopt. Seq 0 is even = consistent.
            std::sync::atomic::fence(Ordering::Release);
            (*shm).magic = CURSOR_MAGIC;
            MappedSection { handle: map, view }
        };
        // Desktop origin of this monitor's source — for the desktop→frame coordinate shift.
        let rect = pf_win_display::win_display::source_desktop_rect(ccd);
        let origin = rect.map(|(x, y, _w, _h)| (x, y)).unwrap_or((0, 0));
        Ok(CursorShared {
            section,
            origin,
            cached_id: 0,
            cached: None,
        })
    }

    pub(super) fn section_handle(&self) -> HANDLE {
        HANDLE(self.section.handle.as_raw_handle())
    }

    /// Seqlock-read the latest publish as a frame-relative [`pf_frame::CursorOverlay`].
    /// `None` until the first publish. Hidden pointer → `Some` with `visible: false` — the
    /// forwarder turns that into the client's relative-mode hint, as on Linux.
    pub(super) fn read(&mut self) -> Option<pf_frame::CursorOverlay> {
        let shm = self.section.ptr::<CursorShm>();
        // SAFETY: the view spans `CURSOR_SHM_SIZE` for `self`'s lifetime; `seq` is
        // 4-aligned at offset 4 in the fixed layout.
        let seq = unsafe { &*std::ptr::addr_of!((*shm).seq).cast::<AtomicU32>() };
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None; // seq 0: no publish yet (even, but not a valid snapshot)
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue; // odd seq: writer mid-update
            }
            // SAFETY: header is inside the mapped view; a torn read is discarded by the
            // seq re-check below.
            let hdr = unsafe { std::ptr::read_volatile(shm) };
            if hdr.visible != 0 && hdr.shape_id != self.cached_id {
                let rows = hdr.height.min(CURSOR_SHAPE_MAX) as usize;
                let width = hdr.width.min(CURSOR_SHAPE_MAX) as usize;
                let pitch = (hdr.pitch as usize).min(CURSOR_SHAPE_BYTES / rows.max(1));
                let mut raw = vec![0u8; rows * pitch];
                // SAFETY: the shape region is `CURSOR_SHAPE_BYTES` from `CURSOR_SHAPE_OFFSET`
                // in the mapped view; `rows * pitch` is clamped to it above.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.section.ptr::<u8>().add(CURSOR_SHAPE_OFFSET),
                        raw.as_mut_ptr(),
                        rows * pitch,
                    );
                }
                // Writer raced mid-shape (seq moved) — retry without caching.
                if seq.load(Ordering::Acquire) != s1 {
                    continue;
                }
                self.cached = Some(convert_shape(&hdr, &raw, width, rows, pitch));
                self.cached_id = hdr.shape_id;
            } else if seq.load(Ordering::Acquire) != s1 {
                continue;
            }
            let shape = self.cached.as_ref()?;
            return Some(pf_frame::CursorOverlay {
                x: hdr.x - self.origin.0,
                y: hdr.y - self.origin.1,
                w: shape.w,
                h: shape.h,
                rgba: shape.rgba.clone(),
                serial: u64::from(hdr.shape_id),
                hot_x: shape.hot_x,
                hot_y: shape.hot_y,
                visible: hdr.visible != 0,
            });
        }
        None // writer wedged mid-seq; skip this tick
    }
}

/// Pack 32-bpp pitch-strided rows into straight RGBA. ALPHA is BGRA (swap R↔B).
/// MASKED_COLOR: `alpha == 0` is opaque color; `0xFF` is XOR, which we cannot honor
/// client-side — mid-gray so inversion cursors stay visible instead of vanishing.
fn convert_shape(
    hdr: &CursorShm,
    raw: &[u8],
    width: usize,
    rows: usize,
    pitch: usize,
) -> ConvertedShape {
    let masked = hdr.cursor_type == CURSOR_TYPE_MASKED_COLOR;
    let mut rgba = Vec::with_capacity(width * rows * 4);
    for y in 0..rows {
        let row = &raw[y * pitch..];
        for x in 0..width {
            let o = x * 4;
            if o + 4 > row.len() {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let (b, g, r, a) = (row[o], row[o + 1], row[o + 2], row[o + 3]);
            if masked {
                if a == 0 {
                    rgba.extend_from_slice(&[r, g, b, 0xFF]);
                } else {
                    rgba.extend_from_slice(&[0x80, 0x80, 0x80, 0xB4]);
                }
            } else {
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
    }
    ConvertedShape {
        rgba: std::sync::Arc::new(rgba),
        w: width as u32,
        h: rows as u32,
        hot_x: hdr.hot_x.min(width.saturating_sub(1) as u32),
        hot_y: hdr.hot_y.min(rows.saturating_sub(1) as u32),
    }
}
