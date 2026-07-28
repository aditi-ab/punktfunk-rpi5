//! The audited unsafe-primitive layer under the punktfunk UMDF gamepad drivers (`pf-xusb`,
//! `pf-gamepad`).
//!
//! A UMDF driver cannot be literally free of `unsafe` — WDF dispatch, Win32 section mapping and
//! cross-process shared memory are FFI by nature. What Rust *can* buy is confining every raw
//! operation to one small, reviewed layer with explicit contracts, so the drivers' business logic
//! (the sealed-channel state machine, report plumbing, IOCTL policy) is **100 % safe code** and a
//! memory-safety bug can only live in this crate. Three modules:
//!
//! * [`section`] — [`section::MappedView`]: bounds- and alignment-checked access to a mapped shared
//!   section (atomics for the cross-process sync fields), plus the leaked-view [`section::ViewCell`].
//! * [`channel`] — [`channel::ChannelClient`]: the sealed pad channel's driver side
//!   (`design/gamepad-channel-sealing.md`), a **`#[forbid(unsafe_code)]` module** — the entire
//!   handshake/validation/adoption state machine is safe Rust over [`section`]'s API.
//! * [`wdf`] — [`wdf::Request`] + queue/device-property helpers: each framework callback converts
//!   its raw `WDFREQUEST` into a token exactly once (`unsafe`, with the framework's validity as the
//!   contract); everything after that is safe.
//! * [`hid`] — the HID minidrivers' **channel proof** answer (also `#[forbid(unsafe_code)]`): how
//!   `pf_gamepad`/`pf_mouse` tell the host, over the device stack, which process is serving this
//!   devnode. That is what the host trusts instead of the LocalService-writable bootstrap mailbox.
//!
//! Lint gates (mirrored in every driver crate, enforced by the drivers CI clippy step):
//! `unsafe_op_in_unsafe_fn` + `clippy::undocumented_unsafe_blocks` — every remaining `unsafe {}`
//! must carry a `// SAFETY:` proof.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod channel;
pub mod hid;
pub mod section;
pub mod wdf;

/// `NT_SUCCESS` — an NTSTATUS is an error iff negative.
#[inline]
#[must_use]
pub const fn nt_success(status: wdk_sys::NTSTATUS) -> bool {
    status >= 0
}
