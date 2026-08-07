//! Skia on the presenter's device: `DirectContext` over the shared handles, a ring of
//! two offscreen render-target surfaces (the presenter runs one frame in flight and may
//! still be sampling the previous image while we render the next), damage-driven
//! redraws (content/size change only — an unchanged OSD costs zero GPU work per frame).
//!
//! Two personas over one `Overlay`: the CONSOLE (the full shell — home, library,
//! settings, pairing — always dirty, the aurora animates) and the stream chrome (stats
//! OSD, capture hint, the auto-fading start banner).

use crate::model::{ConsoleBus, ConsoleShared, HostRow};
use crate::screens::Screen;
use crate::shell::{ConsoleOptions, Shell};
use crate::theme::{match_first_family, Fonts};
use anyhow::{anyhow, Context as _, Result};
use ash::vk as avk;
use ash::vk::Handle as _;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use pf_presenter::overlay::{
    FrameCtx, Overlay, OverlayAction, OverlayFrame, SessionPhase, SharedDevice,
};
use skia_safe::gpu::vk as skvk;
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Font, FontMgr, Paint, Point, RRect, Rect, Surface};
use std::time::Instant;

/// Skia's GPU resource budget — poster art plus a few screen layers; 64 MB fits
/// Deck-class shared memory.
const RESOURCE_CACHE_BYTES: usize = 64 << 20;

/// How long the start-of-stream banner lingers (fading through the tail).
const BANNER_S: f64 = 6.0;
const BANNER_FADE_S: f64 = 0.6;

/// One offscreen target: the Skia surface + the raw Vulkan handles the presenter
/// samples. The image is Skia-owned (freed with the surface); the view is ours.
struct Slot {
    surface: Surface,
    image: avk::Image,
    view: avk::ImageView,
    width: u32,
    height: u32,
}

/// What the current ring slot has drawn — re-render only when this changes.
#[derive(PartialEq, Clone, Default)]
struct Drawn {
    width: u32,
    height: u32,
    stats: Option<String>,
    hint: Option<String>,
    /// The mic-mute badge is up. Part of the damage key like everything else here — the badge
    /// is static once drawn, so a muted stream still re-renders nothing per frame.
    mic_muted: bool,
    /// The UI scale this was drawn at, in percent — part of the damage key so dragging the window
    /// to a differently-scaled monitor re-renders the chrome at the new size instead of keeping
    /// the stale one (the text is identical, so nothing else here would notice).
    scale_pct: u16,
    /// The start banner's alpha, quantized — a fade step is a redraw, steady is not.
    banner_step: u8,
    /// The resize scrim's spinner phase, quantized — a nonzero, ever-changing step while a
    /// mid-stream resize is in flight forces the per-frame redraw the spinner needs; `0`
    /// when no resize is showing (so a still stream stays damage-free).
    resize_step: u16,
}

/// The stream chrome's base metrics, in pixels at 100 % scale (96 dpi). Everything here is
/// multiplied by `FrameCtx::scale` before it is drawn: the overlay composites into the swapchain
/// 1:1 in PHYSICAL pixels, so on a 4K panel at 200 % an unscaled 14 px OSD renders at half its
/// intended physical size — legible on a 1080p monitor, a squint on a HiDPI laptop.
mod base {
    /// The monospace OSD/hint/label size. Also the size the shared `Font` is built at, so the
    /// scale factor below is exactly the multiplier applied to it.
    pub const FONT_PX: f32 = 14.0;
    /// Top-left inset of the stats panel.
    pub const OSD_MARGIN: f32 = 12.0;
    /// Stats-panel inner padding and corner radius.
    pub const OSD_PAD_X: f32 = 10.0;
    pub const OSD_PAD_Y: f32 = 8.0;
    pub const OSD_RADIUS: f32 = 8.0;
    /// Hint/banner pill padding and its gap from the bottom edge.
    pub const PILL_PAD_X: f32 = 14.0;
    pub const PILL_PAD_Y: f32 = 8.0;
    pub const PILL_BOTTOM: f32 = 24.0;
}

/// Where the console starts (the session binary's `--browse` forms).
pub enum ConsoleEntry {
    /// The host list (bare `--browse`).
    Home,
    /// Home with this host's library already pushed (`--browse host` — the Decky
    /// per-host launch; B backs out to Home). Boxed: `HostRow` outgrew the dataless
    /// `Home` variant when it learned its profile chips.
    Library(Box<HostRow>),
}

