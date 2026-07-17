//! Swapchain recreate / resize / HDR reconfiguration.

use super::setup::pick_formats;
use super::{OverlayPipe, Presenter};
use crate::csc::CscPass;
use anyhow::{Context as _, Result};
use ash::vk;

impl Presenter {
    /// (Re)build the swapchain for the window's current pixel size. Also the resize path.
    pub fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        self.quiesce_own()?;
        // Drain the queue before touching presentation objects: after this, every prior
        // present's semaphore-wait operation has completed, so the OLD swapchain and its
        // render semaphores are safe to destroy immediately below. (The previous scheme
        // parked them and destroyed after one fence cycle — but the fence proves only
        // OUR submit, not the presentation engine's semaphore consumption:
        // VUID-vkDestroySemaphore-05149 / VUID-vkDestroySwapchainKHR-01282 on every
        // recreate, and destroy-in-use is exactly the kind of misuse that turns into an
        // intermittent VK_ERROR_DEVICE_LOST.) Safe against the pump's FFmpeg submits —
        // both sides hold the shared queue lock — and cheap: a recreate already stalls
        // the stream for a frame, and only happens on resize/HDR-flip/OUT_OF_DATE.
        {
            let _q = self.queue_lock.guard();
            unsafe { self.device.queue_wait_idle(self.queue) }
                .context("vkQueueWaitIdle (swapchain recreate)")?;
        }

