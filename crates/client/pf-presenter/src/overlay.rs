//! Presenter↔console-UI overlay contract: shared Vulkan handles, per-frame
//! scene facts, and the `Overlay` trait the run loop drives.
//!
//! The presenter composites at most one sampled premultiplied RGBA quad per
//! frame. The overlay (pf-console-ui, Skia) fills offscreen images on its own
//! damage-driven schedule. No Skia type crosses this crate. `frame()` returning
//! `None` records no quad.
//!
//! Pin: `Option<Box<dyn Overlay>>` from the session binary; `None` is the
//! Skia-free build. Evidence: punktfunk-planning `linux-client-rearchitecture.md`.

use ash::vk;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use punktfunk_core::config::GamepadPref;

/// Presenter's Vulkan handles. Overlay allocations use this device and queue.
/// Valid for the presenter's lifetime; the run loop drops the overlay first.
pub struct SharedDevice {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
    /// Decode submits to `queue` from the pump thread. Overlay flush/submit
    /// holds this via [`pf_client_core::video::QueueLock::guard`].
    pub queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    /// Overlay function-table cap: min of [`crate::vk::INSTANCE_API_VERSION`]
    /// and the loader. Do not probe `vkEnumerateInstanceVersion` — extra
    /// `vkGetDeviceProcAddr` entry points above that are null.
    pub api_version: u32,
    /// This device decodes AV1 in hardware ([`pf_client_core::video::av1_advertised`]) —
    /// the same answer that gates the `CODEC_AV1` advertisement, so a settings UI on this
    /// overlay says what the Hello asks.
    pub av1_decode: bool,
}

/// Per-frame overlay scene. The run loop fills this from session state.
pub struct FrameCtx<'a> {
    /// Swapchain size in pixels. Overlay renders 1:1.
    pub width: u32,
    pub height: u32,
    /// The swapchain is 10-bit; the overlay draws in 10 too, or its gradients band.
    pub ten_bit: bool,
    /// Window display scale (`1.0` at 96 dpi / 100 %) × `PUNKTFUNK_OSD_SCALE`.
    /// Overlay chrome is physical pixels; multiply every metric by this. The
    /// run loop (`overlay_scale`) clamps it finite and > 0.
    pub scale: f32,
    /// Stats overlay lines, painted by role. `None` = overlay off.
    pub stats: Option<&'a [HudLine]>,
    /// The stats panel's corner, and its size on top of [`Self::scale`].
    pub stats_corner: punktfunk_core::hud::HudCorner,
    pub stats_scale: f32,
    /// Show the exit hint when a stream starts; the overlay owns its timing.
    pub exit_hint: bool,
    pub hint: Option<&'a str>,
    /// Access chip. `None` for a full-control permanent session.
    pub access: Option<&'a str>,
    /// Transient access toast. Occupies the hint slot while up; the run loop owns timing.
    pub notice: Option<&'a str>,
    /// The player just muted the microphone (`true`) or opened it again
    /// ([`punktfunk_core::client::mic_mute_notice`]); `None` otherwise. Independent of the
    /// stats tier, so it still shows with chrome off.
    pub mic_notice: Option<bool>,
    /// The stream's mute sentence ([`punktfunk_core::client::audio_mute_notice`]); `None`
    /// while it is audible, or once a local mute has been read. A host mute stands.
    /// Independent of the stats tier.
    pub audio_mute: Option<&'a str>,
    /// Mid-stream Match-window resize in flight (`design/midstream-resolution-resize.md`).
    /// Draw a full-screen scrim and spinner until the new-resolution frame arrives.
    pub resizing: bool,
    pub pad: Option<&'a str>,
    /// Resolved pad kind for button glyphs (PlayStation shapes vs ABXY).
    pub pad_pref: Option<GamepadPref>,
    pub pads: &'a [pf_client_core::gamepad::PadInfo],
    /// Quick-action ring facts (`design/touch-client-overlay.md`). `None` outside a stream.
    pub ring: Option<&'a RingFacts>,
    /// The window's display as a stream request: what a Native connect asks for.
    pub native: punktfunk_core::Mode,
}

/// Overlay image ready to composite: RGBA, premultiplied, already in
/// `SHADER_READ_ONLY_OPTIMAL`. A stale `width`×`height` during resize
/// stretches for one frame.
pub struct OverlayFrame {
    /// Every pixel is opaque; only the full console sets this.
    pub opaque: bool,
    pub image: vk::Image,
    pub view: vk::ImageView,
    /// The image's format; the native Wayland lane copies it into a buffer of the same.
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    /// Vertical pixel band containing non-transparent overlay content.
    pub scissor_y: u32,
    pub scissor_height: u32,
}