/// The binary's ends of the console: models to write, commands to serve.
pub struct ConsoleHandles {
    pub console: ConsoleShared,
    pub library: crate::library::LibraryShared,
    pub bus: ConsoleBus,
}

pub struct SkiaOverlay {
    /// Set by `init`; `None` until then (and after an init failure the run loop drops
    /// the whole overlay, so mid-session these are always `Some`).
    gpu: Option<Gpu>,
    slots: [Option<Slot>; 2],
    /// Which slot the LAST returned frame lives in — the next render takes the other.
    current: usize,
    drawn: Drawn,
    /// The stats OSD's monospace face (system-resolved; the console uses Geist).
    font: Option<Font>,
    fonts: Option<Fonts>,
    /// The console shell (`--browse`) — `None` for a plain `--connect` session.
    shell: Option<Shell>,
    /// When the current stream started presenting — drives the start banner.
    streaming_since: Option<Instant>,
    /// The banner's words (set per stream from the active-pad state).
    banner_text: Option<String>,
    /// When the current mid-stream resize scrim began showing — drives its spinner phase.
    /// `None` = no resize in flight (`FrameCtx::resizing` was false last frame).
    resizing_since: Option<Instant>,
}

struct Gpu {
    device: ash::Device,
    queue_family_index: u32,
    context: DirectContext,
    /// The device's shared queue lock (see `SharedDevice::queue_lock`): Skia submits
    /// on the presenter's graphics queue, which FFmpeg's decode prep also uses from
    /// the pump thread — every `flush*`/`submit` below holds this.
    queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    // Keep the loader library + instance dispatch alive as long as the DirectContext
    // (its baked fn pointers live inside libvulkan).
    _entry: ash::Entry,
    _instance: ash::Instance,
}

impl SkiaOverlay {
    #[allow(clippy::new_without_default)]
    pub fn new() -> SkiaOverlay {
        SkiaOverlay {
            gpu: None,
            slots: [None, None],
            current: 0,
            drawn: Drawn::default(),
            font: None,
            fonts: None,
            shell: None,
            streaming_since: None,
            banner_text: None,
            resizing_since: None,
        }
    }

    /// The `--browse` overlay: the full console shell between streams, stream chrome
    /// during them. Returns the binary's handles (models + command bus).
    pub fn console(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
    ) -> Result<(SkiaOverlay, ConsoleHandles)> {
        let console = ConsoleShared::default();
        let library = crate::library::LibraryShared::default();
        let bus = ConsoleBus::default();
        let stack = match entry {
            ConsoleEntry::Home => vec![Screen::Home(crate::screens::home::HomeScreen::new())],
            ConsoleEntry::Library(host) => vec![
                Screen::Home(crate::screens::home::HomeScreen::new()),
                Screen::Library(crate::screens::library::LibraryScreen::new(&host)),
            ],
        };
        let shell = Shell::new(console.clone(), library.clone(), bus.clone(), opts, stack)?;
        let mut o = SkiaOverlay::new();
        o.shell = Some(shell);
        Ok((
            o,
            ConsoleHandles {
                console,
                library,
                bus,
            },
        ))
    }

    fn console_visible(&self) -> bool {
        self.shell.as_ref().is_some_and(|s| !s.in_stream)
    }
}

impl Drop for SkiaOverlay {
    /// The run loop quiesces the queue before dropping us; releasing the views + Skia
    /// surfaces (which free their VkImages) is then safe. Field order drops the slots
    /// before the DirectContext.
    fn drop(&mut self) {
        if let Some(gpu) = &mut self.gpu {
            for slot in self.slots.iter_mut().flat_map(Option::take) {
                // SAFETY: the view belongs to this slot and the overlay is being dropped, so no
                // further recording can reference it; the flush/submit + queue guard below is what
                // retires any work that still could.
                unsafe { gpu.device.destroy_image_view(slot.view, None) };
                drop(slot.surface);
            }
            let _q = gpu.queue_lock.guard(); // queue external sync vs FFmpeg's pump
            gpu.context.flush_and_submit();
        }
    }
}

