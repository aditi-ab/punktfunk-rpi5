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

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SPINT_ACTIVE,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
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
unsafe fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
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
    .with_context(|| format!("DeviceIoControl(code={code:#x})"))?;
    Ok(returned)
}

/// Reap the ghost (NOT-present) "punktfunk" virtual-monitor device nodes that `IddCxMonitorDeparture`
/// leaves behind. Each departed monitor leaves a not-present "Generic Monitor (punktfunk)" PDO that keeps
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

/// Kick the pf-vdisplay ADAPTER device (disable → enable) — the in-process equivalent of
/// `reset-pf-vdisplay.ps1` step 3. A crashed/killed WUDFHost can leave the devnode "started" yet
/// HOSTLESS (PnP Status OK, no WUDFHost process, zero device-interface instances) — a zombie no
/// session can open until the stack reloads; on-glass, only a device cycle recovered it. Called by
/// [`VdisplayDriver::open`] when `open_device` finds no openable interface; the caller retries the
/// open afterwards. Best-effort + bounded (~7 s inside the script). Returns whether a punktfunk
/// adapter devnode was found (and therefore cycled) — `false` means the driver genuinely is not
/// installed and a retry is pointless.
fn restart_vdisplay_device() -> bool {
    // Mirrors reset-pf-vdisplay.ps1's Get-PfAdapter selector ('punktfunk Virtual Display' is the INF
    // device description — locale-invariant). Same spawn shape as `reap_ghost_monitors` above.
    const CYCLE_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
        $ad = Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'punktfunk Virtual Display' } | Select-Object -First 1; \
        if ($ad) { \
            Disable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false; Start-Sleep -Seconds 3; \
            Enable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false; Start-Sleep -Seconds 3; \
            $st = (Get-PnpDevice -InstanceId $ad.InstanceId).Status; \
            if ($st -ne 'OK') { Enable-PnpDevice -InstanceId $ad.InstanceId -Confirm:$false; Start-Sleep -Seconds 2; \
                $st = (Get-PnpDevice -InstanceId $ad.InstanceId).Status }; \
            Write-Output $st \
        } else { Write-Output 'ABSENT' }";
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
            CYCLE_PS,
        ])
        .output()
    {
        Ok(o) => {
            let status = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if status == "ABSENT" {
                tracing::warn!("pf-vdisplay: no adapter devnode to cycle — driver not installed");
            } else {
                tracing::warn!(
                    %status,
                    "pf-vdisplay: cycled the adapter device (hostless-zombie recovery)"
                );
            }
            status != "ABSENT"
        }
        Err(e) => {
            tracing::warn!(error = %e, "pf-vdisplay: adapter cycle could not spawn powershell");
            false
        }
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
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let req = control::SetRenderAdapterRequest {
        luid_low: luid.LowPart,
        luid_high: luid.HighPart,
    };
    let mut none: [u8; 0] = [];
    ioctl(
        h,
        control::IOCTL_SET_RENDER_ADAPTER,
        bytemuck::bytes_of(&req),
        &mut none,
    )
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

unsafe fn open_device() -> Result<HANDLE> {
    // SAFETY: plain SetupAPI enumeration call; the returned list is solely owned by the RAII wrapper.
    let hdev = DevInfoList(
        unsafe {
            SetupDiGetClassDevsW(
                Some(&PF_VDISPLAY_INTERFACE),
                PCWSTR::null(),
                None,
                DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
            )
        }
        .context("SetupDiGetClassDevsW(pf-vdisplay) — is the pf-vdisplay driver installed?")?,
    );

    // Enumerate EVERY interface instance, not just index 0: after a driver upgrade a present-but-
    // failed devnode (Code 10) can hold index 0 while the LIVE node's interface sits at a later
    // index — the old single-index read then failed every session with "driver not installed"
    // even though a working interface existed. `SPINT_ACTIVE` filters dead interfaces (an interface
    // is active only while its owning device is started); the first active + openable one wins.
    let mut inactive = 0u32;
    let mut last_err: Option<anyhow::Error> = None;
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
            inactive += 1;
            continue;
        }
        let mut required = 0u32;
        // SAFETY: sizing call — null buffer plus a valid `required` out-param; the expected
        // ERROR_INSUFFICIENT_BUFFER "failure" is ignored and only `required` is consumed.
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(hdev.0, &idata, None, 0, Some(&mut required), None)
        };
        if (required as usize) < size_of::<u32>() {
            continue; // sizing failed — never stamp a cbSize through an under-sized buffer
        }
        let mut buf = vec![0u8; required as usize];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        // SAFETY: `buf` is `required` bytes (>= 4, checked above), so stamping `cbSize` and letting
        // the API fill up to `required` bytes stays in bounds; `detail` aliases `buf` only within
        // this iteration, and the `DevicePath` pointer is read before `buf` is dropped.
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
            Ok(h) => return Ok(h),
            // A raced-away or wedged device — remember the error, try the next interface.
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "no ACTIVE pf-vdisplay device interface found ({inactive} inactive) — is the \
             pf-vdisplay driver installed and its device started?"
        )
    }))
}

/// The pf-vdisplay IOCTL surface behind the shared [`VirtualDisplayManager`](super::manager::VirtualDisplayManager)
/// (Goal-1 §2.5) — the wire contract is owned by `pf_driver_proto::control` (versioned, hard-checked).
pub(crate) struct PfVdisplayDriver;

