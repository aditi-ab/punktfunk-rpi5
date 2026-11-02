//! The pagefile-backed sections of a pad channel and the DACLs they carry: the unnamed,
//! SYSTEM-only DATA section and the named mailbox SYSTEM and LocalService share.

use crate::pad_shm_ring::SectionView;
use crate::pad_slots::PadCreateFault;
use anyhow::{anyhow, Context, Result};
use pf_paths_win::SecurityAttributes;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Duration;
use windows::core::{HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE,
    INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// Pagefile-backed section plus its mapped RW view. Drop unmaps the view, then the
/// [`OwnedHandle`] closes the section. Unnamed = sealed DATA; named = bootstrap mailbox.
pub(crate) struct Shm {
    /// Duplication source for the sealed channel.
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    /// Bytes mapped at `view`.
    len: usize,
}

impl Shm {
    /// Unnamed `size`-byte section, mapped RW — the sealed DATA section.
    /// SDDL `D:P(A;;GA;;;SY)`: no name to open or squat; the driver gets a duplicated handle
    /// that carries this process's access without re-checking the DACL (`design/idd-push-security.md`).
    pub(crate) fn create_unnamed(size: usize) -> Result<Shm> {
        let sa = SecurityAttributes::new("D:P(A;;GA;;;SY)")?;
        // Unnamed: nothing to collide with, so the flag is always false.
        Self::create_inner(&sa, PCWSTR::null(), size)
            .map(|(shm, _)| shm)
            .context("create unnamed gamepad DATA section")
    }

    /// Named `size`-byte section, mapped RW — the bootstrap mailbox.
    /// SDDL `D:P(A;;GA;;;SY)(A;;GA;;;LS)`: SYSTEM plus LocalService (WUDFHost opens by name).
    ///
    /// `Global\` names are creatable by any `SeCreateGlobalPrivilege` holder. If the name
    /// already exists, `CreateFileMappingW` silently opens it (`ERROR_ALREADY_EXISTS`);
    /// close and retry, then fail rather than handshake through a foreign mailbox.
    /// A DACL we cannot open never reaches that branch: it is `ERROR_ACCESS_DENIED`,
    /// which [`classify_named_create_failure`] splits from "cannot create Global\\".
    pub(crate) fn create_named(name: &HSTRING, size: usize) -> Result<Shm> {
        // One descriptor for the squat-retry loop. `D:P` strips inherited ACEs so only
        // SYSTEM + LocalService are granted.
        let sa = SecurityAttributes::new("D:P(A;;GA;;;SY)(A;;GA;;;LS)")?;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: clearing the thread error slot so ERROR_ALREADY_EXISTS below is unambiguous.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let (shm, existed) = match Self::create_inner(&sa, PCWSTR(name.as_ptr()), size) {
                Ok(pair) => pair,
                Err(e) => return Err(classify_named_create_failure(name, e)),
            };
            // The verdict comes from the create itself, not from a re-read of the error slot
            // after the mapping call — and a section we merely opened was not zeroed.
            if !existed {
                return Ok(shm);
            }
            // `shm` drops here → unmap + close our handle to the foreign object, then retry.
        }
        Err(anyhow!(
            "bootstrap mailbox {name} already exists and stayed alive across retries — another \
             punktfunk-host instance is serving this pad index, or a local service is squatting the \
             name (gamepad DoS attempt?)"
        )
        .context(PadCreateFault::IndexOwnedElsewhere))
    }

    /// `true` in the tuple means `CreateFileMappingW` OPENED an existing section rather than
    /// creating one — the caller decides what to do about it. Read here, immediately after the
    /// create, because the mapping call below can clobber the thread error slot.
    fn create_inner(sa: &SecurityAttributes, name: PCWSTR, size: usize) -> Result<(Shm, bool)> {
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the caller's SDDL; the
        // descriptor behind `sa` outlives this call, and the section copies it.
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa.as_ptr()),
                PAGE_READWRITE,
                0,
                size as u32,
                name,
            )?
        };
        // SAFETY: read before anything else can touch the thread error slot.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: `map` is a fresh section handle we own; take ownership immediately so the early
        // return (and drop) closes it. `map` is `Copy`; `from_raw_handle` only copies the pointer.
        let handle = unsafe { OwnedHandle::from_raw_handle(map.0) };
        // SAFETY: `map` is a valid section handle; map the whole thing read/write.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            // `handle` drops here → closes the section. No view to unmap.
            return Err(anyhow!("MapViewOfFile failed"));
        }
        if !existed {
            // Only ours. Windows zero-fills a new pagefile-backed section, so this is belt —
            // but running it on a section we merely OPENED would wipe a live mailbox that
            // belongs to another host instance, which the caller is about to back away from.
            // SAFETY: `view` points at `size` writable bytes (just mapped).
            unsafe { core::ptr::write_bytes(view.Value as *mut u8, 0, size) };
        }
        Ok((
            Shm {
                handle,
                view,
                len: size,
            },
            existed,
        ))
    }

    /// The mapping, stable for this `Shm`'s lifetime (`MapViewOfFile` pins the address).
    pub(crate) fn view(&self) -> SectionView<'_> {
        // SAFETY: `MapViewOfFile` mapped `len` RW bytes at `view`, page-aligned; `Drop` unmaps
        // them only after this borrow ends. The shared bytes are only reached through the view.
        unsafe { SectionView::from_raw(self.view.Value.cast(), self.len) }
    }

    pub(super) fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: `view` came from `MapViewOfFile`; unmap it BEFORE the `handle` field closes the
        // section (struct fields drop only after this `Drop::drop` returns).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

