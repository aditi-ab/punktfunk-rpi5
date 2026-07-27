//! OS display-event listener — the attribution sensor for the periodic-stutter disturbance class.
//!
//! The capture-stall watch (the IDD-push capturer in `pf-capture`) can SAY "DWM stopped composing
//! on a stable period", but not WHY. Field evidence (Apollo's Stuttering Clinic, Apollo #384,
//! Tom's HW "stutter from disabled-but-connected monitors") points at a connected-but-idle sink
//! (standby TV/monitor, active HDMI cable, KVM/AVR) re-probing the link every few seconds; the GPU
//! driver services each probe below the topology layer, and on some boxes Windows additionally
//! tears down + re-arrives the monitor's devnode each time. This module timestamps everything
//! Windows lets user mode see of that reaction so the stall log can name the disturbance instead
//! of guessing:
//!
//! - `WM_DEVICECHANGE` + `RegisterDeviceNotificationW(GUID_DEVINTERFACE_MONITOR)`: monitor device
//!   interface arrival/removal — fires on devnode churn even when the final topology is unchanged
//!   (the "reaction cascade" class `pnp_disable_monitors` suppresses), with the interface path
//!   naming WHICH monitor pulsed.
//! - `DBT_DEVNODES_CHANGED`: the broadcast catch-all for PnP tree churn (no payload).
//! - `WM_DISPLAYCHANGE`: an actual mode/topology commit reached the desktop (this one does NOT
//!   fire for a pure probe with no mode delta — its absence is itself a signal).
//!
//! A pure driver-internal probe (EDID/DDC read, DP link retrain) emits NONE of these — that
//! absence, paired with metronomic stalls, is what discriminates "driver services a standby sink
//! below the OS" from "Windows re-enumerates the monitor". Kernel-precise attribution (DxgKrnl ETW
//! event 272 `DxgkCbIndicateChildStatus`) is a possible follow-up; this listener is the cheap
//! always-on first stage.
//!
//! The listener thread also keeps a cached CCD target inventory (refreshed on each event + a slow
//! timer), so the capture thread can name connected-but-inactive external displays — the prime
//! suspects — without ever touching the CCD lock itself (the display-config lock is exactly what
//! stalls during churn; the capture thread must never block on it).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::VecDeque;
use std::sync::{Mutex, Once, OnceLock};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::GUID_DEVINTERFACE_MONITOR;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    RegisterDeviceNotificationW, SetTimer, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVNODES_CHANGED, DBT_DEVTYP_DEVICEINTERFACE, DEVICE_NOTIFY_WINDOW_HANDLE,
    DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HDR, MSG, WINDOW_EX_STYLE, WM_DEVICECHANGE,
    WM_DISPLAYCHANGE, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

/// One OS-visible display event, timestamped at receipt.
#[derive(Clone)]
pub struct DisplayEvent {
    pub at: Instant,
    pub kind: DisplayEventKind,
    /// Monitor device instance id for arrival/removal (e.g. `DISPLAY\GSM83CD\...`), else `None`.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayEventKind {
    /// A monitor device interface ARRIVED — a sink (re)connected as Windows sees it.
    MonitorArrival,
    /// A monitor device interface was REMOVED — a sink dropped as Windows sees it.
    MonitorRemoval,
    /// PnP device-tree churn (broadcast, no payload) — re-enumeration passed through.
    DevNodesChanged,
    /// A mode/topology commit reached the desktop (resolution/layout actually changed).
    DisplayChange,
}

impl DisplayEventKind {
    fn label(self) -> &'static str {
        match self {
            Self::MonitorArrival => "monitor-arrival",
            Self::MonitorRemoval => "monitor-removal",
            Self::DevNodesChanged => "devnodes-changed",
            Self::DisplayChange => "display-change",
        }
    }
}

struct State {
    /// Recent events, oldest-first, capped at [`RING_CAP`].
    events: VecDeque<DisplayEvent>,
    /// Cached CCD target inventory (see module docs for why the cache exists).
    inventory: Vec<crate::win_display::TargetInventory>,
}

