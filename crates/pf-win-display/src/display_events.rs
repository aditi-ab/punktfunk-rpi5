//! OS display-event listener for capture-stall attribution.
//!
//! The IDD-push capturer can report that DWM stopped composing on a period, not why. This thread
//! timestamps the three user-mode signals Windows exposes:
//!
//! - `WM_DEVICECHANGE` + `RegisterDeviceNotificationW(GUID_DEVINTERFACE_MONITOR)`: monitor
//!   interface arrival/removal, including devnode churn that leaves topology unchanged.
//! - `DBT_DEVNODES_CHANGED`: PnP-tree churn with no payload.
//! - `WM_DISPLAYCHANGE`: a mode/topology commit reached the desktop. Absence is a signal: a
//!   probe with no mode delta does not fire this.
//!
//! A driver-internal probe (EDID/DDC, DP retrain) emits none of these. Pair that silence with
//! metronomic stalls to tell a KMD-below-OS sink from a Windows re-enumeration.
//!
//! CCD inventory is cached on each event and a slow timer so the capture thread can name
//! connected-but-inactive physicals without taking the display-config lock (that lock is what
//! stalls during churn).

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

#[derive(Clone)]
pub struct DisplayEvent {
    pub at: Instant,
    pub kind: DisplayEventKind,
    /// Monitor instance id on arrival/removal; `None` otherwise.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayEventKind {
    MonitorArrival,
    MonitorRemoval,
    /// PnP tree churn (broadcast, no payload).
    DevNodesChanged,
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
    events: VecDeque<DisplayEvent>,
    inventory: Vec<crate::win_display::TargetInventory>,
}

/// 128 ≈ 1 min at a 2 s probe cycle with ≤4 events each. The correlator only reads the last gap.
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

/// Window or registration failure leaves the ring empty; streaming continues.
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

pub fn events_between(from: Instant, to: Instant) -> Vec<DisplayEvent> {
    let st = state().lock().unwrap();
    st.events
        .iter()
        .filter(|e| e.at >= from && e.at <= to)
        .cloned()
        .collect()
}

/// One log field: `"monitor-removal x2 (DISPLAY\\…), devnodes-changed x1"`; `"none"` if empty.
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

/// External standby sinks and the laptop panel Exclusive isolate deactivated. Both get the same
/// ~2 s driver-level probe. Virtual/indirect targets stay out. Never takes the CCD lock.
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
    let inv = crate::win_display::target_inventory();
    if !inv.is_empty() {
        state().lock().unwrap().inventory = inv;
    }
}

/// 15 s keeps the suspect list fresher than the 30 s warn rate-limit, without extra CCD traffic.
const INVENTORY_TIMER_MS: u32 = 15_000;

/// `DBT_DEVNODES_CHANGED` arrives as `wParam` on `WM_DEVICECHANGE` without a registration; interface
/// arrival/removal needs `RegisterDeviceNotificationW` below.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            // lParam is the new primary resolution. Distinguishes a real mode change from a same-mode re-commit.
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
                // SAFETY: for these two events lParam is a DEV_BROADCAST_HDR or 0 (checked). Header
                // fields only, then DEV_BROADCAST_DEVICEINTERFACE_W after dbch_devicetype matches,
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

/// Message-only windows receive neither `WM_DISPLAYCHANGE` nor broadcast `WM_DEVICECHANGE`.
fn pump() {
    refresh_inventory();

    let class: Vec<u16> = "pf-display-events\0".encode_utf16().collect();
    // SAFETY: Win32 window bring-up on this thread. `class` outlives every pointer use (lives to
    // fn end; the pump loops forever). Handles are the preceding calls' returns; any failure
    // returns from the thread (degraded, see `spawn_once`). The filter is a fully initialised
    // local that RegisterDeviceNotificationW reads synchronously.
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
            WS_OVERLAPPED, // hidden; exists only to receive broadcasts
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
