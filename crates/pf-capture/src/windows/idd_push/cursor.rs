//! Host side of the v5 hardware-cursor channel (remote-desktop-sweep M2c): the capturer creates
//! an unnamed [`CursorShm`] section, delivers it to the pf-vdisplay driver (which declares an
//! IddCx hardware cursor — DWM then EXCLUDES the pointer from the frames we consume), and reads
//! the driver's seqlock publishes here at encode-tick pace, converting them into the same
//! [`pf_frame::CursorOverlay`] the Linux portal path produces — everything downstream (the
//! cursor forwarder, the wire, the client renderer) is shared.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::*;
use pf_driver_proto::cursor::{
    CursorShm, CURSOR_MAGIC, CURSOR_SHAPE_BYTES, CURSOR_SHAPE_MAX, CURSOR_SHAPE_OFFSET,
    CURSOR_SHM_SIZE, CURSOR_TYPE_MASKED_COLOR,
};
use std::sync::atomic::AtomicU32;

/// The host end of one monitor's cursor channel: the section (we created it — the mapping stays
/// valid for the capturer's life) plus the reader's conversion cache.
pub(super) struct CursorShared {
    section: MappedSection,
    /// The monitor's desktop origin — IddCx reports positions in DESKTOP coordinates; the
    /// overlay wants frame-relative. Fetched at attach (the virtual monitor's placement is
    /// stable for the session; a topology change recreates the pipeline anyway).
    origin: (i32, i32),
    /// Conversion cache: the last `shape_id` whose pixels were converted, and the result.
    /// Position-only updates (the common case) reuse it — a refcount bump, no pixel work.
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
    pub(super) fn create(target_id: u32) -> Result<CursorShared> {
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
            // Magic LAST-ish (the driver validates it at adopt; seq 0 = even = consistent).
            std::sync::atomic::fence(Ordering::Release);
            (*shm).magic = CURSOR_MAGIC;
            MappedSection { handle: map, view }
        };
        // Desktop origin of this monitor's source — for the desktop→frame coordinate shift.
        let rect = pf_win_display::win_display::source_desktop_rect(target_id);
        let origin = rect.map(|(x, y, _w, _h)| (x, y)).unwrap_or((0, 0));
        Ok(CursorShared {
            section,
            origin,
            cached_id: 0,
            cached: None,
        })
    }

    /// The section handle for the broker's duplication into the WUDFHost.
    pub(super) fn section_handle(&self) -> HANDLE {
        HANDLE(self.section.handle.as_raw_handle())
    }

    /// Seqlock-read the driver's latest publish → a frame-relative [`pf_frame::CursorOverlay`].
    /// `None` until the first publish lands (or while the pointer has never been seen). A hidden
    /// pointer returns `Some` with `visible: false` — the forwarder turns that into the client's
    /// relative-mode hint, exactly like the Linux path.
    pub(super) fn read(&mut self) -> Option<pf_frame::CursorOverlay> {
        let shm = self.section.ptr::<CursorShm>();
        // SAFETY: the view spans CURSOR_SHM_SIZE for self's lifetime; seq is 4-aligned in the
        // fixed layout (offset 4).
        let seq = unsafe { &*std::ptr::addr_of!((*shm).seq).cast::<AtomicU32>() };
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None; // no publish yet
            }
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue; // writer mid-update
            }
            // SAFETY: header reads within the mapped view; consistency is validated by the
            // seq re-check below (a torn read is discarded and retried).
            let hdr = unsafe { std::ptr::read_volatile(shm) };
            // Shape pixels: convert only when the OS minted a new shape id.
            if hdr.visible != 0 && hdr.shape_id != self.cached_id {
                let rows = hdr.height.min(CURSOR_SHAPE_MAX) as usize;
                let width = hdr.width.min(CURSOR_SHAPE_MAX) as usize;
                let pitch = (hdr.pitch as usize).min(CURSOR_SHAPE_BYTES / rows.max(1));
                let mut raw = vec![0u8; rows * pitch];
                // SAFETY: the shape region spans CURSOR_SHAPE_BYTES from CURSOR_SHAPE_OFFSET
                // inside the mapped view; `rows * pitch` is clamped to it above.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.section.ptr::<u8>().add(CURSOR_SHAPE_OFFSET),
                        raw.as_mut_ptr(),
                        rows * pitch,
                    );
                }
                // Discard the copy if the writer raced us mid-shape (seq moved) — retry.
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
        None // persistent tearing (writer wedged mid-seq) — skip this tick
    }
}

/// Convert the OS's 32-bpp pitch-strided shape rows into the overlay's packed straight RGBA.
/// ALPHA cursors are BGRA with straight per-pixel alpha (swap R↔B). MASKED_COLOR approximates:
/// alpha 0x00 = opaque color pixel; 0xFF = an XOR pixel we cannot honor client-side — rendered
/// as a translucent mid-gray so inversion cursors stay visible instead of vanishing.
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
