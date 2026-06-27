//! Input injection (plan §4): turn client [`punktfunk_core::input::InputEvent`]s into host input.
//!
//! The headless Sway compositor runs with `WLR_LIBINPUT_NO_DEVICES=1`, so kernel `uinput`
//! devices are never picked up. Instead we inject through the wlroots virtual-input Wayland
//! protocols — `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` — which
//! Sway always advertises. We connect as an ordinary Wayland client (the host process
//! inherits Sway's `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`), bind the two managers, and translate
//! events into virtual pointer/keyboard requests. Keyboard codes are Linux evdev; we upload a
//! standard evdev/US xkb keymap and track modifier state so the compositor resolves shifted
//! keysyms correctly.

use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};

/// Injects input events into the host session. Not `Send`: an injector owns compositor
/// resources (a Wayland connection, an xkb state) and lives entirely on the control thread
/// that creates it.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Preferred injection backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// wlroots virtual pointer + keyboard Wayland protocols — the headless-Sway path.
    WlrVirtual,
    /// KWin `org_kde_kwin_fake_input` — direct injection, no RemoteDesktop portal / approval dialog
    /// (authorized by the host's `.desktop`). The headless KDE-Desktop path; what krdpserver uses.
    KwinFakeInput,
    /// libei via `reis` — Wayland-native (RemoteDesktop portal). Not yet implemented.
    Libei,
    /// libei directly against gamescope's own EIS socket (no portal): input lands in the
    /// nested game — the SteamOS-like session.
    GamescopeEi,
    /// `/dev/uinput` — universal fallback (but invisible to `WLR_LIBINPUT_NO_DEVICES=1`).
    Uinput,
    /// Windows `SendInput` (Win32 KeyboardAndMouse) — the Windows host path.
    SendInput,
}

pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::WlrVirtual => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(wlr::WlrootsInjector::open()?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("wlroots virtual input requires Linux + a Wayland compositor")
            }
        }
        Backend::KwinFakeInput => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(kwin_fake_input::KwinFakeInjector::open()?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("KWin fake_input requires Linux + a KWin Wayland session")
            }
        }
        Backend::Libei => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(
                    libei::LibeiInjector::open_with(libei_ei_source())?,
                ))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("libei input requires Linux + a RemoteDesktop portal")
            }
        }
        Backend::GamescopeEi => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(libei::LibeiInjector::open_with(
                    libei::EiSource::SocketPathFile(
                        crate::vdisplay::gamescope_ei_socket_file().into(),
                    ),
                )?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("gamescope EIS input requires Linux")
            }
        }
        Backend::SendInput => {
            #[cfg(target_os = "windows")]
            {
                Ok(Box::new(sendinput::SendInputInjector::open()?))
            }
            #[cfg(not(target_os = "windows"))]
            {
                anyhow::bail!("SendInput injection requires Windows")
            }
        }
        other => anyhow::bail!("injection backend {other:?} not implemented"),
    }
}

