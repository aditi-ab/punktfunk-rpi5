//! The virtual-display driver **seam**, carved out of the manager (plan §W3): the REMOVE-key type,
//! the `add_monitor` reply, and the IOCTL trait. It isolates the DRIVER's wire protocol from the
//! lifecycle — the refcount machine, linger, pinger and CCD/GDI glue are all driver-neutral in
//! [`super::VirtualDisplayManager`]. It was born as a two-backend seam (SudoVDA vs pf-vdisplay) and
//! has exactly one implementor since SudoVDA was removed: `crate::driver::PfVdisplayDriver` (the
//! flattened module name of `vdisplay/windows/pf_vdisplay.rs`). Kept as a trait because it is also
//! the only place the IOCTL surface can be faked, not because a second backend is expected.

use super::*;

/// The per-driver REMOVE key stamped on ADD and consumed on REMOVE. pf-vdisplay keys monitors by a
/// monotonic `u64` session id.
///
/// `Guid` is a RETAINED, UNUSED variant: it keyed SudoVDA's monitors (a fresh `GUID` per monitor) and
/// nothing constructs it since that backend was removed — the `else` arms in `pf_vdisplay`'s
/// `update_modes`/`remove_monitor` that reject it are therefore dead today. Left in place so the
/// enum still documents that the key is a per-driver choice rather than a `u64` by nature.
#[derive(Clone, Copy)]
pub(crate) enum MonitorKey {
    Guid(windows::core::GUID),
    Session(u64),
}

/// What a backend's `add_monitor` returns: the REMOVE key + the OS target id + the render LUID + the
/// driver's WUDFHost pid (the sealed frame channel's handle-duplication target) + the monitor id the
/// driver actually resolved (the per-client stable id when honored; diagnostics on the slot).
pub(crate) struct AddedMonitor {
    pub key: MonitorKey,
    pub target_id: u32,
    pub luid: LUID,
    pub wudf_pid: u32,
    pub resolved_monitor_id: u32,
    /// The driver reports the OS target already carries an IRREVOCABLE hardware-cursor declare
    /// from an earlier session (`AddReply::cursor_excluded`, remote-desktop-sweep §8.6): DWM
    /// excludes the pointer from this target's frames forever, so a session without the cursor
    /// channel must self-composite (GDI poller + blend) or stream a cursor-less desktop.
    pub cursor_excluded: bool,
}

/// The driver's IOCTL surface — everything else (the refcount machine, the linger, the pinger, the
/// CCD/GDI glue) is driver-neutral and shared in [`VirtualDisplayManager`]. `Send + Sync` because the
/// manager (and so the boxed driver) is a `&'static` singleton reached from the pinger + linger
/// threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// Find + open the control device, validate it (version handshake), and read the watchdog
    /// timeout. `reap_orphans` (the FIRST open of the process only) additionally `CLEAR_ALL`s
    /// monitors orphaned by a crashed previous host — a REOPEN (after a dead handle was retired)
    /// must NOT, since sessions this process still considers live may be racing it. Returns the
    /// owned handle + watchdog seconds + the driver's reported protocol version (the in-place
    /// resize gates on it).
    ///
    /// SAFE, and owning — unlike every other method here, which takes the raw `dev` handle. It has
    /// no caller obligation: it takes only a `bool`, opens the handle it then IOCTLs, and hands back
    /// an `OwnedHandle` that closes on drop. It used to be an `unsafe fn` whose `# Safety` section
    /// ("issues setup-API + `DeviceIoControl` calls; runs in the caller's apartment") restated what
    /// the body does rather than naming anything a caller could uphold — an un-checkable proof
    /// obligation at the one call site, which trains a reviewer to wave through the neighbouring
    /// blocks where the `dev` precondition is real.
    fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32, u32)>;
    /// ADD a virtual monitor at `mode`, pinning the IDD render GPU to `render_luid` first if `Some`, and
    /// requesting `preferred_monitor_id` (the host's per-client stable id; `0` = auto). `client_hdr`
    /// is the CLIENT display's HDR volume for the monitor's EDID CTA HDR block (`None` = the
    /// driver's built-in defaults). Returns the REMOVE key + target id + the IddCx DISPLAY adapter
    /// LUID from the ADD reply (`IDARG_OUT_MONITORARRIVAL.OsAdapterLuid` — NOT the render GPU; the
    /// driver reports its render adapter only in the shared frame header).
    ///
    /// # Safety
    /// `dev` must be the live control handle from [`open`](Self::open).
    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        hw_cursor: bool,
    ) -> Result<AddedMonitor>;
    /// Refresh the LIVE monitor `key`'s advertised mode list to lead with `mode` (the in-place
    /// mid-stream resize, latency plan P2 — pf-vdisplay `IOCTL_UPDATE_MODES`, driver protocol v4).
    /// The monitor is NOT departed; the caller CCD-forces the freshly-advertised mode afterwards.
    /// The default errs so a backend without support routes to the re-arrival fallback.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn update_modes(&self, dev: HANDLE, key: &MonitorKey, mode: Mode) -> Result<()> {
        let _ = (dev, key, mode);
        anyhow::bail!("backend does not support in-place mode updates")
    }
    /// REMOVE the monitor identified by `key`.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn remove_monitor(&self, dev: HANDLE, key: &MonitorKey) -> Result<()>;
    /// Watchdog keepalive PING (issued every `watchdog/3` from the pinger thread).
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn ping(&self, dev: HANDLE) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that implements nothing but the required methods — so the DEFAULTED `update_modes`
    /// is what gets called.
    struct FakeDriver;

    impl VdisplayDriver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn open(&self, _reap_orphans: bool) -> Result<(OwnedHandle, u32, u32)> {
            anyhow::bail!("fake driver has no control device")
        }
        unsafe fn add_monitor(
            &self,
            _dev: HANDLE,
            _mode: Mode,
            _render_luid: Option<LUID>,
            _preferred_monitor_id: u32,
            _client_hdr: Option<punktfunk_core::quic::HdrMeta>,
            _hw_cursor: bool,
        ) -> Result<AddedMonitor> {
            anyhow::bail!("fake driver adds no monitors")
        }
        unsafe fn remove_monitor(&self, _dev: HANDLE, _key: &MonitorKey) -> Result<()> {
            Ok(())
        }
        unsafe fn ping(&self, _dev: HANDLE) -> Result<()> {
            Ok(())
        }
    }

    /// The `update_modes` default must ERR, not silently succeed: `resize_in_place` treats `Ok(())`
    /// as "the driver refreshed the monitor's advertised mode list" and goes straight on to the CCD
    /// force-set + settle — so a default that returned `Ok` would burn the full 1.5 s settle against
    /// a mode list nobody updated, on every mid-stream resize, before falling back to the
    /// re-arrival it should have taken immediately.
    #[test]
    fn the_defaulted_update_modes_reports_not_supported() {
        let d = FakeDriver;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        // SAFETY: the defaulted `update_modes` discharges its `dev` obligation by never using it —
        // the body discards all three arguments and errs — so the null handle is never touched.
        let err = unsafe { d.update_modes(HANDLE::default(), &MonitorKey::Session(1), mode) }
            .expect_err("the default must not report success");
        assert!(
            err.to_string()
                .contains("does not support in-place mode updates"),
            "unexpected error text: {err:#}"
        );
    }
}