impl Overlay for SkiaOverlay {
    fn init(&mut self, shared: &SharedDevice) -> Result<()> {
        // Skia resolves its Vulkan entry points through us: instance-scoped names via
        // the loader, device-scoped via the device — the exact same dispatch ash uses.
        // Resolution completes inside `make_vulkan` (the DirectContext bakes its fn
        // table); the closure and its clones end with `init`.
        let entry = shared.entry.clone();
        let instance = shared.instance.clone();
        let get_proc = move |of: skvk::GetProcOf| -> *const std::ffi::c_void {
            // SAFETY: Skia calls this loader with raw instance/device handles it received from the
            // `BackendContext` below — i.e. the very ones owned by `shared`, still live for the
            // overlay's lifetime — and `from_raw` only rewraps them for the ash entry points. Each
            // name is a NUL-terminated C string from Skia, borrowed for the call.
            unsafe {
                match of {
                    skvk::GetProcOf::Instance(raw_instance, name) => entry
                        .get_instance_proc_addr(avk::Instance::from_raw(raw_instance as _), name)
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void),
                    skvk::GetProcOf::Device(raw_device, name) => {
                        (instance.fp_v1_0().get_device_proc_addr)(
                            avk::Device::from_raw(raw_device as _),
                            name,
                        )
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
                    }
                }
            }
        };
        // SAFETY: the instance/physical-device/device handles come from `shared`, which owns them
        // and outlives this backend context, and `get_proc` above resolves through those same
        // handles. Skia stores them but does not take ownership — teardown stays ours.
        let backend = unsafe {
            skvk::BackendContext::new(
                shared.instance.handle().as_raw() as _,
                shared.physical_device.as_raw() as _,
                shared.device.handle().as_raw() as _,
                (
                    shared.queue.as_raw() as _,
                    shared.queue_family_index as usize,
                ),
                &get_proc,
            )
        };
        let mut context = gpu::direct_contexts::make_vulkan(&backend, None)
            .ok_or_else(|| anyhow!("Skia DirectContext over the shared device"))?;
        context.set_resource_cache_limit(RESOURCE_CACHE_BYTES);

        let typeface = match_first_family(
            &FontMgr::new(),
            &["monospace", "Consolas", "Cascadia Mono", "Courier New"],
            skia_safe::FontStyle::normal(),
        )
        .context("no monospace typeface (fontconfig alias or system family)")?;
        self.font = Some(Font::new(typeface, base::FONT_PX));
        self.fonts = Some(crate::theme::build_fonts()?);

