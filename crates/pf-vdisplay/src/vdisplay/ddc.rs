//! DDC/CI monitor panel power control — the EXPERIMENTAL `ddc_power_off` display-policy axis.
//!
//! DDC/CI is the VESA command channel to the monitor itself: an I²C bus inside the video cable
//! (dedicated pins on VGA/DVI/HDMI, tunneled over the AUX channel on DisplayPort) whose MCCS
//! "VCP codes" expose the monitor's OSD knobs to software. VCP 0xD6 is the power mode; we command
//! `0x04` (DPMS off — panel + backlight dark, firmware still listening) and never `0x05`
//! (power-button off — many monitors kill their DDC controller in that state and need a physical
//! button press to come back).
//!
//! Why: the "periodic double-jolt while the virtual display is the SOLE active display" stutter
//! class (Apollo #179/#358/#368/#563/#776 and our own field report). When an `Exclusive` isolate
//! deactivates the physical monitor, its link drops and the monitor falls into its no-signal flow:
//! standby with periodic auto-input-scan / link probing that the GPU driver services with
//! display-subsystem stalls at a seconds-scale cadence. A panel commanded off over DDC/CI believes
//! it has an owner and (on cooperating firmware) stops probing. This is deliberately shipped as an
//! experiment: whether it helps discriminates *who initiates* the churn — monitor firmware (DDC-off
//! fixes it) vs. the driver servicing a dark head regardless (only a driven link fixes it, i.e.
//! topology `primary`/`extend`).
//!
//! Everything here is best-effort and warn-and-continue: monitors without DDC/CI support (or with
//! it disabled in the OSD), docks/KVMs that don't pass the channel through, and laptop-internal
//! panels (ACPI backlight, no DDC) all simply probe as unsupported and are skipped. Each DDC
//! transaction can block for tens of ms — callers run at session acquire/teardown, never on the
//! frame path.

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

/// MCCS VCP code 0xD6 — display power mode.
const VCP_POWER_MODE: u8 = 0xD6;
/// VCP 0xD6 value: on.
const POWER_ON: u32 = 0x01;
/// VCP 0xD6 value: DPMS off (dark panel, DDC controller stays responsive). Deliberately NOT 0x05.
const POWER_OFF: u32 = 0x04;

/// One active display: its HMONITOR and GDI device name (`\\.\DISPLAYn`).
struct ActiveMonitor {
    hmon: HMONITOR,
    device: String,
}

/// Enumerate the active displays (HMONITOR + GDI name). HMONITORs are only valid while a display
/// is part of the desktop — which is exactly why the off-command must run BEFORE a CCD isolate
/// and the on-command AFTER the restore.
fn active_monitors() -> Vec<ActiveMonitor> {
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut windows::Win32::Foundation::RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: `data` is the `&mut Vec<ActiveMonitor>` passed by `active_monitors` below,
        // valid for the duration of the synchronous EnumDisplayMonitors call that invokes us.
        let out = unsafe { &mut *(data.0 as *mut Vec<ActiveMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `hmon` is the live monitor handle the enumeration just handed us; `info` is a
        // properly-sized MONITORINFOEXW local whose cbSize is set, which GetMonitorInfoW requires
        // to safely write the extended (szDevice) variant.
        if unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo) }.as_bool() {
            let len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            out.push(ActiveMonitor {
                hmon,
                device: String::from_utf16_lossy(&info.szDevice[..len]),
            });
        }
        true.into() // keep enumerating
    }

    let mut out: Vec<ActiveMonitor> = Vec::new();
    // SAFETY: `collect` matches MONITORENUMPROC; `&mut out` outlives the synchronous enumeration
    // and is only dereferenced inside the callback (single-threaded — user32 invokes it inline).
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut Vec<ActiveMonitor> as isize),
        )
    };
    out
}

