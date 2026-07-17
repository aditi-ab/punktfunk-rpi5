//! The sealed frame channel's handle-duplication broker (plan §W4, carved out of the IDD-push
//! capturer): duplicates the unnamed shared header / ring / event handles into the driver's WUDFHost
//! and delivers them as bare handle values over the SYSTEM-only control device.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::*;

/// The sealed channel's handle-duplication broker (`design/idd-push-security.md`): the frame objects
/// are unnamed, so the ONLY way the driver can reach them is handles this broker duplicates into its
/// WUDFHost process and delivers — as bare handle VALUES — over the SYSTEM-only control device
/// (`IOCTL_SET_FRAME_CHANNEL`). Ownership is a strict hand-off: on IOCTL success the DRIVER owns the
/// duplicates (it closes them); on any failure [`Self::send`] reaps every duplicate it already made
/// (`DUPLICATE_CLOSE_SOURCE`), so a half-delivered channel never leaks handles in WUDFHost.
pub(super) struct ChannelBroker {
    /// `PROCESS_DUP_HANDLE | SYNCHRONIZE` handle to the driver's WUDFHost (pid from the ADD reply;
    /// `ProcessSharingDisabled` makes that process exclusively pf-vdisplay's). `SYNCHRONIZE` lets the
    /// handle double as the driver-death probe ([`Self::driver_alive`]).
    process: OwnedHandle,
    /// The WUDFHost pid `process` refers to (diagnostics for the driver-death bail).
    pub(super) wudf_pid: u32,
    /// Delivers a filled `SetFrameChannelRequest` to the pf-vdisplay driver
    /// (`IOCTL_SET_FRAME_CHANNEL`). The host facade builds this from the vdisplay control device +
    /// `send_frame_channel` IOCTL wrapper, so this crate delivers the channel without reaching into
    /// the orchestrator's `vdisplay` module (plan §W6). Called once per generation, never per-frame.
    sender: crate::FrameChannelSender,
}

