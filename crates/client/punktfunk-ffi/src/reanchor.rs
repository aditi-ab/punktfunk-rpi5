//! C wrapper for [`AuAdmission`]: one per elementary stream, every AU in receive order. The
//! freeze itself has no handle: a client reports through `punktfunk_connection_recovery_*`.

use crate::*;
use punktfunk_core::reanchor::{AuAdmission, DecoderClass};

/// [`punktfunk_au_admission_note`]'s `concealed`: the lane has no concealer.
pub const PUNKTFUNK_CONCEALED_NONE: u32 = 0;
/// The concealer left every reference on a picture the decoder holds.
pub const PUNKTFUNK_CONCEALED_DECODABLE: u32 = 1;
/// Nothing can stand in for the lost reference.
pub const PUNKTFUNK_CONCEALED_UNRECOVERABLE: u32 = 2;

/// Create an admission rule. Free with [`punktfunk_au_admission_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_au_admission_new() -> *mut AuAdmission {
    Box::into_raw(Box::default())
}

/// Free a rule created by [`punktfunk_au_admission_new`]. NULL is a no-op.
///
/// # Safety
/// `a` was returned by [`punktfunk_au_admission_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_au_admission_free(a: *mut AuAdmission) {
    guard_void(|| {
        if !a.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(a) });
        }
    });
}

/// Fold one AU: its frame index, the index gap ahead of it (0 for none), its wire flags,
/// whether the decoder is strict, and a `PUNKTFUNK_CONCEALED_*`. Writes whether to keep the
/// AU off the decoder and whether to ask for a keyframe. An unknown `concealed` returns
/// [`PunktfunkStatus::InvalidArg`].
///
/// # Safety
/// `a` is a valid handle; the out pointers are writable or NULL.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_au_admission_note(
    a: *mut AuAdmission,
    index: u32,
    gap: u32,
    flags: u32,
    strict: bool,
    concealed: u32,
    out_withhold: *mut bool,
    out_ask_keyframe: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut` never dereferences null.
        let Some(a) = (unsafe { a.as_mut() }) else {
            return PunktfunkStatus::NullPointer;
        };
        let verdict = match concealed {
            PUNKTFUNK_CONCEALED_NONE => None,
            PUNKTFUNK_CONCEALED_DECODABLE => Some(punktfunk_core::reanchor::Concealment::Decodable),
            PUNKTFUNK_CONCEALED_UNRECOVERABLE => {
                Some(punktfunk_core::reanchor::Concealment::Unrecoverable)
            }
            _ => return PunktfunkStatus::InvalidArg,
        };
        let class = if strict {
            DecoderClass::Strict
        } else {
            DecoderClass::Lenient
        };
        let step = a.note(index, gap, flags, class, verdict);
        // SAFETY: caller out-params, null or writable. `put` writes without reading, so C may
        // pass them uninitialised.
        unsafe {
            put(out_withhold, step.withhold);
            put(out_ask_keyframe, step.ask_keyframe);
        }
        PunktfunkStatus::Ok
    })
}
