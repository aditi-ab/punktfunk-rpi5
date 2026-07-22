//! The backend-specific virtual-display **seam** (SudoVDA vs pf-vdisplay), carved out of the manager
//! (plan §W3): the REMOVE-key type, the `add_monitor` reply, and the IOCTL trait. This is the ONLY
//! thing that differs between the two Windows backends — the refcount machine, linger, pinger, and
//! CCD/GDI glue are all backend-neutral in [`super::VirtualDisplayManager`].

use super::*;

/// The per-backend REMOVE key the driver stamps on ADD and consumes on REMOVE. SudoVDA keys monitors by
/// a fresh `GUID`; pf-vdisplay keys them by a monotonic `u64` session id.
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

/// The backend-specific IOCTL surface — the *only* thing that differs between SudoVDA and pf-vdisplay.
/// Everything else (the refcount machine, the linger, the pinger, the CCD/GDI glue) is shared in
/// [`VirtualDisplayManager`]. `Send + Sync` because the manager (and so the boxed driver) is a
/// `&'static` singleton reached from the pinger + linger threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// Find + open the control device, validate it (version handshake), and read the watchdog
    /// timeout. `reap_orphans` (the FIRST open of the process only) additionally `CLEAR_ALL`s
    /// monitors orphaned by a crashed previous host — a REOPEN (after a dead handle was retired)
    /// must NOT, since sessions this process still considers live may be racing it. Returns the
    /// owned handle + watchdog seconds + the driver's reported protocol version (the in-place
    /// resize gates on it).
    ///
    /// # Safety
    /// Issues setup-API + `DeviceIoControl` calls; runs in the caller's apartment.
    unsafe fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32, u32)>;
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
