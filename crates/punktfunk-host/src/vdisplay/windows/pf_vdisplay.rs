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
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
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

/// True if `e`'s chain carries the IddCx monitor-slot-exhaustion wedge HRESULT (0x80070490,
/// `ERROR_NOT_FOUND`) — the `IOCTL_ADD` failure that ghost-PDO accumulation produces. The hex code is
/// locale-invariant (the OS message text is not), so we match on it.
fn is_slot_exhaustion_wedge(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("0x80070490")
}

/// Pin the pf-vdisplay IddCx's RENDER GPU to `luid` (the analogue of Apollo's `SetRenderAdapter`). No
/// output buffer. Issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target
/// renders on — on a multi-adapter box this stops DXGI from reparenting the virtual output onto a
/// different adapter than the one we duplicate/encode on (the ACCESS_LOST storm).
///
/// NOTE: the pf-vdisplay driver currently returns `STATUS_NOT_IMPLEMENTED` for this IOCTL (a STEP-4
/// stub), so this call WILL fail today. Callers tolerate the `Err` (warn + continue) — exactly as the
/// SudoVDA backend tolerated the driver IGNORING the pin.
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

unsafe fn open_device() -> Result<HANDLE> {
    let hdev = SetupDiGetClassDevsW(
        Some(&PF_VDISPLAY_INTERFACE),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    )
    .context("SetupDiGetClassDevsW(pf-vdisplay) — is the pf-vdisplay driver installed?")?;

    let mut idata = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    SetupDiEnumDeviceInterfaces(hdev, None, &PF_VDISPLAY_INTERFACE, 0, &mut idata)
        .context("SetupDiEnumDeviceInterfaces(pf-vdisplay)")?;

    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &idata, None, 0, Some(&mut required), None);
    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    SetupDiGetDeviceInterfaceDetailW(hdev, &idata, Some(detail), required, None, None)
        .context("SetupDiGetDeviceInterfaceDetailW(pf-vdisplay)")?;

    let handle = CreateFileW(
        PCWSTR((*detail).DevicePath.as_ptr()),
        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(pf-vdisplay device)")?;
    let _ = SetupDiDestroyDeviceInfoList(hdev);
    Ok(handle)
}

/// The pf-vdisplay IOCTL surface behind the shared [`VirtualDisplayManager`](super::manager::VirtualDisplayManager)
/// (Goal-1 §2.5) — the wire contract is owned by `pf_driver_proto::control` (versioned, hard-checked).
pub(crate) struct PfVdisplayDriver;

impl VdisplayDriver for PfVdisplayDriver {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    unsafe fn open(&self) -> Result<(OwnedHandle, u32)> {
        // SAFETY: `open_device` is `unsafe` only because it issues SetupAPI enumeration + `CreateFileW`
        // FFI; it takes no arguments and returns an owned raw `HANDLE` (or `Err`). Called here on the
        // backend-init thread, with no precondition beyond a valid thread context.
        let device = unsafe { open_device()? };
        // HARD protocol-version check (unlike SudoVDA's best-effort log): a mismatched host/driver pair
        // fails loudly here rather than corrupting the IOCTL stream.
        let mut info_buf = [0u8; size_of::<control::InfoReply>()];
        // SAFETY: `ioctl` requires `h` to be a valid device handle and its slices to be valid for the
        // call. `device` is the live handle just returned by `open_device`. `IOCTL_GET_INFO` takes no
        // input (`&[]`) and writes into `info_buf`, a stack `[u8; size_of::<InfoReply>()]` whose length
        // is passed as the output size — so `DeviceIoControl` can't write OOB — and which outlives this
        // synchronous call.
        unsafe { ioctl(device, control::IOCTL_GET_INFO, &[], &mut info_buf) }
            .context("pf-vdisplay IOCTL_GET_INFO (version handshake)")?;
        let info: control::InfoReply =
            bytemuck::pod_read_unaligned(&info_buf[..size_of::<control::InfoReply>()]);
        if info.protocol_version != pf_driver_proto::PROTOCOL_VERSION {
            // SAFETY: `device` is the valid raw handle from `open_device` and has NOT yet been wrapped
            // in an `OwnedHandle` (that happens only on the success path below), so this error path is
            // the sole owner closing it exactly once — no double-close.
            unsafe {
                let _ = CloseHandle(device);
            }
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
        // Reap monitors orphaned by a crashed previous host — a FIRST-CLASS op (driver returns SUCCESS).
        let mut none: [u8; 0] = [];
        // SAFETY: `device` is the live handle from `open_device` (still owned here, before it is wrapped
        // below). `IOCTL_CLEAR_ALL` has no input and no output: `&[]` and the empty `none` slice pass
        // zero-length buffers, so nothing is read or written through them.
        if unsafe { ioctl(device, control::IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
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
        Ok((
            // SAFETY: `device` is the valid handle from `open_device`, still owned here and NOT closed
            // on this success path (the error paths above close it and return). `from_raw_handle`'s
            // contract — caller owns a valid handle — holds, so ownership transfers cleanly into the
            // `OwnedHandle`: exactly one owner, which `CloseHandle`s it on drop.
            unsafe { OwnedHandle::from_raw_handle(device.0 as _) },
            watchdog_s,
        ))
    }

    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
    ) -> Result<AddedMonitor> {
        let session_id = next_session_id();
        let add = control::AddRequest {
            session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            preferred_monitor_id,
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
        add_res.with_context(|| {
            format!(
                "pf-vdisplay ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        // `pod_read_unaligned` (NOT `from_bytes`): `out` is a stack `[u8; N]` with no guaranteed 4-byte
        // alignment, and `from_bytes` PANICS on a mismatch. This copies into an aligned `AddReply`.
        let reply: control::AddReply =
            bytemuck::pod_read_unaligned(&out[..size_of::<control::AddReply>()]);
        let luid = LUID {
            LowPart: reply.adapter_luid_low,
            HighPart: reply.adapter_luid_high,
        };
        tracing::info!(
            "pf-vdisplay created {}x{}@{} (target_id={}, adapter_luid={:#x})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            reply.target_id,
            luid.LowPart
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
        if let Some(pin) = render_luid {
            if luid.LowPart == pin.LowPart && luid.HighPart == pin.HighPart {
                tracing::info!("pf-vdisplay ADD render adapter matches the pinned GPU (pin took)");
            } else {
                tracing::warn!(
                    add = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    pinned = format!("{:08x}:{:08x}", pin.HighPart, pin.LowPart),
                    "pf-vdisplay ADD render adapter DIFFERS from pinned — driver ignored SET_RENDER_ADAPTER?"
                );
            }
        }
        Ok(AddedMonitor {
            key: MonitorKey::Session(session_id),
            target_id: reply.target_id,
            luid,
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
}

impl PfVdisplayDisplay {
    pub fn new() -> Result<Self> {
        super::manager::init(Box::new(PfVdisplayDriver)).open_backend()?;
        Ok(Self { client_fp: None })
    }
}

impl VirtualDisplay for PfVdisplayDisplay {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        super::manager::vdm().acquire(mode, self.client_fp)
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