/// Pick the injection backend for the current session. gamescope hosts its own EIS server (no
/// portal), so a gamescope session injects directly into it. wlroots/Sway only implements the
/// ScreenCast portal (no RemoteDesktop), so libei can't run there — use the wlr virtual-input
/// protocols. **KWin** exposes `org_kde_kwin_fake_input` (direct injection, no portal / approval
/// dialog — the only headless-capable path; what krdpserver uses), so prefer it there. **GNOME**
/// has neither fake_input nor the wlr protocols, so it uses libei via the RemoteDesktop portal
/// (which needs a user to approve, or a pre-seeded grant — not truly headless).
/// `PUNKTFUNK_INPUT_BACKEND=wlr|kwin|libei|gamescope|uinput` overrides the auto-detection.
pub fn default_backend() -> Backend {
    if let Ok(v) = std::env::var("PUNKTFUNK_INPUT_BACKEND") {
        match v.trim().to_ascii_lowercase().as_str() {
            "wlr" | "wlroots" | "wlrvirtual" => return Backend::WlrVirtual,
            "kwin" | "fakeinput" | "fake_input" | "kwin-fake-input" => {
                return Backend::KwinFakeInput
            }
            "libei" | "ei" | "portal" => return Backend::Libei,
            "gamescope" | "gamescope-ei" => return Backend::GamescopeEi,
            "uinput" => return Backend::Uinput,
            "sendinput" | "win" | "windows" => return Backend::SendInput,
            other => tracing::warn!(
                value = other,
                "unknown PUNKTFUNK_INPUT_BACKEND — auto-detecting"
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        Backend::SendInput
    }
    #[cfg(not(target_os = "windows"))]
    {
        // An explicit compositor pick (set per connect / mid-stream) is the strongest signal.
        let compositor = crate::config::config().compositor.clone();
        if let Some(c) = compositor.as_deref() {
            let c = c.trim();
            if c.eq_ignore_ascii_case("gamescope") {
                return Backend::GamescopeEi;
            }
            if c.eq_ignore_ascii_case("kwin") {
                return Backend::KwinFakeInput;
            }
            if c.eq_ignore_ascii_case("wlroots") || c.eq_ignore_ascii_case("sway") {
                return Backend::WlrVirtual;
            }
            // mutter (GNOME) falls through to the XDG_CURRENT_DESKTOP check below.
        }
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let d = desktop.to_ascii_uppercase();
        if d.contains("KDE") {
            Backend::KwinFakeInput
        } else if d.contains("GNOME") {
            Backend::Libei
        } else {
            Backend::WlrVirtual
        }
    }
}

/// Host-lifetime pointer/keyboard injector running on its OWN thread, fed over a clonable `Send`
/// channel. The injector backend owns non-`Send` compositor state (a Wayland connection / xkb / EIS
/// socket), so it must live on a single thread; both the GameStream control plane and the native
/// punktfunk/1 plane forward their decoded keyboard/mouse events here instead of injecting inline, so
/// a slow inject (a portal stall, a desktop switch) never head-blocks the network thread's
/// keepalive/retransmit servicing.
pub(crate) struct InjectorService {
    tx: std::sync::mpsc::Sender<InputEvent>,
}

impl InjectorService {
    pub(crate) fn start() -> InjectorService {
        let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-injector".into())
            .spawn(move || injector_service_thread(rx))
        {
            tracing::error!(error = %e, "injector service thread spawn failed — pointer/keyboard input disabled");
        }
        InjectorService { tx }
    }

    /// A sender a session/plane forwards its pointer/keyboard events to. Cloned per caller; dropping a
    /// clone does NOT stop the service (it runs while any sender — incl. the service's own — lives).
    pub(crate) fn sender(&self) -> std::sync::mpsc::Sender<InputEvent> {
        self.tx.clone()
    }
}

/// Backoff between reopen attempts after the injector backend fails to open or its worker dies, so a
/// persistently-unavailable portal isn't hammered once per event.
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// The host-lifetime injector worker: lazily open the pointer/keyboard backend, then inject every
/// forwarded event. Reopen (after [`INJECTOR_REOPEN_BACKOFF`]) on open failure, on a backend change
/// (input follows the active session), or if the backend's worker dies mid-stream. Exits only when
/// every sender has dropped (host shutdown), which drops the injector and closes its portal session.
///
/// Each wake drains the whole backlog and [`coalesce`]s redundant motion before injecting, so a slow
/// backend never builds up a queue of stale relative-mouse/scroll events (latency) — while button,
/// key, and absolute-move ordering is preserved exactly.
fn injector_service_thread(rx: std::sync::mpsc::Receiver<InputEvent>) {
    let mut injector: Option<Box<dyn InputInjector>> = None;
    let mut open_backend: Option<Backend> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    while let Ok(first) = rx.recv() {
        // Drain everything already queued behind `first` so we coalesce a whole burst at once.
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }

        // The resolved input backend (PUNKTFUNK_INPUT_BACKEND, set per connect / mid-stream session
        // switch) may have changed since we opened. Reopen against it so input FOLLOWS the active
        // session instead of injecting into a stale, still-warm backend (e.g. the managed gamescope's
        // EIS socket after the user switched to the KDE desktop).
        let want = default_backend();
        if injector.is_some() && open_backend != Some(want) {
            tracing::info!(
                ?open_backend,
                ?want,
                "input: backend changed — reopening injector for the active session"
            );
            injector = None;
            last_failed = None; // re-resolve immediately
        }
        if injector.is_none() {
            // Open on the first event; after a failure wait out the backoff before retrying (a few
            // events drop during setup — acceptable, input is lossy).
            let ready = last_failed.is_none_or(|t| t.elapsed() >= INJECTOR_REOPEN_BACKOFF);
            if ready {
                match open(want) {
                    Ok(i) => {
                        tracing::info!(backend = ?want, "input injector ready (host-lifetime)");
                        injector = Some(i);
                        open_backend = Some(want);
                        last_failed = None;
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "pointer/keyboard injection unavailable — will retry");
                        last_failed = Some(std::time::Instant::now());
                    }
                }
            }
        }
        if let Some(inj) = injector.as_mut() {
            for ev in coalesce(batch) {
                if let Err(e) = inj.inject(&ev) {
                    // The backend's worker (portal session / EIS socket) died — drop it and reopen on
                    // a later event (covers a gamescope EIS socket that respawns with its session).
                    tracing::warn!(error = %format!("{e:#}"), "inject failed — reopening injector");
                    injector = None;
                    open_backend = None;
                    last_failed = Some(std::time::Instant::now());
                    break; // abandon the rest of this batch; the next one reopens
                }
            }
        }
    }
    tracing::debug!("injector service stopped (host shutting down)");
}

