//! The worker's mapping of the host's AU section, as the session's writer reaches it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64};

use pf_driver_proto::section;
use pf_encode_session::section::SectionView;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
};

/// `len` bytes of the section, mapped read/write until drop.
pub struct View {
    /// What `Drop` unmaps.
    base: *mut u8,
    /// Never copied out of this field, so its `'static` ends with `Drop`.
    bytes: section::SectionView<'static>,
}

// SAFETY: the pointers name a mapping this value owns for its whole life, and every access goes
// through atomics or the single-writer heap copy, so moving it between threads moves nothing
// thread-bound.
unsafe impl Send for View {}
// SAFETY: as above — shared access is atomics and one writer's `memcpy` into bytes no reader
// looks at before the publishing store.
unsafe impl Sync for View {}

impl View {
    /// Map `len` bytes of the section `handle` names. `None` when it names no read/write
    /// section of at least that size; the handle is left as it was either way.
    pub fn map(handle: HANDLE, len: usize) -> Option<Self> {
        // SAFETY: `MapViewOfFile` validates the handle against this process's table and fails
        // rather than faults on anything that is not a mappable section.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
        if view.Value.is_null() {
            return None;
        }
        let base = view.Value.cast::<u8>();
        // SAFETY: `base` is a new page-aligned read/write mapping of `len` bytes. Only `Drop`
        // unmaps it, after the last access, and no Rust reference points into it.
        let bytes = unsafe { section::SectionView::from_raw(base, len) };
        Some(Self { base, bytes })
    }
}

impl SectionView for View {
    fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        self.bytes.atomic_u32(off)
    }

    fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        self.bytes.atomic_u64(off)
    }

    fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.bytes.read_bytes(off, dst);
    }

    fn copy_from_slice(&self, off: usize, src: &[u8]) {
        self.bytes.copy_from_slice(off, src);
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let addr = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.base.cast::<c_void>(),
        };
        // SAFETY: `base` is the live view from `MapViewOfFile`, unmapped exactly once, here.
        let _ = unsafe { UnmapViewOfFile(addr) };
    }
}