        self.gpu = Some(Gpu {
            device: shared.device.clone(),
            queue_family_index: shared.queue_family_index,
            context,
            queue_lock: shared.queue_lock.clone(),
            _entry: shared.entry.clone(),
            _instance: shared.instance.clone(),
        });
        tracing::info!("Skia console UI on the presenter's device");
        Ok(())
    }

    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool {
        // Keyboard + text into the console while it's on screen — never for
        // chord-modified keys (those stay the run loop's), never during a stream.
        if !self.console_visible() {
            return false;
        }
        let Some(shell) = &mut self.shell else {
            return false;
        };
        match event {
            sdl3::event::Event::KeyDown {
                scancode: Some(sc),
                keymod,
                repeat,
                ..
            } => {
                use sdl3::keyboard::Mod;
                if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LALTMOD | Mod::RALTMOD) {
                    return false;
                }
                shell.key(*sc, *repeat)
            }
            sdl3::event::Event::TextInput { text, .. } => {
                shell.text_input(text);
                true
            }
            _ => false,
        }
    }

    fn handle_menu(&mut self, event: MenuEvent) -> Option<MenuPulse> {
        if self.console_visible() {
            self.shell.as_mut().and_then(|s| s.handle_menu(event))
        } else {
            None
        }
    }

    fn take_action(&mut self) -> Option<OverlayAction> {
        self.shell.as_mut().and_then(|s| s.take_action())
    }

    fn text_input_active(&self) -> bool {
        self.console_visible() && self.shell.as_ref().is_some_and(Shell::editing)
    }

    fn session_phase(&mut self, phase: SessionPhase) {
        let Some(shell) = &mut self.shell else { return };
        match phase {
            SessionPhase::Connecting => {} // the shell raised the Launch; already showing
            SessionPhase::Streaming => {
                shell.session_streaming();
                self.streaming_since = Some(Instant::now());
            }
            SessionPhase::Failed(msg) => shell.session_failed(msg),
            SessionPhase::Ended(reason) => {
                shell.session_ended(reason);
                self.streaming_since = None;
            }
            // The stream stopped but a new dial is already in flight: toast WHY (the
            // codec changed under the user) AND raise the connecting takeover, because
            // nothing else will — the run loop starts the retry's pump directly rather
            // than through a `Launch`, so the shell would otherwise sit in a state where
            // a menu press could start a second session over the running one.
            SessionPhase::Reconnecting(msg) => {
                shell.session_reconnecting(msg);
                self.streaming_since = None;
            }
        }
    }

    fn frame(&mut self, ctx: &FrameCtx) -> Result<Option<OverlayFrame>> {
        // The console: full-screen, opaque, and always dirty (the aurora animates
        // every frame — the GPU port's whole point).
        if self.console_visible() {
            let next = 1 - self.current;
            self.ensure_slot(next, ctx.width, ctx.height)?;
            let Self {
                gpu,
                slots,
                shell,
                fonts,
                ..
            } = self;
            let gpu = gpu.as_mut().expect("init ran");
            let slot = slots[next].as_mut().expect("just ensured");
            let shell = shell.as_mut().expect("console_visible");
            let fonts = fonts.as_ref().expect("init ran");
            shell.render(
                slot.surface.canvas(),
                ctx.width,
                ctx.height,
                fonts,
                ctx.pad,
                ctx.pad_pref,
                ctx.pads,
            );
            {
                // Queue external sync vs FFmpeg's pump-thread submits (same queue).
                let _q = gpu.queue_lock.guard();
                gpu.context.flush_surface_with_texture_state(
                    &mut slot.surface,
                    &gpu::FlushInfo::default(),
                    Some(&skvk::mutable_texture_states::new_vulkan(
                        skvk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        gpu.queue_family_index,
                    )),
                );
                gpu.context.submit(None);
            }
            self.current = next;
            self.drawn = Drawn::default(); // stream chrome re-renders when it returns
            let slot = self.slots[next].as_ref().expect("just rendered");
            return Ok(Some(OverlayFrame {
                image: slot.image,
                view: slot.view,
                width: slot.width,
                height: slot.height,
            }));
        }

        // --- Stream chrome: stats OSD + capture hint + start banner + resize scrim -----
        let banner_alpha = self.banner_alpha(ctx);
        let banner_step = (banner_alpha * 32.0).round() as u8;
        let resize_phase = self.resize_phase(ctx);
        // 120 steps/s: every ~16 ms frame lands on a fresh step, so the spinner keeps
        // spinning through the damage gate; `+ 1` keeps an active resize's step nonzero
        // even on its first frame (phase 0) so the guard below doesn't skip it.
        let resize_step = resize_phase.map_or(0, |p| (p * 120.0) as u16 + 1);
        if ctx.stats.is_none()
            && ctx.hint.is_none()
            && !ctx.mic_muted
            && banner_step == 0
            && resize_step == 0
        {
            self.drawn = Drawn::default(); // forget content so re-show re-renders
            return Ok(None);
        }
        // 1 % granularity: fine enough that no real display scale is rounded into another, coarse
        // enough that float noise on the same monitor can't churn the damage gate every frame.
        let scale = ctx.scale.clamp(0.5, 4.0);
        let want = Drawn {
            width: ctx.width,
            height: ctx.height,
            stats: ctx.stats.map(str::to_owned),
            hint: ctx.hint.map(str::to_owned),
            mic_muted: ctx.mic_muted,
            scale_pct: (scale * 100.0).round() as u16,
            banner_step,
            resize_step,
        };
        if want == self.drawn {
            // Unchanged — hand the presenter the already-rendered image.
            return Ok(self.slots[self.current].as_ref().map(|s| OverlayFrame {
                image: s.image,
                view: s.view,
                width: s.width,
                height: s.height,
            }));
        }

        // Render into the OTHER slot — the presenter may still be sampling the current
        // one (one frame in flight; the ring of two is exactly deep enough).
        let next = 1 - self.current;
        self.ensure_slot(next, ctx.width, ctx.height)?;
        let gpu = self.gpu.as_mut().expect("init ran");
        let slot = self.slots[next].as_mut().expect("just ensured");

        let canvas = slot.surface.canvas();
        canvas.clear(Color4f::new(0.0, 0.0, 0.0, 0.0));
        // Each drawer re-derives the face at its own (fit-clamped) size rather than the canvas
        // being transformed: Skia hints and rasterizes glyphs at the requested size, so this
        // stays crisp where a magnified 14 px bitmap would be mush. Only on a damage redraw —
        // a steady stream re-renders nothing at all.
        let font = self.font.as_ref().expect("init ran");
        // The resize scrim sits UNDER the OSD/hint so those stay legible over it.
        if let Some(phase) = resize_phase {
            draw_resize_scrim(canvas, font, ctx.width, ctx.height, phase, scale);
        }
        if let Some(stats) = &want.stats {
            draw_osd_panel(canvas, font, stats, ctx.width, scale);
        }
        // Top-RIGHT, so it never collides with the stats panel or the bottom pill: the badge
        // has to stay readable at every stats tier, including Off.
        if want.mic_muted {
            draw_mic_muted_badge(canvas, font, ctx.width, scale);
        }
        if let Some(hint) = &want.hint {
            draw_hint_pill(canvas, font, hint, ctx.width, ctx.height, 1.0, scale);
        } else if banner_step > 0 {
            // The start banner: the leave/stats shortcuts, fading out on its own —
            // discoverable without the stats overlay, gone before it annoys.
            if let Some(text) = &self.banner_text {
                draw_hint_pill(
                    canvas,
                    font,
                    text,
                    ctx.width,
                    ctx.height,
                    banner_alpha as f32,
                    scale,
                );
            }
        }

        // Flush on the shared queue, ending in SHADER_READ_ONLY on our family — the
        // layout the presenter's composite samples (its own barrier covers visibility).
        // Lock: queue external sync vs FFmpeg's pump-thread submits (same queue).
        {
            let _q = gpu.queue_lock.guard();
            gpu.context.flush_surface_with_texture_state(
                &mut slot.surface,
                &gpu::FlushInfo::default(),
                Some(&skvk::mutable_texture_states::new_vulkan(
                    skvk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    gpu.queue_family_index,
                )),
            );
            gpu.context.submit(None);
        }

        self.current = next;
        self.drawn = want;
        let slot = self.slots[next].as_ref().expect("just rendered");
        Ok(Some(OverlayFrame {
            image: slot.image,
            view: slot.view,
            width: slot.width,
            height: slot.height,
        }))
    }
}

