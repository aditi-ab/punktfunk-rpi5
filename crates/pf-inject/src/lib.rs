//! Input injection (plan §4): turn client [`punktfunk_core::input::InputEvent`]s into host input.
//!
//! The headless Sway compositor runs with `WLR_LIBINPUT_NO_DEVICES=1`, so kernel `uinput`
//! devices are never picked up. Instead we inject through the wlroots virtual-input Wayland
//! protocols — `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` — which
//! Sway always advertises. We connect as an ordinary Wayland client (the host process
//! inherits Sway's `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`), bind the two managers, and translate
//! events into virtual pointer/keyboard requests. Keyboard codes are Linux evdev; we upload an
//! xkb keymap (the host's layout via `XKB_DEFAULT_LAYOUT` et al., defaulting to evdev/US) and
//! track modifier state so the compositor resolves shifted keysyms correctly.
//!
//! Extracted into a subsystem crate (plan §W6): consumes `punktfunk_core::input` (the neutral
//! event vocabulary) + `pf-driver-proto` (the HID wire contract), never the orchestrator.

// Scaffold: trait methods + per-OS backends are defined ahead of the target that uses them.
#![allow(dead_code)]
// Every unsafe block in this crate carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]
// …and its companion: without this, an `unsafe fn` body needs no blocks, so an unproven FFI call
// could hide inside one and still satisfy the deny above. The workspace keeps
// `unsafe_op_in_unsafe_fn` at `warn` while the encoder backends are cleared; this crate is at zero.
#![deny(unsafe_op_in_unsafe_fn)]

use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};

#[path = "inject/keymap.rs"]
mod keymap;
#[cfg(target_os = "linux")]
pub(crate) use keymap::gs_button_to_evdev;
pub use keymap::KEY_FLAG_SEMANTIC_VK;
// vk_to_evdev is consumed by the Linux injectors (kwin/libei/wlr) and — on Windows — only by the
// SendInput mirror test; keep the shared `crate::vk_to_evdev` re-export unconditionally.
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
pub use keymap::vk_to_evdev;

/// Device-agnostic dedup for the rich HID-output feedback plane (0xCD), shared by the virtual-pad
/// managers ([`uhid_manager`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/hidout_dedup.rs"]
pub mod hidout_dedup;

/// Injects input events into the host session. Not `Send`: an injector owns compositor
/// resources (a Wayland connection, an xkb state) and lives entirely on the control thread
/// that creates it.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Preferred injection backend. Which variants exist is **per-OS**: the factory ([`open`]) is a
/// single per-target block, so it can only be handed a backend that exists on the target — an
/// impossible OS/backend pairing is a compile error, not a runtime `bail!` (plan §2.3).
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// wlroots virtual pointer + keyboard Wayland protocols — the headless-Sway path.
    WlrVirtual,
    /// KWin `org_kde_kwin_fake_input` — direct injection, no RemoteDesktop portal / approval dialog
    /// (authorized by the host's `.desktop`). The headless KDE-Desktop path; what krdpserver uses.
    KwinFakeInput,
    /// libei via `reis` — Wayland-native. Reaches EIS through the RemoteDesktop portal, or on
    /// GNOME through Mutter's direct RemoteDesktop API (see `libei_ei_source`).
    Libei,
    /// libei directly against gamescope's own EIS socket (no portal): input lands in the
    /// nested game — the SteamOS-like session.
    GamescopeEi,
}

/// Preferred injection backend. Windows has exactly one path (`SendInput`).
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Windows `SendInput` (Win32 KeyboardAndMouse) — the Windows host path.
    SendInput,
}

/// Preferred injection backend. No injector exists on this platform; [`open`] rejects it.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Placeholder so the host still builds; the platform has no input injection.
    Unsupported,
}

