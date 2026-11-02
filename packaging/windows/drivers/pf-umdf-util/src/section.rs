//! Safe access to Win32 shared-memory sections through a bounds-checked [`MappedView`].
//!
//! Synchronization fields use native-width atomics. A process-local mutex serializes potentially
//! overlapping access widths, and bulk report bytes use relaxed atomics; the channel's sequence
//! fences still provide aggregate consistency with the peer process. The raw accesses are
//! [`SectionView`]'s; this module maps, unmaps and locks.

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use pf_driver_proto::section::SectionView;
use std::sync::Mutex;

const FILE_MAP_RW: u32 = 0x0002 | 0x0004; // FILE_MAP_WRITE | FILE_MAP_READ

// kernel32 file-mapping APIs (resolved via std's kernel32 import; UMDF permits file mapping).
unsafe extern "system" {
    fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn MapViewOfFile(h: *mut c_void, access: u32, hi: u32, lo: u32, len: usize) -> *mut c_void;
    fn UnmapViewOfFile(addr: *const c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

/// A read/write view over exactly `len` bytes of a shared section.
///
/// Every accessor bounds-checks its range; native-width synchronization fields also check
/// alignment. A local lock prevents overlapping atomic widths across callbacks, while protocol
/// sequence fields provide acquire/release publication for each complete peer report.
pub struct MappedView {
    /// What `Drop` unmaps.
    base: *mut u8,
    /// Never copied out of this field, so its `'static` ends with `Drop`.
    view: SectionView<'static>,
    access: Mutex<()>,
}

// SAFETY: the mapping stays live until `Drop`; moving the unique wrapper cannot invalidate it.
unsafe impl Send for MappedView {}
// SAFETY: `access` serializes overlapping local widths, and each physical memory access is atomic.
// Compound reads may tear against the peer; sequence fields provide their aggregate consistency.
// `copy_from_slice` alone is a plain write: its single-writer protocol keeps readers off the range.
unsafe impl Sync for MappedView {}

impl MappedView {
    /// Open the named section `name` and map its first `len` bytes read/write. `None` if the name
    /// does not exist (e.g. the host is gone) or the mapping fails. The section handle is closed
    /// immediately — the view keeps the section alive.
    pub fn open_named(name: &str, len: usize) -> Option<MappedView> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string for the duration of the call.
        let h = unsafe { OpenFileMappingW(FILE_MAP_RW, 0, wide.as_ptr()) };
        if h.is_null() {
            return None;
        }
        // SAFETY: `h` is the valid mapping handle just opened; map `len` bytes read/write. The view
        // keeps the section alive, so the handle can be closed right away.
        let base = unsafe { MapViewOfFile(h, FILE_MAP_RW, 0, 0, len) } as *mut u8;
        // SAFETY: `h` is the valid handle from `OpenFileMappingW`, owned solely by this function.
        unsafe { CloseHandle(h) };
        if base.is_null() {
            return None;
        }
        Some(Self::adopt(base, len))
    }

    /// Map `len` bytes from a duplicated raw section-handle value without consuming the handle.
    ///
    /// Returns `None` when the value does not fit this target's pointer width or does not name a
    /// read/write mapping. The caller closes an accepted handle after validating its contents.
    pub fn from_handle_value(value: u64, len: usize) -> Option<MappedView> {
        let value = usize::try_from(value).ok().filter(|&value| value != 0)?;
        // SAFETY: `MapViewOfFile` rejects values that do not name an RW section handle in this
        // process. The checked conversion above prevents pointer-width truncation.
        let base =
            unsafe { MapViewOfFile(value as *mut c_void, FILE_MAP_RW, 0, 0, len) } as *mut u8;
        if base.is_null() {
            return None;
        }
        Some(Self::adopt(base, len))
    }

    /// Own the fresh view `MapViewOfFile` returned at `base`.
    fn adopt(base: *mut u8, len: usize) -> MappedView {
        // SAFETY: `base` is a new page-aligned read/write mapping of `len` bytes. Only `Drop`
        // unmaps it, after the last access, and no Rust reference points into it.
        let view = unsafe { SectionView::from_raw(base, len) };
        MappedView {
            base,
            view,
            access: Mutex::new(()),
        }
    }

    /// How many bytes this view maps — the gate for tail-extension features (a caller may only
    /// touch offsets `< mapped_len()`; see `ChannelConfig::min_data_size`).
    pub fn mapped_len(&self) -> usize {
        self.view.len()
    }

    /// Held across every access that goes through a fixed width, so two widths never overlap
    /// the same bytes at once in this process.
    #[inline]
    fn lock_access(&self) -> std::sync::MutexGuard<'_, ()> {
        self.access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Load one 4-aligned synchronization word with `order`.
    #[inline]
    pub fn load_u32(&self, off: usize, order: Ordering) -> u32 {
        let _access = self.lock_access();
        self.view.atomic_u32(off).load(order)
    }

    /// Atomic `u32` store at `off` (must be 4-aligned).
    #[inline]
    pub fn store_u32(&self, off: usize, v: u32, order: Ordering) {
        let _access = self.lock_access();
        self.view.atomic_u32(off).store(v, order);
    }

    /// Atomic `u64` load at `off` (must be 8-aligned).
    #[inline]
    pub fn load_u64(&self, off: usize, order: Ordering) -> u64 {
        let _access = self.lock_access();
        self.view.atomic_u64(off).load(order)
    }

    /// Read one bulk byte with relaxed atomic semantics.
    #[inline]
    pub fn read_u8(&self, off: usize) -> u8 {
        let _access = self.lock_access();
        self.view.atomic_u8(off).load(Ordering::Relaxed)
    }

    /// Write one bulk byte with relaxed atomic semantics.
    #[inline]
    pub fn write_u8(&self, off: usize, v: u8) {
        let _access = self.lock_access();
        self.view.atomic_u8(off).store(v, Ordering::Relaxed);
    }

    /// Relaxed `u16` load at `off` (must be 2-aligned): one instruction, so a peer's aligned
    /// store is seen whole — byte-wise reads can return a value neither side ever wrote.
    #[inline]
    pub fn read_u16(&self, off: usize) -> u16 {
        let _access = self.lock_access();
        self.view.atomic_u16(off).load(Ordering::Relaxed)
    }

    /// Relaxed `u32` load at `off` (must be 4-aligned); see [`Self::read_u16`].
    #[inline]
    pub fn read_u32(&self, off: usize) -> u32 {
        self.load_u32(off, Ordering::Relaxed)
    }

    /// Relaxed `u32` store at `off` (must be 4-aligned); see [`Self::read_u16`].
    #[inline]
    pub fn write_u32(&self, off: usize, v: u32) {
        self.store_u32(off, v, Ordering::Relaxed);
    }

    /// Relaxed `i16` load at `off` (must be 2-aligned); see [`Self::read_u16`].
    #[inline]
    pub fn read_i16(&self, off: usize) -> i16 {
        self.read_u16(off) as i16
    }

    /// Load `dst.len()` bulk bytes with relaxed atomic semantics.
    pub fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        let _access = self.lock_access();
        self.view.read_bytes(off, dst);
    }

    /// Store `src` as relaxed atomic bulk bytes.
    pub fn write_bytes(&self, off: usize, src: &[u8]) {
        let _access = self.lock_access();
        self.view.write_bytes(off, src);
    }

    /// A 4-aligned word as the atomic it is, for a protocol field the caller owns end to end
    /// (compare-exchange, Acquire/Release pairs). Bounds- and alignment-checked once; not under
    /// the local lock, so never mix it with the locked accessors on the same field.
    #[inline]
    pub fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        self.view.atomic_u32(off)
    }

    /// [`Self::atomic_u32`] for an 8-aligned u64.
    #[inline]
    pub fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        self.view.atomic_u64(off)
    }

    /// One `memcpy` of `src` into `off..`, for a region with a single writer whose readers only
    /// look after a later Release store names it (the AU heap). Bounds-checked once; the
    /// per-byte atomic path is [`Self::write_bytes`].
    pub fn copy_from_slice(&self, off: usize, src: &[u8]) {
        self.view.copy_from_slice(off, src);
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        // SAFETY: `base` is the live view from `MapViewOfFile`, unmapped exactly once (here).
        unsafe {
            UnmapViewOfFile(self.base as *const c_void);
        }
    }
}

