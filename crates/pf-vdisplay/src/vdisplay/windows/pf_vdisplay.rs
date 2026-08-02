//! Windows virtual-display backend driving **pf-vdisplay** — punktfunk's OWN IddCx Indirect Display
//! Driver (the clean-room replacement for SudoVDA). The Windows analogue of the Linux per-compositor
//! backends: [`create`](VirtualDisplay::create) adds a virtual monitor at the client's exact `WxH@Hz`
//! (the mode is baked into the ADD IOCTL — no EDID seeding), starts the mandatory watchdog ping, and
//! the returned [`VirtualOutput`]'s keepalive `Drop` removes it (RAII).
//!
//! Control surface: a device-interface-GUID + `CreateFileW` + `DeviceIoControl` IOCTL protocol, with
//! the wire contract OWNED by [`pf_driver_proto::control`] (versioned + `#[repr(C)] Pod` structs,
//! NOT the SudoVDA ABI). No DLL, no named pipe. See `design/windows-host-rewrite.md`.
//!
//! This is a faithful clone of [`super::sudovda`] (the shipping fallback) repointed at the new driver:
//! same reference-counted/lingering monitor lifecycle, same CCD isolation + active-mode forcing — those
//! backend-NEUTRAL helpers are REUSED from `sudovda` (a pf-vdisplay monitor's `target_id` is a real OS
//! target id, so the CCD/DXGI code works unchanged). Only the driver-specific bits (GUID, IOCTL codes,
//! request/reply structs, the version handshake) differ, per `pf_driver_proto`.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

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

// pf-vdisplay device-interface GUID (pf_driver_proto::PF_VDISPLAY_INTERFACE_GUID_U128). Deliberately
// NOT SudoVDA's `{e5bcc234-…}` — we own this driver, so a private interface GUID signals it and avoids
// any accidental coexistence with a real SudoVDA install.
const PF_VDISPLAY_INTERFACE: GUID =
    GUID::from_u128(pf_driver_proto::PF_VDISPLAY_INTERFACE_GUID_U128);

/// Monotonic per-session id keying a pf-vdisplay monitor for `IOCTL_ADD`/`IOCTL_REMOVE`. Unlike
/// SudoVDA's 16-byte GUID + pid-mangling, the proto keys monitors by a plain `u64` — the host-level
/// refcount manager (MGR) owns collision safety (a stale session can never REMOVE a live one), so a
/// simple monotonic counter suffices. Unique per (process, session) within this host's lifetime.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// One `DeviceIoControl` round trip (METHOD_BUFFERED). `input`/`output` may be empty. Identical to the
/// SudoVDA backend's wrapper; struct<->bytes conversion happens at the call sites via `bytemuck`.
///
/// # Safety
///
/// `h` must be a live handle to the pf-vdisplay control device — one returned by [`open_device`]
/// and not yet closed. Every other obligation is discharged inside: the two buffer pointers are
/// derived from the caller's slices and are passed with exactly those slices' lengths, and the
/// slices outlive the call.
unsafe fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
    // SAFETY: `h` is a live control-device handle by this fn's contract. `inp`/`outp` are derived
    // from `input`/`output` and paired with those slices' own lengths, so the kernel reads exactly
    // `input.len()` initialised bytes and writes at most `output.len()` bytes it is entitled to;
    // both slices are borrowed for the whole call. `Some(&mut returned)` is a live local. This is
    // METHOD_BUFFERED, so the kernel copies through its own system buffer rather than retaining
    // either pointer, and `None` for the OVERLAPPED makes the call synchronous — nothing outlives
    // the call to alias.
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

/// Reap the ghost (NOT-present) "punktfunk" virtual-monitor device nodes that `IddCxMonitorDeparture`
/// leaves behind. Each departed monitor leaves a not-present "Generic Monitor (Punktfunk)" PDO that keeps
/// pinning an OS VidPN target against the IddCx adapter's fixed monitor-slot budget; once ~16 accumulate,
/// `IOCTL_ADD` wedges at 0x80070490 (`ERROR_NOT_FOUND`) and every session black-screens until a manual
/// reset/reboot. Removing the not-present PDOs frees the slots — the in-process equivalent of
/// `reset-pf-vdisplay.ps1` step 2 (proven on-box). Best-effort + idempotent: only NOT-present nodes
/// (`Status != OK`) are removed, so the LIVE session's monitor (`Status OK`) is never touched; any
/// failure is logged and swallowed. Returns the number removed.
fn reap_ghost_monitors() -> u32 {
    // Mirrors reset-pf-vdisplay.ps1 step 2. powershell is always present for the SYSTEM service; the
    // matched tokens ('OK', 'punktfunk', the InstanceId) are locale-invariant, so this is safe on a
    // non-English box (unlike a .ps1 *file* read in the machine codepage).
    const REAP_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $g = Get-PnpDevice -Class Monitor | Where-Object { $_.Status -ne 'OK' -and $_.FriendlyName -match 'punktfunk' }; \
        $n = 0; foreach ($d in $g) { pnputil /remove-device $d.InstanceId *> $null; if ($LASTEXITCODE -eq 0) { $n++ } }; \
        Write-Output $n";
    // Resolve powershell by full path — the LocalSystem service's PATH is not guaranteed to include
    // System32 — with a bare-name fallback.
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
            let n = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .unwrap_or(0);
            if n > 0 {
                tracing::warn!(
                    reaped = n,
                    "pf-vdisplay: reaped ghost (not-present) virtual-monitor nodes — IddCx slot-exhaustion prevention"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: ghost-monitor reap could not spawn powershell");
            0
        }
    }
}