/// Coalesce a drained burst: sum consecutive relative-mouse deltas and consecutive same-axis scroll
/// deltas (identical net effect, far fewer injects), passing buttons, keys, absolute moves, and any
/// type change through untouched and in order. Only *adjacent* same-type events merge, so a button
/// or key between two moves flushes the accumulated motion first — ordering is never reshuffled.
fn coalesce(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for ev in events {
        match out.last_mut() {
            Some(last) if last.kind == InputKind::MouseMove && ev.kind == InputKind::MouseMove => {
                last.x = last.x.saturating_add(ev.x);
                last.y = last.y.saturating_add(ev.y);
            }
            Some(last)
                if last.kind == InputKind::MouseScroll
                    && ev.kind == InputKind::MouseScroll
                    && last.code == ev.code =>
            {
                last.x = last.x.saturating_add(ev.x);
            }
            _ => out.push(ev),
        }
    }
    out
}

/// How the libei backend reaches its EIS server. KWin goes through the `RemoteDesktop` *portal*
/// (with a pre-seeded grant), but GNOME's portal `Start()` needs an interactive approval a
/// headless host can't answer — so GNOME goes straight to Mutter's *direct* RemoteDesktop EIS
/// (`org.gnome.Mutter.RemoteDesktop`), the same direct API the Mutter video backend uses.
#[cfg(target_os = "linux")]
fn libei_ei_source() -> libei::EiSource {
    let gnome = crate::config::config()
        .compositor
        .as_deref()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("mutter"))
        || std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains("GNOME");
    if gnome {
        libei::EiSource::MutterEis
    } else {
        libei::EiSource::Portal
    }
}