/// Close one duplicated raw handle after a mapping adopts its section.
///
/// Zero or a value wider than this target's handle width is ignored. Other invalid values are
/// rejected by `CloseHandle`; no memory is dereferenced through them.
pub fn close_handle_value(value: u64) {
    let Ok(value) = usize::try_from(value) else {
        return;
    };
    if value == 0 {
        return;
    }
    // SAFETY: `CloseHandle` validates the pointer-width value against this process's handle table.
    unsafe { CloseHandle(value as *mut c_void) };
}

/// A lock-free cell holding the driver's adopted DATA view as a **leaked** `&'static MappedView`.
/// [`set`](Self::set) leaks the new view (and abandons the old one) instead of ever unmapping:
/// a concurrent framework callback may still be reading through a previously-returned reference, so
/// the mapping must never be torn down — a deliberate, bounded leak (one small view per delivery,
/// at most a handful per pad lifetime).
pub struct ViewCell(AtomicPtr<MappedView>);

impl Default for ViewCell {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewCell {
    pub const fn new() -> ViewCell {
        ViewCell(AtomicPtr::new(core::ptr::null_mut()))
    }

    /// The current view, if one was published. The `'static` lifetime is real: published views are
    /// leaked and never unmapped.
    pub fn get(&self) -> Option<&'static MappedView> {
        let p = self.0.load(Ordering::Acquire);
        // SAFETY: `p` is either null or a `Box::leak`ed `MappedView` published by `set`, which is
        // never dropped or unmapped — so the reference is valid for the process lifetime.
        (!p.is_null()).then(|| unsafe { &*p })
    }

    /// Publish `view`, leaking it (and abandoning — NOT freeing — any previous view; see the type
    /// docs for why the old mapping must stay alive).
    pub fn set(&self, view: MappedView) {
        let leaked: &'static mut MappedView = Box::leak(Box::new(view));
        self.0.swap(leaked, Ordering::Release);
    }
}
