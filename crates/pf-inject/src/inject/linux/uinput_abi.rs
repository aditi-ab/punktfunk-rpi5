//! `/dev/uinput` ABI (`linux/uinput.h`) shared by the uinput devices: the ioctl numbers, the
//! `#[repr(C)]` setup structs and [`UinputDevice`], which owns the fd and destroys the device
//! on drop. Capabilities (keys, axes, FF, props) are each device's own data.
//!
//! The numbers are the generic Linux ioctl encoding, the same on x86_64 and arm64; the
//! `size_of` asserts pin the struct sizes they encode. `/dev/uinput` needs the udev rule and
//! the `input` group (`scripts/60-punktfunk.rules`).

use anyhow::{bail, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(crate) const UI_DEV_CREATE: libc::c_ulong = 0x5501;
pub(crate) const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
pub(crate) const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
pub(crate) const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
pub(crate) const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
pub(crate) const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
pub(crate) const UI_SET_FFBIT: libc::c_ulong = 0x4004_556b;
pub(crate) const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const SYN_REPORT: u16 = 0;

#[repr(C)]
pub(crate) struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
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
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct InputEventRaw {
    pub time: libc::timeval,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

// `<linux/uinput.h>` sizes, the ones the ioctl numbers above encode.
const _: () = {
    assert!(std::mem::size_of::<UinputSetup>() == 92);
    assert!(std::mem::size_of::<UinputAbsSetup>() == 28);
    assert!(std::mem::size_of::<InputEventRaw>() == 24);
};

fn ioctl_int(fd: RawFd, req: libc::c_ulong, arg: libc::c_int, what: &str) -> Result<()> {
    // SAFETY: callers pass UI_SET_*/UI_DEV_CREATE/UI_DEV_DESTROY — integer ioctls whose third
    // arg the kernel takes BY VALUE, so nothing is dereferenced through `arg`. `fd` is the live
    // `/dev/uinput` fd; a stale fd returns EBADF, not UB.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn ioctl_ptr<T>(fd: RawFd, req: libc::c_ulong, arg: *mut T, what: &str) -> Result<()> {
    // SAFETY: `fd` is the caller's live `/dev/uinput` fd. Call sites pass `&mut x` for a
    // uniquely-borrowed `#[repr(C)]` `T` whose size matches the request (`UI_DEV_SETUP`
    // 0x405c_5503 → 0x5c=92; `UI_ABS_SETUP` → 0x1c=28; FF upload/erase → 0x68/0x0c — pinned
    // by the `size_of` asserts). The kernel copies that many bytes; the `&mut` lives for
    // the whole synchronous call.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// One `/dev/uinput` device. Set its capabilities, then [`create`](Self::create); drop sends
/// `UI_DEV_DESTROY` before the fd closes.
pub(crate) struct UinputDevice {
    fd: OwnedFd,
}

impl UinputDevice {
    /// Open `/dev/uinput` non-blocking, so a read drains the FF queue without waiting.
    pub(crate) fn open() -> Result<UinputDevice> {
        // SAFETY: `c"/dev/uinput"` is a 'static NUL-terminated C string; `open` reads it as a
        // path, returns a fresh fd (or -1) and retains nothing.
        let raw = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            bail!(
                "open /dev/uinput: {} (install the udev rule granting the 'input' group access \
                 — see scripts/60-punktfunk.rules — and add the user to the 'input' group)",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: `raw >= 0` (the `< 0` branch already bailed). The fd is freshly opened and
        // not stored elsewhere; `OwnedFd` becomes its unique owner and closes it once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(UinputDevice { fd })
    }

    /// Enable each of `codes` with one `UI_SET_*BIT` request.
    pub(crate) fn set_bits(&self, req: libc::c_ulong, what: &str, codes: &[u16]) -> Result<()> {
        for &code in codes {
            ioctl_int(
                self.raw_fd(),
                req,
                code.into(),
                &format!("{what}({code:#x})"),
            )?;
        }
        Ok(())
    }

    pub(crate) fn abs(&self, code: u16, absinfo: AbsInfo) -> Result<()> {
        let mut a = UinputAbsSetup {
            code,
            _pad: 0,
            absinfo,
        };
        ioctl_ptr(self.raw_fd(), UI_ABS_SETUP, &mut a, "UI_ABS_SETUP")
    }

    /// `UI_DEV_SETUP` then `UI_DEV_CREATE`. `name` is truncated to the 79 bytes the setup holds.
    pub(crate) fn create(&self, id: InputId, name: &[u8], ff_effects_max: u32) -> Result<()> {
        let mut setup = UinputSetup {
            id,
            name: [0; 80],
            ff_effects_max,
        };
        let n = name.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&name[..n]);
        ioctl_ptr(self.raw_fd(), UI_DEV_SETUP, &mut setup, "UI_DEV_SETUP")?;
        ioctl_int(self.raw_fd(), UI_DEV_CREATE, 0, "UI_DEV_CREATE")
    }

    /// Best-effort: a full kernel queue drops the event, and the next frame re-syncs state.
    pub(crate) fn emit(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEventRaw {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        // SAFETY: `self.fd` is live for the call. `write` READS `size_of::<InputEventRaw>()`
        // initialized bytes from local `ev` (`#[repr(C)]` all-integer, no padding, size 24) and
        // retains nothing past return.
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &ev as *const _ as *const libc::c_void,
                std::mem::size_of::<InputEventRaw>(),
            )
        };
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is still live here (`OwnedFd` closes only after this `drop`
        // returns). UI_DEV_DESTROY takes 0 BY VALUE, so nothing is dereferenced.
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}