/// Map a Windows Virtual-Key code (as sent by Moonlight/GameStream) to a Linux evdev key code.
pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    match vk {
        // --- Navigation / editing / whitespace ---
        0x08 => Some(14),  // VK_BACK     -> KEY_BACKSPACE
        0x09 => Some(15),  // VK_TAB      -> KEY_TAB
        0x0D => Some(28),  // VK_RETURN   -> KEY_ENTER
        0x13 => Some(119), // VK_PAUSE    -> KEY_PAUSE
        0x14 => Some(58),  // VK_CAPITAL  -> KEY_CAPSLOCK
        0x1B => Some(1),   // VK_ESCAPE   -> KEY_ESC
        0x20 => Some(57),  // VK_SPACE    -> KEY_SPACE
        0x21 => Some(104), // VK_PRIOR    -> KEY_PAGEUP
        0x22 => Some(109), // VK_NEXT     -> KEY_PAGEDOWN
        0x23 => Some(107), // VK_END      -> KEY_END
        0x24 => Some(102), // VK_HOME     -> KEY_HOME
        0x25 => Some(105), // VK_LEFT     -> KEY_LEFT
        0x26 => Some(103), // VK_UP       -> KEY_UP
        0x27 => Some(106), // VK_RIGHT    -> KEY_RIGHT
        0x28 => Some(108), // VK_DOWN     -> KEY_DOWN
        0x2C => Some(99),  // VK_SNAPSHOT -> KEY_SYSRQ
        0x2D => Some(110), // VK_INSERT   -> KEY_INSERT
        0x2E => Some(111), // VK_DELETE   -> KEY_DELETE

        // --- Generic modifiers ---
        0x10 => Some(42), // VK_SHIFT   -> KEY_LEFTSHIFT
        0x11 => Some(29), // VK_CONTROL -> KEY_LEFTCTRL
        0x12 => Some(56), // VK_MENU    -> KEY_LEFTALT

        // --- Digit row (KEY_0 is 11, KEY_1..KEY_9 are 2..10) ---
        0x30 => Some(11), // VK_0
        0x31 => Some(2),  // VK_1
        0x32 => Some(3),  // VK_2
        0x33 => Some(4),  // VK_3
        0x34 => Some(5),  // VK_4
        0x35 => Some(6),  // VK_5
        0x36 => Some(7),  // VK_6
        0x37 => Some(8),  // VK_7
        0x38 => Some(9),  // VK_8
        0x39 => Some(10), // VK_9

        // --- Letters A-Z (NOT sequential in evdev) ---
        0x41 => Some(30), // A
        0x42 => Some(48), // B
        0x43 => Some(46), // C
        0x44 => Some(32), // D
        0x45 => Some(18), // E
        0x46 => Some(33), // F
        0x47 => Some(34), // G
        0x48 => Some(35), // H
        0x49 => Some(23), // I
        0x4A => Some(36), // J
        0x4B => Some(37), // K
        0x4C => Some(38), // L
        0x4D => Some(50), // M
        0x4E => Some(49), // N
        0x4F => Some(24), // O
        0x50 => Some(25), // P
        0x51 => Some(16), // Q
        0x52 => Some(19), // R
        0x53 => Some(31), // S
        0x54 => Some(20), // T
        0x55 => Some(22), // U
        0x56 => Some(47), // V
        0x57 => Some(17), // W
        0x58 => Some(45), // X
        0x59 => Some(21), // Y
        0x5A => Some(44), // Z

        // --- Meta / context-menu ---
        0x5B => Some(125), // VK_LWIN -> KEY_LEFTMETA
        0x5C => Some(126), // VK_RWIN -> KEY_RIGHTMETA
        0x5D => Some(127), // VK_APPS -> KEY_COMPOSE

        // --- Numpad ---
        0x60 => Some(82), // KP0
        0x61 => Some(79), // KP1
        0x62 => Some(80), // KP2
        0x63 => Some(81), // KP3
        0x64 => Some(75), // KP4
        0x65 => Some(76), // KP5
        0x66 => Some(77), // KP6
        0x67 => Some(71), // KP7
        0x68 => Some(72), // KP8
        0x69 => Some(73), // KP9
        0x6A => Some(55), // VK_MULTIPLY  -> KEY_KPASTERISK
        0x6B => Some(78), // VK_ADD       -> KEY_KPPLUS
        0x6C => Some(96), // VK_SEPARATOR -> KEY_KPENTER
        0x6D => Some(74), // VK_SUBTRACT  -> KEY_KPMINUS
        0x6E => Some(83), // VK_DECIMAL   -> KEY_KPDOT
        0x6F => Some(98), // VK_DIVIDE    -> KEY_KPSLASH

        // --- Function keys (F1..F10 = 59..68, F11/F12 = 87/88) ---
        0x70 => Some(59),
        0x71 => Some(60),
        0x72 => Some(61),
        0x73 => Some(62),
        0x74 => Some(63),
        0x75 => Some(64),
        0x76 => Some(65),
        0x77 => Some(66),
        0x78 => Some(67),
        0x79 => Some(68),
        0x7A => Some(87),
        0x7B => Some(88),

        // --- Locks ---
        0x90 => Some(69), // VK_NUMLOCK -> KEY_NUMLOCK
        0x91 => Some(70), // VK_SCROLL  -> KEY_SCROLLLOCK

        // --- Left/right modifiers ---
        0xA0 => Some(42),  // VK_LSHIFT   -> KEY_LEFTSHIFT
        0xA1 => Some(54),  // VK_RSHIFT   -> KEY_RIGHTSHIFT
        0xA2 => Some(29),  // VK_LCONTROL -> KEY_LEFTCTRL
        0xA3 => Some(97),  // VK_RCONTROL -> KEY_RIGHTCTRL
        0xA4 => Some(56),  // VK_LMENU    -> KEY_LEFTALT
        0xA5 => Some(100), // VK_RMENU    -> KEY_RIGHTALT

        // --- OEM punctuation (US layout) ---
        0xBA => Some(39), // VK_OEM_1      -> KEY_SEMICOLON
        0xBB => Some(13), // VK_OEM_PLUS   -> KEY_EQUAL
        0xBC => Some(51), // VK_OEM_COMMA  -> KEY_COMMA
        0xBD => Some(12), // VK_OEM_MINUS  -> KEY_MINUS
        0xBE => Some(52), // VK_OEM_PERIOD -> KEY_DOT
        0xBF => Some(53), // VK_OEM_2      -> KEY_SLASH
        0xC0 => Some(41), // VK_OEM_3      -> KEY_GRAVE
        0xDB => Some(26), // VK_OEM_4      -> KEY_LEFTBRACE
        0xDC => Some(43), // VK_OEM_5      -> KEY_BACKSLASH
        0xDD => Some(27), // VK_OEM_6      -> KEY_RIGHTBRACE
        0xDE => Some(40), // VK_OEM_7      -> KEY_APOSTROPHE
        0xE2 => Some(86), // VK_OEM_102    -> KEY_102ND

        _ => None,
    }
}