/// Ring depth: at the observed worst case (a probe cycle every ~2 s, ≤4 events per cycle) this
/// holds well over a minute of history — the stall correlator only ever asks about the last gap.
const RING_CAP: usize = 128;

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            events: VecDeque::with_capacity(RING_CAP),
            inventory: Vec::new(),
        })
    })
}

/// Start the listener thread (idempotent). Degraded-not-fatal: if window/registration creation
/// fails the ring just stays empty — the stall log then reports "listener unavailable" naturally
/// via empty summaries, and streaming is unaffected.
pub fn spawn_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("pf-display-events".into())
            .spawn(pump);
        if let Err(e) = spawned {
            tracing::warn!(
                error = %e,
                "display-event listener thread failed to spawn — stall logs won't carry OS event attribution"
            );
        }
    });
}

/// Events with `from <= at <= to`, oldest-first.
pub fn events_between(from: Instant, to: Instant) -> Vec<DisplayEvent> {
    let st = state().lock().unwrap();
    st.events
        .iter()
        .filter(|e| e.at >= from && e.at <= to)
        .cloned()
        .collect()
}

/// Compact one-line summary for log fields: `"monitor-removal x2 (DISPLAY\GSM83CD\...),
/// devnodes-changed x1"`; `"none"` when empty.
pub fn summarize(events: &[DisplayEvent]) -> String {
    if events.is_empty() {
        return "none".into();
    }
    let mut out: Vec<String> = Vec::new();
    for kind in [
        DisplayEventKind::MonitorArrival,
        DisplayEventKind::MonitorRemoval,
        DisplayEventKind::DevNodesChanged,
        DisplayEventKind::DisplayChange,
    ] {
        let hits: Vec<&DisplayEvent> = events.iter().filter(|e| e.kind == kind).collect();
        if hits.is_empty() {
            continue;
        }
        let detail = hits
            .iter()
            .rev()
            .find_map(|e| e.detail.as_deref())
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        out.push(format!("{} x{}{}", kind.label(), hits.len(), detail));
    }
    out.join(", ")
}

/// The prime suspects for link-probe/dark-head disturbances, from the cached inventory: PHYSICAL
/// displays — external connectors AND internal panels — that are CONNECTED but not part of the
/// desktop. External = the classic standby TV / input-switched monitor; internal = the laptop
/// panel the exclusive isolate deactivated, whose driver-level servicing produces the identical
/// ~2 s metronome on hybrid laptops (field A/B 2026-07-27: reporter's Legion, exclusive
/// 16.3 stalls/min vs primary 0 — the old external-only filter reported `none` there and steered
/// the diagnosis AWAY from the real cause for hours). Virtual/indirect targets stay excluded
/// (precision rule). Rendered as `"<friendly> (<connector>)"`. Never blocks on the CCD lock.
pub fn connected_inactive_physicals() -> Vec<String> {
    let st = state().lock().unwrap();
    st.inventory
        .iter()
        .filter(|t| (t.external_physical || t.internal_panel) && !t.active)
        .map(|t| {
            let name = if t.friendly.is_empty() && t.internal_panel {
                "laptop panel"
            } else {
                &t.friendly
            };
            format!("{} ({})", name, t.tech)
        })
        .collect()
}

fn push_event(kind: DisplayEventKind, detail: Option<String>) {
    let mut st = state().lock().unwrap();
    if st.events.len() >= RING_CAP {
        st.events.pop_front();
    }
    st.events.push_back(DisplayEvent {
        at: Instant::now(),
        kind,
        detail,
    });
}

fn refresh_inventory() {
    // SAFETY: `target_inventory` only runs read-only CCD queries over locals (see its SAFETY doc);
    // called from the listener thread, never the capture thread.
    let inv = unsafe { crate::win_display::target_inventory() };
    if !inv.is_empty() {
        state().lock().unwrap().inventory = inv;
    }
}

/// Inventory refresh timer: 15 s keeps the "connected-but-inactive" suspect list fresh enough for
/// a warn that rate-limits to 30 s, without measurable CCD traffic.
const INVENTORY_TIMER_MS: u32 = 15_000;