/// Open the injector for `backend`. The body is one per-OS block: on each target `backend` can only
/// name a backend that platform has, so there are no cross-OS `bail!` arms (plan §2.3).
#[cfg(target_os = "linux")]
pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::WlrVirtual => Ok(Box::new(wlr::WlrootsInjector::open()?)),
        Backend::KwinFakeInput => Ok(Box::new(kwin_fake_input::KwinFakeInjector::open()?)),
        Backend::Libei => Ok(Box::new(
            libei::LibeiInjector::open_with(libei_ei_source())?,
        )),
        Backend::GamescopeEi => Ok(Box::new(libei::LibeiInjector::open_with(
            libei::EiSource::SocketPathFile(pf_paths::gamescope_ei_socket_file()),
        )?)),
    }
}

/// Open the injector for `backend` (Windows: always `SendInput`).
#[cfg(target_os = "windows")]
pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::SendInput => Ok(Box::new(sendinput::SendInputInjector::open()?)),
    }
}

/// No input-injection backend exists on this platform.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open(_backend: Backend) -> Result<Box<dyn InputInjector>> {
    anyhow::bail!("no input-injection backend on this platform")
}

/// Which output the session's **absolute** coordinates belong to, by identity rather than by size.
///
/// libei hands the injector one region per logical monitor and the region set carries no output
/// name, so the backend has to decide which one a normalized client position maps into. Matching on
/// *size* — all it could do before — is a coin flip the moment two heads share a mode, and it
/// resolved wrong on-glass once already (GNOME, a dummy HDMI beside the virtual primary: the seat
/// cursor never entered the streamed monitor). These are the two keys that actually identify a
/// region: the protocol's own `mapping_id`, and the origin (two outputs can share a size; they can
/// never share a top-left).
///
/// Best-effort by design: an anchor that matches no region warns and falls back to the size ladder,
/// because the region set is the truth and the anchor is our belief about it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AbsoluteAnchor {
    /// The target output's top-left in the compositor's global logical space.
    pub origin: Option<(i32, i32)>,
    /// The EI `mapping_id` of the target output, when the capture side knows it — the protocol's
    /// blessed way to correlate a region with a video stream, so it wins over the origin.
    pub mapping_id: Option<String>,
}

impl AbsoluteAnchor {
    /// Nothing to match on — treated as "no anchor" so callers can build one unconditionally.
    pub fn is_empty(&self) -> bool {
        self.origin.is_none() && self.mapping_id.is_none()
    }
}

/// The current absolute-coordinate anchor. A `RwLock` rather than an env var: the injector is
/// host-lifetime and lives behind a channel, so a *session* can only reach it through process
/// state — and process state that is typed and lock-guarded beats the `set_var` pattern the
/// backend-select still uses (security-review 2026-06-28 #7).
static ABSOLUTE_ANCHOR: std::sync::RwLock<Option<AbsoluteAnchor>> = std::sync::RwLock::new(None);

/// Anchor absolute coordinates at a specific output. `None` (the default) keeps the size-matched
/// behavior.
///
/// ⚠️ **This is a HOST-level pin, not per-session state.** The injector is host-lifetime and every
/// concurrent session's input flows through the same one, so an anchor set per session would apply
/// to all of them — the last connect silently re-aiming everyone else's pointer. That is fine for
/// what this exists for (`PUNKTFUNK_CAPTURE_MONITOR`, a host-wide pin — the host-pinned decision of
/// record in `design/per-monitor-portal-capture.md` §5.3) and wrong for anything per-client. A
/// per-session anchor needs the injector to become session-aware first; don't call this from a
/// session path until it is.
pub fn set_absolute_anchor(anchor: Option<AbsoluteAnchor>) {
    let anchor = anchor.filter(|a| !a.is_empty());
    tracing::debug!(?anchor, "input: absolute-coordinate anchor set");
    *ABSOLUTE_ANCHOR.write().unwrap_or_else(|e| e.into_inner()) = anchor;
}

