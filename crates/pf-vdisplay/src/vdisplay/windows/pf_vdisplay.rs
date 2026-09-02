//! Windows virtual-display backend for **pf-vdisplay**, punktfunk's IddCx Indirect Display Driver.
//!
//! [`create`](VirtualDisplay::create) adds a virtual monitor at the client's `WxH@Hz` (mode baked
//! into the ADD IOCTL; no EDID seeding), starts the watchdog ping, and the returned
//! [`VirtualOutput`]'s keepalive `Drop` removes it.
//!
//! Control surface: device-interface GUID + `CreateFileW` + `DeviceIoControl`. Wire contract is
//! [`pf_driver_proto::control`] (versioned `#[repr(C)] Pod` structs). See
//! `design/windows-host-rewrite.md`.
//!
//! Lifecycle, CCD isolation, and active-mode forcing live in [`super::manager`] and
//! `pf_win_display::win_display` — a pf-vdisplay `target_id` is a real OS target id. This module
//! owns only GUID, IOCTL codes, request/reply structs, and the version handshake.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SPINT_ACTIVE,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use pf_driver_proto::control;

use super::manager::{AddedMonitor, MonitorKey, VdisplayDriver};
use super::{Mode, VirtualDisplay, VirtualOutput};

// Own interface GUID (`PF_VDISPLAY_INTERFACE_GUID_U128`), not SudoVDA's `{e5bcc234-…}` —
// a private GUID is how we refuse to open a real SudoVDA install.
const PF_VDISPLAY_INTERFACE: GUID =
    GUID::from_u128(pf_driver_proto::PF_VDISPLAY_INTERFACE_GUID_U128);

/// Per-session `u64` for `IOCTL_ADD`/`IOCTL_REMOVE`. Collision safety lives in the host refcount
/// manager (a stale session cannot REMOVE a live one), so a monotonic counter is enough.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// One METHOD_BUFFERED `DeviceIoControl`. Empty `input`/`output` are allowed; `bytemuck` at the
/// call site.
///
/// # Safety
///
/// `h` must be a live pf-vdisplay control handle from [`open_device`]. Buffer pointers come from
/// the caller's slices, with those slices' lengths, and the slices outlive the call.
unsafe fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
    // SAFETY: `h` is a live control-device handle by this fn's contract. `inp`/`outp` are derived
    // from `input`/`output` and paired with those slices' lengths; both slices outlive the call.
    // METHOD_BUFFERED copies through a system buffer, so neither pointer is retained; `None`
    // OVERLAPPED makes the call synchronous.
    unsafe {
        DeviceIoControl(
            h,
            code,
            inp,
            input.len() as u32,
            outp,
            output.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .with_context(|| format!("DeviceIoControl(code={code:#x})"))?;
    Ok(returned)
}

/// Remove not-present "punktfunk" monitor PDOs that `IddCxMonitorDeparture` leaves behind.
/// Each ghost pins a VidPN target against IddCx's ~16-slot budget; once full, `IOCTL_ADD`
/// returns 0x80070490 (`ERROR_NOT_FOUND`). Best-effort: only `Present==false` AND
/// `Status==Unknown` nodes are removed, so a live session is never touched. Returns how many
/// were removed. Logs found and removed even when both are zero — silence hid a failed reap.
fn reap_ghost_monitors() -> u32 {
    // Presence, not health: `Status -ne 'OK'` would `pnputil /remove-device` a live monitor in a
    // transient problem state. `Status -eq 'Unknown'` guards `Present` reading null (`-not $null`
    // is true and would select every device). Full-path pnputil; `$LASTEXITCODE=1` before launch
    // so a miss cannot look like exit 0. Tokens are locale-invariant.
    const REAP_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $g = @(Get-PnpDevice -Class Monitor | Where-Object { -not $_.Present -and $_.Status -eq 'Unknown' -and $_.FriendlyName -match 'punktfunk' }); \
        $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); \
        $n = 0; foreach ($d in $g) { $LASTEXITCODE = 1; if (Test-Path $pnp) { & $pnp /remove-device $d.InstanceId *> $null }; if ($LASTEXITCODE -eq 0) { $n++ } }; \
        Write-Output ($g.Count.ToString() + ' ' + $n)";
    // Full-path powershell: LocalSystem PATH need not include System32.
    let ps = std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|_| "powershell.exe".to_string());
    match std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            REAP_PS,
        ])
        .output()
    {
        Ok(o) => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let Some((found, removed)) = parse_reap_output(&raw) else {
                tracing::warn!(
                    output = %raw.trim(),
                    "pf-vdisplay: ghost-monitor reap died before reporting — ghost nodes (if any) still pin IddCx monitor slots"
                );
                return 0;
            };
            if found == 0 {
                tracing::info!("pf-vdisplay: no ghost (not-present) virtual-monitor nodes to reap");
            } else if removed < found {
                tracing::warn!(
                    found,
                    removed,
                    "pf-vdisplay: ghost-monitor reap could NOT remove every ghost node — the leftovers keep pinning IddCx monitor slots toward the 0x80070490 wedge"
                );
            } else {
                tracing::warn!(
                    reaped = removed,
                    "pf-vdisplay: reaped ghost (not-present) virtual-monitor nodes — IddCx slot-exhaustion prevention"
                );
            }
            removed
        }
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: ghost-monitor reap could not spawn powershell");
            0
        }
    }
}

/// Parse `"<found> <removed>"` from [`reap_ghost_monitors`]. `None` = the script died before
/// reporting (callers treat that as removed-nothing, loudly).
fn parse_reap_output(out: &str) -> Option<(u32, u32)> {
    let mut it = out.split_whitespace().map(str::parse::<u32>);
    match (it.next(), it.next()) {
        (Some(Ok(found)), Some(Ok(removed))) => Some((found, removed)),
        _ => None,
    }
}