/// What an adapter-cycle attempt actually DID — deliberately NOT the devnode's PnP status afterwards.
/// The old script reported that status, and a device it had failed to touch at all still reads `OK`,
/// so a no-op cycle was indistinguishable from a real one in the log (field report 2026-08-02: a
/// woken host logged `cycled … status=OK` and then failed the session for a missing interface).
enum AdapterCycle {
    /// The driver stack was genuinely reloaded. `how` names the lever that worked.
    Reloaded { how: &'static str, status: String },
    /// No punktfunk adapter devnode exists at all — the driver is not installed and retrying is
    /// pointless.
    NotInstalled,
    /// A devnode exists but could not be reloaded; carries the reason (already whitespace-collapsed).
    Refused(String),
}

/// Reload the pf-vdisplay ADAPTER device — the in-process equivalent of `reset-pf-vdisplay.ps1`
/// step 3. A crashed/killed WUDFHost can leave the devnode "started" yet HOSTLESS (PnP Status OK, no
/// WUDFHost process, zero device-interface instances) — a zombie no session can open until the stack
/// reloads; on-glass, only a device reload recovered it.
///
/// Two levers, in order. `Disable-PnpDevice` + `Enable-PnpDevice` is the one `reset-pf-vdisplay.ps1`
/// uses — but that script stops the host service FIRST, precisely because the host holds the driver's
/// control device open (its step 1), and a disable can be refused for a device in use. This runs
/// INSIDE the host, so it structurally cannot take that step: the retired-but-never-closed handles in
/// [`DeviceSlot`](super::manager) are still open on the very device being disabled. So a refusal is
/// the expected case here, not the exotic one, and `pnputil /restart-device` — which reloads a device
/// that is in use — is the fallback. Whichever runs, the failure paths re-enable, so a half-completed
/// cycle can never leave the adapter DISABLED.
///
/// Best-effort + bounded (~6 s inside the script).
fn reload_vdisplay_adapter() -> AdapterCycle {
    // Mirrors reset-pf-vdisplay.ps1's Get-PfAdapter selector ('punktfunk Virtual Display' is the INF
    // device description — locale-invariant). Same spawn shape as `reap_ghost_monitors` above; the
    // reported tokens are ours, so parsing them is locale-invariant too.
    //
    // Every step that can fail is `-ErrorAction Stop` inside a `try` — the old script ran the whole
    // cycle under `SilentlyContinue` and then reported `(Get-PnpDevice …).Status`, which reports the
    // DEVICE, not the cycle: a disable that was refused left the device untouched, started, and
    // reading `OK`, so the host logged a successful recovery it had never performed.
    //
    // `$LASTEXITCODE = 1` before the pnputil call for the same reason: no native command runs before
    // it, so an unlaunchable pnputil would otherwise leave the variable holding whatever it held and
    // let "never ran" read as "returned 0". Pre-seeding a failure means only a real exit 0 reports a
    // reload. pnputil is resolved by full path — a LocalSystem service's PATH need not include
    // System32.
    const CYCLE_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $ad = Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'punktfunk Virtual Display' } | Select-Object -First 1; \
        if (-not $ad) { Write-Output 'ABSENT'; exit }; \
        $id = $ad.InstanceId; $err = ''; \
        try { \
            Disable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop; Start-Sleep -Seconds 2; \
            try { Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop } \
            catch { Start-Sleep -Seconds 2; Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop }; \
            Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED cycle ' + (Get-PnpDevice -InstanceId $id).Status); exit \
        } catch { $err = ($_.Exception.Message -replace '\\s+', ' ') }; \
        $pnp = ($env:SystemRoot + '\\System32\\pnputil.exe'); $LASTEXITCODE = 1; \
        if (Test-Path $pnp) { & $pnp /restart-device $id *> $null }; \
        if ($LASTEXITCODE -eq 0) { Start-Sleep -Seconds 2; \
            Write-Output ('RELOADED restart ' + (Get-PnpDevice -InstanceId $id).Status) } \
        else { Enable-PnpDevice -InstanceId $id -Confirm:$false; Write-Output ('REFUSED ' + $err) }";
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

/// Parse [`reload_vdisplay_adapter`]'s script output. Split out to be testable without a box: the
/// bug this whole change answers was a recovery that MISreported its own outcome, so the decoding of
/// that outcome is worth pinning down.
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
            // Held as `&'static str` so the two levers stay distinguishable in a field report:
            // `restart` means the disable was refused, i.e. something still holds the device open —
            // worth knowing when a reload does not fix the box.
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
        // Covers `REFUSED <reason>` and anything unrecognised, including an empty stdout (powershell
        // died before writing). All of them mean an un-reloaded devnode, which is the only thing
        // callers act on; the text rides along for the log.
        _ => AdapterCycle::Refused(if rest.trim().is_empty() {
            format!("unexpected adapter-reload output: {out:?}")
        } else {
            rest.trim().to_string()
        }),
    }
}

/// True if `e`'s chain carries the IddCx monitor-slot-exhaustion wedge HRESULT (0x80070490,
/// `ERROR_NOT_FOUND`) — the `IOCTL_ADD` failure that ghost-PDO accumulation produces. The hex code is
/// locale-invariant (the OS message text is not), so we match on it.
fn is_slot_exhaustion_wedge(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("0x80070490")
}

/// Pin the pf-vdisplay IddCx's RENDER GPU to `luid` (the analogue of Apollo's `SetRenderAdapter`). No
/// output buffer. Issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target
/// renders on — on a multi-adapter box this stops DXGI from reparenting the virtual output onto a
/// different adapter than the one we duplicate/encode on (the ACCESS_LOST storm). The driver
/// implements it (`control.rs` → `adapter::set_render_adapter`); callers still tolerate an `Err`
/// (warn + continue) since the driver reports its real render LUID in the shared header either way.
///
/// # Safety
///
/// `h` must be a live handle to the pf-vdisplay control device — [`ioctl`]'s obligation, and the
/// only one. `luid` is plain `Copy` data with no validity requirement of its own.
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let req = control::SetRenderAdapterRequest {
        luid_low: luid.LowPart,
        luid_high: luid.HighPart,
    };
    let mut none: [u8; 0] = [];
    // SAFETY: `h` is a live control-device handle by this fn's contract — the one thing `ioctl`
    // asks of its caller. The request is a `Pod` struct viewed through `bytemuck::bytes_of`, so
    // the input slice is exactly its initialised bytes, and the empty output slice matches the
    // IOCTL's "no output buffer" contract.
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

/// Deliver a monitor's sealed frame channel to the driver: the handle values `req` carries were just
/// duplicated into the driver's WUDFHost by the IDD-push capturer's broker (`idd_push::ChannelBroker`),
/// and on IOCTL success the DRIVER owns them. No output buffer. The caller reaps the remote duplicates
/// on failure (the broker's `DUPLICATE_CLOSE_SOURCE` sweep) so no path leaks WUDFHost handles.
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_frame_channel(dev: HANDLE, req: &control::SetFrameChannelRequest) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: per this fn's contract `dev` is the live control handle. `bytes_of(req)` borrows the
    // caller's request for the duration of this synchronous call as the input bytes; `none` is empty,
    // so there is no output buffer.
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

