//! Windows virtual-display backend driving **SudoVDA** (the SudoMaker Virtual Display Adapter —
//! the Indirect Display Driver the Apollo Sunshine-fork ships). The Windows analogue of the
//! Linux per-compositor backends: [`create`](VirtualDisplay::create) adds a virtual monitor at the
//! client's exact `WxH@Hz` (the mode is baked into the ADD IOCTL — no EDID seeding), starts the
//! mandatory watchdog ping, and the returned [`VirtualOutput`]'s keepalive `Drop` removes it (RAII).
//!
//! Control surface (verified live against SudoVDA 0.2.1): a device-interface-GUID + `CreateFileW`
//! + `DeviceIoControl` IOCTL protocol. No DLL, no named pipe. See `docs/windows-host.md`.

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
// (CCD `Devices::Display` + `Graphics::Gdi` imports moved with the display helpers to `win_display`.)
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::manager::{AddedMonitor, MonitorKey, VdisplayDriver};
use super::{Mode, VirtualDisplay, VirtualOutput};

// SudoVDA device-interface GUID (Common/Include/sudovda-ioctl.h).
const SUVDA_INTERFACE: GUID = GUID::from_u128(0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D);

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0).
const fn ctl(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}
const IOCTL_ADD: u32 = ctl(0x800);
const IOCTL_REMOVE: u32 = ctl(0x801);
const IOCTL_SET_RENDER_ADAPTER: u32 = ctl(0x802); // == 0x0022_2008
const IOCTL_GET_WATCHDOG: u32 = ctl(0x803);
/// pf-vdisplay extension (NOT in SudoVDA): tear down every virtual monitor. Sent once on host startup
/// to reap monitors orphaned by a crashed/killed previous host. SudoVDA returns invalid (ignored).
const IOCTL_CLEAR_ALL: u32 = ctl(0x804);
const IOCTL_DRIVER_PING: u32 = ctl(0x888);
const IOCTL_GET_VERSION: u32 = ctl(0x8FF);