/// What the cycle DID, not the devnode's PnP status afterwards. A device never touched still
/// reads `OK`, so status-after cannot tell a no-op from a reload.
enum AdapterCycle {
    Reloaded { how: &'static str, status: String },
    NotInstalled,
    Refused(String),
}

/// Reload the pf-vdisplay adapter — in-process `reset-pf-vdisplay.ps1` step 3. A killed WUDFHost
/// can leave the devnode "started" yet hostless (PnP OK, no process, zero interfaces).
///
/// Disable+enable is the script's lever, but that script stops the host first: this process
/// still holds [`DeviceSlot`](super::manager) handles, so disable is expected to refuse.
/// `pnputil /restart-device` reloads a device in use. Failure paths re-enable so a half cycle
/// cannot leave the adapter disabled. Best-effort, ~6 s inside the script.
fn reload_vdisplay_adapter() -> AdapterCycle {
    // Prefer live nodes (`Present` or `Status -ne 'Unknown'`): `-First 1` can pick a phantom whose
    // disable and restart both fail. `-ErrorAction Stop` inside `try` — reporting PnP Status after
    // a refused disable looks like `OK`. `$LASTEXITCODE=1` before pnputil so "never ran" ≠ 0.
    // REFUSED carries counts, Status, problem code, restart exit (3010 = needs reboot).
    const CYCLE_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $all = @(Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'punktfunk Virtual Display' }); \
        if ($all.Count -eq 0) { Write-Output 'ABSENT'; exit }; \
        $live = @($all | Where-Object { $_.Present -or $_.Status -ne 'Unknown' } | Sort-Object { $_.Status -ne 'OK' }); \
        if ($live.Count -eq 0) { Write-Output ('REFUSED only phantom (not-present) adapter devnodes remain (' + $all.Count + ') - the device node itself is gone and no reload can revive it; reinstalling the host re-creates it'); exit }; \
        $ad = $live[0]; $id = $ad.InstanceId; $err = ''; \
        try { \
            Disable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop; Start-Sleep -Seconds 2; \
            try { Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop } \
            catch { Start-Sleep -Seconds 2; Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop }; \
            Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED cycle ' + (Get-PnpDevice -InstanceId $id).Status); exit \
        } catch { $err = ($_.Exception.Message -replace '\\s+', ' ') }; \
        $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); $LASTEXITCODE = 1; \
        if (Test-Path $pnp) { & $pnp /restart-device $id *> $null }; \
        $rx = $LASTEXITCODE; \
        if ($rx -eq 0) { Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED restart ' + (Get-PnpDevice -InstanceId $id).Status) } \
        else { Enable-PnpDevice -InstanceId $id -Confirm:$false; \
            Write-Output ('REFUSED devnodes=' + $all.Count + ' live=' + $live.Count + ' status=' + $ad.Status + ' problem=' + $ad.ConfigManagerErrorCode + ' restart_exit=' + $rx + ' ' + $err) }";
    let ps = std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|_| "powershell.exe".to_string());
    let out = match std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            CYCLE_PS,
        ])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: adapter reload could not spawn powershell");
            return AdapterCycle::Refused(format!("could not spawn powershell: {e}"));
        }
    };
    let outcome = classify_reload_output(&out);
    match &outcome {
        AdapterCycle::NotInstalled => {
            tracing::warn!("pf-vdisplay: no adapter devnode to reload — driver not installed");
        }
        AdapterCycle::Reloaded { how, status } => tracing::warn!(
            how,
            %status,
            "pf-vdisplay: reloaded the adapter device (hostless-zombie recovery)"
        ),
        AdapterCycle::Refused(why) => tracing::warn!(
            reason = %why,
            "pf-vdisplay: the adapter devnode exists but could NOT be reloaded — a session cannot \
             recover from this without a host-service restart or a reboot"
        ),
    }
    outcome
}

/// Parse [`reload_vdisplay_adapter`] stdout. Split out so the decoder is testable without a box.
fn classify_reload_output(out: &str) -> AdapterCycle {
    let out = out.trim();
    let (verb, rest) = out.split_once(char::is_whitespace).unwrap_or((out, ""));
    match verb {
        "ABSENT" => AdapterCycle::NotInstalled,
        "RELOADED" => {
            let (how, status) = rest
                .trim()
                .split_once(char::is_whitespace)
                .unwrap_or((rest.trim(), ""));
            // `&'static str` so the two levers stay distinct: `restart` means disable was refused
            // (something still holds the device open).
            let how: &'static str = if how == "restart" {
                "pnputil /restart-device"
            } else {
                "disable+enable"
            };
            AdapterCycle::Reloaded {
                how,
                status: status.trim().to_string(),
            }
        }
        // `REFUSED <reason>`, empty stdout, or anything else: the devnode was not reloaded.
        _ => AdapterCycle::Refused(if rest.trim().is_empty() {
            format!("unexpected adapter-reload output: {out:?}")
        } else {
            rest.trim().to_string()
        }),
    }
}

/// True if `e`'s chain carries 0x80070490 (`ERROR_NOT_FOUND`) — IddCx slot exhaustion. The hex
/// is locale-invariant; the OS message text is not.
fn is_slot_exhaustion_wedge(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("0x80070490")
}

/// Pin the IddCx render GPU to `luid` before `IOCTL_ADD`. On a multi-adapter box this stops DXGI
/// reparenting the virtual output onto a different GPU than the one we encode on (ACCESS_LOST).
/// Callers tolerate `Err`: the driver reports the real render LUID in the shared header anyway.
///
/// # Safety
///
/// `h` must be a live pf-vdisplay control handle ([`ioctl`]'s only obligation). `luid` is `Copy`.
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let req = control::SetRenderAdapterRequest {
        luid_low: luid.LowPart,
        luid_high: luid.HighPart,
    };
    let mut none: [u8; 0] = [];
    // SAFETY: `h` is a live control-device handle by this fn's contract — `ioctl`'s only
    // obligation. The request is a `Pod` viewed through `bytemuck::bytes_of`; empty output
    // matches the IOCTL's "no output buffer" contract.
    unsafe {
        ioctl(
            h,
            control::IOCTL_SET_RENDER_ADAPTER,
            bytemuck::bytes_of(&req),
            &mut none,
        )
    }
    .map(|_| ())
    .context("pf-vdisplay SET_RENDER_ADAPTER")
}

/// Deliver a monitor's sealed frame channel. On IOCTL success the driver owns the handles
/// duplicated into WUDFHost; the caller reaps remote duplicates on failure so none leak. Always
/// the v2 shape (WP7): a pre-fence driver reads the v1 prefix of the longer buffer unchanged.
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_frame_channel(
    dev: HANDLE,
    req: &control::SetFrameChannelRequestV2,
) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: `dev` is the live control handle by this fn's contract. `bytes_of(req)` borrows the
    // caller's request for this synchronous call; `none` is empty, so there is no output buffer.
    unsafe {
        ioctl(
            dev,
            control::IOCTL_SET_FRAME_CHANNEL,
            bytemuck::bytes_of(req),
            &mut none,
        )
    }
    .map(|_| ())
    .context("pf-vdisplay SET_FRAME_CHANNEL")
}

/// Deliver a monitor's hardware-cursor section (`IOCTL_SET_CURSOR_CHANNEL`, proto v5). Same
/// delivery/ownership contract as [`send_frame_channel`].
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_cursor_channel(
    dev: HANDLE,
    req: &control::SetCursorChannelRequest,
) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: `dev` is the live control handle by this fn's contract; `bytes_of(req)` borrows the
    // caller's request across this synchronous call; no output buffer.
    unsafe {
        ioctl(
            dev,
            control::IOCTL_SET_CURSOR_CHANNEL,
            bytemuck::bytes_of(req),
            &mut none,
        )
    }
    .map(|_| ())
    .context("pf-vdisplay SET_CURSOR_CHANNEL")
}

/// Flip a live monitor's hardware-cursor declaration (`IOCTL_SET_CURSOR_FORWARD`, proto v6).
/// Fails against a pre-v6 driver; callers log and keep the declared-at-ADD behavior.
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_cursor_forward(
    dev: HANDLE,
    req: &control::SetCursorForwardRequest,
) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: `dev` is the live control handle by this fn's contract; `bytes_of(req)` borrows the
    // caller's request across this synchronous call; no output buffer.
    unsafe {
        ioctl(
            dev,
            control::IOCTL_SET_CURSOR_FORWARD,
            bytemuck::bytes_of(req),
            &mut none,
        )
    }
    .map(|_| ())
    .context("pf-vdisplay SET_CURSOR_FORWARD")
}

/// RAII SetupAPI device-info list. Every [`open_device`] exit path must destroy it; a driverless
/// box probes repeatedly and a leaked `HDEVINFO` per failed open would accumulate.
struct DevInfoList(HDEVINFO);

impl Drop for DevInfoList {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live device-info list this wrapper solely owns; destroyed
        // exactly once here.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Device-interface enumeration result. `active`/`inactive` let [`ensure_available`] tell a
/// mid-transition devnode (registered, not started) from a missing one. Only the latter is
/// worth reloading; cycling the former lengthens the outage it is waiting out.
struct Probe {
    handle: Option<OwnedHandle>,
    /// `SPINT_ACTIVE` set — owning device is started.
    active: u32,
    /// `SPINT_ACTIVE` clear — registered, owning device not started.
    inactive: u32,
    last_err: Option<anyhow::Error>,
}

impl Probe {
    /// No instance of any kind. With an adapter present this is a hostless WUDFHost crash; with
    /// none, the driver is not installed. Waiting alone will not fix either.
    fn is_absent(&self) -> bool {
        self.handle.is_none() && self.active == 0 && self.inactive == 0
    }