/// `DBT_DEVNODES_CHANGED` arrives as `wParam` on `WM_DEVICECHANGE` without a registration; the
/// interface arrivals/removals need the `RegisterDeviceNotificationW` below.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            // lParam packs the new primary resolution — worth carrying: it distinguishes a real
            // mode change from a same-mode re-commit when reading a field log.
            let (w, h) = (
                (lparam.0 & 0xffff) as u32,
                ((lparam.0 >> 16) & 0xffff) as u32,
            );
            push_event(DisplayEventKind::DisplayChange, Some(format!("{w}x{h}")));
            refresh_inventory();
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            let event = wparam.0 as u32;
            if event == DBT_DEVNODES_CHANGED {
                push_event(DisplayEventKind::DevNodesChanged, None);
                refresh_inventory();
            } else if event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE {
                let kind = if event == DBT_DEVICEARRIVAL {
                    DisplayEventKind::MonitorArrival
                } else {
                    DisplayEventKind::MonitorRemoval
                };
                // SAFETY: for these two events lParam is documented to point at a
                // DEV_BROADCAST_HDR (or be 0 — checked). We only read the header fields, and only
                // reinterpret as DEV_BROADCAST_DEVICEINTERFACE_W after checking dbch_devicetype,
                // reading at most dbch_size bytes — the size the sender declared.
                let detail = unsafe {
                    let hdr = lparam.0 as *const DEV_BROADCAST_HDR;
                    if hdr.is_null() || (*hdr).dbch_devicetype != DBT_DEVTYP_DEVICEINTERFACE {
                        None
                    } else {
                        let di = hdr as *const DEV_BROADCAST_DEVICEINTERFACE_W;
                        let head = std::mem::offset_of!(DEV_BROADCAST_DEVICEINTERFACE_W, dbcc_name);
                        let bytes = ((*hdr).dbch_size as usize).saturating_sub(head);
                        let name = std::slice::from_raw_parts(
                            std::ptr::addr_of!((*di).dbcc_name).cast::<u16>(),
                            (bytes / 2).min(512),
                        );
                        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                        crate::monitor_devnode::instance_id_from_interface_path(
                            &String::from_utf16_lossy(&name[..end]),
                        )
                    }
                };
                push_event(kind, detail);
                refresh_inventory();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            refresh_inventory();
            LRESULT(0)
        }
        // SAFETY: default handling for everything else — the standard wndproc tail call.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The listener thread: a hidden TOP-LEVEL window (message-ONLY windows receive neither
/// `WM_DISPLAYCHANGE` nor broadcast `WM_DEVICECHANGE` — the classic pitfall) + a device-interface
/// registration for monitors + a blocking message pump. Runs for the process lifetime.
fn pump() {
    refresh_inventory(); // baseline before any event arrives

    let class: Vec<u16> = "pf-display-events\0".encode_utf16().collect();
    // SAFETY: straight-line Win32 window bring-up on this thread. `class` outlives every use of
    // its pointer (it lives to the end of the fn, the pump loops forever). All handles passed on
    // are the ones the preceding calls returned; failure of any step returns out of the thread
    // (degraded mode, see `spawn_once`). The DEV_BROADCAST_DEVICEINTERFACE_W filter is a fully
    // initialised local read synchronously by RegisterDeviceNotificationW.
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            tracing::warn!(
                "display-event listener: GetModuleHandleW failed — no OS event attribution"
            );
            return;
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!(
                "display-event listener: RegisterClassW failed — no OS event attribution"
            );
            return;
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WS_OVERLAPPED, // hidden: never shown, never gets focus — exists only to receive broadcasts
            0,
            0,
            0,
            0,
            None,
            None,
            Some(wc.hInstance),
            None,
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "display-event listener: CreateWindowExW failed — no OS event attribution");
                return;
            }
        };
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: GUID_DEVINTERFACE_MONITOR,
            ..Default::default()
        };
        if let Err(e) = RegisterDeviceNotificationW(
            HANDLE(hwnd.0),
            std::ptr::from_ref(&filter).cast(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            // DBT_DEVNODES_CHANGED and WM_DISPLAYCHANGE still arrive — partial attribution.
            tracing::warn!(error = %e, "display-event listener: monitor-interface registration failed — arrival/removal detail unavailable");
        }
        SetTimer(Some(hwnd), 1, INVENTORY_TIMER_MS, None);
        tracing::debug!(
            "display-event listener running (monitor hot-plug + display-change attribution)"
        );
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}
