//! Skia on the presenter's device: `DirectContext` over the shared handles, a ring of
//! two offscreen render-target surfaces (the presenter runs one frame in flight and may
//! still be sampling the previous image while we render the next), damage-driven
//! redraws (content/size change only — an unchanged OSD costs zero GPU work per frame).

use anyhow::{anyhow, Context as _, Result};
use ash::vk as avk;
use ash::vk::Handle as _;
use pf_presenter::overlay::{FrameCtx, Overlay, OverlayFrame, SharedDevice};
use skia_safe::gpu::vk as skvk;
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Font, FontMgr, Paint, Point, RRect, Rect, Surface};

/// Skia's GPU resource budget — the OSD/HUD need a few MB; the console library will
/// revisit (the plan budgets 64 MB on Deck-class shared memory).
const RESOURCE_CACHE_BYTES: usize = 64 << 20;

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
}

pub struct SkiaOverlay {
    /// Set by `init`; `None` until then (and after an init failure the run loop drops
    /// the whole overlay, so mid-session these are always `Some`).
    gpu: Option<Gpu>,
    slots: [Option<Slot>; 2],
    /// Which slot the LAST returned frame lives in — the next render takes the other.
    current: usize,
    drawn: Drawn,
    font: Option<Font>,
}

struct Gpu {
    device: ash::Device,
    queue_family_index: u32,
    context: DirectContext,
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
        }
    }
}

impl Drop for SkiaOverlay {
    /// The run loop quiesces the queue before dropping us; releasing the views + Skia
    /// surfaces (which free their VkImages) is then safe. Field order drops the slots
    /// before the DirectContext.
    fn drop(&mut self) {
        if let Some(gpu) = &mut self.gpu {
            for slot in self.slots.iter_mut().flat_map(Option::take) {
                unsafe { gpu.device.destroy_image_view(slot.view, None) };
                drop(slot.surface);
            }
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
            unsafe {
                match of {
                    skvk::GetProcOf::Instance(raw_instance, name) => entry
                        .get_instance_proc_addr(avk::Instance::from_raw(raw_instance as _), name)
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void),
                    skvk::GetProcOf::Device(raw_device, name) => (instance.fp_v1_0()
                        .get_device_proc_addr)(
                        avk::Device::from_raw(raw_device as _), name
                    )
                    .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void),
                }
            }
        };
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

        let typeface = FontMgr::new()
            .match_family_style("monospace", skia_safe::FontStyle::normal())
            .context("no monospace typeface via fontconfig")?;
        self.font = Some(Font::new(typeface, 14.0));

        self.gpu = Some(Gpu {
            device: shared.device.clone(),
            queue_family_index: shared.queue_family_index,
            context,
            _entry: shared.entry.clone(),
            _instance: shared.instance.clone(),
        });
        tracing::info!("Skia console UI on the presenter's device");
        Ok(())
    }

    fn handle_event(&mut self, _event: &sdl3::event::Event) -> bool {
        false // the OSD/HUD consume nothing; the console library will
    }

    fn frame(&mut self, ctx: &FrameCtx) -> Result<Option<OverlayFrame>> {
        if ctx.stats.is_none() && ctx.hint.is_none() {
            self.drawn = Drawn::default(); // forget content so re-show re-renders
            return Ok(None);
        }
        let want = Drawn {
            width: ctx.width,
            height: ctx.height,
            stats: ctx.stats.map(str::to_owned),
            hint: ctx.hint.map(str::to_owned),
        };
        if want == self.drawn {
            // Unchanged — hand the presenter the already-rendered image.
            return Ok(self
                .slots[self.current]
                .as_ref()
                .map(|s| OverlayFrame {
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
        let font = self.font.as_ref().expect("init ran");
        if let Some(stats) = &want.stats {
            draw_osd_panel(canvas, font, stats, 12.0, 12.0);
        }
        if let Some(hint) = &want.hint {
            draw_hint_pill(canvas, font, hint, ctx.width, ctx.height);
        }

        // Flush on the shared queue, ending in SHADER_READ_ONLY on our family — the
        // layout the presenter's composite samples (its own barrier covers visibility).
        gpu.context.flush_surface_with_texture_state(
            &mut slot.surface,
            &gpu::FlushInfo::default(),
            Some(&skvk::mutable_texture_states::new_vulkan(
                skvk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                gpu.queue_family_index,
            )),
        );
        gpu.context.submit(None);

        self.current = next;
        self.drawn = want;
        Ok(Some(OverlayFrame {
            image: slot.image,
            view: slot.view,
            width: slot.width,
            height: slot.height,
        }))
    }
}

impl SkiaOverlay {
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
            unsafe { gpu.device.destroy_image_view(old.view, None) };
        }
        let info = skia_safe::ImageInfo::new_n32_premul(
            (width.max(1) as i32, height.max(1) as i32),
            None,
        );
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
        let texture =
            gpu::surfaces::get_backend_texture(
                &mut surface,
                skia_safe::surface::BackendHandleAccess::FlushRead,
            )
                .context("surface backend texture")?;
        let image_info = texture
            .vulkan_image_info()
            .context("backend texture is not Vulkan")?;
        let image = avk::Image::from_raw(*image_info.image() as u64);
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

/// The stats OSD: a translucent rounded panel, one text line per `\n` (the GTK OSD's
/// look, minus the toolkit).
fn draw_osd_panel(canvas: &Canvas, font: &Font, text: &str, x: f32, y: f32) {
    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent + metrics.leading;
    let lines: Vec<&str> = text.lines().collect();
    let widest = lines
        .iter()
        .map(|l| font.measure_str(l, None).0)
        .fold(0.0f32, f32::max);
    let (pad_x, pad_y) = (10.0, 8.0);
    let panel = Rect::from_xywh(
        x,
        y,
        widest + 2.0 * pad_x,
        line_h * lines.len() as f32 + 2.0 * pad_y,
    );
    canvas.draw_rrect(
        RRect::new_rect_xy(panel, 8.0, 8.0),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.62), None),
    );
    let text_paint = Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.92), None);
    for (i, line) in lines.iter().enumerate() {
        canvas.draw_str(
            line,
            Point::new(
                x + pad_x,
                y + pad_y - metrics.ascent + line_h * i as f32,
            ),
            font,
            &text_paint,
        );
    }
}

/// The capture hint: a centered pill near the bottom edge (the GTK hint's position).
fn draw_hint_pill(canvas: &Canvas, font: &Font, text: &str, width: u32, height: u32) {
    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let text_w = font.measure_str(text, None).0;
    let (pad_x, pad_y) = (14.0, 8.0);
    let w = text_w + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let x = (width as f32 - w) / 2.0;
    let y = height as f32 - h - 24.0;
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.62), None),
    );
    canvas.draw_str(
        text,
        Point::new(x + pad_x, y + pad_y - metrics.ascent),
        font,
        &Paint::new(Color4f::new(1.0, 1.0, 1.0, 0.92), None),
    );
}
