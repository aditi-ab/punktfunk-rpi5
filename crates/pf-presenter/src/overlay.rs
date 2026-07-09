//! The presenter↔console-UI contract (punktfunk-planning
//! `linux-client-rearchitecture.md` §6.1): the presenter exposes its device and
//! composites at most ONE sampled RGBA quad per frame; the overlay implementation
//! (pf-console-ui, Skia) fills offscreen images on its own damage-driven schedule. No
//! Skia type crosses this line — everything here is ash — and a `frame()` returning
//! `None` costs the hot path nothing (the quad isn't even recorded).

use ash::vk;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use punktfunk_core::config::GamepadPref;

/// The presenter's device, shared with the overlay so its renderer (Skia's
/// `DirectContext`) creates resources on the same VkDevice/queue. Handles stay valid for
/// the presenter's lifetime — the overlay must be dropped before it (the run loop owns
/// both and drops the overlay first).
pub struct SharedDevice {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
}

/// What the overlay may draw this frame — composed by the run loop from session state.
/// Milestone 1 (OSD/HUD) is text-shaped; the console library replaces this with a
/// richer scene enum when it moves in.
pub struct FrameCtx<'a> {
    /// Swapchain size in pixels — the overlay renders 1:1.
    pub width: u32,
    pub height: u32,
    /// Multi-line stats OSD (top-left panel); `None` = hidden.
    pub stats: Option<&'a str>,
    /// The capture hint (bottom-center pill, "click to capture…"); `None` = hidden.
    pub hint: Option<&'a str>,
    /// The active gamepad's name (the console library's controller chip).
    pub pad: Option<&'a str>,
    /// The active pad's resolved kind — drives the console UI's button glyphs
    /// (PlayStation shapes for DualSense/DualShock, ABXY letters otherwise).
    pub pad_pref: Option<GamepadPref>,
    /// Every connected pad (the console settings' "Use controller" row).
    pub pads: &'a [pf_client_core::gamepad::PadInfo],
}

/// One overlay image ready to composite: RGBA, PREMULTIPLIED alpha, already in
/// `SHADER_READ_ONLY_OPTIMAL`, sized `width`×`height` (normally the `FrameCtx` size; a
/// stale size during a resize just stretches for a frame).
pub struct OverlayFrame {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
}

/// An action the overlay raises out of its input handling (browse mode). Only actions
/// the RUN LOOP must act on live here — starting/canceling sessions and quitting; data
/// work (pairing, discovery, library fetches…) rides the console command bus instead.
pub enum OverlayAction {
    /// Start a session on this host. `launch` carries a library title id on the Hello
    /// (`None` streams the desktop); `title` is display-only (window title).
    Launch {
        addr: String,
        port: u16,
        fp_hex: String,
        launch: Option<String>,
        title: String,
    },
    /// Abort an in-flight connect (B while Connecting) — the console keeps browsing.
    /// The run loop stops the pump; a dial that already won the race is quit-closed.
    CancelConnect,
    /// Quit the launcher (B at the root) — ends the process, Gaming Mode returns.
    Quit,
}

/// Session lifecycle notifications into the overlay (browse mode drives its scenes off
/// these; the OSD/HUD ignore them).
pub enum SessionPhase<'a> {
    /// A launch action was accepted — the connect is in flight.
    Connecting,
    /// Connected; frames are coming.
    Streaming,
    /// The connect failed (browse mode returns to the library with this message).
    Failed(&'a str),
    /// The session ran and ended (`Some` = abnormal reason for the status strip).
    Ended(Option<&'a str>),
}

/// The console-UI side. Object-safe; the session binary passes
/// `Option<Box<dyn Overlay>>` (None = the Skia-free power-user build).
pub trait Overlay {
    /// One-time setup on the presenter's device.
    fn init(&mut self, shared: &SharedDevice) -> anyhow::Result<()>;

    /// Input routing, before capture sees the event. `true` = consumed (the library or
    /// a menu is up) — the event must not reach capture/forwarding.
    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool;

    /// Gamepad menu-mode navigation (browse mode; the run loop drains the service's
    /// menu channel). Returns a haptic pulse to play on the menu pad, if any.
    fn handle_menu(&mut self, _event: MenuEvent) -> Option<MenuPulse> {
        None
    }

    /// Drain one pending action raised by handled input. Called once per loop
    /// iteration; return `None` when idle.
    fn take_action(&mut self) -> Option<OverlayAction> {
        None
    }

    /// A session lifecycle edge (browse mode scene driving).
    fn session_phase(&mut self, _phase: SessionPhase) {}

    /// True while a text field is being edited — the run loop starts/stops SDL text
    /// input to match (IME + `Event::TextInput` delivery on desktop; under gamescope
    /// this is also what lets Steam's on-screen keyboard type into the app).
    fn text_input_active(&self) -> bool {
        false
    }

    /// Once per presenter iteration. Damage-driven: re-render (flush + transition to
    /// SHADER_READ_ONLY) only when the content or size changed, else return the previous
    /// image. `None` = nothing to composite. The returned image must stay untouched
    /// until `frame()` runs again (the presenter runs one frame in flight and the
    /// implementation keeps a ring of two, so alternating satisfies this).
    fn frame(&mut self, ctx: &FrameCtx) -> anyhow::Result<Option<OverlayFrame>>;
}
