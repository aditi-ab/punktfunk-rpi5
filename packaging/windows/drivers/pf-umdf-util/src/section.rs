//! Safe access to Win32 shared-memory sections: [`MappedView`] wraps a mapped view of a known
//! length and exposes bounds- and alignment-checked accessors, so callers never touch the raw base
//! pointer. Cross-process sync fields (seqs, pids, handle values) go through real atomics; bulk
//! report regions use plain unaligned copies, guarded by the channel protocol's seq fields — the
//! same access discipline the host side uses (`inject/windows/gamepad_raii.rs`).

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

const FILE_MAP_RW: u32 = 0x0002 | 0x0004; // FILE_MAP_WRITE | FILE_MAP_READ

// kernel32 file-mapping APIs (resolved via std's kernel32 import; UMDF permits file mapping).
unsafe extern "system" {
    fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn MapViewOfFile(h: *mut c_void, access: u32, hi: u32, lo: u32, len: usize) -> *mut c_void;
    fn UnmapViewOfFile(addr: *const c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

/// A read/write view over a mapped shared section of exactly `len` bytes. Every accessor
/// bounds-checks (and, for the atomic ones, alignment-checks) its offset, so no caller can read or
/// write outside the mapping — the offsets are `offset_of!` constants from `pf_driver_proto`, making
/// a failed check a compile-shaped logic bug (it aborts the WUDFHost rather than corrupting).
///
/// Concurrency: the peer process writes the section concurrently. Fields used for cross-process
/// synchronization must be accessed through the `load_*`/`store_*` atomic accessors; the bulk
/// byte/scalar accessors are plain unaligned accesses whose consistency is guarded by the channel
/// protocol (seq-fenced publishes), exactly as on the host side.
pub struct MappedView {
    base: *mut u8,
    len: usize,
}

// SAFETY: `MappedView` is a pointer + length over an OS mapping that stays valid until
// `UnmapViewOfFile` in `Drop` (or forever, once leaked into a `ViewCell`). All access goes through
// the checked accessors — atomics for shared sync fields, unaligned reads/writes for bulk data —
// none of which require a single-thread owner, so sharing/sending the view across the driver's
// callback threads is sound.
unsafe impl Send for MappedView {}
// SAFETY: `&MappedView` IS shared across threads for real — `ChannelState::data()` hands out
// `&'static MappedView`, and every driver using it dispatches its queue
// `WdfIoQueueDispatchParallel` with `NumberOfPresentedRequests = u32::MAX`, so two callbacks can
// hold it at once. What makes that sound is NOT that the accessors are individually race-free:
// `read_u8`/`write_u8`/`read_u16`/… are plain unaligned accesses through `&self` and would race if
// two threads hit the same offset. It is that these bytes are a section mapped into ANOTHER PROCESS
// that writes them concurrently, so no Rust-level exclusivity over them is achievable in the first
// place — the peer defeats it regardless of what this type does. Consistency is therefore the
// channel protocol's job (seq-fenced publishes, per the struct doc), the cross-process
// synchronization fields go through the `load_*`/`store_*` ATOMIC accessors, and each side reads
// only fields the protocol says are stable.
//
// ⚠ The rule this proof implies, for anything added later: a new accessor may use the plain path
// ONLY for bytes the protocol already fences. Anything that must be coherent between threads or
// between processes belongs in the atomic set — a plain accessor over such a field is a race the
// `Sync` here does not cover.
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
        Some(MappedView { base, len })
    }

    /// Map `len` bytes of a section from a raw handle VALUE (the sealed channel's delivery — a
    /// handle the host duplicated into this process). `None` if the value does not resolve to a
    /// mappable section. The handle itself is NOT consumed — the caller decides after validating
    /// the mapped content (see [`close_handle_value`]).
    pub fn from_handle_value(value: u64, len: usize) -> Option<MappedView> {
        if value == 0 {
            return None;
        }
        // SAFETY: `MapViewOfFile` on an arbitrary handle value is safe — it fails (returns null)
        // unless the value resolves to a section handle in this process's table with RW access.
        let base = unsafe { MapViewOfFile(value as usize as *mut c_void, FILE_MAP_RW, 0, 0, len) }
            as *mut u8;
        if base.is_null() {
            return None;
        }
        Some(MappedView { base, len })
    }

    /// How many bytes this view maps — the gate for tail-extension features (a caller may only
    /// touch offsets `< mapped_len()`; see `ChannelConfig::min_data_size`).
    pub fn mapped_len(&self) -> usize {
        self.len
    }

    /// Assert `off..off+n` is inside the view and, for atomics, `align`-aligned. The view base is
    /// page-aligned (`MapViewOfFile`), so field alignment reduces to offset alignment.
    #[inline]
    fn check(&self, off: usize, n: usize, align: usize) {
        assert!(
            off.is_multiple_of(align) && off.checked_add(n).is_some_and(|end| end <= self.len),
            "MappedView access out of bounds/alignment (off={off}, n={n}, len={})",
            self.len
        );
    }

    /// Atomic `u32` load at `off` (must be 4-aligned) — the cross-process sync accessor.
    #[inline]
    pub fn load_u32(&self, off: usize, order: Ordering) -> u32 {
        self.check(off, 4, 4);
        // SAFETY: `off` is in-bounds + 4-aligned per `check`, and the page-aligned mapping stays
        // valid while `&self` lives; an `AtomicU32` view over shared memory is the defined way to
        // race the peer process.
        unsafe { (*(self.base.add(off) as *const AtomicU32)).load(order) }
    }

    /// Atomic `u32` store at `off` (must be 4-aligned).
    #[inline]
    pub fn store_u32(&self, off: usize, v: u32, order: Ordering) {
        self.check(off, 4, 4);
        // SAFETY: as `load_u32` — in-bounds, aligned, valid for `&self`'s lifetime.
        unsafe { (*(self.base.add(off) as *const AtomicU32)).store(v, order) }
    }

    /// Atomic `u64` load at `off` (must be 8-aligned).
    #[inline]
    pub fn load_u64(&self, off: usize, order: Ordering) -> u64 {
        self.check(off, 8, 8);
        // SAFETY: as `load_u32`, with 8-byte size/alignment checked.
        unsafe { (*(self.base.add(off) as *const AtomicU64)).load(order) }
    }

    /// Plain byte read at `off` (bulk-region accessor — protocol-guarded, see the type docs).
    #[inline]
    pub fn read_u8(&self, off: usize) -> u8 {
        self.check(off, 1, 1);
        // SAFETY: in-bounds per `check`; a one-byte read cannot tear.
        unsafe { *self.base.add(off) }
    }

    /// Plain byte write at `off`.
    #[inline]
    pub fn write_u8(&self, off: usize, v: u8) {
        self.check(off, 1, 1);
        // SAFETY: in-bounds per `check`; a one-byte write cannot tear.
        unsafe { *self.base.add(off) = v }
    }

    /// Plain (unaligned) `u16` read at `off`.
    #[inline]
    pub fn read_u16(&self, off: usize) -> u16 {
        self.check(off, 2, 1);
        // SAFETY: in-bounds per `check`; `read_unaligned` has no alignment requirement.
        unsafe { core::ptr::read_unaligned(self.base.add(off) as *const u16) }
    }

    /// Plain (unaligned) `u32` read at `off` — the bulk-region accessor for a DATA-section scalar
    /// (host-written state / a driver-written publish counter; consistency comes from the channel
    /// protocol's seq fences, not from this access, exactly as on the host side).
    #[inline]
    pub fn read_u32(&self, off: usize) -> u32 {
        self.check(off, 4, 1);
        // SAFETY: in-bounds per `check`; `read_unaligned` has no alignment requirement.
        unsafe { core::ptr::read_unaligned(self.base.add(off) as *const u32) }
    }

    /// Plain (unaligned) `u32` write at `off` (bulk-region accessor).
    #[inline]
    pub fn write_u32(&self, off: usize, v: u32) {
        self.check(off, 4, 1);
        // SAFETY: in-bounds per `check`; `write_unaligned` has no alignment requirement.
        unsafe { core::ptr::write_unaligned(self.base.add(off) as *mut u32, v) }
    }

    /// Plain (unaligned) `i16` read at `off`.
    #[inline]
    pub fn read_i16(&self, off: usize) -> i16 {
        self.check(off, 2, 1);
        // SAFETY: in-bounds per `check`; `read_unaligned` has no alignment requirement.
        unsafe { core::ptr::read_unaligned(self.base.add(off) as *const i16) }
    }

    /// Copy `dst.len()` bytes out of the view starting at `off`.
    pub fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.check(off, dst.len(), 1);
        // SAFETY: the source range is in-bounds per `check`; `dst` is a live exclusive borrow of
        // `dst.len()` writable bytes and cannot overlap the foreign mapping.
        unsafe { core::ptr::copy_nonoverlapping(self.base.add(off), dst.as_mut_ptr(), dst.len()) }
    }

    /// Copy `src` into the view starting at `off`.
    pub fn write_bytes(&self, off: usize, src: &[u8]) {
        self.check(off, src.len(), 1);
        // SAFETY: the destination range is in-bounds per `check`; `src` is a live borrow that
        // cannot overlap the foreign mapping.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(off), src.len()) }
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

/// Close a raw handle VALUE owned by this process — the sealed channel's adopt-on-success step
/// (the mapped view keeps the section alive after the close). Closing a value that is not a live
/// handle of this process is a logic error the OS rejects (returns FALSE); it is not memory-unsafe.
pub fn close_handle_value(value: u64) {
    if value == 0 {
        return;
    }
    // SAFETY: `CloseHandle` validates the value against this process's handle table; no memory is
    // dereferenced through it.
    unsafe { CloseHandle(value as usize as *mut c_void) };
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
