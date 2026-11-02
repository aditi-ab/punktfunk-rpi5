//! A process the host duplicates handles into: the driver's WUDFHost or a capture worker.
//!
//! [`RemoteProcess`] is the one duplicator both use, and the [`HandleTarget`] a remote
//! source's encoder plants its AU section through. A value it returns is valid only in that
//! process; the far side owns it once the request that carries it succeeds, and the host reaps
//! it otherwise. Evidence: `design/idd-push-security.md`.

use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS,
    HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{GetCurrentProcess, WaitForSingleObject};

use super::section;
use super::transport::{HandleTarget, Planted};

/// A process handle with the duplicate and synchronize rights its opener holds.
pub(crate) struct RemoteProcess {
    handle: OwnedHandle,
    /// The process as an error names it.
    what: &'static str,
}

impl RemoteProcess {
    pub(crate) fn new(handle: OwnedHandle, what: &'static str) -> Self {
        Self { handle, what }
    }

    /// The live handle, for a call this type does not wrap.
    pub(crate) fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// The process has exited: a 0 ms wait on its handle.
    pub(crate) fn exited(&self) -> bool {
        // SAFETY: `handle` is this value's live handle; a 0 ms wait only reads its state.
        unsafe { WaitForSingleObject(self.raw(), 0) == WAIT_OBJECT_0 }
    }

    /// Duplicate `h` into the process. `Some(rights)` grants exactly those rights; `None`
    /// copies the source (`DUPLICATE_SAME_ACCESS`).
    pub(crate) fn dup_into(&self, h: BorrowedHandle<'_>, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is borrowed, so live; `handle` is the live target process with the
        // duplicate right; `&mut out` is a valid out-param. Explicit mask (options == 0) or
        // `DUPLICATE_SAME_ACCESS` (desired ignored) — never both.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                HANDLE(h.as_raw_handle()),
                self.raw(),
                &mut out,
                desired,
                false,
                options,
            )
        }
        .with_context(|| format!("DuplicateHandle into {}", self.what))?;
        Ok(out.0 as usize as u64)
    }

    /// Close a handle VALUE in the process's table. `DUPLICATE_CLOSE_SOURCE` with no target
    /// closes the source; the result is ignored.
    pub(crate) fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `handle` is the live target process; `value` is a handle this host created
        // there that the process never adopted. Closing it touches no other process, and fails
        // harmlessly once the process is gone.
        unsafe {
            let _ = DuplicateHandle(
                self.raw(),
                HANDLE(value as usize as *mut core::ffi::c_void),
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }
}

impl HandleTarget for RemoteProcess {
    fn plant(&self, bytes: usize) -> Result<Planted> {
        section::plant(self, bytes)
    }

    fn close_remote(&self, value: u64) {
        RemoteProcess::close_remote(self, value);
    }
}