        let caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        let (pw, ph) = window.size_in_pixels();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: pw.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: ph.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            // Minimized — keep the old swapchain; presents will report OUT_OF_DATE and
            // land back here once the window has a size again.
            return Ok(());
        }
        let mut min_images = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            min_images = min_images.min(caps.max_image_count);
        }

        let old = self.swapchain;
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(min_images)
            .image_format(self.format.format)
            .image_color_space(self.format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // TRANSFER_DST is the whole phase-1 pipeline (clear + blit); COLOR_ATTACHMENT
            // keeps the phase-2 render pass from forcing a swapchain rebuild contract change.
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            .clipped(true)
            .old_swapchain(old);
        let swapchain =
            unsafe { self.swap_d.create_swapchain(&info, None) }.context("vkCreateSwapchainKHR")?;
        // The old swapchain and everything tied to its images dies NOW: the fence
        // quiesce covered our own command buffers, the queue drain above covered the
        // presentation engine's semaphore waits — nothing can still reference them.
        let (overlay_views, overlay_framebuffers) = self.overlay_pipe.take_targets();
        unsafe {
            for fb in overlay_framebuffers {
                self.device.destroy_framebuffer(fb, None);
            }
            for v in overlay_views {
                self.device.destroy_image_view(v, None);
            }
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            if old != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(old, None);
            }
        }
        self.swapchain = swapchain;
        self.images = unsafe { self.swap_d.get_swapchain_images(swapchain) }?;
        self.extent = extent;
        self.overlay_pipe.rebuild_targets(
            &self.device,
            &self.images,
            self.format.format,
            extent,
        )?;

        for _ in 0..self.images.len() {
            self.render_sems.push(unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }
        tracing::debug!(
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            "swapchain (re)created"
        );
        // HDR metadata is per-swapchain state: a rebuilt HDR10 swapchain needs it pushed
        // again (this also covers set_hdr_mode's entry into HDR10, which lands here).
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
        Ok(())
    }

    /// Whether the swapchain is actually in HDR10/PQ mode — as opposed to a PQ stream
    /// being tone-mapped onto an SDR surface. This, not the stream's own signaling, is
    /// what user-facing "HDR" indicators should report.
    pub fn hdr_active(&self) -> bool {
        self.hdr_active
    }

    /// Record the host's ST.2086 mastering + content-light metadata (the 0xCE plane),
    /// pushing it to the swapchain immediately when HDR10 mode is live. Cheap and
    /// idempotent per distinct value — callers just drain the plane into it.
    pub fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        if self.hdr_meta == Some(meta) {
            return;
        }
        self.hdr_meta = Some(meta);
        if self.hdr_active {
            self.apply_hdr_metadata();
        }
    }

    /// Push the current metadata (the host's, or a generic HDR10 baseline until 0xCE
    /// arrives) to the presentation engine via `vkSetHdrMetadataEXT`. Compositors gate
    /// their HDR-app signaling on this — picking the HDR10 colorspace alone leaves
    /// gamescope treating the app as SDR (no SteamOS HDR badge, no per-app tone-map
    /// target). No-op where the driver lacks the extension.
    fn apply_hdr_metadata(&self) {
        let Some(ext) = &self.hdr_metadata_d else {
            return;
        };
        // Same generic baseline as the Windows presenter: BT.2020 primaries + D65
        // white, 1000-nit mastering display, MaxCLL 1000 / MaxFALL 400.
        let m = self.hdr_meta.unwrap_or(punktfunk_core::quic::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 1,
            max_cll: 1000,
            max_fall: 400,
        });
        // Protocol fields are HDR10 SEI fixed-point (chromaticity 1/50000, luminance
        // 0.0001 cd/m², primaries in ST.2086 G,B,R order); Vulkan wants floats in
        // 0..1 chromaticity and whole nits, primaries named R/G/B.
        let xy = |p: [u16; 2]| vk::XYColorEXT {
            x: p[0] as f32 / 50_000.0,
            y: p[1] as f32 / 50_000.0,
        };
        let [g, b, r] = m.display_primaries;
        let md = vk::HdrMetadataEXT::default()
            .display_primary_red(xy(r))
            .display_primary_green(xy(g))
            .display_primary_blue(xy(b))
            .white_point(xy(m.white_point))
            .max_luminance(m.max_display_mastering_luminance as f32 / 10_000.0)
            .min_luminance(m.min_display_mastering_luminance as f32 / 10_000.0)
            .max_content_light_level(m.max_cll as f32)
            .max_frame_average_light_level(m.max_fall as f32);
        unsafe { ext.set_hdr_metadata(&[self.swapchain], &[md]) };
        tracing::debug!(from_host = self.hdr_meta.is_some(), "HDR metadata pushed");
    }
    /// Flip the presenter between SDR and HDR10 output (stream SDR↔PQ, in-band). A
    /// fence quiesce, then everything format-bound is rebuilt: the CSC pass + video
    /// image (10-bit intermediate — PQ in 8 bits bands visibly), the overlay pipe, and
    /// the swapchain (old one parked per the deferred-destroy rules).
    pub(super) fn set_hdr_mode(&mut self, window: &sdl3::video::Window, on: bool) -> Result<()> {
        let target = if on {
            self.hdr10_format.expect("caller checked availability")
        } else {
            // Recompute the SDR pick? It never changed — the sdr format is immutable.
            // (self.format currently holds the HDR pairing.)
            pick_formats(&self.surface_i, self.pdev, self.surface, false)?.0
        };
        tracing::info!(hdr = on, format = ?target, "switching presentation mode");
        self.quiesce_own()?;
        self.video_format = if on {
            vk::Format::A2B10G10R10_UNORM_PACK32
        } else {
            vk::Format::R8G8B8A8_UNORM
        };
        self.csc.destroy(&self.device); // fence-safe: only our cmd bufs reference it
        #[cfg(all(target_os = "linux", feature = "pyrowave"))]
        if let Some(p) = &self.csc_planar {
            p.destroy(&self.device);
        }
        self.csc = CscPass::new(&self.device, self.video_format)?;
        if let Some(v) = self.video.take() {
            unsafe {
                self.device.destroy_framebuffer(v.framebuffer, None);
                self.device.destroy_image_view(v.view, None);
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
        }
        // New overlay pipe against the new swapchain format. The old one's targets
        // (views/framebuffers over the current swapchain's images) are only ever
        // referenced by our own command buffers — the fence quiesce above makes them
        // safe to destroy right here; the swapchain itself rides the recreate below.
        let mut old_pipe = std::mem::replace(
            &mut self.overlay_pipe,
            OverlayPipe::new(&self.device, target.format)?,
        );
        let (overlay_views, overlay_framebuffers) = old_pipe.take_targets();
        unsafe {
            for fb in overlay_framebuffers {
                self.device.destroy_framebuffer(fb, None);
            }
            for v in overlay_views {
                self.device.destroy_image_view(v, None);
            }
        }
        old_pipe.destroy(&self.device);
        self.format = target;
        self.hdr_active = on;
        self.recreate_swapchain(window)
    }
}