impl SkiaOverlay {
    /// The start banner's current alpha (1 → 0 across the fade tail), refreshing its
    /// words while fully visible so a pad hot-plug updates the leave hint.
    fn banner_alpha(&mut self, ctx: &FrameCtx) -> f64 {
        let Some(since) = self.streaming_since else {
            self.banner_text = None;
            return 0.0;
        };
        let age = since.elapsed().as_secs_f64();
        if age >= BANNER_S {
            self.streaming_since = None;
            self.banner_text = None;
            return 0.0;
        }
        self.banner_text = Some(if ctx.pad.is_some() {
            "Hold L1 + R1 + Start + Select to leave · Ctrl+Alt+Shift+S stats".to_string()
        } else {
            "Ctrl+Alt+Shift+Q releases input · Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats"
                .to_string()
        });
        ((BANNER_S - age) / BANNER_FADE_S).min(1.0)
    }

    /// The mid-stream-resize spinner's phase (elapsed seconds since the scrim came up), or
    /// `None` when no resize is in flight. Latches the start on the first `resizing` frame
    /// and clears it the moment the run loop drops the flag (the target frame landed or the
    /// switch timed out), so the next resize starts its spinner from zero.
    fn resize_phase(&mut self, ctx: &FrameCtx) -> Option<f64> {
        if !ctx.resizing {
            self.resizing_since = None;
            return None;
        }
        Some(
            self.resizing_since
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs_f64(),
        )
    }

