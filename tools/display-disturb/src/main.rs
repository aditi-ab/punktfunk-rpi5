//! `display-disturb` — deterministic display-stack disturbance generator (Windows).
//!
//! The vdisplay stall-immunity bench (design: punktfunk-planning
//! `design/vdisplay-disturbance-immunity.md`) needs both stall classes reproducible on demand,
//! without waiting for a standby TV or a monitor-tool storm:
//!
//! * `ddc` — Class 2: DDC/CI traffic through the win32k → dxgkrnl → miniport I2C path, exactly
//!   what Twinkle-Tray/PowerDisplay-class tools emit after every HPD blip (one VCP read ≈ 100 ms,
//!   a capabilities string up to ~1 s — serialized per physical I2C bus). Requires a physical
//!   monitor; virtual displays expose no DDC handle.
//! * `modeset` — Class 1: a same-mode `ChangeDisplaySettingsExW(CDS_RESET)` re-commit — a
//!   Level-Two modeset-class DDI entry that idles the whole adapter ("the graphics hardware is
//!   idle") without changing anything Win32-visible.
//!
//! Every operation prints `epoch_ms op target duration_ms result` so stalls in a concurrent
//! stream's host.log correlate line-for-line. The per-op duration is itself measurement: it is
//! the I2C/modeset service time the GPU driver spent, per disturbance.
//!
//! Usage: `display-disturb ddc [--interval-ms 2000] [--caps] [--vcp 0x10]`
//!        `display-disturb modeset [--interval-ms 2000]`

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("display-disturb is Windows-only (it exercises the WDDM display stack).");
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
fn main() {
    win::main()
}

#[cfg(target_os = "windows")]
mod win {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows::{
        core::{BOOL, PCWSTR},
        Win32::{
            Devices::Display::{
                CapabilitiesRequestAndCapabilitiesReply, DestroyPhysicalMonitor,
                GetCapabilitiesStringLength, GetNumberOfPhysicalMonitorsFromHMONITOR,
                GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, PHYSICAL_MONITOR,
            },
            Foundation::{HANDLE, LPARAM, RECT},
            Graphics::Gdi::{
                ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplayMonitors,
                EnumDisplaySettingsW, CDS_RESET, DEVMODEW, DISPLAY_DEVICEW,
                DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_MIRRORING_DRIVER,
                DISP_CHANGE_SUCCESSFUL, ENUM_CURRENT_SETTINGS, HDC, HMONITOR,
            },
        },
    };

    fn epoch_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// One timed op: print the correlation line the bench greps for.
    fn report(op: &str, target: &str, took: Duration, ok: bool) {
        println!(
            "{} {op} {target} took_ms={} ok={ok}",
            epoch_ms(),
            took.as_millis()
        );
    }

    struct Args {
        mode: String,
        interval: Duration,
        caps: bool,
        vcp: u8,
    }

    fn parse_args() -> Args {
        let argv: Vec<String> = std::env::args().collect();
        let mode = argv.get(1).cloned().unwrap_or_default();
        if !matches!(mode.as_str(), "ddc" | "modeset") {
            eprintln!(
                "usage: display-disturb <ddc|modeset> [--interval-ms N] [--caps] [--vcp 0xNN]"
            );
            std::process::exit(2);
        }
        let mut a = Args {
            mode,
            interval: Duration::from_millis(2000),
            caps: false,
            vcp: 0x10, // brightness — universally implemented, read-only harmless
        };
        let mut i = 2;
        while i < argv.len() {
            match argv[i].as_str() {
                "--interval-ms" => {
                    i += 1;
                    a.interval = Duration::from_millis(
                        argv.get(i).and_then(|s| s.parse().ok()).unwrap_or(2000),
                    );
                }
                "--caps" => a.caps = true,
                "--vcp" => {
                    i += 1;
                    let s = argv.get(i).map(String::as_str).unwrap_or("0x10");
                    a.vcp = u8::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0x10);
                }
                other => {
                    eprintln!("unknown arg: {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        a
    }

    pub fn main() {
        let args = parse_args();
        eprintln!(
            "display-disturb: mode={} interval={}ms (Ctrl-C to stop) — correlate `took_ms` lines \
             against the host log's stall reports",
            args.mode,
            args.interval.as_millis()
        );
        match args.mode.as_str() {
            "ddc" => ddc_loop(&args),
            _ => modeset_loop(&args),
        }
    }

    // ---- Class 2: the DDC hammer ----

    /// Collect the desktop's HMONITORs (the DDC handles hang off them).
    fn monitors() -> Vec<HMONITOR> {
        unsafe extern "system" fn cb(mon: HMONITOR, _dc: HDC, _rc: *mut RECT, out: LPARAM) -> BOOL {
            // SAFETY: `out.0` is the `&mut Vec<HMONITOR>` this enumeration call passed in below,
            // alive for the whole synchronous enumeration.
            unsafe { &mut *(out.0 as *mut Vec<HMONITOR>) }.push(mon);
            BOOL(1)
        }
        let mut v: Vec<HMONITOR> = Vec::new();
        // SAFETY: null dc/clip = enumerate all display monitors; `cb` only touches the Vec whose
        // address rides in LPARAM for the duration of this synchronous call.
        let _ =
            unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize)) };
        v
    }

