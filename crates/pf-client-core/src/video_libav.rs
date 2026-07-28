//! Shared libav ownership helpers for the hardware decoders (`video_vaapi`, `video_vulkan`,
//! `video_d3d11`).
//!
//! The host has its own copy of this in `pf-encode`'s `enc/libav.rs`. The two crates do not depend
//! on each other — host encode and client decode share no code path — and the client's copy needs
//! something the host's does not ([`AvBuffer::into_raw`], for the decoder contexts that take
//! ownership of our ref), so a shared crate would have to carry an ffmpeg dependency and both sets
//! of semantics to save ~20 lines. It is not worth the edge.

use ffmpeg_next::ffi;

/// An owned `AVBufferRef`, unref'd exactly once when it drops.
///
/// Each decoder constructor creates a hwdevice and then does several more fallible things with it
/// — find the codec, alloc a context, open it — and every one of those failure branches used to
/// unref the device by hand, alongside a `Drop` doing it once more. Miss a branch and a decoder
/// device leaks per failed negotiation; double it up and the process aborts. Ownership lives here
/// instead, so an early `bail!` releases whatever exists and the branches carry no cleanup.
pub(crate) struct AvBuffer(*mut ffi::AVBufferRef);

impl AvBuffer {
    /// Take ownership of a freshly-created `AVBufferRef`, rejecting the null an ffmpeg allocator
    /// returns on failure.
    ///
    /// # Safety
    /// `p` must be null, or a live `AVBufferRef` whose ownership passes to the returned value —
    /// nothing else may unref it.
    pub(crate) unsafe fn from_raw(p: *mut ffi::AVBufferRef) -> Option<Self> {
        (!p.is_null()).then_some(AvBuffer(p))
    }

    /// The borrowed pointer, for calls that read the ref without taking it (e.g. `av_buffer_ref`,
    /// which makes its own).
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.0
    }

    /// Give up ownership: the caller becomes responsible for the unref.
    ///
    /// This exists for the `get_format` callbacks, which hand a frames context to the codec —
    /// `(*ctx).hw_frames_ctx = fr` means *the codec owns our ref now*, and it unrefs it when the
    /// context closes. Dropping an `AvBuffer` there as well would be the double-unref this type is
    /// meant to prevent, so the transfer is made explicit rather than left implicit.
    pub(crate) fn into_raw(self) -> *mut ffi::AVBufferRef {
        let p = self.0;
        std::mem::forget(self);
        p
    }
}

impl Drop for AvBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null ref `from_raw` took ownership of, and this type is its
        // sole owner (neither `Clone` nor `Copy`; `as_ptr` only lends, and `into_raw` forgets
        // instead of dropping), so this runs exactly once for that reference. `av_buffer_unref`
        // drops the one reference and nulls the pointer through the `&mut`.
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}
