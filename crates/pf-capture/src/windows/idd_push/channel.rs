//! Handle-duplication broker for the sealed IDD-push frame channel.
//!
//! Frame objects are unnamed. The driver reaches them only through handles this
//! broker duplicates into its WUDFHost and delivers as bare values over the
//! SYSTEM-only control device (`IOCTL_SET_FRAME_CHANNEL`). Evidence:
//! `design/idd-push-security.md`.
//!
//! Pin a generation: [`ChannelBroker::open`] then [`ChannelBroker::send`]. The
//! process handle is also the driver-death probe ([`ChannelBroker::driver_alive`]).

use super::*;

/// On IOCTL success the driver owns (and closes) the duplicates; on any failure
/// [`Self::send`] reaps every duplicate already made (`DUPLICATE_CLOSE_SOURCE`).
pub(super) struct ChannelBroker {
    /// WUDFHost process (`ProcessSharingDisabled` — exclusively pf-vdisplay's).
    /// `SYNCHRONIZE` doubles as the driver-death probe ([`Self::driver_alive`]).
    process: OwnedHandle,
    pub(super) wudf_pid: u32,
    /// `IOCTL_SET_FRAME_CHANNEL`. Once per generation, never per-frame.
    sender: crate::FrameChannelSender,
}

impl ChannelBroker {
    /// Open the WUDFHost duplication target. `wudf_pid == 0` is a driver that
    /// predates the sealed channel. [`verify_is_wudfhost`] runs before any
    /// desktop-frame handle is duplicated into it: a spoofed ADD pid (same
    /// interface GUID, different process) would otherwise receive the frames
    /// (`design/idd-push-security.md`).
    pub(super) fn open(wudf_pid: u32, sender: crate::FrameChannelSender) -> Result<Self> {
        if wudf_pid == 0 {
            bail!("driver reported no WUDFHost pid for the frame channel");
        }
        // SAFETY: `wudf_pid` is a copy. The handle (`?`-checked) is owned solely here and
        // moved into `OwnedHandle`; `verify_is_wudfhost` borrows it for this call and forms
        // no lasting alias.
        let process = unsafe {
            let h = OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                wudf_pid,
            )
            .context("OpenProcess(PROCESS_DUP_HANDLE) on the driver's WUDFHost")?;
            let process = OwnedHandle::from_raw_handle(h.0 as _);
            verify_is_wudfhost(HANDLE(process.as_raw_handle()), wudf_pid, "frame-channel")?;
            process
        };
        Ok(Self {
            process,
            wudf_pid,
            sender,
        })
    }

    /// `SYNCHRONIZE` wait: signaled ⇔ WUDFHost exited. At the ring a dead driver and an
    /// idle desktop both just stop publishing, so this is the only driver-death signal.
    pub(super) fn driver_alive(&self) -> bool {
        // SAFETY: `process` is this broker's live `OwnedHandle` (borrowed for the call); a
        // 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) != WAIT_OBJECT_0 }
    }

    /// Duplicate `h` into the WUDFHost table. The returned value is valid only there.
    /// `Some(rights)` grants exactly those rights; `None` copies the source
    /// (`DUPLICATE_SAME_ACCESS`) — used only for DXGI shared textures already scoped at
    /// `CreateSharedHandle` to `DXGI_SHARED_RESOURCE_READ|WRITE`.
    ///
    /// # Safety
    /// `h` must be a live handle of the current process.
    unsafe fn dup_into(&self, h: HANDLE, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is live per the contract; `self.process` is the live PROCESS_DUP_HANDLE
        // target; `&mut out` is a valid out-param. Explicit mask (options == 0) or
        // `DUPLICATE_SAME_ACCESS` (desired ignored) — never both.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                HANDLE(self.process.as_raw_handle()),
                &mut out,
                desired,
                false,
                options,
            )
        }
        .context("DuplicateHandle into the driver's WUDFHost")?;
        Ok(out.0 as usize as u64)
    }

    /// Duplicate a cursor section into WUDFHost with the same `SECTION_MAP_RW` as the
    /// frame header.
    ///
    /// # Safety
    /// `h` must be a live handle of the current process.
    pub(super) unsafe fn dup_into_public(&self, h: HANDLE) -> Result<u64> {
        // SAFETY: forwarded — `h` is live per this fn's contract.
        unsafe { self.dup_into(h, Some(SECTION_MAP_RW)) }
    }

    /// Failure-path reaper for a cursor-channel duplicate the driver never adopted.
    pub(super) fn close_remote_public(&self, value: u64) {
        self.close_remote(value);
    }

    /// Close a handle VALUE in the WUDFHost table. `DUPLICATE_CLOSE_SOURCE` with no
    /// target closes the source; the result is ignored.
    fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `self.process` is the live duplication target; `value` is a handle this
        // broker just created there and the driver never received. Closing it cannot touch
        // any other process's handles.
        unsafe {
            let _ = DuplicateHandle(
                HANDLE(self.process.as_raw_handle()),
                HANDLE(value as usize as *mut core::ffi::c_void),
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }

    /// Duplicate the ring into WUDFHost and deliver via `IOCTL_SET_FRAME_CHANNEL`.
    /// Adopt-on-success only: the driver closes the handles iff the IOCTL succeeded;
    /// we reap them iff it did not. No value is closed twice.
    ///
    /// # Safety
    /// `header` and `event` must be live handles of the current process, borrowed for
    /// this call.
    pub(super) unsafe fn send(
        &self,
        target_id: u32,
        generation: u32,
        header: HANDLE,
        event: HANDLE,
        slots: &[HostSlot],
    ) -> Result<()> {
        // Error, not `debug_assert`: a release-build panic inside `duplicate_and_deliver`
        // unwinds past the reap below and leaks every planted WUDFHost duplicate. Refuse
        // before the first duplication.
        if slots.len() > control::RING_LEN_USIZE {
            anyhow::bail!(
                "frame channel: {} ring slots exceeds the wire limit of {}",
                slots.len(),
                control::RING_LEN_USIZE
            );
        }
        let mut req = control::SetFrameChannelRequest {
            target_id,
            generation,
            ring_len: slots.len() as u32,
            _pad: 0,
            header_handle: 0,
            event_handle: 0,
            texture_handles: [0; control::RING_LEN_USIZE],
        };
        // SAFETY: `header`/`event` are live per this fn's contract; each slot's `shared` is
        // the live `OwnedHandle` the slot keeps for this duplication.
        let result = unsafe { self.duplicate_and_deliver(&mut req, header, event, slots) };
        if result.is_err() {
            self.close_remote(req.header_handle);
            self.close_remote(req.event_handle);
            for v in req.texture_handles {
                self.close_remote(v);
            }
        }
        result
    }

    /// Fill `req` with duplicates, then issue the IOCTL. Split out so [`Self::send`] can
    /// reap whatever landed in `req` on error.
    ///
    /// # Safety
    /// As [`Self::send`].
    unsafe fn duplicate_and_deliver(
        &self,
        req: &mut control::SetFrameChannelRequest,
        header: HANDLE,
        event: HANDLE,
        slots: &[HostSlot],
    ) -> Result<()> {
        // SAFETY: forwarded from the caller — `header`/`event`/each `slot.shared` are live
        // handles of this process. `sender`'s live control-handle precondition is upheld by
        // the host facade that built it.
        unsafe {
            req.header_handle = self.dup_into(header, Some(SECTION_MAP_RW))?;
            req.event_handle = self.dup_into(event, Some(EVENT_MODIFY_STATE))?;
            for (k, s) in slots.iter().enumerate() {
                req.texture_handles[k] = self.dup_into(HANDLE(s.shared.as_raw_handle()), None)?;
            }
            (self.sender)(req)
        }
    }
}
