//! Device-node plumbing shared by the uinput pads, the uinput pen, and the raw_gadget Deck:
//! `open` through std (CLOEXEC, errno kept), `ioctl` over typed references, and the
//! `<linux/uinput.h>` structs both uinput backends fill.
//!
//! Request numbers and layouts are the 64-bit kernel ABI (x86_64 and aarch64 agree).

use anyhow::{anyhow, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;

/// A type the kernel may read and overwrite whole: no padding, every bit pattern valid.
///
/// # Safety
/// Implement only for `#[repr(C)]` (or packed) aggregates of integers and integer arrays
/// with no padding bytes.
pub(crate) unsafe trait Pod {}

// SAFETY: a byte array has no padding and every bit pattern is a valid value.
unsafe impl<const N: usize> Pod for [u8; N] {}

/// The argument size an `_IOC` request number encodes.
const fn arg_size(req: libc::c_ulong) -> usize {
    ((req >> 16) & 0x3fff) as usize
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// `ioctl(fd, req, value)` for a request that takes its argument by value.
/// Panics on a request that encodes a copy out to the caller.
pub(crate) fn ioctl_value(
    fd: BorrowedFd<'_>,
    req: libc::c_ulong,
    value: libc::c_ulong,
) -> io::Result<libc::c_int> {
    assert!(
        req >> 31 == 0,
        "ioctl {req:#x}: the kernel writes through its argument"
    );
    // SAFETY: every caller's `req` reads `value` as an integer and writes through nothing;
    // `fd` is borrowed, so it stays open for the call.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), req as _, value) })
}

/// `ioctl(fd, req, arg)`: the kernel copies `arg` in, out, or both.
/// Panics unless `req` encodes `size_of::<T>()`.
pub(crate) fn ioctl_with<T: Pod>(
    fd: BorrowedFd<'_>,
    req: libc::c_ulong,
    arg: &mut T,
) -> io::Result<libc::c_int> {
    assert_eq!(
        arg_size(req),
        size_of::<T>(),
        "ioctl {req:#x}: argument size"
    );
    // SAFETY: `arg` is a live, unique `T` of exactly the size `req` makes the kernel copy,
    // and `Pod` makes any bytes it writes back a valid `T`.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), req as _, std::ptr::from_mut(arg)) })
}

/// Open a device node read-write and non-blocking; std adds `O_CLOEXEC`.
pub(crate) fn open_nonblock(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Open `/dev/uinput` for one virtual device, naming the remedy on failure.
pub(crate) fn open_uinput() -> Result<File> {
    open_nonblock("/dev/uinput").map_err(|e| {
        anyhow!(
            "open /dev/uinput: {e} (install the udev rule granting the 'input' group access \
             — see scripts/60-punktfunk.rules — and add the user to the 'input' group)"
        )
    })
}

pub(crate) const UI_DEV_CREATE: libc::c_ulong = 0x5501;
pub(crate) const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
pub(crate) const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
pub(crate) const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
pub(crate) const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
pub(crate) const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;

#[repr(C)]
pub(crate) struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
pub(crate) struct UinputSetup {
    pub id: InputId,
    pub name: [u8; 80],
    pub ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(crate) struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
pub(crate) struct UinputAbsSetup {
    pub code: u16,
    pub _pad: u16,
    pub absinfo: AbsInfo,
}

const _: () = {
    assert!(size_of::<UinputSetup>() == 92);
    assert!(size_of::<UinputAbsSetup>() == 28);
    assert!(size_of::<libc::input_event>() == INPUT_EVENT_LEN);
};

// SAFETY: `#[repr(C)]` integers and a byte array; the sizes above are the field sums, so
// neither struct has padding.
unsafe impl Pod for UinputSetup {}
// SAFETY: as `UinputSetup`.
unsafe impl Pod for UinputAbsSetup {}

/// `struct input_event`: a 16-byte `timeval` the kernel stamps, then type, code, value.
pub(crate) const INPUT_EVENT_LEN: usize = 24;

pub(crate) fn input_event(type_: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_LEN] {
    let mut ev = [0u8; INPUT_EVENT_LEN];
    ev[16..18].copy_from_slice(&type_.to_ne_bytes());
    ev[18..20].copy_from_slice(&code.to_ne_bytes());
    ev[20..24].copy_from_slice(&value.to_ne_bytes());
    ev
}

/// `(type, code, value)` of an event read back from the node.
pub(crate) fn parse_input_event(ev: &[u8; INPUT_EVENT_LEN]) -> (u16, u16, i32) {
    (
        u16::from_ne_bytes([ev[16], ev[17]]),
        u16::from_ne_bytes([ev[18], ev[19]]),
        i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_round_trips() {
        assert_eq!(
            parse_input_event(&input_event(0x15, 0x50, -7)),
            (0x15, 0x50, -7)
        );
    }

    #[test]
    fn request_sizes_match_their_structs() {
        assert_eq!(arg_size(UI_DEV_SETUP), size_of::<UinputSetup>());
        assert_eq!(arg_size(UI_ABS_SETUP), size_of::<UinputAbsSetup>());
    }
}