/// Apply `value` to VCP 0xD6 on every physical monitor behind `hmon` that answers a 0xD6 probe.
/// Returns how many panels acknowledged the set. `device` is for the log lines only.
fn set_power(hmon: HMONITOR, device: &str, value: u32) -> u32 {
    let mut n = 0u32;
    // SAFETY: `hmon` is a live monitor handle from the enumeration; `n` is a valid out-param.
    if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut n) }.is_err() || n == 0 {
        return 0;
    }
    let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
    // SAFETY: `phys` is sized to exactly the count the API just reported for this handle.
    if unsafe { GetPhysicalMonitorsFromHMONITOR(hmon, &mut phys) }.is_err() {
        return 0;
    }
    let mut acked = 0u32;
    for p in &phys {
        // PHYSICAL_MONITOR is `packed(1)` (dxva2 header pragma) — copy the fields OUT by value
        // before touching them; a reference into a packed field is rejected (E0793, UB).
        let handle = p.hPhysicalMonitor;
        let desc_raw = p.szPhysicalMonitorDescription;
        let len = desc_raw
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc_raw.len());
        let desc = String::from_utf16_lossy(&desc_raw[..len]);
        // Probe first: a monitor without DDC/CI (or with it disabled in the OSD, or behind a
        // dock/KVM that drops the channel) fails here and is skipped — never blind-write to a
        // bus we can't read.
        let (mut current, mut max) = (0u32, 0u32);
        // SAFETY: `handle` is the live physical-monitor handle (valid until
        // DestroyPhysicalMonitors below); the value pointers are valid locals ('None' for the
        // code-type out-param we don't need).
        let probe = unsafe {
            GetVCPFeatureAndVCPFeatureReply(
                handle,
                VCP_POWER_MODE,
                None,
                &mut current,
                Some(&mut max),
            )
        };
        if probe == 0 {
            tracing::debug!(
                device,
                monitor = desc,
                "DDC/CI: no reply to the power-mode (0xD6) probe — skipping (no DDC/CI, \
                 disabled in the OSD, or not passed through)"
            );
            continue;
        }
        // SAFETY: as the probe above — same live physical-monitor handle, plain value args.
        let set = unsafe { SetVCPFeature(handle, VCP_POWER_MODE, value) };
        if set == 0 {
            tracing::warn!(
                device,
                monitor = desc,
                value,
                "DDC/CI: power-mode set failed after a successful probe"
            );
        } else {
            tracing::info!(
                device,
                monitor = desc,
                from = current,
                to = value,
                "DDC/CI: panel power mode commanded"
            );
            acked += 1;
        }
    }
    // SAFETY: `phys` holds exactly the handles GetPhysicalMonitorsFromHMONITOR opened for us;
    // each is destroyed once, here.
    if let Err(e) = unsafe { DestroyPhysicalMonitors(&phys) } {
        tracing::debug!(device, "DDC/CI: DestroyPhysicalMonitors failed: {e}");
    }
    acked
}

/// Command every physical panel EXCEPT `exclude_gdi` (the virtual display) off via DDC/CI
/// (VCP 0xD6 → DPMS off). Call while the physical displays are still ACTIVE — i.e. immediately
/// before the `Exclusive` CCD isolate. Returns how many panels acknowledged.
pub fn panel_off_except(exclude_gdi: &str) -> u32 {
    let mut acked = 0;
    for m in active_monitors() {
        if m.device.eq_ignore_ascii_case(exclude_gdi) {
            continue;
        }
        acked += set_power(m.hmon, &m.device, POWER_OFF);
    }
    if acked == 0 {
        // INFO, not debug: the user opted into this axis, so "it did nothing" is an answer they
        // asked for. The common case is a laptop — internal eDP/LVDS panels have NO DDC/CI at
        // all (their brightness/power runs over the driver's own channel), so `ddc_power_off`
        // is structurally a no-op for them (reporter feedback 2026-07-27).
        tracing::info!(
            "DDC/CI: no panel accepted the DPMS-off command — the ddc_power_off axis did \
             nothing on this display set (internal eDP/LVDS panels expose no DDC/CI; external \
             monitors may have it disabled in the OSD or dropped by a dock/KVM)"
        );
    }
    acked
}

/// Best-effort wake: command ON to every physical panel that answers. Call AFTER the CCD restore
/// has re-activated the physical paths — the returning signal alone wakes DPMS-off panels on most
/// firmware; this is the belt-and-braces for the rest.
pub fn panel_on_all() -> u32 {
    let mut acked = 0;
    for m in active_monitors() {
        acked += set_power(m.hmon, &m.device, POWER_ON);
    }
    acked
}