    /// Why no handle came back, naming what was seen. "0 interfaces" and "1 inactive" are
    /// different diagnoses; call only on a miss.
    fn into_error(self) -> anyhow::Error {
        let seen = format!("{} active, {} inactive", self.active, self.inactive);
        if self.handle.is_some() {
            return anyhow::anyhow!("pf-vdisplay device interface opened ({seen})");
        }
        match self.last_err {
            Some(e) => e.context(format!("no openable pf-vdisplay device interface ({seen})")),
            None => anyhow::anyhow!(
                "no pf-vdisplay device interface found ({seen}) — is the pf-vdisplay driver \
                 installed and its device started?"
            ),
        }
    }

    fn into_result(mut self) -> Result<OwnedHandle> {
        match self.handle.take() {
            Some(h) => Ok(h),
            None => Err(self.into_error()),
        }
    }
}

/// Open the pf-vdisplay control device. Safe and owning: no caller obligation, close is `Drop`.
fn open_device() -> Result<OwnedHandle> {
    probe_device().into_result()
}

/// [`open_device`], reporting what was found rather than only success.
fn probe_device() -> Probe {
    let mut probe = Probe {
        handle: None,
        active: 0,
        inactive: 0,
        last_err: None,
    };
    // SAFETY: SetupAPI enumeration; the returned list is solely owned by the RAII wrapper.
    let hdev = match unsafe {
        SetupDiGetClassDevsW(
            Some(&PF_VDISPLAY_INTERFACE),
            PCWSTR::null(),
            None,
            DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
        )
    }
    .context("SetupDiGetClassDevsW(pf-vdisplay) — is the pf-vdisplay driver installed?")
    {
        Ok(h) => DevInfoList(h),
        Err(e) => {
            probe.last_err = Some(e);
            return probe;
        }
    };

    // Every instance, not index 0: after an upgrade a Code-10 node can sit at 0 while the live
    // interface is later. First `SPINT_ACTIVE` + openable wins.
    for index in 0..64u32 {
        let mut idata = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `hdev.0` is the live list; `idata` is a valid, size-stamped out-param.
        if unsafe {
            SetupDiEnumDeviceInterfaces(hdev.0, None, &PF_VDISPLAY_INTERFACE, index, &mut idata)
        }
        .is_err()
        {
            break; // ERROR_NO_MORE_ITEMS — no further candidates
        }
        if idata.Flags & SPINT_ACTIVE == 0 {
            probe.inactive += 1;
            continue;
        }
        probe.active += 1;
        let mut required = 0u32;
        // SAFETY: sizing call — null buffer plus a valid `required` out-param; the expected
        // ERROR_INSUFFICIENT_BUFFER "failure" is ignored and only `required` is consumed.
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, None, 0, Some(&mut required), None)
        };
        // Against the struct's size, not `u32`: `cbSize` below is
        // `size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()`.
        if (required as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            continue; // sizing failed — never stamp a cbSize through an under-sized buffer
        }
        // `u64`, not `u8`: the buffer is written as `SP_DEVICE_INTERFACE_DETAIL_DATA_W` (4-byte
        // align); `Vec<u8>` only promises 1.
        let mut buf = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // SAFETY: `buf` is ≥ `required` bytes and 8-aligned (so also 4). `detail` aliases `buf`
        // only in this iteration; `DevicePath` is read before `buf` drops. The path is a RAW
        // place projection so it keeps the whole allocation's provenance: `DevicePath` is
        // `[u16; 1]` (FAM stub), so `.as_ptr()` would tag two bytes while `CreateFileW` reads
        // the full NUL-terminated path — everything past `[0]` OOB, and the compiler may fold
        // the zero-init into an empty device name.
        let opened = unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, Some(detail), required, None, None)
                .context("SetupDiGetDeviceInterfaceDetailW(pf-vdisplay)")
                .and_then(|()| {
                    CreateFileW(
                        PCWSTR((&raw const (*detail).DevicePath).cast::<u16>()),
                        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                    .context("CreateFileW(pf-vdisplay device)")
                })
        };
        match opened {
            Ok(h) => {
                // SAFETY: `h` is the handle `CreateFileW` just returned to this call and nothing
                // else holds it; `OwnedHandle` is the single owner that closes it on drop.
                probe.handle = Some(unsafe { OwnedHandle::from_raw_handle(h.0 as _) });
                return probe;
            }
            // Raced-away or wedged device — remember the error, try the next interface.
            Err(e) => probe.last_err = Some(e),
        }
    }
    probe
}

/// pf-vdisplay IOCTL surface behind [`VirtualDisplayManager`](super::manager::VirtualDisplayManager).
/// Wire contract: `pf_driver_proto::control` (versioned, hard-checked).
pub(crate) struct PfVdisplayDriver;