impl OverlayFrame {
    /// Only a full opaque redraw may discard the picture underneath it.
    pub(crate) fn replaces_video(&self, redraw: bool, width: u32, height: u32) -> bool {
        redraw
            && self.opaque
            && width > 0
            && height > 0
            && self.width == width
            && self.height == height
            && self.scissor_y == 0
            && self.scissor_height == height
    }
}

// Shared with the Android GL host; lives in `pf_client_core::console`
// (pf-console-ui sits above this crate). Re-exported so the run loop and the
// console keep the `pf_presenter::overlay::…` path.
pub use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
pub use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
pub use punktfunk_core::hud::{HudLine, Role};

/// Console-UI side. The session binary passes `Option<Box<dyn Overlay>>`;
/// `None` is the Skia-free build.
pub trait Overlay {
    fn init(&mut self, shared: &SharedDevice) -> anyhow::Result<()>;

    /// Before capture. `true` consumes the event; do not forward.
    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool;

    /// Browse-mode gamepad menu nav. The run loop drains the service channel.
    fn handle_menu(&mut self, _event: MenuEvent) -> Option<MenuPulse> {
        None
    }

    /// Pointer in swapchain pixels, before capture. `true` consumes; do not
    /// forward. Separate from [`Self::handle_event`]: window→pixel belongs to
    /// the run loop (it holds the window); the overlay never sees display scale.
    fn handle_pointer(&mut self, _input: PointerInput) -> bool {
        false
    }

    /// Drain one action raised by handled input. Once per loop iteration.
    fn take_action(&mut self) -> Option<OverlayAction> {
        None
    }

    /// Browse-mode session lifecycle. OSD/HUD ignore this.
    fn session_phase(&mut self, _phase: SessionPhase) {}

    /// The host no longer has the profile the connect named. Comes just before that
    /// connect's [`SessionPhase::Failed`].
    fn profile_gone(&mut self) {}

    /// A text field is being edited. The run loop starts and stops SDL text
    /// input (IME / `Event::TextInput`) from this.
    fn text_input_active(&self) -> bool {
        false
    }

    /// A fresh console image can be drawn now; cached idle frames return false.
    fn redraw_due(&self) -> bool {
        false
    }

    /// Once per presenter iteration. Re-render (flush + `SHADER_READ_ONLY`) only
    /// on content or size change. The returned image stays untouched until the
    /// next `frame()` — one frame in flight; a ring of two is enough.
    fn frame(&mut self, ctx: &FrameCtx) -> anyhow::Result<Option<OverlayFrame>>;

    fn ring_input(&mut self, _input: RingInput) {}

    /// Ring is up: pointer and keys belong to it; touch fingers stop feeding
    /// the gesture engine.
    fn ring_open(&self) -> bool {
        false
    }

    /// The console is drawn over a live stream (a launch hold) and takes the pad
    /// as menu events, exactly as the ring does.
    fn holds_stream(&self) -> bool {
        false
    }

    /// Drain one ring command. Each iteration while streaming.
    fn take_ring_command(&mut self) -> Option<RingCommand> {
        None
    }
}

#[cfg(test)]
mod opaque_tests {
    use super::*;

    #[test]
    fn only_opaque_full_cover_redraw_discards_video() {
        let mut frame = OverlayFrame {
            opaque: true,
            image: vk::Image::null(),
            view: vk::ImageView::null(),
            format: vk::Format::R8G8B8A8_UNORM,
            width: 2560,
            height: 1440,
            scissor_y: 0,
            scissor_height: 1440,
        };
        assert!(frame.replaces_video(true, 2560, 1440));
        assert!(!frame.replaces_video(false, 2560, 1440));
        assert!(!frame.replaces_video(true, 3840, 1600));
        frame.opaque = false;
        assert!(!frame.replaces_video(true, 2560, 1440));
        frame.opaque = true;
        frame.scissor_height = 300;
        assert!(!frame.replaces_video(true, 2560, 1440));
        frame.scissor_height = 1440;
        frame.scissor_y = 1;
        assert!(!frame.replaces_video(true, 2560, 1440));
        frame.scissor_y = 0;
        frame.width = 0;
        assert!(!frame.replaces_video(true, 0, 1440));
    }
}