/// Deliver a monitor's hardware-cursor section (`IOCTL_SET_CURSOR_CHANNEL`, proto v5) — the
/// cursor sibling of [`send_frame_channel`], same delivery/ownership contract.
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_cursor_channel(
    dev: HANDLE,
    req: &control::SetCursorChannelRequest,
) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: per this fn's contract `dev` is the live control handle; `bytes_of(req)` borrows the
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

/// Flip a LIVE monitor's hardware-cursor declaration (`IOCTL_SET_CURSOR_FORWARD`, proto v6) —
/// the mid-stream cursor-render flip. Fails against a pre-v6 driver (unknown IOCTL); callers
/// log and keep the declared-at-ADD behavior.
///
/// # Safety
/// `dev` must be a live pf-vdisplay control handle (see [`super::manager::control_device_handle`]).
pub unsafe fn send_cursor_forward(
    dev: HANDLE,
    req: &control::SetCursorForwardRequest,
) -> Result<()> {
    let mut none: [u8; 0] = [];
    // SAFETY: per this fn's contract `dev` is the live control handle; `bytes_of(req)` borrows the
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

/// RAII over a SetupAPI device-info list: every exit path of [`open_device`] destroys it (the error
/// paths used to leak one `HDEVINFO` per failed open — and a driverless / mid-upgrade box probes
/// repeatedly).
struct DevInfoList(HDEVINFO);

impl Drop for DevInfoList {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the live device-info list this wrapper solely owns; destroyed exactly
        // once here.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// What a device-interface enumeration found. The counts are what let [`ensure_available`] tell a
/// devnode that is MID-TRANSITION (present, interface registered, not started yet — resuming from
/// sleep, restarting, reloading) apart from one that is genuinely gone. Only the second is worth
/// answering with device surgery; cycling the first only lengthens the outage it is waiting out.
struct Probe {
    /// The control handle, if any interface instance opened.
    handle: Option<OwnedHandle>,
    /// Instances seen with `SPINT_ACTIVE` set — the owning device is started.
    active: u32,
    /// Instances seen with `SPINT_ACTIVE` clear — registered, but the owning device is not started.
    inactive: u32,
    /// The last enumeration/open failure, kept for the diagnostic.
    last_err: Option<anyhow::Error>,
}

impl Probe {
    /// No interface instance of ANY kind. With an adapter devnode present this is the hostless-zombie
    /// state a WUDFHost crash leaves; with none, the driver is not installed. Either way, waiting
    /// alone will not fix it.
    fn is_absent(&self) -> bool {
        self.handle.is_none() && self.active == 0 && self.inactive == 0
    }

    /// Why no handle came back, NAMING what was seen — "0 interfaces" and "1 inactive interface" are
    /// completely different diagnoses (not installed vs. still coming up), and the old message
    /// collapsed both into "is the driver installed?". Call only on a miss; a hit reports as much.
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

    /// Consume into the [`open_device`] result.
    fn into_result(mut self) -> Result<OwnedHandle> {
        match self.handle.take() {
            Some(h) => Ok(h),
            None => Err(self.into_error()),
        }
    }
}

/// Open the pf-vdisplay control device.
///
/// SAFE, and owning. It has no caller obligation — it takes no arguments and every precondition is
/// internal — so the `unsafe fn` it used to be pushed a proof burden onto four call sites that had
/// nothing to prove; two of them then re-established ownership by hand with `CloseHandle`, a shape
/// this file has already leaked from once (see the wrap-IMMEDIATELY comment in `open`). Returning an
/// `OwnedHandle` makes the close a `Drop`, so there is exactly one way to get it wrong: not at all.
fn open_device() -> Result<OwnedHandle> {
    probe_device().into_result()
}

/// [`open_device`], reporting WHAT it found rather than only whether it succeeded.
fn probe_device() -> Probe {
    let mut probe = Probe {
        handle: None,
        active: 0,
        inactive: 0,
        last_err: None,
    };
    // SAFETY: plain SetupAPI enumeration call; the returned list is solely owned by the RAII wrapper.
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

    // Enumerate EVERY interface instance, not just index 0: after a driver upgrade a present-but-
    // failed devnode (Code 10) can hold index 0 while the LIVE node's interface sits at a later
    // index — the old single-index read then failed every session with "driver not installed"
    // even though a working interface existed. `SPINT_ACTIVE` filters dead interfaces (an interface
    // is active only while its owning device is started); the first active + openable one wins.
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
        // Against the struct's own size, not `u32`'s: the value stamped into `cbSize` below is
        // `size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()`, so that is what the buffer must hold.
        if (required as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            continue; // sizing failed — never stamp a cbSize through an under-sized buffer
        }
        // `u64`, not `u8`: this buffer is written through as `SP_DEVICE_INTERFACE_DETAIL_DATA_W`,
        // which needs 4-byte alignment, and a `Vec<u8>` only promises 1. The old SAFETY comment
        // proved bounds and aliasing and was silent on alignment — the one obligation the code did
        // not actually discharge.
        let mut buf = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // SAFETY: `buf` is at least `required` bytes and aligned to 8 (so also to the struct's 4),
        // so stamping `cbSize` and letting the API fill up to `required` bytes stays in bounds;
        // `detail` aliases `buf` only within this iteration, and the `DevicePath` pointer is read
        // before `buf` is dropped.
        let opened = unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, Some(detail), required, None, None)
                .context("SetupDiGetDeviceInterfaceDetailW(pf-vdisplay)")
                .and_then(|()| {
                    CreateFileW(
                        PCWSTR((*detail).DevicePath.as_ptr()),
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
                // SAFETY: `h` is the handle `CreateFileW` just returned to THIS call and nothing
                // else holds it, so transferring it into the `OwnedHandle` gives it a single owner
                // that closes it exactly once on drop.
                probe.handle = Some(unsafe { OwnedHandle::from_raw_handle(h.0 as _) });
                return probe;
            }
            // A raced-away or wedged device — remember the error, try the next interface.
            Err(e) => probe.last_err = Some(e),
        }
    }
    probe
}

/// The pf-vdisplay IOCTL surface behind the shared [`VirtualDisplayManager`](super::manager::VirtualDisplayManager)
/// (Goal-1 §2.5) — the wire contract is owned by `pf_driver_proto::control` (versioned, hard-checked).
pub(crate) struct PfVdisplayDriver;

impl VdisplayDriver for PfVdisplayDriver {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    unsafe fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32, u32)> {
        // A short re-probe, and deliberately NO adapter reload — this replaces the second, impatient
        // copy of the recovery that used to live here. Session bring-up already ran the full
        // `ensure_available` before constructing the backend, so anything left for this open to
        // absorb is a race, not a wedge. `hw_cursor_capable` also lands here, mid client handshake,
        // where a reload's tens of seconds would be entirely the wrong trade for one capability bool
        // — and where reloading would deadlock besides, since `ensure_device` calls us holding the
        // manager's `device` mutex (see the `RECOVERY` ordering contract).
        let device = wait_for_interface(BRIEF_RETRY, false).0?;
        // `open_device` hands back an `OwnedHandle`, so every `?` below closes the device exactly
        // once by construction — the shape this used to reach by wrapping the raw handle here, and
        // which leaked whenever GET_INFO itself failed before that wrap was moved up.
        let raw = HANDLE(device.as_raw_handle());
        // HARD protocol-version check (unlike SudoVDA's best-effort log): a mismatched host/driver pair
        // fails loudly here rather than corrupting the IOCTL stream.
        let mut info_buf = [0u8; size_of::<control::InfoReply>()];
        // SAFETY: `ioctl` requires `h` to be a valid device handle and its slices to be valid for the
        // call. `raw` borrows the live `OwnedHandle` above for this synchronous call. `IOCTL_GET_INFO`
        // takes no input (`&[]`) and writes into `info_buf`, a stack `[u8; size_of::<InfoReply>()]`
        // whose length is passed as the output size — so `DeviceIoControl` can't write OOB — and which
        // outlives this synchronous call.
        let n = unsafe { ioctl(raw, control::IOCTL_GET_INFO, &[], &mut info_buf) }
            .context("pf-vdisplay IOCTL_GET_INFO (version handshake)")?;
        // Fail closed on a short driver reply instead of decoding trusted-looking zeros — the decoded
        // `protocol_version` (and below, the ADD reply's pid/luid/target) gate host behavior, so a
        // buggy/compromised driver under-writing the buffer must not be silently trusted
        // (security-review 2026-07-17).
        if (n as usize) < size_of::<control::InfoReply>() {
            anyhow::bail!(
                "pf-vdisplay IOCTL_GET_INFO returned {n} bytes, expected {}",
                size_of::<control::InfoReply>()
            );
        }
        let info: control::InfoReply =
            bytemuck::pod_read_unaligned(&info_buf[..size_of::<control::InfoReply>()]);
        // HARD floor/ceiling instead of strict equality since v4: v4 is ADDITIVE over v3
        // (IOCTL_UPDATE_MODES — the in-place resize), so this host still drives a v3 driver and
        // simply gates the in-place path on the reported version (re-arrival fallback). Anything
        // below the floor or ABOVE this host's own version stays a loud failure.
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
        if info.protocol_version < pf_driver_proto::PROTOCOL_VERSION {
            tracing::warn!(
                "pf-vdisplay protocol {} (host supports {}): driver lacks the in-place resize — \
                 mid-stream resizes use the monitor re-arrival path until the driver is updated",
                info.protocol_version,
                pf_driver_proto::PROTOCOL_VERSION
            );
        } else {
            tracing::info!(
                "pf-vdisplay protocol {} (watchdog timeout {}s)",
                info.protocol_version,
                watchdog_s
            );
        }
        // Reap monitors orphaned by a crashed previous host — a FIRST-CLASS op (driver returns
        // SUCCESS). FIRST open of the process only: a REOPEN (the manager retired a dead handle after
        // a driver upgrade / WUDFHost restart) can race sessions that still believe they are live, and
        // an unconditional CLEAR_ALL there would raze them.
        if !reap_orphans {
            reap_ghost_monitors();
            return Ok((device, watchdog_s, info.protocol_version));
        }
        let mut none: [u8; 0] = [];
        // SAFETY: `raw` borrows the live `OwnedHandle` above. `IOCTL_CLEAR_ALL` has no input and no
        // output: `&[]` and the empty `none` slice pass zero-length buffers, so nothing is read or
        // written through them.
        if unsafe { ioctl(raw, control::IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        } else {
            tracing::warn!("pf-vdisplay IOCTL_CLEAR_ALL failed on startup (continuing)");
        }
        // CLEAR_ALL only departs the driver's own (in-process) monitor list; it can NOT remove the
        // OS-side not-present "Generic Monitor (Punktfunk)" PDOs that a previous host-run's monitor
        // departures left behind. Reap those here so a fresh host start begins with a clean IddCx
        // monitor-slot budget — prevents the 0x80070490 slot-exhaustion wedge from carrying across
        // restarts (the reason a restart's CLEAR_ALL alone never recovered it before).
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
        // The client display's volume rides into the monitor's EDID CTA HDR block; all-zero =
        // unknown → the driver keeps its built-in defaults (also what an un-upgraded driver, which
        // reads only the legacy 24-byte prefix, does).
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
            // v5 cursor channel: the driver declares an IddCx hardware cursor for this monitor
            // (DWM stops compositing the pointer into the frame); the capture layer delivers the
            // CursorShm section right after its ring. Zero toward older drivers is harmless —
            // the host only sets this when the handshake-reported proto is >= 5.
            hw_cursor: hw_cursor as u32,
        };
        // SET_RENDER_ADAPTER (opt-in; pf-vdisplay IMPLEMENTS it). Non-fatal on failure: the driver reports
        // its real render LUID in the shared header, so the host binds correctly even if this is ignored.
        if let Some(luid) = render_luid {
            // SAFETY: `add_monitor`'s `# Safety` contract guarantees `dev` is the live control handle,
            // which is `set_render_adapter`'s precondition; we forward it unchanged. `luid` is a plain
            // `Copy` `LUID` passed by value — no borrow crosses the call.
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
        // SAFETY: per `add_monitor`'s contract `dev` is the live control handle. `bytemuck::bytes_of(&add)`
        // borrows the local `AddRequest` (alive across this synchronous call) as the input bytes, and
        // `out` is a stack `[u8; size_of::<AddReply>()]` whose length bounds the kernel's write — both
        // buffers outlive the call.
        let add_res = unsafe { ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out) };
        let add_res = match add_res {
            Err(e) if is_slot_exhaustion_wedge(&e) => {
                // The IddCx monitor-slot pool is exhausted by accumulated ghost (departed-but-not-present)
                // virtual-monitor PDOs → ADD failed 0x80070490. Reap the ghosts in-process and retry ONCE
                // so the wedge SELF-HEALS instead of hard-failing every session until a manual reset/reboot
                // (the long-standing failure mode). pnputil removal is synchronous; a brief settle lets the
                // OS recompute the adapter's monitor budget before the retry.
                let reaped = reap_ghost_monitors();
                tracing::warn!(
                    reaped,
                    "pf-vdisplay ADD wedged (0x80070490 ERROR_NOT_FOUND) — reaped ghost monitor nodes, retrying ADD"
                );
                // pnputil removal is durable (the ghosts are gone permanently), but the OS reclaims the
                // IddCx VidPN-target slots via ASYNC PnP teardown that can lag the synchronous pnputil
                // return. Retry the ADD a few times (300 ms apart, NO re-reap — the ghosts are already
                // removed) to ride out that variable reclaim latency rather than guess one magic settle.
                // ~1.5 s worst case, only on the rare wedge path.
                let mut res = Err(anyhow::anyhow!("pf-vdisplay ADD retry loop did not run"));
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // SAFETY: identical to the first IOCTL_ADD above — `dev` is the live control handle
                    // (`add_monitor`'s contract), and `bytemuck::bytes_of(&add)` + `&mut out` borrow locals
                    // that outlive this synchronous call.
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
        // Fail closed on a short reply — `target_id`/`wudf_pid`/`luid` below feed OpenProcess + the
        // WUDFHost verification, so don't decode a partially-written (zeroed) reply as authoritative.
        // The LEGACY size, not the full struct: an un-upgraded driver writes only the prefix before
        // the `cursor_excluded` tail; `out` is zero-initialized, so the missing tail reads `0`
        // (= unknown/clean — exactly what a driver that can't track declares should report).
        if (n as usize) < control::ADD_REPLY_LEGACY_SIZE {
            // The IOCTL SUCCEEDED — the driver has already created the monitor and taken an IddCx
            // slot; only its reply was short. Bailing without undoing that leaks both, and the slot
            // pool is small enough that ~16 leaks wedge every later ADD at 0x80070490 (the wedge the
            // ghost-reap above exists to recover from). Compensate with the REMOVE this session's id
            // addresses, then fail.
            let req = control::RemoveRequest { session_id };
            let mut none: [u8; 0] = [];
            // SAFETY: `dev` is the live control handle (`add_monitor`'s contract); `bytes_of(&req)`
            // borrows a local alive across this synchronous call, and `none` is the empty output the
            // IOCTL expects.
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
        // `pod_read_unaligned` (NOT `from_bytes`): `out` is a stack `[u8; N]` with no guaranteed 4-byte
        // alignment, and `from_bytes` PANICS on a mismatch. This copies into an aligned `AddReply`.
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
        // Per-client identity diagnostic: did the driver honor the host's preferred (stable) monitor id?
        // A pre-Phase-2 driver leaves resolved_monitor_id=0 (it ignored the field); a current driver echoes
        // the id it actually used. A mismatch means this session fell back to an auto id, so Windows won't
        // reapply this client's saved per-monitor config (scaling) until it gets its stable id back.
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
        // NOTE: `reply.adapter_luid` is the IddCx DISPLAY adapter
        // (`IDARG_OUT_MONITORARRIVAL.OsAdapterLuid`), NOT the render GPU, so it can NOT validate
        // SET_RENDER_ADAPTER — a comparison against the pin here fired "DIFFERS from pinned" on
        // every ADD (verified on-glass: reply 0x22c05 vs pin 0x15b05 on a single-4090 box). The
        // driver reports its ACTUAL render adapter in the shared frame header; the IDD-push
        // capturer checks it there and rebinds on a mismatch.
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
        // borrows the local `UpdateModesRequest` for the duration of this synchronous call as the
        // input bytes; `none` is empty, so there is no output buffer.
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
        // borrows the local `RemoveRequest` for the duration of this synchronous call as the input
        // bytes; `none` is empty, so there is no output buffer.
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
        // SAFETY: per `ping`'s contract `dev` is the live control handle. `IOCTL_PING` has no input
        // (`&[]`) and no output (`none` is empty), so no memory is read or written through the buffers.
        unsafe { ioctl(dev, control::IOCTL_PING, &[], &mut none) }.map(|_| ())
    }
}

/// The Windows pf-vdisplay virtual-display backend. Near-stateless — the lifecycle lives in the shared
/// [`VirtualDisplayManager`](super::manager::VirtualDisplayManager); it only carries the connecting
/// client's fingerprint so the manager can assign a STABLE per-client monitor id (config persistence).
pub struct PfVdisplayDisplay {
    /// The connecting client's cert fingerprint (`None` = anonymous/GameStream → the manager's auto id).
    /// Set by [`set_client_identity`](VirtualDisplay::set_client_identity) before `create`.
    client_fp: Option<[u8; 32]>,
    /// The client display's HDR colour volume (`None` = unknown/SDR → the driver's built-in EDID
    /// defaults). Set by [`set_client_hdr`](VirtualDisplay::set_client_hdr) before `create`; a
    /// freshly created monitor's EDID advertises this volume so host apps tone-map to the client's
    /// real panel.
    client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    /// Declare an IddCx hardware cursor on the created monitor (the M2c cursor channel). Set by
    /// [`set_hw_cursor`](VirtualDisplay::set_hw_cursor) before `create`; only honored when the
    /// driver handshake reported proto >= 5.
    hw_cursor: bool,
    /// The session's deliberate-quit flag (`None` = no signal → the linger policy applies). Set by
    /// [`set_quit_flag`](VirtualDisplay::set_quit_flag) before `create`; rides into every lease this
    /// backend mints so a user "stop" tears the monitor down immediately instead of lingering.
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

/// Readiness probe: can we open the pf-vdisplay control device?
pub fn probe() -> Result<()> {
    // The handle closes on drop.
    open_device().map(|_| ())
}

/// Is the pf-vdisplay driver present (device interface enumerable)?
pub fn is_available() -> bool {
    open_device().is_ok()
}

/// How often the interface is re-probed while waiting.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// How long a devnode whose interface exists but is NOT-READY (no active instance, or `CreateFileW`
/// refused) is given to come up on its own before the adapter is reloaded.
///
/// This is the wake-from-sleep window. Resuming re-enters D0 and re-registers the interface while
/// the rest of the resume storm is still running, and a client reconnecting a second after wake
/// arrives inside that gap — which the old code, probing exactly ONCE, answered by disabling and
/// re-enabling a display adapter that was seconds from being ready anyway.
const NOT_READY_GRACE: Duration = Duration::from_secs(15);

/// How long a fully ABSENT interface is given before the adapter is reloaded. Short — a hostless
/// devnode does not heal itself, and that is the case this recovery exists for — but non-zero, so a
/// resume that briefly de-registers the interface is not met with device surgery either.
const ABSENT_SETTLE: Duration = Duration::from_secs(3);

/// How long the interface is given to ARRIVE after a reload.
///
/// Was 4 s, which a quiet box meets and a box still finishing a resume does not: PnP is contended
/// right after wake. Field report 2026-08-02 — a woken host logged a successful adapter cycle and
/// then failed the session 4 s later for a missing interface, and the client could not connect.
const ARRIVAL_AFTER_RELOAD: Duration = Duration::from_secs(15);

/// Hard ceiling on the whole wait, so display prep can never block for an unbounded sum of the
/// windows above. Without it a devnode wedged NOT-READY costs the full grace, then the reload, then
/// the full arrival window before failing — the pathological case paying nearly a minute per session.
/// Patience for a device that is coming back is the point; patience for one that never will is not.
const TOTAL_BUDGET: Duration = Duration::from_secs(30);

/// The budget a caller that must NOT stall gives the interface: no adapter reload, just a short
/// re-probe to ride out a race. [`VdisplayDriver::open`] uses it — by the time the manager opens,
/// session bring-up has already run the full [`ensure_available`] above, and the OTHER path that
/// reaches it (`manager::hw_cursor_capable`, a best-effort capability answer during the client
/// handshake) must never hold the Welcome for tens of seconds to decide one bool.
const BRIEF_RETRY: Duration = Duration::from_secs(3);

/// Serializes the recovery so N sessions racing in after a wake perform ONE adapter reload between
/// them rather than N interleaved ones — each of which tears down the stack the others are waiting
/// on. The second caller through typically finds the interface already up and returns at once.
///
/// Taken ONLY by [`ensure_available`], which holds no manager lock, and released before the retire
/// hook below takes the manager's `device` mutex. That is what keeps the lock order one-way:
/// [`VdisplayDriver::open`] runs *inside* that same `device` mutex, so if it could also take this
/// lock the two orders would invert and deadlock. It cannot — it never reloads.
static RECOVERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`is_available`], with self-heal — and with PATIENCE, which is the part that matters after a
/// wake from sleep.
///
/// Returns the reason on failure instead of a bare `false`: the caller used to replace it with a
/// flat "the driver is not installed", which is what a field report showed on a box whose driver was
/// installed, started, and merely mid-resume.
pub fn ensure_available() -> Result<()> {
    // Poisoning carries no meaning here — the guard protects a `()`, not state a panic could leave
    // inconsistent — so a previous panic must not wedge every later session out of recovery.
    let (result, reloaded) = {
        let _serialize = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
        wait_for_interface(NOT_READY_GRACE, true)
    };
    // OUTSIDE the recovery lock, by the ordering contract on `RECOVERY`. A reload tore the driver
    // stack down and back up, so any control handle a previous session cached is dead by
    // construction — retire it while we know that for certain, rather than leaving the next session
    // to discover it by having an IOCTL fail. No-op before any backend opened the device.
    if reloaded {
        super::manager::invalidate_cached_device(
            "the pf-vdisplay adapter was reloaded (hostless-zombie recovery)",
        );
    }
    result.map(|_| ())
}

/// Wait for an openable control interface, reloading the adapter if `reload` and the devnode looks
/// genuinely hostless. Returns the handle (so the manager's own open can keep it) alongside whether
/// a reload ran.
///
/// Two distinguishable states hide behind "cannot open the interface", and they want opposite
/// treatment:
///
/// * **Not ready** — instances are registered but none is active (or the open is refused). The
///   devnode is THERE and coming up: resuming from sleep, restarting, reloading. It heals itself;
///   reloading the adapter underneath it only lengthens the outage.
/// * **Absent** — no instance at all. With an adapter devnode present this is the hostless-zombie
///   state a WUDFHost crash leaves (validated on-glass: PnP Status OK, no WUDFHost process, zero
///   interface instances). Only a reload clears it.
///
/// So: probe, wait out a not-ready device, reload an absent one after a short settle, and give the
/// interface a real arrival window afterwards. A reload is still attempted once at the end of
/// `not_ready_grace`, so a devnode wedged not-ready (a failed start) recovers exactly as it did
/// before. A genuinely uninstalled driver — no adapter devnode — still fails FAST, with no wait.
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
        // Track how long we have seen NOTHING. Reset by any sighting, so a device that flickers
        // between absent and not-ready is treated as the transition it is.
        if probe.is_absent() {
            absent_since.get_or_insert_with(Instant::now);
        } else {
            absent_since = None;
        }
        let absent_long_enough = absent_since.is_some_and(|t| t.elapsed() >= ABSENT_SETTLE);
        if reload && !reloaded && (absent_long_enough || Instant::now() >= deadline) {
            match reload_vdisplay_adapter() {
                // No devnode at all — waiting cannot conjure a driver. Fail immediately rather than
                // burning the arrival window on a box that simply does not have it installed.
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

    /// The recovery must not be able to claim success it did not achieve. This is the whole bug:
    /// the old script ran the cycle under `SilentlyContinue` and reported `(Get-PnpDevice).Status`,
    /// so a device whose disable had been REFUSED — untouched, still started — reported `OK`, and
    /// the host logged `cycled the adapter device … status=OK` while nothing had been cycled at all
    /// (field report 2026-08-02). A refusal must decode as a refusal, carrying its reason.
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
        // A bare device status — what the OLD script emitted on every path — must NEVER decode as a
        // successful reload now, however healthy it looks.
        for stale in ["OK", "Error", "Unknown"] {
            assert!(
                matches!(classify_reload_output(stale), AdapterCycle::Refused(_)),
                "{stale:?} is a device status, not a reload outcome"
            );
        }
    }

    /// The outcomes callers branch on: `NotInstalled` fails a session fast, `Reloaded` earns the
    /// arrival window, and the lever that worked stays visible in the log (`restart` means the
    /// disable was refused and something still holds the device open).
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
        // powershell died before writing anything — an un-reloaded devnode, so `Refused`, not a
        // silent success.
        assert!(matches!(
            classify_reload_output("   "),
            AdapterCycle::Refused(_)
        ));
    }

    /// `is_absent` is what decides between WAITING and performing device surgery, so the two states
    /// it separates are pinned here. An interface that is registered but not yet ACTIVE is a devnode
    /// mid-transition — the wake-from-sleep case — and reloading the adapter under it only lengthens
    /// the outage it is already recovering from.
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
        // And the diagnostic names what was seen — the old message collapsed every one of these
        // into "is the driver installed?", which sent a field report down the wrong path.
        assert!(probe(0, 2).into_error().to_string().contains("2 inactive"));
    }

    fn variant(c: &AdapterCycle) -> &'static str {
        match c {
            AdapterCycle::Reloaded { .. } => "Reloaded",
            AdapterCycle::NotInstalled => "NotInstalled",
            AdapterCycle::Refused(_) => "Refused",
        }
    }

    /// Live hardware round trip — `#[ignore]`d (needs the pf-vdisplay driver installed); run with
    /// `cargo test -p pf-vdisplay -- --ignored live_create_drop`. Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_create_drop() {
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
        drop(vout); // triggers REMOVE + stops the pinger
    }

    /// Forces `Topology::Exclusive` for the duration of a case and puts the operator's real policy
    /// back on drop — including when the case panics.
    ///
    /// The isolate branch this file's Phase-3 cases exercise runs ONLY under `Exclusive`, and a real
    /// install is usually configured otherwise (.173 is `"topology": "extend"`, which is why the
    /// first attempt at these cases silently never ran an isolate at all — `topology_action()`
    /// returns `effective_topology()` as soon as ANY policy is configured). Note this writes the
    /// host's `display-settings.json`; the guard is what makes that safe to do on a real box.
    struct ExclusiveTopology(crate::policy::DisplayPolicy);

    impl ExclusiveTopology {
        fn force() -> Self {
            let original = crate::policy::prefs().get();
            let mut forced = original.clone();
            forced.preset = crate::policy::Preset::Custom; // explicit fields are ignored otherwise
            forced.topology = crate::policy::Topology::Exclusive;
            crate::policy::prefs()
                .set(forced)
                .expect("force Topology::Exclusive for this case");
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

    /// Run `f` on a worker thread and give up after `budget`, so a HANG fails the case instead of
    /// wedging the box.
    ///
    /// Earned the hard way: this file's 3.2 case hung inside `create`, and killing the harness
    /// skipped every `Drop`, leaking an IddCx monitor. A few of those exhaust the driver's slot
    /// pool, after which every later run wedges too and only a reboot clears it. A bounded wait
    /// lets the harness exit NORMALLY, which is what lets the driver reap the session.
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

    /// §5 3.2 on glass: when the FIRST member's isolate fails, a later member's isolate must be
    /// ADOPTED as the group's restore snapshot — otherwise it deactivates the operator's panels
    /// with nothing able to put them back.
    ///
    /// This leg only fires on a FAILED `isolate_displays_ccd`, which real hardware does not
    /// produce, so it shipped unexercised. `manager::FAIL_NEXT_ISOLATES` (a `#[cfg(test)]` seam)
    /// fails exactly the first isolate, against the real driver and a real panel; the second member
    /// then isolates for real and the physical genuinely goes dark mid-test.
    ///
    /// The assertion is the user-visible one: after both members are torn down, the operator's
    /// external panel is ACTIVE again. Without the adoption the group holds no snapshot,
    /// `teardown_removed`'s restore is gated on it and never runs, and the panel stays deactivated.
    ///
    /// ⚠️ Two members means two SLOTS, which is what `slot_id_for(client_fp, …)` keys on — hence the
    /// two distinct client fingerprints. Needs `Topology::Exclusive`, which is the default when no
    /// policy is configured and `PUNKTFUNK_NO_ISOLATE` is unset; the test asserts an isolate really
    /// happened rather than trusting that.
    ///
    /// ⚠️ If this test leaves the desk dark, recover from the CONSOLE session with
    /// `SetDisplayConfig(0,null,0,null, SDC_USE_DATABASE_CURRENT|SDC_APPLY)` — measured rc=0 on
    /// .173. `SDC_TOPOLOGY_EXTEND` will NOT do it with a single connected display (rc=31).
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_a_failed_first_isolate_is_recovered_by_adopting_the_next() {
        // Without this the run is BLIND: the adoption arm and the dark-desk backstop announce
        // themselves only through `tracing`, and a bare test harness has no subscriber. The first
        // on-glass run could see the panel stay dark but not say WHICH link broke.
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

        // Fail EXACTLY the first member's isolate.
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
        // ⭐ THE MEASUREMENT THAT SEPARATES THE TWO CANDIDATES. Member 1's isolate was injected to
        // fail, so nothing of OURS deactivated anything here. If the operator's panel is ALREADY
        // dark at this point, the arriving IddCx monitor took the desktop on its own — and every
        // snapshot taken from here on records "panel off", so member 2's adopted snapshot is
        // POISONED AT BIRTH and restoring it faithfully restores darkness. If the panel is still
        // lit here, poisoning is excluded and the failure is downstream (adoption never fired, or
        // the restore/backstop did and could not re-light it).
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

        // The seam must have been consumed — otherwise the injection never took and a pass here
        // would prove nothing about the recovery.
        assert_eq!(
            super::super::manager::FAIL_NEXT_ISOLATES.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the injected isolate failure was never consumed — no isolate ran, so this run proves \
             nothing (is the topology really Exclusive?)"
        );

        drop(out2);
        drop(out1);
        thread::sleep(Duration::from_secs(6)); // async PnP removal + the restore settling

        let physicals_after = active_physicals();
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

    /// What `/display/monitors` will now answer on Windows — the operator's real screens.
    ///
    /// Read-only, so it is safe against a live host. Before `monitors::list_windows` existed this
    /// endpoint returned an empty list plus a LINUX error string on every Windows box (`detect()`
    /// fell through to an `XDG_CURRENT_DESKTOP` sniff), so the console could show no physical
    /// screen and could not honestly say why.
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
        // The point of the change: a real, non-managed head is visible to the console.
        assert!(
            ms.iter().any(|m| !m.managed),
            "every enumerated head is one of OURS — the operator's physical screen is still missing"
        );
    }

    /// The ACTIVE display targets, as `(target_id, friendly)` — not just a count.
    ///
    /// Counting alone cannot tell "the physical is still lit" from "the physical was deactivated
    /// and the virtual took its place", which on a single-panel box are both `1`. Every on-glass
    /// claim in this module about panels going dark rests on the identities, so read them.
    fn active_targets() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// Surface the manager/backend `tracing` output on stdout for a live case.
    ///
    /// These on-glass cases drive decision points — the isolate ladder, the snapshot-adoption arm,
    /// `restore_displays_ccd`'s dark-desk backstop — whose ONLY account of what they chose is a
    /// `tracing` event. A bare `cargo test` harness installs no subscriber, so those events go
    /// nowhere and a failing run cannot say which link broke; that is exactly what left §5 3.2's
    /// two candidates undistinguished after the first on-glass run.
    ///
    /// `with_test_writer` routes through the harness's capture, so the output appears under
    /// `--nocapture` (and on failure) rather than racing `println!`. Idempotent and non-fatal: the
    /// global default can only be set once per process, and several live cases may run in one
    /// binary, so a second call is a no-op rather than a panic that would fail an unrelated test.
    /// `RUST_LOG` still wins when set; the default is `debug` for our own crates, which is where
    /// the ladder's reasoning lives.
    fn init_test_tracing() {
        use tracing_subscriber::{fmt, EnvFilter};
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("pf_vdisplay=debug,pf_win_display=debug"));
        let _ = fmt().with_env_filter(filter).with_test_writer().try_init();
    }

    /// The active targets that are EXTERNAL PHYSICAL panels — the operator's actual desk.
    fn active_physicals() -> Vec<(u32, String)> {
        pf_win_display::win_display::target_inventory()
            .into_iter()
            .filter(|t| t.active && t.external_physical)
            .map(|t| (t.target_id, format!("{} [{}]", t.friendly, t.tech)))
            .collect()
    }

    /// `SDC_TOPOLOGY_EXTEND` needs something to extend ACROSS — and that is the state its callers
    /// are in, which is why this looked like a defect and is not.
    ///
    /// `force_extend_topology` carries two jobs: stop a fresh IddCx monitor being CLONED onto the
    /// existing panel, and serve as `restore_displays_ccd`'s last-resort "the desk is not left
    /// dark" backstop. Probed directly on .173 with only the LG TV connected, the preset returns
    /// **rc=31 ERROR_GEN_FAILURE** (`SDC_USE_DATABASE_CURRENT` returns 0), which reads like an
    /// inert backstop.
    ///
    /// On glass it is not, and this case is the measurement that settled it — active paths
    /// `1 -> (virtual up) 1 -> (after force-EXTEND) 2`:
    ///
    /// * With one connected display there is nothing to extend across, hence rc=31.
    /// * With the virtual present there are two, and the preset applies. Both real call sites run
    ///   in exactly that state — the restore fires BEFORE the REMOVE, so the virtual is still
    ///   there — so the backstop does work where it fires.
    /// * ⭐ It also caught the clone hazard live: the arriving virtual monitor did **not** get its
    ///   own active path (1 -> 1), only the forced EXTEND gave it one (-> 2). That is precisely the
    ///   "no distinct source -> no frames" case `force_extend_topology`'s own doc describes.
    ///
    /// ⚠️ Residual worth remembering rather than asserting: a restore that fails once the virtual
    /// is already gone is back to one connected display, where EXTEND returns 31 and cannot
    /// re-light anything.
    ///
    /// Reports the counts rather than pinning a topology — which answer is "correct" depends on the
    /// box. It does assert the desk is not left with zero active paths.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_force_extend_with_a_virtual_display_present() {
        let before = active_targets();
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
        thread::sleep(Duration::from_secs(6)); // PnP removal is async — a short wait reads a ghost
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
            !active_physicals().is_empty(),
            "the operator's physical panel was left DEACTIVATED after teardown: {after_drop:?}"
        );
    }

    /// Live in-place resize spike — `#[ignore]`d (needs a v4 pf-vdisplay driver installed + the host
    /// service STOPPED, single-instance guard); run with `-- --ignored live_inplace_resize`. Answers the
    /// P2 open questions on real glass with no streaming client: create at one mode, then acquire
    /// the SAME session's slot at a DIFFERENT mode — the manager's resize branch runs UPDATE_MODES
    /// → mode-advertised wait → set_active_mode → verified settle. In-place success is visible as
    /// the SAME OS target id on the second output (a re-arrival fallback mints a new one) plus the
    /// committed active resolution; the test reports which path ran and asserts the mode landed.
    #[test]
    #[ignore = "needs the pf-vdisplay driver on real hardware; run with --ignored"]
    fn live_inplace_resize() {
        // Live-run diagnostics: surface the manager/backend tracing (activation ladder, settle
        // waits, UPDATE_MODES) on stdout — a bare test harness has no subscriber, which made the
        // first on-glass run blind. `tracing-subscriber` is now a dev-dependency, so this case no
        // longer has to be re-run through the host binary to be traced.
        init_test_tracing();
        // Context probe: can this process see the CCD active-path set at all? (`None` = the query
        // itself fails in this session/window-station — the whole ladder would be blind, and a
        // "monitor never activated" verdict would be an artifact of the test context.)
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
                                               // A deliberately arbitrary (window-drag-shaped) mode the ADD never advertised.
        let t0 = std::time::Instant::now();
        let second = vd
            .create(Mode {
                width: 2356,
                height: 1332,
                refresh_hz: 60,
            })
            .expect("in-place resize acquire");
        let resize_ms = t0.elapsed().as_millis();
        let t2 = second
            .win_capture
            .as_ref()
            .expect("no capture target")
            .target_id;
        let in_place = t1 == t2;
        let active = pf_win_display::win_display::active_resolution(t2);
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