impl VdisplayDriver for PfVdisplayDriver {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32, u32)> {
        // Brief re-probe, no adapter reload. `ensure_available` already ran; leftover is a race.
        // `hw_cursor_capable` also lands here mid-handshake. Reloading would deadlock:
        // `ensure_device` calls us holding the manager `device` mutex (`RECOVERY` lock order).
        let device = wait_for_interface(BRIEF_RETRY, false).0?;
        // `OwnedHandle` so every `?` closes the device. Wrapping later leaked when GET_INFO failed.
        let raw = HANDLE(device.as_raw_handle());
        // Hard version check: a mismatch must not proceed to corrupt the IOCTL stream.
        let mut info_buf = [0u8; size_of::<control::InfoReply>()];
        // SAFETY: `raw` borrows the live `OwnedHandle` above for this synchronous call.
        // `IOCTL_GET_INFO` takes no input (`&[]`) and writes into `info_buf`, a stack
        // `[u8; size_of::<InfoReply>()]` whose length is the output size — so the write cannot
        // go OOB — and which outlives the call.
        let n = unsafe { ioctl(raw, control::IOCTL_GET_INFO, &[], &mut info_buf) }
            .context("pf-vdisplay IOCTL_GET_INFO (version handshake)")?;
        // Fail closed on a short reply: `protocol_version` (and the ADD reply below) gate host
        // behaviour; zeros from an under-written buffer must not be trusted.
        if (n as usize) < size_of::<control::InfoReply>() {
            anyhow::bail!(
                "pf-vdisplay IOCTL_GET_INFO returned {n} bytes, expected {}",
                size_of::<control::InfoReply>()
            );
        }
        let info: control::InfoReply =
            bytemuck::pod_read_unaligned(&info_buf[..size_of::<control::InfoReply>()]);
        // Floor/ceiling, not equality: v4+ is additive, so a v3 driver still works and the
        // in-place path is gated on the reported version. Below the floor or above this host fails.
        if info.protocol_version < pf_driver_proto::MIN_DRIVER_PROTOCOL_VERSION
            || info.protocol_version > pf_driver_proto::PROTOCOL_VERSION
        {
            anyhow::bail!(
                "pf-vdisplay protocol mismatch: host drives {}..={}, driver reports {} — install \
                 matching host + driver",
                pf_driver_proto::MIN_DRIVER_PROTOCOL_VERSION,
                pf_driver_proto::PROTOCOL_VERSION,
                info.protocol_version
            );
        }
        let watchdog_s = info.watchdog_timeout_s.max(1);
        // Only log of the negotiated watchdog; pinger cadence is `watchdog/3`.
        tracing::info!(
            "pf-vdisplay protocol {} (host drives {}..={}, watchdog timeout {}s)",
            info.protocol_version,
            pf_driver_proto::MIN_DRIVER_PROTOCOL_VERSION,
            pf_driver_proto::PROTOCOL_VERSION,
            watchdog_s
        );
        // Per-version gaps. Bumps since v3 are additive; a blanket `< PROTOCOL_VERSION` named
        // the wrong missing capability (told a v4 driver it lacked a v4 feature).
        if info.protocol_version < 4 {
            tracing::warn!(
                "pf-vdisplay protocol {}: driver lacks the in-place mid-stream resize \
                 (IOCTL_UPDATE_MODES, added in v4) — every mid-stream resize costs a monitor \
                 re-arrival (one hotplug per switch) until the driver is updated",
                info.protocol_version
            );
        }
        if info.protocol_version < 5 {
            tracing::warn!(
                "pf-vdisplay protocol {}: driver lacks the IddCx hardware-cursor channel (added in \
                 v5) — the pointer stays composited into the captured frame",
                info.protocol_version
            );
        }
        if info.protocol_version < 6 {
            tracing::info!(
                "pf-vdisplay protocol {}: driver lacks the mid-stream cursor-forward flip \
                 (IOCTL_SET_CURSOR_FORWARD, added in v6) — the cursor model declared at monitor ADD \
                 stands for the whole session",
                info.protocol_version
            );
        }
        // CLEAR_ALL only on the first open of the process. A reopen can race sessions that still
        // believe they are live; an unconditional CLEAR_ALL would raze them.
        if !reap_orphans {
            reap_ghost_monitors();
            return Ok((device, watchdog_s, info.protocol_version));
        }
        let mut none: [u8; 0] = [];
        // SAFETY: `raw` borrows the live `OwnedHandle` above. `IOCTL_CLEAR_ALL` has no input and
        // no output: `&[]` and empty `none` pass zero-length buffers.
        if unsafe { ioctl(raw, control::IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        } else {
            tracing::warn!("pf-vdisplay IOCTL_CLEAR_ALL failed on startup (continuing)");
        }
        // CLEAR_ALL cannot remove OS-side not-present "Generic Monitor (Punktfunk)" PDOs.
        // Reap those so a restart starts with a clean IddCx slot budget.
        reap_ghost_monitors();
        Ok((device, watchdog_s, info.protocol_version))
    }

    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        hw_cursor: bool,
    ) -> Result<AddedMonitor> {
        let session_id = next_session_id();
        // EDID CTA HDR block; all-zero = unknown → driver defaults (also what a driver that
        // reads only the legacy 24-byte prefix does).
        let (max_luminance_nits, max_frame_avg_nits, min_luminance_millinits) = client_hdr
            .map(|m| pf_frame::hdr::vdisplay_luminance_fields(&m))
            .unwrap_or((0, 0, 0));
        if max_luminance_nits > 0 {
            tracing::info!(
                max_luminance_nits,
                max_frame_avg_nits,
                min_luminance_millinits,
                "pf-vdisplay ADD: advertising the client display's HDR volume in the monitor EDID"
            );
        }
        let add = control::AddRequest {
            session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            preferred_monitor_id,
            max_luminance_nits,
            max_frame_avg_nits,
            min_luminance_millinits,
            // v5: driver declares an IddCx hardware cursor (DWM stops compositing the pointer).
            // Zero toward older drivers is harmless — host sets this only when proto ≥ 5.
            hw_cursor: hw_cursor as u32,
        };
        // Opt-in; non-fatal: the driver reports the real render LUID in the shared header.
        if let Some(luid) = render_luid {
            // SAFETY: `add_monitor`'s contract guarantees `dev` is the live control handle,
            // `set_render_adapter`'s precondition. `luid` is `Copy` by value — no borrow crosses.
            match unsafe { set_render_adapter(dev, luid) } {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "pf-vdisplay SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                Err(e) => tracing::warn!(
                    "pf-vdisplay SET_RENDER_ADAPTER failed (continuing on the natural adapter): {e:#}"
                ),
            }
        }
        let mut out = [0u8; size_of::<control::AddReply>()];
        // SAFETY: per `add_monitor`'s contract `dev` is the live control handle.
        // `bytemuck::bytes_of(&add)` borrows the local `AddRequest` across this synchronous call;
        // `out` is a stack `[u8; size_of::<AddReply>()]` whose length bounds the kernel write.
        let add_res = unsafe { ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out) };
        let add_res = match add_res {
            Err(e) if is_slot_exhaustion_wedge(&e) => {
                // Ghost PDOs exhausted the IddCx slot pool (0x80070490). Reap and retry so the
                // wedge self-heals instead of hard-failing every session.
                let reaped = reap_ghost_monitors();
                tracing::warn!(
                    reaped,
                    "pf-vdisplay ADD wedged (0x80070490 ERROR_NOT_FOUND) — reaped ghost monitor nodes, retrying ADD"
                );
                // pnputil is durable; VidPN slot reclaim is async and can lag the return.
                // 5 × 300 ms, no re-reap (~1.5 s worst case, wedge path only).
                let mut res = Err(anyhow::anyhow!("pf-vdisplay ADD retry loop did not run"));
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // SAFETY: identical to the first IOCTL_ADD — `dev` is the live control handle
                    // (`add_monitor`'s contract); `bytes_of(&add)` + `&mut out` outlive the call.
                    res = unsafe {
                        ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out)
                    };
                    if res.is_ok() {
                        break;
                    }
                }
                res
            }
            other => other,
        };
        let n = add_res.with_context(|| {
            format!(
                "pf-vdisplay ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        // Fail closed on a short reply — `target_id`/`wudf_pid`/`luid` feed OpenProcess.
        // Legacy size, not the full struct: an old driver writes only the prefix; `out` is
        // zeroed so the missing `cursor_excluded` tail reads 0 (unknown/clean).
        if (n as usize) < control::ADD_REPLY_LEGACY_SIZE {
            // IOCTL succeeded: the driver already created the monitor and took a slot. Bailing
            // without REMOVE leaks it; ~16 leaks wedge later ADDs at 0x80070490.
            let req = control::RemoveRequest { session_id };
            let mut none: [u8; 0] = [];
            // SAFETY: `dev` is the live control handle (`add_monitor`'s contract); `bytes_of(&req)`
            // borrows a local across this synchronous call; `none` is the empty output the IOCTL
            // expects.
            let undo = unsafe {
                ioctl(
                    dev,
                    control::IOCTL_REMOVE,
                    bytemuck::bytes_of(&req),
                    &mut none,
                )
            };
            match undo {
                Ok(_) => tracing::warn!(
                    session_id,
                    "pf-vdisplay ADD returned a short reply — removed the monitor it had already \
                     created so its IddCx slot is not leaked"
                ),
                Err(e) => tracing::error!(
                    session_id,
                    error = %format!("{e:#}"),
                    "pf-vdisplay ADD returned a short reply AND the compensating REMOVE failed — \
                     this monitor's IddCx slot is leaked until the driver is cycled"
                ),
            }
            anyhow::bail!(
                "pf-vdisplay ADD returned {n} bytes, expected at least {}",
                control::ADD_REPLY_LEGACY_SIZE
            );
        }
        // `pod_read_unaligned`, not `from_bytes`: `out` has no 4-byte alignment guarantee.
        let reply: control::AddReply =
            bytemuck::pod_read_unaligned(&out[..size_of::<control::AddReply>()]);
        let luid = LUID {
            LowPart: reply.adapter_luid_low,
            HighPart: reply.adapter_luid_high,
        };
        tracing::info!(
            target_id = reply.target_id,
            adapter_luid = %format_args!("{:#x}", luid.LowPart),
            wudf_pid = reply.wudf_pid,
            cursor_excluded = reply.cursor_excluded != 0,
            "pf-vdisplay monitor created {}x{}@{}",
            mode.width,
            mode.height,
            mode.refresh_hz
        );
        // Did the driver honor the preferred (stable) monitor id? 0 = ignored; a mismatch
        // means Windows will not reapply this client's saved per-monitor config this session.
        if preferred_monitor_id != 0 {
            if reply.resolved_monitor_id == preferred_monitor_id {
                tracing::info!(
                    monitor_id = preferred_monitor_id,
                    "pf-vdisplay: per-client monitor id honored (stable identity → saved config persists)"
                );
            } else {
                tracing::warn!(
                    preferred = preferred_monitor_id,
                    resolved = reply.resolved_monitor_id,
                    "pf-vdisplay: preferred monitor id NOT honored (live-id collision, or a pre-Phase-2 \
                     driver) — per-client config persistence degraded to auto identity this session"
                );
            }
        }
        // `reply.adapter_luid` is the IddCx *display* adapter (`OsAdapterLuid`), not the
        // render GPU — it cannot validate SET_RENDER_ADAPTER. Real render LUID is in the
        // shared frame header; the IDD-push capturer rebinds on a mismatch.
        Ok(AddedMonitor {
            key: MonitorKey::Session(session_id),
            target_id: reply.target_id,
            luid,
            wudf_pid: reply.wudf_pid,
            resolved_monitor_id: reply.resolved_monitor_id,
            cursor_excluded: reply.cursor_excluded != 0,
        })
    }

    unsafe fn update_modes(&self, dev: HANDLE, key: &MonitorKey, mode: Mode) -> Result<()> {
        let MonitorKey::Session(session_id) = key else {
            anyhow::bail!("pf-vdisplay: unexpected monitor key kind");
        };
        let req = control::UpdateModesRequest {
            session_id: *session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            _reserved: 0,
        };
        let mut none: [u8; 0] = [];
        // SAFETY: per `update_modes`'s contract `dev` is the live control handle. `bytes_of(&req)`
        // borrows the local `UpdateModesRequest` across this synchronous call; `none` is empty.
        unsafe {
            ioctl(
                dev,
                control::IOCTL_UPDATE_MODES,
                bytemuck::bytes_of(&req),
                &mut none,
            )
        }
        .map(|_| ())
        .with_context(|| {
            format!(
                "pf-vdisplay UPDATE_MODES {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })
    }

    unsafe fn remove_monitor(&self, dev: HANDLE, key: &MonitorKey) -> Result<()> {
        let MonitorKey::Session(session_id) = key else {
            anyhow::bail!("pf-vdisplay: unexpected monitor key kind");
        };
        let req = control::RemoveRequest {
            session_id: *session_id,
        };
        let mut none: [u8; 0] = [];
        // SAFETY: per `remove_monitor`'s contract `dev` is the live control handle. `bytes_of(&req)`
        // borrows the local `RemoveRequest` across this synchronous call; `none` is empty.
        unsafe {
            ioctl(
                dev,
                control::IOCTL_REMOVE,
                bytemuck::bytes_of(&req),
                &mut none,
            )
        }
        .map(|_| ())
    }

    unsafe fn ping(&self, dev: HANDLE) -> Result<()> {
        let mut none: [u8; 0] = [];
        // SAFETY: per `ping`'s contract `dev` is the live control handle. `IOCTL_PING` has no
        // input (`&[]`) and no output (`none` is empty).
        unsafe { ioctl(dev, control::IOCTL_PING, &[], &mut none) }.map(|_| ())
    }
}

/// Windows pf-vdisplay backend. Lifecycle lives in
/// [`VirtualDisplayManager`](super::manager::VirtualDisplayManager); this only carries the
/// connecting client's fingerprint so the manager can assign a stable per-client monitor id.
pub struct PfVdisplayDisplay {
    /// Connecting client's cert fingerprint (`None` = anonymous/GameStream → auto id).
    client_fp: Option<[u8; 32]>,
    /// Client HDR volume (`None` = unknown/SDR → driver EDID defaults). Advertised in the
    /// created monitor's EDID so host apps tone-map to the client's panel.
    client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    /// Declare an IddCx hardware cursor. Honored only when the handshake reported proto ≥ 5.
    hw_cursor: bool,
    /// Deliberate-quit flag (`None` = linger policy). A user "stop" tears the monitor down
    /// immediately instead of lingering.
    quit: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl PfVdisplayDisplay {
    pub fn new() -> Result<Self> {
        super::manager::init(Box::new(PfVdisplayDriver)).open_backend()?;
        Ok(Self {
            client_fp: None,
            client_hdr: None,
            hw_cursor: false,
            quit: None,
        })
    }
}

impl VirtualDisplay for PfVdisplayDisplay {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn set_client_hdr(&mut self, hdr: Option<punktfunk_core::quic::HdrMeta>) {
        self.client_hdr = hdr;
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn set_quit_flag(&mut self, quit: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.quit = Some(quit);
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        super::manager::vdm().acquire(
            mode,
            self.client_fp,
            self.client_hdr,
            self.hw_cursor,
            self.quit.clone(),
        )
    }
}

pub fn probe() -> Result<()> {
    open_device().map(|_| ())
}

pub fn is_available() -> bool {
    open_device().is_ok()
}

const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// How long a registered-but-not-ready interface may come up before a reload. Wake-from-sleep:
/// D0 re-registers the interface while resume is still running; probing once would
/// disable/enable an adapter that was seconds from ready.
const NOT_READY_GRACE: Duration = Duration::from_secs(15);

/// Absent-interface settle before reload. Short (hostless does not self-heal) but non-zero so
/// a resume that briefly de-registers is not met with device surgery.
const ABSENT_SETTLE: Duration = Duration::from_secs(3);

/// Arrival window after a reload. 15 s: PnP is contended right after wake; 4 s missed it.
const ARRIVAL_AFTER_RELOAD: Duration = Duration::from_secs(15);

/// Ceiling on the whole wait. Without it, not-ready + reload + arrival can approach a minute.
const TOTAL_BUDGET: Duration = Duration::from_secs(30);

/// Budget when the caller must not stall: re-probe only, no reload. [`VdisplayDriver::open`]
/// and `hw_cursor_capable` (a handshake bool) must not hold Welcome for tens of seconds.
const BRIEF_RETRY: Duration = Duration::from_secs(3);

/// Serializes recovery so N racing sessions perform one adapter reload, not N interleaved
/// ones that tear down the stack the others wait on.
///
/// Taken only by [`ensure_available`] (no manager lock). Order is `RECOVERY` → `device`:
/// `invalidate_cached_device` takes `device` while this is held. [`VdisplayDriver::open`]
/// runs *inside* `device` and must never take this lock or the orders invert and deadlock.
static RECOVERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`is_available`] with self-heal and patience after wake. Returns the reason on failure
/// rather than a bare `false` (callers must not flatten that into "driver not installed").
pub fn ensure_available() -> Result<()> {
    // Guard is `()`; a poison must not wedge every later session out of recovery.
    let (result, reloaded) = {
        let _serialize = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
        wait_for_interface(NOT_READY_GRACE, true)
    };
    // Reload tore the stack; a handle cached during the arrival window is dead. Usually
    // a no-op: recovery already released the manager's reference so PnP can proceed.
    if reloaded {
        super::manager::invalidate_cached_device(
            "the pf-vdisplay adapter was reloaded (hostless-zombie recovery)",
        );
    }
    result.map(|_| ())
}

/// Wait for an openable control interface; reload if `reload` and the devnode looks hostless.
/// Returns the handle (so the manager's open can keep it) and whether a reload ran.
///
/// Two states want opposite treatment:
/// * **Not ready** — instances registered, none active (or open refused). The devnode is
///   coming up; reloading lengthens the outage.
/// * **Absent** — no instance. Hostless WUDFHost crash; only a reload clears it.
///
/// Probe, wait out not-ready, reload absent after [`ABSENT_SETTLE`], then
/// [`ARRIVAL_AFTER_RELOAD`]. A reload is still attempted at the end of `not_ready_grace`
/// (wedged not-ready). No adapter devnode fails immediately.
fn wait_for_interface(not_ready_grace: Duration, reload: bool) -> (Result<OwnedHandle>, bool) {
    let started = Instant::now();
    let mut deadline = started + not_ready_grace;
    let mut absent_since: Option<Instant> = None;
    let mut reloaded = false;
    loop {
        let mut probe = probe_device();
        if let Some(h) = probe.handle.take() {
            if reloaded || started.elapsed() > PROBE_INTERVAL {
                tracing::info!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    reloaded,
                    "pf-vdisplay: control interface available"
                );
            }
            return (Ok(h), reloaded);
        }
        // Reset by any sighting: flicker between absent and not-ready is a transition.
        if probe.is_absent() {
            if absent_since.is_none() && reload {
                // Drop the manager's control-handle ref now so `ABSENT_SETTLE` drains outstanding
                // `Arc` clones; an open handle vetoes PnP disable/restart. Gated on `reload`:
                // `BRIEF_RETRY` runs inside the `device` mutex (taking it again deadlocks) and
                // never reloads.
                super::manager::invalidate_cached_device(
                    "control interface absent — releasing the host's own device handle ahead of a \
                     possible adapter reload",
                );
            }
            absent_since.get_or_insert_with(Instant::now);
        } else {
            absent_since = None;
        }
        let absent_long_enough = absent_since.is_some_and(|t| t.elapsed() >= ABSENT_SETTLE);
        if reload && !reloaded && (absent_long_enough || Instant::now() >= deadline) {
            // Not-ready never took the absent-sighting release; drop the ref now (idempotent).
            super::manager::invalidate_cached_device(
                "adapter reload imminent — releasing the host's own device handle (open handles \
                 veto the PnP cycle)",
            );
            match reload_vdisplay_adapter() {
                // No adapter at all — waiting cannot conjure a driver.
                AdapterCycle::NotInstalled => {
                    let e = Err(probe.into_error()).context(
                        "no punktfunk virtual-display adapter devnode exists — the driver is not \
                         installed",
                    );
                    return (e, reloaded);
                }
                AdapterCycle::Refused(why) => {
                    let e = Err(probe.into_error()).context(format!(
                        "the pf-vdisplay adapter devnode could not be reloaded ({why})"
                    ));
                    return (e, reloaded);
                }
                AdapterCycle::Reloaded { .. } => {
                    reloaded = true;
                    absent_since = None;
                    deadline = (Instant::now() + ARRIVAL_AFTER_RELOAD).min(started + TOTAL_BUDGET);
                }
            }
        }
        if Instant::now() >= deadline {
            let e = Err(probe.into_error()).context(format!(
                "the pf-vdisplay control interface did not appear within {:?}{}",
                started.elapsed(),
                if reloaded {
                    " (including an adapter reload)"
                } else {
                    ""
                }
            ));
            return (e, reloaded);
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// A refusal must decode as a refusal, carrying its reason. PnP Status after a refused
    /// disable still reads `OK` and must never decode as `Reloaded`.
    #[test]
    fn a_refused_reload_is_not_reported_as_a_reload() {
        let refused =
            classify_reload_output("REFUSED This device cannot be disabled because it is in use.");
        match refused {
            AdapterCycle::Refused(why) => {
                assert!(why.contains("in use"), "the reason must survive: {why:?}")
            }
            other => panic!("a refused reload decoded as {}", variant(&other)),
        }
        // A bare device status must never decode as a reload.
        for stale in ["OK", "Error", "Unknown"] {
            assert!(
                matches!(classify_reload_output(stale), AdapterCycle::Refused(_)),
                "{stale:?} is a device status, not a reload outcome"
            );
        }
    }

    /// A refusal must carry its evidence (counts, Status, problem code, restart exit) and a
    /// phantom-only state must decode as refused — reload cannot revive a gone device.
    #[test]
    fn a_refusal_keeps_its_evidence() {
        let why = match classify_reload_output(
            "REFUSED devnodes=2 live=1 status=OK problem=0 restart_exit=3010 Generic failure",
        ) {
            AdapterCycle::Refused(why) => why,
            other => panic!("expected Refused, got {}", variant(&other)),
        };
        for token in ["devnodes=2", "live=1", "status=OK", "restart_exit=3010"] {
            assert!(why.contains(token), "{token} must survive: {why:?}");
        }
        assert!(matches!(
            classify_reload_output(
                "REFUSED only phantom (not-present) adapter devnodes remain (2) - the device node \
                 itself is gone and no reload can revive it; reinstalling the host re-creates it"
            ),
            AdapterCycle::Refused(why) if why.contains("phantom")
        ));
    }

    /// `NotInstalled` fails fast, `Reloaded` earns the arrival window, and `restart` means
    /// disable was refused (something still holds the device open).
    #[test]
    fn reload_outcomes_decode() {
        assert!(matches!(
            classify_reload_output("ABSENT"),
            AdapterCycle::NotInstalled
        ));
        match classify_reload_output("RELOADED cycle OK") {
            AdapterCycle::Reloaded { how, status } => {
                assert_eq!(how, "disable+enable");
                assert_eq!(status, "OK");
            }
            other => panic!("expected Reloaded, got {}", variant(&other)),
        }
        match classify_reload_output("RELOADED restart OK\r\n") {
            AdapterCycle::Reloaded { how, status } => {
                assert_eq!(how, "pnputil /restart-device");
                assert_eq!(status, "OK");
            }
            other => panic!("expected Reloaded, got {}", variant(&other)),
        }
        // Empty stdout: un-reloaded, so `Refused`, not a silent success.
        assert!(matches!(
            classify_reload_output("   "),
            AdapterCycle::Refused(_)
        ));
    }

    /// Found and removed travel separately so a leftover ghost is loud. A single number or
    /// a script that died before reporting must not decode as a reap.
    #[test]
    fn reap_output_decodes_found_and_removed() {
        assert_eq!(parse_reap_output("3 3\r\n"), Some((3, 3)));
        assert_eq!(
            parse_reap_output("4 0"),
            Some((4, 0)),
            "pnputil unlaunchable"
        );
        assert_eq!(parse_reap_output("0 0"), Some((0, 0)), "clean box");
        for dead in ["5", "", "   ", "garbage", "OK"] {
            assert_eq!(
                parse_reap_output(dead),
                None,
                "{dead:?} is not a reap report"
            );
        }
    }

    /// `is_absent` is what decides wait vs. surgery. Registered-but-inactive is mid-transition
    /// (wake); reloading under it lengthens the outage.
    #[test]
    fn only_a_total_absence_counts_as_absent() {
        let probe = |active, inactive| Probe {
            handle: None,
            active,
            inactive,
            last_err: None,
        };
        assert!(probe(0, 0).is_absent(), "no instances at all = absent");
        assert!(
            !probe(0, 1).is_absent(),
            "a registered-but-inactive instance is a device coming up, not a missing one"
        );
        assert!(
            !probe(1, 0).is_absent(),
            "an active instance we merely failed to open is not a missing device"
        );
        // Diagnostic names what was seen — collapsing these into "is the driver installed?"
        // sends recovery down the wrong path.
        assert!(probe(0, 2).into_error().to_string().contains("2 inactive"));
    }

    fn variant(c: &AdapterCycle) -> &'static str {
        match c {
            AdapterCycle::Reloaded { .. } => "Reloaded",
            AdapterCycle::NotInstalled => "NotInstalled",
            AdapterCycle::Refused(_) => "Refused",
        }
    }

    /// Hardware round trip (`#[ignore]`): open → create → hold → drop (REMOVE). Under the
    /// guard so the drop tears down NOW: with the box's real keep-alive the teardown lingers
    /// 10 s and the next case in the file starts on a still-isolated desktop (measured on .173:
    /// `live_force_extend` red on its precondition, the desktop restored seconds later).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_create_drop() {
        let _policy = ExclusiveTopology::force();
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        assert_eq!(vout.preferred_mode, Some((1920, 1080, 60)));
        thread::sleep(Duration::from_secs(3));
        drop(vout); // REMOVE + stop the pinger
    }

    /// Forces `Topology::Exclusive` **and `KeepAlive::Off`** for the duration of a case and puts
    /// the operator's real policy back on drop — including when the case panics.
    ///
    /// The isolate branch this file's Phase-3 cases exercise runs ONLY under `Exclusive`, and a
    /// real install is usually configured otherwise (`topology_action()` returns
    /// `effective_topology()` as soon as ANY policy is configured). `KeepAlive::Off` is equally
    /// load-bearing: every post-teardown assertion here needs the lease drop to actually tear the
    /// monitor down — under the default 10 s linger the probe races the reaper, and under the
    /// gaming-rig `forever` the group restore never runs at all (measured on .173: both members
    /// PINNED, panel left dark, test red for a policy reason). Note this writes the host's
    /// `display-settings.json`; the guard is what makes that safe to do on a real box.
    struct ExclusiveTopology(crate::policy::DisplayPolicy);

    impl ExclusiveTopology {
        fn force() -> Self {
            let original = crate::policy::prefs().get();
            let mut forced = original.clone();
            forced.preset = crate::policy::Preset::Custom; // explicit fields are ignored otherwise
            forced.topology = crate::policy::Topology::Exclusive;
            forced.keep_alive = crate::policy::KeepAlive::Off;
            crate::policy::prefs()
                .set(forced)
                .expect("force Topology::Exclusive + KeepAlive::Off for this case");
            assert_eq!(
                crate::effective_topology(),
                crate::policy::Topology::Exclusive,
                "the forced policy did not resolve to Exclusive"
            );
            Self(original)
        }
    }

    impl Drop for ExclusiveTopology {
        fn drop(&mut self) {
            if let Err(e) = crate::policy::prefs().set(self.0.clone()) {
                eprintln!("WARNING: could not restore the display policy: {e}");
            }
        }
    }

    /// Run `f` on a worker and give up after `budget`. A hang inside `create` skipped every
    /// `Drop` and leaked IddCx slots; a bounded wait lets the harness exit so the driver can reap.
    fn within<T: Send + 'static>(
        budget: Duration,
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(budget) {
            Ok(v) => v,
            Err(_) => panic!(
                "{what} did not finish within {budget:?} — failing rather than hanging, so the \
                 harness can exit and the driver can reap. Check for a leaked punktfunk monitor \
                 before the next run."
            ),
        }
    }

    /// When the first member's isolate fails, a later member's isolate must be adopted as the
    /// group's restore snapshot — otherwise it deactivates the operator's panels with nothing
    /// able to put them back. `FAIL_NEXT_ISOLATES` fails the first isolate on real hardware.
    ///
    /// After both members tear down, the operator's external panel must be active again.
    /// Two members need two distinct client fingerprints (`slot_id_for` keys on them).
    /// Needs `Topology::Exclusive`. If the desk stays dark, recover from the console with
    /// `SetDisplayConfig(… SDC_USE_DATABASE_CURRENT|SDC_APPLY)`; `SDC_TOPOLOGY_EXTEND`
    /// will not do it with a single connected display (rc=31).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_a_failed_first_isolate_is_recovered_by_adopting_the_next() {
        // Tracing is the only account of the adoption arm / dark-desk backstop; a bare harness
        // has no subscriber.
        init_test_tracing();
        assert!(
            std::env::var("PUNKTFUNK_NO_ISOLATE").is_err(),
            "PUNKTFUNK_NO_ISOLATE forces Topology::Extend — this case needs Exclusive"
        );
        let _topology = ExclusiveTopology::force();
        let physicals_before = active_physicals();
        assert!(
            !physicals_before.is_empty(),
            "no external physical panel is active, so 'the panel came back' cannot be observed — \
             power the display on first (a TV in standby reads as Code 45 / zero CCD paths)"
        );
        println!("physicals before          : {physicals_before:?}");

        super::super::manager::FAIL_NEXT_ISOLATES.store(1, std::sync::atomic::Ordering::Relaxed);

        let mut vd1 = PfVdisplayDisplay::new().expect("open pf-vdisplay (member 1)");
        vd1.set_client_identity(Some([0xA1; 32]));
        let out1 = vd1
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create member 1");
        thread::sleep(Duration::from_secs(2));
        // Member 1's isolate was injected to fail. If the panel is already dark here, IddCx
        // auto-activation poisoned member 2's snapshot at birth. If still lit, the break is
        // downstream (adoption never fired, or restore/backstop could not re-light).
        let physicals_after_m1 = active_physicals();
        println!(
            "after member 1 (isolate INJECTED to fail): {:?}",
            active_targets()
        );
        println!("physicals after member 1  : {physicals_after_m1:?}  <- poisoned-at-birth probe");

        let mut vd2 = PfVdisplayDisplay::new().expect("open pf-vdisplay (member 2)");
        vd2.set_client_identity(Some([0xB2; 32]));
        let out2 = vd2
            .create(Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            })
            .expect("create member 2");
        thread::sleep(Duration::from_secs(2));
        let during = active_physicals();
        println!(
            "after member 2 (isolate REAL)            : {:?}",
            active_targets()
        );
        println!("physicals during                        : {during:?}");

        // Seam must have been consumed — otherwise no isolate ran and a pass proves nothing.
        assert_eq!(
            super::super::manager::FAIL_NEXT_ISOLATES.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the injected isolate failure was never consumed — no isolate ran, so this run proves \
             nothing (is the topology really Exclusive?)"
        );

        drop(out2);
        drop(out1);
        // Bounded poll, not a fixed sleep: the reaper tick + restore + async PnP removal stack up
        // to a box-dependent settle (the old 6 s undershot the default linger outright).
        let physicals_after = wait_for_physicals(Duration::from_secs(20)).unwrap_or_default();
        println!("physicals after teardown  : {physicals_after:?}");
        assert!(
            !physicals_after.is_empty(),
            "the operator's physical panel was left DEACTIVATED after teardown (sweep §5 3.2). \
             Active targets now: {:?}.\n\
             Which candidate this run implicates — read it off the poisoned-at-birth probe above:\n\
             * physicals after member 1 was EMPTY ({m1_empty}) -> the snapshot member 2 adopted was \
             already poisoned: the panel went dark at member 1's create (IddCx auto-activation), so \
             the adopted topology records 'panel off' and restoring it faithfully restores darkness. \
             Adoption is working; the SNAPSHOT SOURCE is the defect.\n\
             * physicals after member 1 was NON-empty -> poisoning is excluded; the break is \
             downstream. Check the trace for 'adopting this member's' (the adoption arm) and for \
             'no external physical display active after the restore' (the dark-desk backstop). A \
             missing adoption line means teardown's restore was never gated on; a backstop line \
             followed by a non-zero force-EXTEND rc means the remedy itself failed.",
            active_targets(),
            m1_empty = physicals_after_m1.is_empty()
        );
    }

    /// What `/display/monitors` answers on Windows. Read-only against a live host.
    #[test]
    #[ignore = "hardware: reads the live display topology"]
    fn live_windows_monitor_enumeration_reports_the_physical_screens() {
        let ms = crate::monitors::list_windows().expect("list_windows");
        for m in &ms {
            println!(
                "connector={:<14} enabled={:<5} managed={:<5} primary={:<5} {:>5}x{:<5} @{:>3}Hz  \
                 pos=({},{})  {:?}",
                m.connector,
                m.enabled,
                m.managed,
                m.primary,
                m.width,
                m.height,
                m.refresh_mhz / 1000,
                m.x,
                m.y,
                m.description
            );
        }
        assert!(!ms.is_empty(), "no monitors enumerated at all");
        assert!(
            ms.iter().any(|m| !m.managed),
            "every enumerated head is one of OURS — the operator's physical screen is still missing"
        );
    }

    /// Active display targets as `(target_id, friendly)`. A count cannot tell "physical still
    /// lit" from "physical deactivated, virtual took its place" — both are `1` on a single panel.
    fn active_targets() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// Surface manager/backend `tracing` on stdout for a live case. Decision points (isolate
    /// ladder, snapshot adoption, dark-desk backstop) have no other account. `with_test_writer`
    /// routes through the harness. Idempotent: the global default can be set once per process.
    /// `RUST_LOG` still wins; default is `debug` for our crates.
    fn init_test_tracing() {
        use tracing_subscriber::{fmt, EnvFilter};
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("pf_vdisplay=debug,pf_win_display=debug"));
        let _ = fmt().with_env_filter(filter).with_test_writer().try_init();
    }

    fn active_physicals() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active && t.external_physical)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// Poll for the operator's panel to come back, up to `budget`. Teardown is timer-driven even
    /// at `KeepAlive::Off` (the linger reaper ticks at 500 ms), the CCD restore then settles, and
    /// PnP removal is async — a fixed sleep undershoots on a slow box and wastes time on a fast
    /// one. `Some(panel set)` the moment one is active; `None` on budget.
    fn wait_for_physicals(budget: Duration) -> Option<Vec<(u32, String)>> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let p = active_physicals();
            if !p.is_empty() {
                return Some(p);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// `SDC_TOPOLOGY_EXTEND` needs something to extend ACROSS — which is the state its callers
    /// are in, and why an isolated probe of the preset misleads.
    ///
    /// `force_extend_topology` both de-clones a fresh IddCx monitor and serves as
    /// `restore_displays_ccd`'s dark-desk backstop. Probed alone with one connected display it
    /// returns rc=31 ERROR_GEN_FAILURE (nothing to extend across) and reads inert; this case is
    /// the on-glass measurement that it works where it actually fires — active paths
    /// `1 -> (virtual up) 1 -> (after force-EXTEND) 2`, both real call sites running with the
    /// virtual still present (the restore fires BEFORE the REMOVE). The same run caught the clone
    /// hazard live: only the forced EXTEND gave the arriving virtual its own active path.
    ///
    /// ⚠️ Residual: a restore that fails once the virtual is already gone is back to one
    /// connected display, where EXTEND returns 31 and cannot re-light anything.
    ///
    /// Reports the counts rather than pinning a topology — which answer is "correct" depends on
    /// the box. It does assert the desk is not left with zero active paths.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_force_extend_with_a_virtual_display_present() {
        init_test_tracing();
        // Pin the lifecycle like the adoption case: without `KeepAlive::Off` the dropped monitor
        // lingers (or pins, on a gaming-rig box) into the next case AND the teardown assertions
        // below sample before any restore ran.
        let _policy = ExclusiveTopology::force();
        let before = active_targets();
        assert!(
            !active_physicals().is_empty(),
            "no external physical panel is active at the start — power the display on first \
             (a TV in standby reads as Code 45 / zero CCD paths); active now: {before:?}"
        );
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        thread::sleep(Duration::from_secs(2));
        let with_virtual = active_targets();
        let physicals_with_virtual = active_physicals();
        pf_win_display::win_display::force_extend_topology();
        thread::sleep(Duration::from_secs(2));
        let after_extend = active_targets();
        drop(vout);
        // Bounded poll, not a fixed sleep — same settle stack as the adoption case above.
        let panel_back = wait_for_physicals(Duration::from_secs(20));
        let after_drop = active_targets();
        println!("force-EXTEND on glass, ACTIVE TARGETS at each step:");
        println!("  before          : {before:?}");
        println!("  virtual up      : {with_virtual:?}   (physicals: {physicals_with_virtual:?})");
        println!("  after force-EXT : {after_extend:?}");
        println!("  virtual dropped : {after_drop:?}");
        assert!(
            !after_drop.is_empty(),
            "the desk was left with NO active display path after the teardown"
        );
        assert!(
            panel_back.is_some(),
            "the operator's physical panel was left DEACTIVATED after teardown: {after_drop:?}"
        );
    }

    /// Live in-place resize (`#[ignore]`, needs a v4 driver and the host service stopped).
    /// Create at one mode, acquire the same slot at another: UPDATE_MODES path. Success is
    /// the same OS target id plus the committed active resolution.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_inplace_resize() {
        // Surface manager/backend tracing; a bare harness has no subscriber.
        init_test_tracing();
        // `None` = CCD query failed in this session — a "never activated" verdict would be
        // an artifact of the test context.
        let active0 = pf_win_display::win_display::count_other_active(&[]);
        println!("spike: CCD active paths visible before create: {active0:?}");
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
        let first = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        let t1 = first
            .win_capture
            .as_ref()
            .expect("no capture target")
            .target_id;
        thread::sleep(Duration::from_secs(2)); // let the activation/settle fully quiesce
                                               // A window-drag-shaped mode the ADD never advertised.
        let t0 = std::time::Instant::now();
        let second = vd
            .create(Mode {
                width: 2356,
                height: 1332,
                refresh_hz: 60,
            })
            .expect("in-place resize acquire");
        let resize_ms = t0.elapsed().as_millis();
        let wc2 = second.win_capture.as_ref().expect("no capture target");
        let t2 = wc2.target_id;
        let in_place = t1 == t2;
        let active = pf_win_display::win_display::active_resolution(
            pf_win_display::win_display::CcdTargetKey::new(wc2.adapter_luid, wc2.target_id),
        );
        println!(
            "in-place resize spike: in_place={in_place} (target {t1} -> {t2}) took {resize_ms} ms, \
             active resolution now {active:?}"
        );
        assert_eq!(
            active,
            Some((2356, 1332)),
            "the new mode did not become the active resolution"
        );
        assert!(
            in_place,
            "the resize fell back to re-arrival (target id changed) — UPDATE_MODES path not taken"
        );
        drop(second);
        drop(first);
    }
}