/// Map a GameStream mouse button id (1=left … 5=X2) to a Linux evdev `BTN_*` code.
#[cfg(target_os = "linux")]
fn gs_button_to_evdev(b: u32) -> Option<u32> {
    Some(match b {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        4 => 0x113, // BTN_SIDE  (X1)
        5 => 0x114, // BTN_EXTRA (X2)
        _ => return None,
    })
}

// Goal-1 stage 6: Linux UHID/uinput/libei/wlr backends under `inject/linux/`, the Windows UMDF/SendInput
// backends under `inject/windows/`, and the transport-independent HID codecs under `inject/proto/`;
// `#[path]` keeps every `crate::inject::*` module name flat.
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualsense.rs"]
pub mod dualsense;
/// Transport-independent DualSense HID contract, shared by the Linux UHID backend ([`dualsense`])
/// and the Windows UMDF-driver backend ([`dualsense_windows`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/dualsense_proto.rs"]
pub mod dualsense_proto;
/// Windows: virtual DualSense via the UMDF minidriver + a shared-memory host channel.
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualsense_windows.rs"]
pub mod dualsense_windows;
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualshock4.rs"]
pub mod dualshock4;
/// Transport-independent DualShock 4 HID codec used by the Windows UMDF-driver backend
/// ([`dualshock4_windows`]). (The Linux backend still carries its own copy — see the module FIXME.)
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/dualshock4_proto.rs"]
pub mod dualshock4_proto;
/// Windows: virtual DualShock 4 via the same UMDF minidriver + shared-memory channel (device-type 1).
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualshock4_windows.rs"]
pub mod dualshock4_windows;
#[cfg(target_os = "linux")]
#[path = "inject/linux/gamepad.rs"]
pub mod gamepad;
/// Windows: virtual Xbox 360 pads via the in-tree XUSB companion UMDF driver (classic XInput).
#[cfg(target_os = "windows")]
#[path = "inject/windows/gamepad_windows.rs"]
pub mod gamepad;
/// Windows: small RAII wrappers (`Shm` section+view, `SwDevice` devnode) shared by the three gamepad
/// backends (DualSense / DualShock 4 / XUSB), so each per-pad resource closes deterministically on drop.
#[cfg(target_os = "windows")]
#[path = "inject/windows/gamepad_raii.rs"]
mod gamepad_raii;
/// Stub — virtual gamepads need Linux uinput or the Windows UMDF drivers; events are dropped elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub mod gamepad {
    #[derive(Default)]
    pub struct GamepadManager;
    impl GamepadManager {
        pub fn new() -> Self {
            GamepadManager
        }
        pub fn handle(&mut self, _ev: &crate::gamestream::gamepad::GamepadEvent) {}
        pub fn pump_rumble(&mut self, _send: impl FnMut(u16, u16, u16)) {}
    }
}
#[cfg(target_os = "linux")]
#[path = "inject/linux/kwin_fake_input.rs"]
mod kwin_fake_input;
#[cfg(target_os = "linux")]
#[path = "inject/linux/libei.rs"]
mod libei;
#[cfg(target_os = "windows")]
#[path = "inject/windows/sendinput.rs"]
mod sendinput;
#[cfg(target_os = "linux")]
#[path = "inject/linux/wlr.rs"]
mod wlr;

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(kind: InputKind, code: u32, x: i32, y: i32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y,
            flags: 0,
        }
    }

    #[test]
    fn coalesce_sums_adjacent_motion_and_preserves_order() {
        let events = vec![
            mk(InputKind::MouseMove, 0, 1, 2),
            mk(InputKind::MouseMove, 0, 3, -1), // → summed with the previous move
            mk(InputKind::KeyDown, 30, 0, 0),   // flushes the move, passes through verbatim
            mk(InputKind::MouseMove, 0, 5, 5),  // a NEW run after the key (not merged across it)
            mk(InputKind::MouseScroll, 0, 1, 0),
            mk(InputKind::MouseScroll, 0, 2, 0), // same axis (code 0) → summed
            mk(InputKind::MouseScroll, 1, 1, 0), // different axis (code 1) → separate
        ];
        let out = coalesce(events);
        assert_eq!(out.len(), 5);
        assert_eq!(
            (out[0].kind, out[0].x, out[0].y),
            (InputKind::MouseMove, 4, 1)
        );
        assert_eq!(out[1].kind, InputKind::KeyDown);
        assert_eq!(
            (out[2].kind, out[2].x, out[2].y),
            (InputKind::MouseMove, 5, 5)
        );
        assert_eq!(
            (out[3].kind, out[3].code, out[3].x),
            (InputKind::MouseScroll, 0, 3)
        );
        assert_eq!(
            (out[4].kind, out[4].code, out[4].x),
            (InputKind::MouseScroll, 1, 1)
        );
    }

    #[test]
    fn coalesce_handles_empty_and_singleton() {
        assert!(coalesce(vec![]).is_empty());
        assert_eq!(coalesce(vec![mk(InputKind::MouseMove, 0, 7, 8)]).len(), 1);
    }
}
