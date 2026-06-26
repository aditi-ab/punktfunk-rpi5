//! Per-pad Windows resource RAII for the gamepad backends (DualSense / DualShock 4 / XUSB).
//!
//! Each virtual pad owns two OS resources: the named shared-memory section (+ its mapped view) the
//! `pf_dualsense`/`pf_xusb` driver reads, and the `SwDeviceCreate`'d software devnode the driver loads
//! on. Before this module, all three backends hand-rolled the same `CreateFileMappingW` +
//! `MapViewOfFile` and an identical `Drop` doing `SwDeviceClose` + `UnmapViewOfFile` + `CloseHandle` —
//! easy to drift or leak on an error path. [`Shm`] and [`SwDevice`] own those resources with RAII, so a
//! backend just holds them and the cleanup (and ordering) happens by construction.

use anyhow::{anyhow, Result};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{SwDeviceClose, HSWDEVICE};
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// A named, anonymous (pagefile-backed) shared section + its mapped read/write view, created with the
/// permissive `D:(A;;GA;;;WD)` SDDL the restricted-token driver needs to open it. RAII: drop unmaps the
/// view, then the [`OwnedHandle`] closes the section handle (in that order). Replaces the three backends'
/// hand-duplicated `CreateFileMappingW` + `MapViewOfFile` + manual `Drop`.
pub(super) struct Shm {
    /// Owns the section handle (closed on drop). Held only for ownership — never read after construction.
    _handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Shm {
    /// Create + zero a `size`-byte section named `name`, mapped read/write. The section handle is owned
    /// immediately, so any failure below (or the returned `Shm`'s drop) closes it.
    pub(super) fn create(name: &HSTRING, size: usize) -> Result<Shm> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL literal is valid; `psd` receives an OS-allocated descriptor (freed at process
        // exit — acceptable for a host-lifetime object).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;WD)"),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the SDDL above.
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                size as u32,
                PCWSTR(name.as_ptr()),
            )?
        };
        // SAFETY: `map` is a fresh section handle we own; take ownership immediately so that the early
        // return below (and the eventual drop) closes it. `map` (a `Copy` `HANDLE`) stays usable for the
        // `MapViewOfFile` borrow that follows — `from_raw_handle` only copies the inner pointer.
        let handle = unsafe { OwnedHandle::from_raw_handle(map.0) };
        // SAFETY: `map` is a valid section handle; map the whole thing read/write.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            // `handle` drops here → closes the section. No view to unmap.
            return Err(anyhow!("MapViewOfFile failed for {name}"));
        }
        // SAFETY: `view` points at `size` writable bytes (just mapped).
        unsafe { core::ptr::write_bytes(view.Value as *mut u8, 0, size) };
        Ok(Shm {
            _handle: handle,
            view,
        })
    }

    /// The mapped section's base pointer. Stable for the `Shm`'s lifetime (moving the `Shm` does not
    /// relocate the OS mapping — the view address is fixed by `MapViewOfFile`).
    pub(super) fn base(&self) -> *mut u8 {
        self.view.Value as *mut u8
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: `view` came from `MapViewOfFile`; unmap it BEFORE the `_handle` field closes the
        // section (struct fields drop only after this `Drop::drop` returns).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

/// A `SwDeviceCreate`'d software devnode; drop removes it via `SwDeviceClose`. Replaces the manual
/// `SwDeviceClose` each backend used to call in its `Drop`.
pub(super) struct SwDevice(HSWDEVICE);

impl SwDevice {
    pub(super) fn new(hsw: HSWDEVICE) -> Self {
        SwDevice(hsw)
    }
}

impl Drop for SwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle `SwDeviceCreate` returned; `SwDeviceClose` removes the devnode.
        unsafe { SwDeviceClose(self.0) };
    }
}