impl VdisplayDriver for PfVdisplayDriver {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    unsafe fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32)> {
        // SAFETY: `open_device` is `unsafe` only because it issues SetupAPI enumeration + `CreateFileW`
        // FFI; it takes no arguments and returns an owned raw `HANDLE` (or `Err`). Called here on the
        // backend-init thread, with no precondition beyond a valid thread context.
        let device = match unsafe { open_device() } {
            Ok(d) => d,
            Err(first) => {
                // No openable interface. If a WUDFHost crash left the devnode a hostless zombie
                // (validated on-glass: PnP Status OK, zero interface instances), a device cycle
                // reloads the stack — kick it once and retry the open over a short arrival window.
                if !restart_vdisplay_device() {
                    return Err(first); // no adapter devnode at all — genuinely not installed
                }
                let mut reopened = Err(first);
                for _ in 0..8 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    // SAFETY: as above — plain SetupAPI + CreateFileW FFI, no preconditions.
                    match unsafe { open_device() } {
                        Ok(d) => {
                            reopened = Ok(d);
                            break;
                        }
                        Err(e) => reopened = Err(e),
                    }
                }
                reopened.context("pf-vdisplay interface still absent after an adapter cycle")?
            }
        };
        // Wrap IMMEDIATELY: every `?` below must close the device exactly once — the old
        // wrap-on-success-only shape leaked the raw handle whenever GET_INFO itself failed.
        // SAFETY: `device` is the valid handle `open_device` just returned; ownership transfers into
        // the `OwnedHandle` (single owner, `CloseHandle` on drop).
        let device = unsafe { OwnedHandle::from_raw_handle(device.0 as _) };
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
        if info.protocol_version != pf_driver_proto::PROTOCOL_VERSION {
            anyhow::bail!(
                "pf-vdisplay protocol mismatch: host expects {}, driver reports {} — install matching \
                 host + driver",
                pf_driver_proto::PROTOCOL_VERSION,
                info.protocol_version
            );
        }
        let watchdog_s = info.watchdog_timeout_s.max(1);
        tracing::info!(
            "pf-vdisplay protocol {} (watchdog timeout {}s)",
            info.protocol_version,
            watchdog_s
        );
        // Reap monitors orphaned by a crashed previous host — a FIRST-CLASS op (driver returns
        // SUCCESS). FIRST open of the process only: a REOPEN (the manager retired a dead handle after
        // a driver upgrade / WUDFHost restart) can race sessions that still believe they are live, and
        // an unconditional CLEAR_ALL there would raze them.
        if !reap_orphans {
            reap_ghost_monitors();
            return Ok((device, watchdog_s));
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
        // OS-side not-present "Generic Monitor (punktfunk)" PDOs that a previous host-run's monitor
        // departures left behind. Reap those here so a fresh host start begins with a clean IddCx
        // monitor-slot budget — prevents the 0x80070490 slot-exhaustion wedge from carrying across
        // restarts (the reason a restart's CLEAR_ALL alone never recovered it before).
        reap_ghost_monitors();
        Ok((device, watchdog_s))
    }

    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
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
            _reserved: 0,
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
        if (n as usize) < size_of::<control::AddReply>() {
            anyhow::bail!(
                "pf-vdisplay ADD returned {n} bytes, expected {}",
                size_of::<control::AddReply>()
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

    fn set_quit_flag(&mut self, quit: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.quit = Some(quit);
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        super::manager::vdm().acquire(mode, self.client_fp, self.client_hdr, self.quit.clone())
    }
}

/// Readiness probe: can we open the pf-vdisplay control device?
pub fn probe() -> Result<()> {
    // SAFETY: `open_device` is `unsafe` only for its SetupAPI + `CreateFileW` FFI; no arguments, returns
    // an owned raw `HANDLE` (or `Err`).
    let h = unsafe { open_device()? };
    // SAFETY: `h` is the handle just opened by `open_device` in this function, owned here and not yet
    // handed anywhere else, so this closes it exactly once — no double-close, no use-after-close.
    unsafe {
        let _ = CloseHandle(h);
    }
    Ok(())
}

/// Is the pf-vdisplay driver present (device interface enumerable)?
pub fn is_available() -> bool {
    // SAFETY: `open_device` returns an owned raw `HANDLE`; on `Ok(h)` the handle is moved into the
    // closure (sole owner) and closed exactly once via `CloseHandle`, on `Err` there is nothing to
    // close — so no double-close and no leak of an opened handle. The `unsafe` covers both FFI calls.
    unsafe { open_device().map(|h| CloseHandle(h)).is_ok() }
}

/// [`is_available`], with self-heal: an interface-less driver whose adapter devnode EXISTS is the
/// hostless-zombie state a WUDFHost crash leaves behind (validated on-glass — PnP reports Status OK
/// with no WUDFHost process and zero interface instances, and every session fails at this gate until
/// the device reloads). Cycle the adapter once and re-probe over a short arrival window. A genuinely
/// uninstalled driver (no adapter devnode) fails fast without the wait.
pub fn ensure_available() -> bool {
    if is_available() {
        return true;
    }
    if !restart_vdisplay_device() {
        return false;
    }
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if is_available() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Live hardware round trip — skipped unless `PUNKTFUNK_PF_VDISPLAY_LIVE=1` (needs the pf-vdisplay
    /// driver installed). Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    fn live_create_drop() {
        if std::env::var("PUNKTFUNK_PF_VDISPLAY_LIVE").is_err() {
            return;
        }
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
}