    /// Make `slots[i]` a render target of exactly `width`×`height` (rebuilt on resize).
    fn ensure_slot(&mut self, i: usize, width: u32, height: u32) -> Result<()> {
        if self.slots[i]
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height)
        {
            return Ok(());
        }
        let gpu = self.gpu.as_mut().expect("init ran");
        if let Some(old) = self.slots[i].take() {
            // Any in-flight sampling of THIS slot ended two presents ago (the ring
            // alternates and the presenter waits its fence before each record).
            // SAFETY: the view belongs to the slot being replaced, and per the comment above any
            // in-flight sampling of THIS slot ended two presents ago — the ring alternates and the
            // presenter waits its fence before each record — so the GPU is done with it.
            unsafe { gpu.device.destroy_image_view(old.view, None) };
        }
        let info =
            skia_safe::ImageInfo::new_n32_premul((width.max(1) as i32, height.max(1) as i32), None);
        let mut surface = gpu::surfaces::render_target(
            &mut gpu.context,
            gpu::Budgeted::Yes,
            &info,
            None,
            SurfaceOrigin::TopLeft,
            None,
            false,
            None,
        )
        .context("Skia render-target surface")?;
        let texture = gpu::surfaces::get_backend_texture(
            &mut surface,
            skia_safe::surface::BackendHandleAccess::FlushRead,
        )
        .context("surface backend texture")?;
        let image_info = texture
            .vulkan_image_info()
            .context("backend texture is not Vulkan")?;
        let image = avk::Image::from_raw(*image_info.image() as u64);
        // SAFETY: a create call on the live device `gpu` owns, over a builder that is a local
        // outliving the call; `image` is the VkImage Skia just reported for this backend texture,
        // which the surface keeps alive. The returned view is owned by the slot stored below.
        let view = unsafe {
            gpu.device.create_image_view(
                &avk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(avk::ImageViewType::TYPE_2D)
                    .format(avk::Format::from_raw(image_info.format as i32))
                    .subresource_range(
                        avk::ImageSubresourceRange::default()
                            .aspect_mask(avk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .context("overlay image view")?;
        self.slots[i] = Some(Slot {
            surface,
            image,
            view,
            width,
            height,
        });
        Ok(())
    }
}

/// The chrome face at `scale`. `with_size` only fails on a nonsensical size (the caller clamps),
/// in which case the unscaled face is still better than no text.
fn chrome_font(font: &Font, scale: f32) -> Font {
    font.with_size(base::FONT_PX * scale)
        .unwrap_or_else(|| font.clone())
}

/// Shrink `scale` until a box of `width_at_scale` (which must be linear in the scale — every
/// chrome metric is) fits in `budget`. Scaling text up by the display's DPI is only an
/// improvement while the result still fits the window: the capture hint is a ~150-character line
/// that already spans most of a 1280 px window at 100 %, so at 200 % it would run off both edges
/// and lose its ends. Fitting keeps it whole, just smaller than the nominal scale.
fn fit_scale(scale: f32, width_at_scale: f32, budget: f32) -> f32 {
    if width_at_scale > budget && width_at_scale > 0.0 {
        (scale * budget / width_at_scale).max(0.1)
    } else {
        scale
    }
}

/// The stats OSD: a translucent rounded panel in the top-left, one text line per `\n` (the GTK
/// OSD's look, minus the toolkit), sized for the display's UI `scale`.
fn draw_osd_panel(canvas: &Canvas, base_font: &Font, text: &str, width: u32, scale: f32) {
    let lines: Vec<&str> = text.lines().collect();
    // Panel width is linear in the scale, so measuring once at the requested scale is enough to
    // solve for the scale that keeps the Detailed tier's long lines inside the window instead of
    // running them past the right edge on a HiDPI display.
    let width_at = |s: f32| {
        let font = chrome_font(base_font, s);
        let widest = lines
            .iter()
            .map(|l| font.measure_str(l, None).0)
            .fold(0.0f32, f32::max);
        widest + 2.0 * (base::OSD_PAD_X + base::OSD_MARGIN) * s
    };
    let scale = fit_scale(scale, width_at(scale), width as f32);
    let font = chrome_font(base_font, scale);

    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent + metrics.leading;
    let widest = lines
        .iter()
        .map(|l| font.measure_str(l, None).0)
        .fold(0.0f32, f32::max);
    let (pad_x, pad_y) = (base::OSD_PAD_X * scale, base::OSD_PAD_Y * scale);
    let (x, y) = (base::OSD_MARGIN * scale, base::OSD_MARGIN * scale);
    let panel = Rect::from_xywh(
        x,
        y,
        widest + 2.0 * pad_x,
        line_h * lines.len() as f32 + 2.0 * pad_y,
    );
    let radius = base::OSD_RADIUS * scale;
    canvas.draw_rrect(
        RRect::new_rect_xy(panel, radius, radius),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.62), None),
    );
    let text_paint = Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.92), None);
    for (i, line) in lines.iter().enumerate() {
        canvas.draw_str(
            line,
            Point::new(x + pad_x, y + pad_y - metrics.ascent + line_h * i as f32),
            &font,
            &text_paint,
        );
    }
}