/// A UNIQUE-per-session SudoVDA monitor GUID. The monitor is keyed by GUID for IOCTL_ADD/REMOVE, so a
/// FIXED GUID makes overlapping sessions (a client reconnecting after a freeze before the old session
/// has torn down, or genuine concurrent sessions) all map to the SAME monitor — then one session's
/// IOCTL_REMOVE on teardown tears the monitor down OUT FROM UNDER a still-live session ("display
/// disconnected" sound + freeze, even with no context change — observed live). Make it unique per
/// (process, session): base GUID with the low 48-bit node = (pid << 16 | session#).
fn next_monitor_guid() -> GUID {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id() as u128;
    GUID::from_u128(0x70756E6B_7466_756E_6B30_000000000000u128 | (pid << 16) | (n & 0xFFFF))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddParams {
    width: u32,
    height: u32,
    refresh: u32,
    guid: GUID,
    device_name: [u8; 14],
    serial: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddOut {
    luid: LUID,
    target_id: u32,
}

// SET_RENDER_ADAPTER input — byte-identical to SudoVDA's `{ LUID AdapterLuid; }` (8 bytes). The
// windows `LUID` is `{ LowPart: u32, HighPart: i32 }` == the C `LUID`, so `#[repr(C)]` is exact.
#[repr(C)]
#[derive(Clone, Copy)]
struct SetRenderAdapterParams {
    luid: LUID,
}

/// Pin the SudoVDA IDD's RENDER GPU to `luid` (Apollo's `SetRenderAdapter`). No output buffer. MUST be
/// issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target renders on — on a
/// multi-adapter box (SudoVDA IDD + a discrete GPU) this stops DXGI from reparenting the virtual
/// output onto a different adapter than the one we duplicate/encode on (the ACCESS_LOST storm).
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let p = SetRenderAdapterParams { luid };
    let bytes = std::slice::from_raw_parts(
        &p as *const _ as *const u8,
        size_of::<SetRenderAdapterParams>(),
    );
    let mut none: [u8; 0] = [];
    ioctl(h, IOCTL_SET_RENDER_ADAPTER, bytes, &mut none)
        .map(|_| ())
        .context("SudoVDA SET_RENDER_ADAPTER")
}

#[repr(C)]
struct RemoveParams {
    guid: GUID,
}

/// One `DeviceIoControl` round trip (METHOD_BUFFERED). `input`/`output` may be empty.
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

unsafe fn open_device() -> Result<HANDLE> {
    let hdev = SetupDiGetClassDevsW(
        Some(&SUVDA_INTERFACE),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    )
    .context("SetupDiGetClassDevsW(SudoVDA) — is the SudoVDA driver installed?")?;

    let mut idata = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    SetupDiEnumDeviceInterfaces(hdev, None, &SUVDA_INTERFACE, 0, &mut idata)
        .context("SetupDiEnumDeviceInterfaces(SudoVDA)")?;

    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &idata, None, 0, Some(&mut required), None);
    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    SetupDiGetDeviceInterfaceDetailW(hdev, &idata, Some(detail), required, None, None)
        .context("SetupDiGetDeviceInterfaceDetailW(SudoVDA)")?;

    let handle = CreateFileW(
        PCWSTR((*detail).DevicePath.as_ptr()),
        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(SudoVDA device)")?;
    let _ = SetupDiDestroyDeviceInfoList(hdev);
    Ok(handle)
}

/// The SudoVDA IOCTL surface behind the shared [`VirtualDisplayManager`](super::manager::VirtualDisplayManager)
/// (Goal-1 §2.5) — the only SudoVDA-specific code left; the monitor lifecycle is the shared state machine.
pub(crate) struct SudoVdaDriver;

impl VdisplayDriver for SudoVdaDriver {
    fn name(&self) -> &'static str {
        "sudovda"
    }

    unsafe fn open(&self) -> Result<(OwnedHandle, u32)> {
        let device = unsafe { open_device()? };
        let mut ver = [0u8; 4];
        if unsafe { ioctl(device, IOCTL_GET_VERSION, &[], &mut ver) }.is_ok() {
            tracing::info!(
                "SudoVDA protocol {}.{}.{} (test={})",
                ver[0],
                ver[1],
                ver[2],
                ver[3]
            );
        }
        let mut wd = [0u8; 8];
        let watchdog_s = if unsafe { ioctl(device, IOCTL_GET_WATCHDOG, &[], &mut wd) }.is_ok() {
            u32::from_le_bytes([wd[0], wd[1], wd[2], wd[3]]).max(1)
        } else {
            3
        };
        tracing::info!("SudoVDA watchdog timeout {}s", watchdog_s);
        // Reap monitors orphaned by a crashed previous host (SudoVDA returns invalid for CLEAR_ALL —
        // ignored; pf-vdisplay honors it).
        let mut none: [u8; 0] = [];
        if unsafe { ioctl(device, IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        }
        // Take ownership — the OwnedHandle CloseHandle's the control device on drop (it was leaked before).
        Ok((unsafe { OwnedHandle::from_raw_handle(device.0 as _) }, watchdog_s))
    }

    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
    ) -> Result<AddedMonitor> {
        // SET_RENDER_ADAPTER (opt-in). On this box SudoVDA IGNORES the pin and the IDD lands on a different
        // adapter than its DXGI output is enumerated under — the cross-GPU ACCESS_LOST source — so the
        // manager only pins under PUNKTFUNK_RENDER_ADAPTER / IDD-push.
        if let Some(luid) = render_luid {
            match unsafe { set_render_adapter(dev, luid) } {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                Err(e) => tracing::warn!("SudoVDA SET_RENDER_ADAPTER failed (continuing): {e:#}"),
            }
        }
        let mut device_name = [0u8; 14];
        let nm = b"punktfunk";
        device_name[..nm.len()].copy_from_slice(nm);
        let session_guid = next_monitor_guid();
        let add = AddParams {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_hz,
            guid: session_guid,
            device_name,
            serial: [0u8; 14],
        };
        let add_bytes = unsafe {
            std::slice::from_raw_parts(&add as *const _ as *const u8, size_of::<AddParams>())
        };
        let mut out = [0u8; size_of::<AddOut>()];
        unsafe { ioctl(dev, IOCTL_ADD, add_bytes, &mut out) }.with_context(|| {
            format!(
                "SudoVDA ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        let ao = unsafe { *(out.as_ptr() as *const AddOut) };
        tracing::info!(
            "SudoVDA created {}x{}@{} (target_id={}, adapter_luid={:#x})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            ao.target_id,
            ao.luid.LowPart
        );
        if let Some(luid) = render_luid {
            if ao.luid.LowPart == luid.LowPart && ao.luid.HighPart == luid.HighPart {
                tracing::info!("SudoVDA ADD render adapter matches the pinned GPU (pin took)");
            } else {
                tracing::warn!(
                    add = format!("{:08x}:{:08x}", ao.luid.HighPart, ao.luid.LowPart),
                    pinned = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA ADD render adapter DIFFERS from pinned — driver ignored SET_RENDER_ADAPTER?"
                );
            }
        }
        Ok(AddedMonitor {
            key: MonitorKey::Guid(session_guid),
            target_id: ao.target_id,
            luid: ao.luid,
        })
    }

    unsafe fn remove_monitor(&self, dev: HANDLE, key: &MonitorKey) -> Result<()> {
        let MonitorKey::Guid(guid) = key else {
            anyhow::bail!("sudovda: unexpected monitor key kind");
        };
        let rp = RemoveParams { guid: *guid };
        let rp_bytes = unsafe {
            std::slice::from_raw_parts(&rp as *const _ as *const u8, size_of::<RemoveParams>())
        };
        let mut none: [u8; 0] = [];
        unsafe { ioctl(dev, IOCTL_REMOVE, rp_bytes, &mut none) }.map(|_| ())
    }

    unsafe fn ping(&self, dev: HANDLE) -> Result<()> {
        let mut none: [u8; 0] = [];
        unsafe { ioctl(dev, IOCTL_DRIVER_PING, &[], &mut none) }.map(|_| ())
    }
}

/// The Windows SudoVDA virtual-display backend. A marker — the lifecycle lives in the shared
/// [`VirtualDisplayManager`](super::manager::VirtualDisplayManager).
pub struct SudoVdaDisplay;

impl SudoVdaDisplay {
    pub fn new() -> Result<Self> {
        super::manager::init(Box::new(SudoVdaDriver)).open_backend()?;
        Ok(Self)
    }
}

impl VirtualDisplay for SudoVdaDisplay {
    fn name(&self) -> &'static str {
        "sudovda"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        super::manager::vdm().acquire(mode)
    }
}

/// Readiness probe: can we open the SudoVDA control device?
pub fn probe() -> Result<()> {
    let h = unsafe { open_device()? };
    unsafe {
        let _ = CloseHandle(h);
    }
    Ok(())
}

/// Is the SudoVDA driver present (device interface enumerable)?
pub fn is_available() -> bool {
    unsafe { open_device().map(|h| CloseHandle(h)).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Live hardware round trip — skipped unless `PUNKTFUNK_SUDOVDA_LIVE=1` (needs the SudoVDA
    /// driver installed). Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    fn live_create_drop() {
        if std::env::var("PUNKTFUNK_SUDOVDA_LIVE").is_err() {
            return;
        }
        let mut vd = SudoVdaDisplay::new().expect("open SudoVDA");
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