/// Split `CreateFileMappingW`'s `ERROR_ACCESS_DENIED` into two causes:
///
/// * Name taken, DACL excludes us — creating over an existing name is an open.
///   Mailbox SDDL is SYSTEM + LocalService, so a LocalSystem host vs an elevated
///   Administrator console is this case, not `ERROR_ALREADY_EXISTS`.
/// * Name free, we cannot create `Global\` — needs `SeCreateGlobalPrivilege`.
///
/// `OpenFileMappingW` looks up before the access check: missing → `ERROR_FILE_NOT_FOUND`,
/// present-but-denied → `ERROR_ACCESS_DENIED`. The taken case carries
/// [`PadCreateFault`] so the pad manager does not tell the operator to reinstall.
fn classify_named_create_failure(name: &HSTRING, e: anyhow::Error) -> anyhow::Error {
    let denied = e
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|w| w.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
    if !denied {
        return e.context(format!("create gamepad bootstrap mailbox {name}"));
    }
    if named_section_exists(name) {
        return e
            .context(PadCreateFault::IndexOwnedElsewhere)
            .context(format!(
            "bootstrap mailbox {name} exists and belongs to a process this one may not open — a \
             live session's pad, held by the LocalSystem host service (its mailboxes grant SYSTEM \
             + LocalService only, so an Administrator console sees ACCESS_DENIED, not \
             ALREADY_EXISTS). Nothing is wrong with the drivers"
        ));
    }
    e.context(format!(
        "create gamepad bootstrap mailbox {name}: access denied although the name is FREE — this \
         process may not create Global\\ objects at all (that needs SeCreateGlobalPrivilege, which \
         SYSTEM and services hold and a user token does not)"
    ))
}

/// Whether a section with this name exists as seen from this process.
/// `true` also when the object is present but closed to us (ACCESS_DENIED on open): a
/// LocalSystem host's mailbox is closed to an Administrator console.
/// Picks a pad slot or an error text, never trust — a squatter can make this say either.
pub(crate) fn named_section_exists(name: &HSTRING) -> bool {
    // SAFETY: `name` is a live NUL-terminated UTF-16 string for the duration of the call. Ask for
    // the least access there is (`FILE_MAP_READ`): the handle is closed immediately and never
    // mapped — we want the lookup's verdict, not the object.
    let opened = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) };
    match opened {
        Ok(h) => {
            // SAFETY: `h` is the handle just opened here and referenced nowhere else.
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        // ERROR_FILE_NOT_FOUND (and anything else) reads as absent; ACCESS_DENIED is presence.
        Err(e) => e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
    }
}