impl ChannelBroker {
    /// Open the duplication target. Fails when the driver predates the sealed channel (`wudf_pid == 0`
    /// can't survive the v2 version handshake, but guard anyway) or the WUDFHost is gone (device
    /// restart mid-open) — either way the caller fails the capture open cleanly.
    ///
    /// `wudf_pid` comes from the driver's ADD reply, so before we duplicate whole-desktop frame handles
    /// INTO it we VERIFY it is a genuine system WUDFHost ([`verify_is_wudfhost`]). Without that check a
    /// spoofed devnode (same interface GUID) could name an arbitrary process and receive the frames; a
    /// fully-compromised REAL pf_vdisplay driver is already a frame endpoint, so this specifically closes
    /// the reachable-without-owning-the-driver case (`design/idd-push-security.md` §hardening).
    pub(super) fn open(wudf_pid: u32, sender: crate::FrameChannelSender) -> Result<Self> {
        if wudf_pid == 0 {
            bail!("driver reported no WUDFHost pid for the frame channel");
        }
        // SAFETY: plain FFI; `wudf_pid` is a copy. The handle (checked by `?`) is owned solely here and
        // moved into the `OwnedHandle` (single owner, closes on drop); `verify_is_wudfhost` borrows it
        // for the duration of the synchronous check and forms no lasting alias.
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

    /// Whether the driver's WUDFHost is still alive. The pinned process handle doubles as the
    /// liveness probe (`SYNCHRONIZE` requested at open): signaled ⇔ the process exited. This is the
    /// definitive "driver died mid-session" signal — at the ring, a dead driver and an idle desktop
    /// are indistinguishable (both simply stop publishing).
    pub(super) fn driver_alive(&self) -> bool {
        // SAFETY: `process` is the live `OwnedHandle` this broker owns (borrowed for this synchronous
        // call); a 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) != WAIT_OBJECT_0 }
    }

    /// Duplicate `h` into the WUDFHost handle table, returning the handle VALUE valid there (and only
    /// there — the value is meaningless in any other process). `access = Some(rights)` grants the
    /// driver's handle exactly those rights (least privilege — see [`SECTION_MAP_RW`]);
    /// `access = None` copies the source handle's access (`DUPLICATE_SAME_ACCESS`), used only where the
    /// source is already scoped (the DXGI shared-texture handles, minted by `CreateSharedHandle` with
    /// just `DXGI_SHARED_RESOURCE_READ|WRITE`).
    ///
    /// # Safety
    /// `h` must be a live handle of the current process.
    unsafe fn dup_into(&self, h: HANDLE, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is live per the contract; `self.process` is the live PROCESS_DUP_HANDLE target;
        // `&mut out` is a valid out-param. Either an explicit least-privilege access mask (options == 0)
        // or `DUPLICATE_SAME_ACCESS` (desired ignored) — never both.
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

    /// Close a handle VALUE inside the WUDFHost table (the failure-path reaper): `DUPLICATE_CLOSE_SOURCE`
    /// with no target closes the source handle regardless of the (ignored) result.
    fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `self.process` is the live duplication target and `value` is a handle value THIS
        // broker just created in that process's table (callers only pass back `dup_into` results the
        // driver never received); closing it there cannot touch any other process's handles.
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

    /// Duplicate the whole ring (header + event + every slot texture) into WUDFHost and deliver the
    /// values via `IOCTL_SET_FRAME_CHANNEL`. All-or-nothing: on any failure every duplicate already
    /// made is reaped remotely and an error returns (the caller fails the open / logs the recreate).
    /// The ownership contract with the driver is adopt-on-success only — it closes the handles iff the
    /// IOCTL succeeded, we reap them iff it didn't, so no value is ever closed twice.
    ///
    /// # Safety
    /// `header` and `event` must be live handles of the current process (the capturer's own section +
    /// event, borrowed for this synchronous call).
    pub(super) unsafe fn send(
        &self,
        target_id: u32,
        generation: u32,
        header: HANDLE,
        event: HANDLE,
        slots: &[HostSlot],
    ) -> Result<()> {
        debug_assert!(slots.len() <= control::RING_LEN_USIZE);
        let mut req = control::SetFrameChannelRequest {
            target_id,
            generation,
            ring_len: slots.len() as u32,
            _pad: 0,
            header_handle: 0,
            event_handle: 0,
            texture_handles: [0; control::RING_LEN_USIZE],
        };
        // SAFETY: `header`/`event` are live per this fn's contract; each slot's `shared` is the live
        // `OwnedHandle` the slot keeps for exactly this purpose.
        let result = unsafe { self.duplicate_and_deliver(&mut req, header, event, slots) };
        if result.is_err() {
            // The driver never adopted the delivery — reap every remote duplicate so nothing lingers.
            self.close_remote(req.header_handle);
            self.close_remote(req.event_handle);
            for v in req.texture_handles {
                self.close_remote(v);
            }
        }
        result
    }

    /// The fallible middle of [`Self::send`]: fill `req` with fresh duplicates, then issue the IOCTL.
    /// Split out so `send` can reap whatever landed in `req` when any step errors.
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
        // SAFETY: forwarded from the caller's contract — `header`/`event`/each `slot.shared` are live
        // handles of this process. The `sender` closure encapsulates the manager's control handle +
        // the `send_frame_channel` IOCTL (its precondition — a live control handle — is upheld by the
        // host facade that built it).
        unsafe {
            // Least privilege per handle: the header maps read/write, the event is only signalled, and
            // the textures keep their already-scoped `CreateSharedHandle` access (see `dup_into`).
            req.header_handle = self.dup_into(header, Some(SECTION_MAP_RW))?;
            req.event_handle = self.dup_into(event, Some(EVENT_MODIFY_STATE))?;
            for (k, s) in slots.iter().enumerate() {
                req.texture_handles[k] = self.dup_into(HANDLE(s.shared.as_raw_handle()), None)?;
            }
            (self.sender)(req)
        }
    }
}