/// The mic-mute badge: a dot in the error colour plus the words, on the same translucent pill
/// as the rest of the chrome, pinned to the TOP-RIGHT corner.
///
/// It is drawn from `FrameCtx::mic_muted` alone — not from the stats text — because it must
/// survive the stats overlay being Off, which is where most people leave it. Words, not a
/// glyph: the chrome font is a system monospace resolved at runtime and cannot be relied on to
/// carry a crossed-out microphone. Persistent by design (no fade): a fading indicator answers
/// "did the chord register?" but not "am I muted right now?", and the second question is the
/// one that matters ten minutes later.
fn draw_mic_muted_badge(canvas: &Canvas, base_font: &Font, width: u32, scale: f32) {
    const LABEL: &str = "Microphone muted";
    // Short line — it fits any window the stream runs in, so it takes the display scale as-is.
    let font = &chrome_font(base_font, scale);
    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let dot_r = 4.0 * scale;
    let dot_gap = 8.0 * scale;
    let text_w = font.measure_str(LABEL, None).0;
    let w = text_w + 2.0 * dot_r + dot_gap + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let margin = base::OSD_MARGIN * scale;
    let (x, y) = (width as f32 - w - margin, margin);
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.62), None),
    );
    canvas.draw_circle(
        Point::new(x + pad_x + dot_r, y + h / 2.0),
        dot_r,
        &Paint::new(crate::theme::ERROR, None),
    );
    canvas.draw_str(
        LABEL,
        Point::new(
            x + pad_x + 2.0 * dot_r + dot_gap,
            y + pad_y - metrics.ascent,
        ),
        font,
        &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.92), None),
    );
}

/// The mid-stream-resize cover: a full-screen dark scrim, the shared rotating spinner, and
/// a "Resizing…" label centered over it — so the host's 0.3–2 s virtual-display + encoder
/// rebuild reads as a deliberate pause rather than the stream stretching to the changed
/// window. This is the presenter's analog of the Apple client's blur overlay: the overlay
/// composites its own RGBA quad and cannot sample the video to blur it, so an opaque scrim
/// hides the stretched in-between frame instead (same intent, one draw).
fn draw_resize_scrim(
    canvas: &Canvas,
    base_font: &Font,
    width: u32,
    height: u32,
    phase: f64,
    scale: f32,
) {
    // Short, centered label — it always fits, so it just takes the display scale as-is.
    let font = &chrome_font(base_font, scale);
    let (wf, hf) = (width as f32, height as f32);
    canvas.draw_rect(
        Rect::from_wh(wf, hf),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.55), None),
    );
    // Spinner slightly above center; the label sits below it.
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let r = (f64::from(width.min(height)) * 0.045).clamp(16.0, 44.0);
    crate::theme::spinner(canvas, cx, cy - r, r, phase);
    let (_, metrics) = font.metrics();
    let label = "Resizing\u{2026}";
    let tw = font.measure_str(label, None).0;
    canvas.draw_str(
        label,
        Point::new((wf - tw) / 2.0, (cy + r * 0.9) as f32 - metrics.ascent),
        font,
        &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.9), None),
    );
}

/// The capture hint / start banner: a centered pill near the bottom edge. `scale` = the display's
/// UI scale (the text size already rides in `font`).
fn draw_hint_pill(
    canvas: &Canvas,
    base_font: &Font,
    text: &str,
    width: u32,
    height: u32,
    alpha: f32,
    scale: f32,
) {
    // The capture hint is one long line that already fills most of a 1280 px window at 100 %;
    // scaled by a 2× display it would overrun both edges, so fit it to the window (a 4 % gutter
    // keeps it off the very edge).
    let pill_w =
        |s: f32| chrome_font(base_font, s).measure_str(text, None).0 + 2.0 * base::PILL_PAD_X * s;
    let scale = fit_scale(scale, pill_w(scale), width as f32 * 0.96);
    let font = &chrome_font(base_font, scale);

    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let text_w = font.measure_str(text, None).0;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let w = text_w + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let x = (width as f32 - w) / 2.0;
    let y = height as f32 - h - base::PILL_BOTTOM * scale;
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.62 * alpha), None),
    );
    canvas.draw_str(
        text,
        Point::new(x + pad_x, y + pad_y - metrics.ascent),
        font,
        &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.92 * alpha), None),
    );
}