    /// The physical (DDC-capable) monitors behind one HMONITOR, with their descriptions.
    fn physical_monitors(mon: HMONITOR) -> Vec<(HANDLE, String)> {
        let mut count = 0u32;
        // SAFETY: valid HMONITOR from enumeration; `count` is a valid out-param.
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(mon, &mut count) }.is_err()
            || count == 0
        {
            return Vec::new();
        }
        let mut phys = vec![PHYSICAL_MONITOR::default(); count as usize];
        // SAFETY: `phys` holds exactly `count` entries as the API requires.
        if unsafe { GetPhysicalMonitorsFromHMONITOR(mon, &mut phys) }.is_err() {
            return Vec::new();
        }
        phys.iter()
            .map(|p| {
                // Copy the field out: PHYSICAL_MONITOR is packed(1), so referencing the array
                // in place would be an unaligned reference (E0793).
                let name = p.szPhysicalMonitorDescription;
                let desc = String::from_utf16_lossy(
                    &name[..name.iter().position(|&c| c == 0).unwrap_or(0)],
                );
                (p.hPhysicalMonitor, desc.trim().to_string())
            })
            .collect()
    }

    fn ddc_loop(args: &Args) -> ! {
        loop {
            let mut any = false;
            for mon in monitors() {
                for (h, desc) in physical_monitors(mon) {
                    any = true;
                    let label = if desc.is_empty() {
                        "monitor".into()
                    } else {
                        desc.clone()
                    };
                    if args.caps {
                        // The heavy transaction: length + full capabilities string (the
                        // PowerDisplay-discovery load, ~100 ms-1 s of serialized I2C).
                        let t = Instant::now();
                        let mut len = 0u32;
                        // SAFETY: `h` is a live physical-monitor handle; `len` a valid out-param.
                        let mut ok =
                            unsafe { GetCapabilitiesStringLength(h, &mut len) } == 1 && len > 0;
                        if ok {
                            let mut buf = vec![0u8; len as usize];
                            // SAFETY: `buf` is exactly the reported capabilities length.
                            ok = unsafe { CapabilitiesRequestAndCapabilitiesReply(h, &mut buf) }
                                == 1;
                        }
                        report("ddc-caps", &label, t.elapsed(), ok);
                    } else {
                        let t = Instant::now();
                        let (mut cur, mut max) = (0u32, 0u32);
                        // SAFETY: `h` is a live physical-monitor handle; out-params are valid; a
                        // read of a standard VCP code mutates nothing monitor-side.
                        let ok = unsafe {
                            GetVCPFeatureAndVCPFeatureReply(
                                h,
                                args.vcp,
                                None,
                                &mut cur,
                                Some(&mut max),
                            )
                        } == 1;
                        report("ddc-vcp", &label, t.elapsed(), ok);
                    }
                    // SAFETY: closing the handle GetPhysicalMonitorsFromHMONITOR returned.
                    let _ = unsafe { DestroyPhysicalMonitor(h) };
                }
            }
            if !any {
                eprintln!(
                    "no DDC-capable (physical) monitor found — the ddc mode needs a physical \
                     display; virtual displays expose no DDC handle"
                );
                std::process::exit(1);
            }
            std::thread::sleep(args.interval);
        }
    }

    // ---- Class 1: the no-op modeset tick ----

    /// The first attached, non-mirroring display device (device name, e.g. `\\.\DISPLAY1`).
    fn first_active_display() -> Option<[u16; 32]> {
        for i in 0u32.. {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: null device name = enumerate adapters by index; `dd.cb` is stamped.
            if unsafe { !EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0).as_bool() } {
                return None;
            }
            if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 != 0
                && (dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER).0 == 0
            {
                return Some(dd.DeviceName);
            }
        }
        None
    }

    fn modeset_loop(args: &Args) -> ! {
        let Some(name) = first_active_display() else {
            eprintln!("no active display device found");
            std::process::exit(1);
        };
        let label = String::from_utf16_lossy(
            &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())],
        );
        loop {
            let mut dm = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: `name` is the NUL-terminated device string from enumeration; `dm.dmSize`
            // is stamped; ENUM_CURRENT_SETTINGS fills the live mode.
            let got = unsafe {
                EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm)
            }
            .as_bool();
            if !got {
                eprintln!("EnumDisplaySettingsW failed for {label}");
                std::process::exit(1);
            }
            let t = Instant::now();
            // SAFETY: re-applying the CURRENT mode with CDS_RESET — the same-mode re-commit that
            // forces the modeset path without changing anything user-visible. Null hwnd + null
            // lparam per the API contract for this flag combination.
            let rc = unsafe {
                ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_RESET, None)
            };
            report(
                "modeset-reset",
                &label,
                t.elapsed(),
                rc == DISP_CHANGE_SUCCESSFUL,
            );
            std::thread::sleep(args.interval);
        }
    }
}