/// The anchor an injector should map absolute coordinates into, if any.
pub fn absolute_anchor() -> Option<AbsoluteAnchor> {
    ABSOLUTE_ANCHOR
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Pick the injection backend for the current session. gamescope hosts its own EIS server (no
/// portal), so a gamescope session injects directly into it. wlroots/Sway only implements the
/// ScreenCast portal (no RemoteDesktop), so libei can't run there — use the wlr virtual-input
/// protocols. **KWin** exposes `org_kde_kwin_fake_input` (direct injection, no portal / approval
/// dialog — authorized by the host's `.desktop`; what krdpserver uses), so prefer it there.
/// **GNOME** has neither fake_input nor the wlr protocols, so it uses libei — reaching EIS through
/// Mutter's *direct* `org.gnome.Mutter.RemoteDesktop` API rather than the portal
/// (`libei_ei_source`), so it is headless-capable too: no interactive approval to answer.
/// `PUNKTFUNK_INPUT_BACKEND=wlr|kwin|libei|gamescope` overrides the auto-detection.
#[cfg(target_os = "linux")]
pub fn default_backend() -> Backend {
    if let Ok(v) = std::env::var("PUNKTFUNK_INPUT_BACKEND") {
        match v.trim().to_ascii_lowercase().as_str() {
            "wlr" | "wlroots" | "wlrvirtual" => return Backend::WlrVirtual,
            "kwin" | "fakeinput" | "fake_input" | "kwin-fake-input" => {
                return Backend::KwinFakeInput
            }
            "libei" | "ei" | "portal" => return Backend::Libei,
            "gamescope" | "gamescope-ei" => return Backend::GamescopeEi,
            other => tracing::warn!(
                value = other,
                "unknown PUNKTFUNK_INPUT_BACKEND — auto-detecting"
            ),
        }
    }
    // An explicit compositor pick (set per connect / mid-stream) is the strongest signal.
    let compositor = pf_host_config::config().compositor.clone();
    if let Some(c) = compositor.as_deref() {
        let c = c.trim();
        if c.eq_ignore_ascii_case("gamescope") {
            return Backend::GamescopeEi;
        }
        if c.eq_ignore_ascii_case("kwin") {
            return Backend::KwinFakeInput;
        }
        if c.eq_ignore_ascii_case("wlroots")
            || c.eq_ignore_ascii_case("sway")
            // Hyprland kept the wlr virtual-input protocols, so it injects through the same
            // backend as sway/river (design/hyprland-support.md D4).
            || c.eq_ignore_ascii_case("hyprland")
        {
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

/// The Windows host has a single injection backend.
#[cfg(target_os = "windows")]
pub fn default_backend() -> Backend {
    Backend::SendInput
}

/// No injector on this platform.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn default_backend() -> Backend {
    Backend::Unsupported
}

/// Whether the session's inject backend can type **committed text**
/// ([`InputKind::TextInput`] — see `HOST_CAP_TEXT_INPUT`): Windows always (`KEYEVENTF_UNICODE`);
/// Linux only on the wlroots backend (a dedicated virtual keyboard with a dynamically-grown
/// Unicode keymap) — KWin fake-input/libei/gamescope can only press keycodes of the host layout.
/// Consulted at Welcome time to advertise the cap; a mid-session backend switch away from a
/// capable one just degrades to dropped text events (input is lossy by design).
#[cfg(target_os = "windows")]
pub fn text_input_supported() -> bool {
    true
}

/// See the Windows variant: Linux types text only through the wlroots virtual-keyboard backend.
#[cfg(target_os = "linux")]
pub fn text_input_supported() -> bool {
    matches!(default_backend(), Backend::WlrVirtual)
}

/// No injector ⇒ no text.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn text_input_supported() -> bool {
    false
}

/// Whether this host can inject full-fidelity stylus input (`HOST_CAP_PEN` —
/// design/pen-tablet-input.md): Linux only, via the [`pen::VirtualPen`] uinput tablet, so the
/// probe is "can we open /dev/uinput" (the same permission the virtual gamepads need) plus the
/// `PUNKTFUNK_PEN=0` operator kill-switch. Consulted at Welcome time; clients without the bit
/// keep folding pen into touch/pointer. Windows PT_PEN synthetic pointers are the design's P3.
#[cfg(target_os = "linux")]
pub fn pen_supported() -> bool {
    if std::env::var("PUNKTFUNK_PEN").as_deref() == Ok("0") {
        return false;
    }
    // SAFETY: 'static NUL-terminated path literal; `open` returns a fresh fd (or -1) and
    // retains nothing.
    let fd = unsafe {
        libc::open(
            c"/dev/uinput".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    // SAFETY: `fd >= 0` is the fd opened above, owned by no one else; closed exactly once here.
    unsafe { libc::close(fd) };
    true
}

/// Windows: pen (and touch) inject via synthetic pointer devices — available on Win10 1809+,
/// probed by actually creating (and immediately destroying) a PT_PEN device. Same
/// `PUNKTFUNK_PEN=0` kill-switch as Linux. The probe result also stands in for PT_TOUCH
/// (both APIs arrived together in 1809).
#[cfg(target_os = "windows")]
pub fn pen_supported() -> bool {
    if std::env::var("PUNKTFUNK_PEN").as_deref() == Ok("0") {
        return false;
    }
    pen::synthetic_pen_available()
}

/// See the Linux/Windows variants — no pen injection elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn pen_supported() -> bool {
    false
}

#[path = "inject/service.rs"]
mod service;
pub use service::InjectorService;

/// How the libei backend reaches its EIS server. KWin goes through the `RemoteDesktop` *portal*
/// (with a pre-seeded grant), but GNOME's portal `Start()` needs an interactive approval a
/// headless host can't answer — so GNOME goes straight to Mutter's *direct* RemoteDesktop EIS
/// (`org.gnome.Mutter.RemoteDesktop`), the same direct API the Mutter video backend uses.
#[cfg(target_os = "linux")]
fn libei_ei_source() -> libei::EiSource {
    let gnome = pf_host_config::config()
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

// Goal-1 stage 6: Linux UHID/uinput/libei/wlr backends under `inject/linux/`, the Windows UMDF/SendInput
// backends under `inject/windows/`, and the transport-independent HID codecs under `inject/proto/`;
// `#[path]` keeps every `crate::*` module name flat.
/// Windows: asks a devnode which process is serving it (`pf_driver_proto::gamepad::ChannelProof`) —
/// the unforgeable answer the sealed pad channel duplicates its DATA section into, replacing the
/// LocalService-writable bootstrap mailbox as the source of that decision.
#[cfg(target_os = "windows")]
#[path = "inject/windows/channel_proof.rs"]
pub mod channel_proof;
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualsense.rs"]
pub mod dualsense;
/// Windows: virtual DualSense **Edge** via the same UMDF minidriver + shared-memory channel
/// (device-type 2) — the wire back grips land on the Edge's native back/Fn buttons.
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualsense_edge_windows.rs"]
pub mod dualsense_edge_windows;
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
/// Transport-independent DualShock 4 HID codec, shared by the Linux UHID backend ([`dualshock4`])
/// and the Windows UMDF-driver backend ([`dualshock4_windows`]).
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
/// Windows: the RESIDENT virtual HID mouse via the pf-mouse UMDF minidriver — keeps
/// `SM_MOUSEPRESENT` true on headless hosts so DWM composites a cursor into the IDD frame
/// (`SendInput` alone moves an invisible pointer when no physical mouse is attached).
#[cfg(target_os = "windows")]
#[path = "inject/windows/mouse_windows.rs"]
pub mod mouse_windows;
/// Shared virtual-pad creation-retry policy ([`pad_gate::PadGate`]), driven by [`pad_slots`] for
/// every backend manager — replaces the per-backend permanent `broken` latch with capped-backoff
/// retry.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/pad_gate.rs"]
pub mod pad_gate;
/// Shared virtual-pad slot table + creation lifecycle ([`pad_slots::PadSlots`]) — the
/// `Vec<Option<Pad>>` table, `active_mask` unplug sweep, and gate-checked create every backend
/// manager used to copy-paste (G12).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/pad_slots.rs"]
pub mod pad_slots;
/// The `sensor_timestamp` every virtual Sony pad stamps into its input reports
/// ([`sensor_clock::SensorClock`]) — real elapsed time in the DualSense's 1/3 µs and the
/// DualShock 4's 5.33 µs units, shared by all four backends.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/sensor_clock.rs"]
pub mod sensor_clock;
/// Linux: virtual Steam Deck via UHID — the kernel `hid-steam` driver binds it as a real Deck.
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_controller.rs"]
pub mod steam_controller;
/// Linux: virtual Steam Controller 2 (Triton, `28DE:1302`) via UHID — as-is raw passthrough of a
/// client-captured physical pad; Steam Input drives the hidraw node (no kernel driver binds it).
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_controller2.rs"]
pub mod steam_controller2;
/// Windows: virtual Steam Deck via the same UMDF minidriver + shared-memory channel
/// (device-type 3) — promoted by Steam Input thanks to the `&MI_02` hardware-id synthesis.
#[cfg(target_os = "windows")]
#[path = "inject/windows/steam_deck_windows.rs"]
pub mod steam_deck_windows;
/// Linux: virtual Steam Deck via the USB gadget subsystem (`raw_gadget` + `dummy_hcd`) — the only
/// virtual-Deck transport Steam Input promotes (presents the controller on USB interface 2).
/// SteamOS-host only (needs `dummy_hcd` + `raw_gadget`).
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_gadget.rs"]
pub mod steam_gadget;
/// Transport-independent Steam Controller / Steam Deck HID contract (descriptor, byte-exact Deck
/// serializer, XInput/rich mappers, rumble parser), used by the Linux UHID backend
/// ([`steam_controller`]) and the Windows UMDF backend ([`steam_deck_windows`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/steam_proto.rs"]
pub mod steam_proto;
/// Pure fallback-remap policy (Steam-only inputs onto a non-Steam backend) + the Deck motion rescale.
/// Shared by the Linux and Windows DualSense/DS4 backends (the slot-less pads that must fold the
/// Steam back grips); the Deck motion rescale is Linux-only but harmless to compile on Windows.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/steam_remap.rs"]
pub mod steam_remap;
/// Linux: virtual Steam Deck over **USB/IP** (`vhci_hcd`) — the shippable, Secure-Boot-clean,
/// Steam-Input-promotable virtual-Deck transport on non-SteamOS hosts (Bazzite/generic), where
/// `dummy_hcd`/`raw_gadget` aren't built. In-tree + signed; no module build, no MOK.
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_usbip.rs"]
pub mod steam_usbip;
/// Linux: virtual Nintendo Switch Pro Controller via UHID (kernel `hid-nintendo`).
#[cfg(target_os = "linux")]
#[path = "inject/linux/switch_pro.rs"]
pub mod switch_pro;
/// Transport-independent Switch Pro Controller codec + the canned `hid-nintendo` handshake
/// replies, used by the Linux UHID backend ([`switch_pro`]).
#[cfg(target_os = "linux")]
#[path = "inject/proto/switch_proto.rs"]
pub mod switch_proto;
/// Transport-independent Steam Controller 2 (Triton) contract: descriptor, SDL-documented report
/// layout, the typed fallback serializer, and the rumble-output parser. Linux-only consumer today
/// ([`steam_controller2`]).
#[cfg(target_os = "linux")]
#[path = "inject/proto/triton_proto.rs"]
pub mod triton_proto;
/// Linux: virtual Steam Controller 2 over **USB/IP** — a real USB device byte-matched to the
/// physical wired pad's captured descriptors, so Steam lists it (the UHID leg is confirmed
/// invisible to Steam). Preferred transport of [`steam_controller2`].
#[cfg(target_os = "linux")]
#[path = "inject/linux/triton_usbip.rs"]
pub mod triton_usbip;
/// Linux: the `/dev/uhid` event ABI shared by every UHID gamepad backend — the constants each
/// used to transcribe for itself, plus the field accessors that read a payload's real length.
#[cfg(target_os = "linux")]
#[path = "inject/linux/uhid_abi.rs"]
pub mod uhid_abi;
/// The generic stateful virtual-pad manager ([`uhid_manager::UhidManager`]) — event routing, frame
/// merge, heartbeat, and feedback pump shared by the five UHID/UMDF backends; each supplies only
/// its per-controller protocol via [`uhid_manager::PadProto`] (G12).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/uhid_manager.rs"]
pub mod uhid_manager;
/// Transport-independent Xbox Wireless Controller HID codec — the report the `pf-gamepad` UMDF
/// driver serves under device-type 4, giving an Xbox pad the HID footing `pf-xusb` never had
/// (Steam / WGI / GameInput / DirectInput cannot see an XUSB-interface-only device).
///
/// Deliberately NOT cfg-gated to linux/windows like its siblings: it is pure byte-packing with no
/// OS surface, so its layout tests compile and run on any host — including the macOS dev machines
/// where the Windows backends cannot be built at all. That is the only automated check this codec
/// has until a Windows box is reachable.
#[path = "inject/proto/xbox_proto.rs"]
pub mod xbox_proto;
/// Stub — virtual gamepads need Linux uinput or the Windows UMDF drivers; events are dropped elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub mod gamepad {
    #[derive(Default)]
    pub struct GamepadManager;
    impl GamepadManager {
        pub fn new() -> Self {
            GamepadManager
        }
        pub fn handle(&mut self, _ev: &punktfunk_core::input::GamepadEvent) {}
        pub fn pump_rumble(&mut self, _send: impl FnMut(u16, u16, u16)) {}
    }
}
/// Linux: the "Punktfunk Pen" uinput virtual tablet (design/pen-tablet-input.md §5) — the
/// per-session stylus device the native pen plane injects through.
#[cfg(target_os = "linux")]
#[path = "inject/linux/pen.rs"]
pub mod pen;
/// Windows: PT_PEN/PT_TOUCH synthetic pointer devices (design/pen-tablet-input.md §6).
/// `pen::VirtualPen` here is the PT_PEN device; `pen::SyntheticTouch` backs the SendInput
/// injector's wire-touch path.
#[cfg(target_os = "windows")]
#[path = "inject/windows/pointer_windows.rs"]
pub mod pen;
/// Windows: the streamed output's desktop rect that every absolute coordinate (pen, touch,
/// absolute mouse) maps into — published by the host at capture bring-up, resolved through the
/// CCD source rect (the cursor-readback poller's resolver, so both directions agree). Mapping
/// over the whole virtual desktop instead is the Extend-topology offset bug the pen exposed
/// (design/pen-tablet-input.md).
#[cfg(target_os = "windows")]
#[path = "inject/windows/stream_target.rs"]
pub mod stream_target;
#[cfg(target_os = "windows")]
pub use stream_target::set_stream_target;
/// Stub — pen injection needs the Linux uinput tablet or Windows synthetic pointers;
/// `pen_supported()` is false here, so no host advertises the cap and no batches arrive.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub mod pen {
    use anyhow::{bail, Result};
    pub struct VirtualPen;
    impl VirtualPen {
        pub fn create() -> Result<VirtualPen> {
            bail!("no pen injection backend on this platform")
        }
        pub fn apply_batch(&mut self, _transitions: &[punktfunk_core::quic::PenTransition]) {}
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
