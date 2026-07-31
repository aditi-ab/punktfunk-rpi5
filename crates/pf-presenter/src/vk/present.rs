//! The per-frame present path (route input → video image → CSC → blit → present). HOT PATH.

use super::gpu::*;
use super::{FrameInput, Presenter, Retired};
use crate::csc::csc_rows;
#[cfg(target_os = "linux")]
use crate::dmabuf::{self, HwFrame};
use crate::overlay::OverlayFrame;
use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pf_client_core::video::VkVideoFrame;

impl Presenter {
    /// Present one frame: route `input` into the video image (staging upload or dmabuf
    /// import + CSC pass; `Redraw` re-blits what's retained), clear, letterbox-blit,
    /// blend the console-UI `overlay` quad if one arrived, present. Returns false when
    /// the swapchain was out of date — the caller recreates (with current window state)
    /// and may retry.
    pub fn present(
        &mut self,
        window: &sdl3::video::Window,
        input: FrameInput,
        overlay: Option<&OverlayFrame>,
    ) -> Result<bool> {
        if self.extent.width == 0 || self.extent.height == 0 {
            return Ok(true); // minimized — nothing to do
        }
        // SDR↔HDR follows the FRAMES' own signaling (the host flips PQ in-band):
        // switch modes before anything touches this frame. Only where the surface
        // offers HDR10 — otherwise PQ stays on the SDR swapchain and the CSC shader
        // tonemaps (mode 1).
        //
        // CPU frames NEVER take the HDR10 surface: software decode uploads swscale RGBA with
        // no CSC/tonemap pass, so on a mode-0 swapchain that sRGB-encoded content would be
        // composed as PQ — the field-reported psychedelic cyan/magenta picture (reproduced
        // 2026-07-21: Fedora-class client, no hw HEVC decode, GNOME/Mesa offering HDR10 even
        // on an SDR desktop). On the SDR swapchain the same frames are merely untonemapped
        // (washed out) — wrong in the known, benign way until the CPU lane grows a real
        // PQ→sRGB pass.
        let frame_pq = match &input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(_) => Some(false),
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => Some(d.color.is_pq()),
            FrameInput::VkFrame(v) => Some(v.color.is_pq()),
            #[cfg(windows)]
            FrameInput::D3d11(d) => Some(d.color.is_pq()),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => Some(f.color.is_pq()),
        };
        if let Some(pq) = frame_pq {
            // A PQ stream we can only tone-map (no HDR10 surface) is the silent failure behind
            // "HDR isn't advertised": the compositor never sees an HDR-committing app. Say so
            // once — its presence proves PQ IS arriving and the surface/compositor is the
            // blocker (on the Deck: gamescope's WSI layer not visible in the flatpak sandbox);
            // its absence, with a plain SDR stream, points back at the host not sending PQ.
            if pq && self.hdr10_format.is_none() && !self.hdr_downgrade_warned {
                self.hdr_downgrade_warned = true;
                tracing::warn!(
                    "PQ (HDR10) stream tone-mapped to SDR — the surface offers no HDR10 \
                     colorspace, so no HDR is committed to the compositor. Under gamescope this \
                     usually means the gamescope Vulkan WSI layer is not visible in the sandbox."
                );
            }
            let want = pq && self.hdr10_format.is_some();
            if want != self.hdr_active {
                self.set_hdr_mode(window, want)?;
            }
        }
        // Hardware frames prepare before anything touches the queue: an import/view the
        // driver rejects must fail out here, before this present consumed the acquire
        // semaphore.
        #[cfg(target_os = "linux")]
        let mut hw_frame: Option<HwFrame> = None;
        #[cfg(windows)]
        let mut win_frame: Option<crate::d3d11::HwFrame> = None;
        let mut vk_frame: Option<(VkVideoFrame, [vk::ImageView; 2])> = None;
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        let mut pyro_frame: Option<pf_client_core::video_pyrowave::PyroWavePlanarFrame> = None;
        let cpu_frame = match input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some(f),
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => {
                let hw = self
                    .hw
                    .as_ref()
                    .context("hardware frame without dmabuf support")?;
                hw_frame = Some(dmabuf::import(&self.device, &hw.ext_mem_fd, d)?);
                None
            }
            #[cfg(windows)]
            FrameInput::D3d11(d) => {
                let hw = self
                    .hw_win
                    .as_ref()
                    .context("D3D11 frame without win32 import support")?;
                win_frame = Some(crate::d3d11::import(&self.device, &hw.ext_mem_win32, &d)?);
                None
            }
            FrameInput::VkFrame(v) => {
                let views = self.vkframe_plane_views(&v)?;
                vk_frame = Some((v, views));
                None
            }
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => {
                pyro_frame = Some(f);
                None
            }
        };

        // One frame in flight: the fence covers the command buffer, the staging buffer
        // AND the previously submitted hw frame — waiting makes all three reusable.
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
            self.device.reset_fences(&[self.fence])?;
        }
        if let Some(old) = self.retired_hw.take() {
            old.destroy(&self.device);
        }

        if let Some(f) = cpu_frame {
            self.stage_frame(f)?;
        }
        #[cfg(target_os = "linux")]
        if let Some(f) = &hw_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            // Safe while nothing in flight references the set — the fence wait above.
            self.csc
                .bind_planes(&self.device, f.luma_view, f.chroma_view);
        }
        #[cfg(windows)]
        if let Some(f) = &win_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
        }
        if let Some((f, views)) = &vk_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            self.csc.bind_planes(&self.device, views[0], views[1]);
        }
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        if let Some(f) = &pyro_frame {
            if self
                .video
                .as_ref()
                .is_none_or(|v| v.width != f.width || v.height != f.height)
            {
                self.rebuild_video_image(f.width, f.height)?;
                tracing::info!(width = f.width, height = f.height, "video image (re)built");
            }
            let planar = self
                .csc_planar
                .as_ref()
                .context("PyroWave frame but the device failed the pyrowave probe")?;
            planar.bind_planes_planar(&self.device, f.views.map(vk::ImageView::from_raw));
        }
        if let Some(o) = overlay {
            // Point the composite at this overlay image (same fence-wait safety).
            let infos = [vk::DescriptorImageInfo::default()
                .image_view(o.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let writes = [vk::WriteDescriptorSet::default()
                .dst_set(self.overlay_pipe.desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos)];
            // SAFETY: per the Vulkan contract above - recorded into a command buffer this code
            // owns and has begun, referencing handles it also owns; nothing is submitted until the
            // recording is ended.
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        }

        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let (index, _suboptimal) = match unsafe {
            self.swap_d.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.acquire_sem,
                vk::Fence::null(),
            )
        } {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // Never submitted — the import (if any) dies here, GPU never saw it.
                #[cfg(target_os = "linux")]
                if let Some(f) = hw_frame {
                    f.destroy(&self.device);
                }
                #[cfg(windows)]
                if let Some(f) = win_frame {
                    f.destroy(&self.device);
                }
                self.recreate_swapchain(window)?;
                return Ok(false);
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        };
        let swap_image = self.images[index as usize];

        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        unsafe {
            self.device.begin_command_buffer(
                self.cmd_buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            // Dmabuf frame: acquire the foreign planes, then the CSC pass renders
            // NV12→RGBA into the video image (render pass ends it in TRANSFER_SRC for
            // the blit below).
            #[cfg(target_os = "linux")]
            if let (Some(f), Some(v)) = (&hw_frame, &self.video) {
                for view_image in [f.luma_image(), f.chroma_image()] {
                    foreign_acquire_barrier(&self.device, self.cmd_buf, view_image, self.qfi);
                }
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                let ten_bit = f.is_p010();
                self.record_csc(
                    v.framebuffer,
                    extent,
                    f.color,
                    if ten_bit { 10 } else { 8 },
                    ten_bit,
                );
            }

            // D3D11 frame: acquire the imported RGB texture from the external "queue
            // family" (the keyed mutex on the submit is the actual cross-API sync) and
            // blit it into the video image — the frame arrives as ready RGB from the
            // decoder's VideoProcessor (sRGB BGRA8, or PQ RGB10A2 on the HDR ring —
            // matching the HDR-mode video image), so there is no CSC pass; the blit
            // converts component order. Same layout dance as the CPU staging path.
            #[cfg(windows)]
            if let (Some(f), Some(v)) = (&win_frame, &self.video) {
                external_acquire_barrier(&self.device, self.cmd_buf, f.image(), self.qfi);
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                );
                let extent = vk::Offset3D {
                    x: v.width as i32,
                    y: v.height as i32,
                    z: 1,
                };
                let blit = vk::ImageBlit::default()
                    .src_subresource(subresource_layers())
                    .src_offsets([vk::Offset3D::default(), extent])
                    .dst_subresource(subresource_layers())
                    .dst_offsets([vk::Offset3D::default(), extent]);
                self.device.cmd_blit_image(
                    self.cmd_buf,
                    f.image(),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::NEAREST, // 1:1 — the composite blit below does the scaling
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                );
            }

            // Vulkan-Video frame: the decoded image is already on THIS device. Read the
            // live sync state under the frames lock (held through submission — the
            // AVVulkanFramesContext contract), acquire from the decode queue family,
            // then the same CSC pass.
            let mut vk_sync: Option<VkFrameSync> = None;
            if let (Some((f, _)), Some(v)) = (&vk_frame, &self.video) {
                let sync = lock_vkframe(f);
                vkframe_acquire_barrier(
                    &self.device,
                    self.cmd_buf,
                    vk::Image::from_raw(sync.image),
                    vk::ImageLayout::from_raw(sync.layout),
                    sync.queue_family,
                    self.qfi,
                );
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                let ten_bit =
                    f.vk_format == vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16.as_raw();
                self.record_csc(
                    v.framebuffer,
                    extent,
                    f.color,
                    if ten_bit { 10 } else { 8 },
                    ten_bit,
                );
                vk_sync = Some(sync);
            }

            // PyroWave frame: the planes are already on THIS device, decode
            // fence-complete and barriered to fragment sampling (GENERAL) by the
            // decoder — no acquire needed, just the planar CSC pass.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            if let (Some(f), Some(v)) = (&pyro_frame, &self.video) {
                let extent = vk::Extent2D {
                    width: v.width,
                    height: v.height,
                };
                self.record_csc_planar(v.framebuffer, extent, f.color);
            }

            // New frame: staging → video image (stride carried by buffer_row_length).
            if let (Some(f), Some(v), Some(s)) = (cpu_frame, &self.video, &self.staging) {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                );
                let region = vk::BufferImageCopy::default()
                    .buffer_row_length((f.stride / 4) as u32)
                    .image_subresource(subresource_layers())
                    .image_extent(vk::Extent3D {
                        width: v.width,
                        height: v.height,
                        depth: 1,
                    });
                self.device.cmd_copy_buffer_to_image(
                    self.cmd_buf,
                    s.buffer,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                );
            }

            // Swapchain image: discard old content, clear to black (the letterbox bars),
            // blit the video in, hand to present.
            barrier(
                &self.device,
                self.cmd_buf,
                swap_image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            self.device.cmd_clear_color_image(
                self.cmd_buf,
                swap_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
                &[subresource_range()],
            );
            if let Some(v) = &self.video {
                let (dst0, dst1) = letterbox(self.extent, v.width, v.height);
                let blit = vk::ImageBlit::default()
                    .src_subresource(subresource_layers())
                    .src_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: v.width as i32,
                            y: v.height as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(subresource_layers())
                    .dst_offsets([dst0, dst1]);
                self.device.cmd_blit_image(
                    self.cmd_buf,
                    v.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            }
            if let Some(o) = overlay {
                // Cross-submit visibility for the overlay image (Skia flushed it on this
                // queue): same-layout barrier = execution + memory dependency only.
                barrier(
                    &self.device,
                    self.cmd_buf,
                    o.image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
                // The composite pass blends the quad and ends the image PRESENT-ready.
                self.device.cmd_begin_render_pass(
                    self.cmd_buf,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(self.overlay_pipe.render_pass)
                        .framebuffer(self.overlay_pipe.framebuffers[index as usize])
                        .render_area(vk::Rect2D {
                            offset: vk::Offset2D { x: 0, y: 0 },
                            extent: self.extent,
                        }),
                    vk::SubpassContents::INLINE,
                );
                self.device.cmd_bind_pipeline(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline,
                );
                self.device.cmd_set_viewport(
                    self.cmd_buf,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: self.extent.width as f32,
                        height: self.extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                self.device.cmd_set_scissor(
                    self.cmd_buf,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: self.extent,
                    }],
                );
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.overlay_pipe.pipeline_layout,
                    0,
                    &[self.overlay_pipe.desc_set],
                    &[],
                );
                self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(self.cmd_buf);
            } else {
                barrier(
                    &self.device,
                    self.cmd_buf,
                    swap_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::PRESENT_SRC_KHR,
                );
            }
            self.device.end_command_buffer(self.cmd_buf)?;

            let render_sem = self.render_sems[index as usize];
            let cmd_bufs = [self.cmd_buf];
            let mut wait_sems = vec![self.acquire_sem];
            let mut wait_stages = vec![vk::PipelineStageFlags::TRANSFER];
            let mut signal_sems = vec![render_sem];
            // The Vulkan-Video frame's timeline semaphore: wait for the decoder's value,
            // signal value+1 when our reads are done (FFmpeg's per-submission contract).
            let mut wait_values = vec![0u64];
            let mut signal_values = vec![0u64];
            if let Some(sync) = &vk_sync {
                let sem = vk::Semaphore::from_raw(sync.semaphore);
                wait_sems.push(sem);
                wait_stages.push(vk::PipelineStageFlags::FRAGMENT_SHADER);
                wait_values.push(sync.sem_value);
                signal_sems.push(sem);
                signal_values.push(sync.sem_value + 1);
            }
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_values)
                .signal_semaphore_values(&signal_values);
            let mut submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&signal_sems);
            if vk_sync.is_some() {
                submit = submit.push_next(&mut timeline);
            }
            // D3D11 frame: bracket the submit in the shared texture's keyed mutex, key 0
            // both ways (the decode side copies under acquire(0)/release(0) too) — the
            // GPU-side acquire is what orders our sampling after the decoder's copy, and
            // our completion release is what unblocks the ring slot's reuse.
            #[cfg(windows)]
            let keyed_mem;
            #[cfg(windows)]
            let keyed_keys = [0u64];
            #[cfg(windows)]
            let keyed_timeouts = [2000u32];
            #[cfg(windows)]
            let mut keyed_info;
            #[cfg(windows)]
            if let Some(f) = &win_frame {
                // Bisect knob: PUNKTFUNK_D3D11_NO_MUTEX=1 skips the acquire/release pair
                // (torn frames possible — debugging only).
                if std::env::var_os("PUNKTFUNK_D3D11_NO_MUTEX").is_none() {
                    keyed_mem = [f.memory()];
                    keyed_info = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
                        .acquire_syncs(&keyed_mem)
                        .acquire_keys(&keyed_keys)
                        .acquire_timeouts(&keyed_timeouts)
                        .release_syncs(&keyed_mem)
                        .release_keys(&keyed_keys);
                    submit = submit.push_next(&mut keyed_info);
                }
            }
            let submitted = {
                // Queue external sync vs the pump's FFmpeg submits (see `queue_lock`).
                let _q = self.queue_lock.guard();
                self.device.queue_submit(self.queue, &[submit], self.fence)
            };
            // Write the new sync state back and release the frames lock REGARDLESS of
            // the submit outcome (an abandoned lock would wedge the decoder).
            if let Some(sync) = vk_sync.take() {
                let ok = submitted.is_ok();
                unlock_vkframe(
                    vk_frame
                        .as_ref()
                        .map(|(f, _)| f)
                        .expect("vk_sync implies vk_frame"),
                    &sync,
                    ok,
                    self.qfi,
                );
            }
            submitted?;
            self.submitted = true;
            // The hw frame is on the GPU now — park it until the fence proves the reads
            // done (destroyed at the next present's fence wait, or in Drop). At most one
            // of hw_frame/vk_frame is set (they route from the same `input`).
            self.retired_hw = vk_frame
                .take()
                .map(|(frame, views)| Retired::Vk { frame, views });
            #[cfg(target_os = "linux")]
            if let Some(f) = hw_frame.take() {
                self.retired_hw = Some(Retired::Dmabuf(f));
            }
            #[cfg(windows)]
            if let Some(f) = win_frame.take() {
                self.retired_hw = Some(Retired::D3d11(f));
            }

            let swapchains = [self.swapchain];
            let indices = [index];
            let present_sems = [render_sem];
            // On-glass timing (T0.2): attach a monotonically increasing present id the
            // PresentTimer's `vkWaitForPresentKHR` resolves to real visibility.
            let ids = [self.next_present_id + 1];
            let mut pid_info = vk::PresentIdKHR::default().present_ids(&ids);
            let mut present_info = vk::PresentInfoKHR::default()
                .wait_semaphores(&present_sems)
                .swapchains(&swapchains)
                .image_indices(&indices);
            if self.present_timer.is_some() {
                self.next_present_id += 1;
                present_info = present_info.push_next(&mut pid_info);
            }
            // Same queue external-sync rule as the submit above. Scoped tightly: the
            // OUT_OF_DATE arm re-enters the lock via recreate_swapchain's queue drain.
            let present_res = {
                let _q = self.queue_lock.guard();
                self.swap_d.queue_present(self.queue, &present_info)
            };
            match present_res {
                Ok(_) => {
                    // A failed present's id may never signal — claimable only on Ok.
                    if self.present_timer.is_some() {
                        self.last_presented = Some((self.swapchain, self.next_present_id));
                    }
                    Ok(true)
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate_swapchain(window)?;
                    Ok(false)
                }
                Err(e) => Err(e).context("vkQueuePresentKHR"),
            }
        }
    }

    /// Record the NV12→RGBA CSC pass into the video image (framebuffer): fullscreen
    /// triangle, CICP-driven push-constant rows. Shared by the dmabuf and Vulkan-Video
    /// paths — only the plane views bound beforehand differ.
    ///
    /// # Safety
    /// `self.cmd_buf` must be in the recording state; the CSC descriptor set must point
    /// at live plane views.
    unsafe fn record_csc(
        &self,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        color: pf_client_core::video::ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) {
        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd_buf,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.csc.render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                self.csc.pipeline,
            );
            self.device.cmd_set_viewport(
                self.cmd_buf,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                self.cmd_buf,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                self.csc.pipeline_layout,
                0,
                &[self.csc.desc_set],
                &[],
            );
            let rows = csc_rows(color, depth, msb_packed);
            // Mode 1 = PQ→SDR tonemap (a PQ stream without an HDR10 surface); mode 0
            // passes the transfer through (SDR as-is, or PQ onto the HDR10 swapchain).
            let mode = if color.is_pq() && !self.hdr_active {
                1.0f32
            } else {
                0.0
            };
            let peak = std::env::var("PUNKTFUNK_TONEMAP_PEAK")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(4.9); // ≈1000 nits over the 203-nit reference
            let mut pc = [0f32; 16];
            pc[..12].copy_from_slice(bytemuck_rows(&rows));
            pc[12] = mode;
            pc[13] = peak;
            let bytes = std::slice::from_raw_parts(pc.as_ptr().cast::<u8>(), 64);
            self.device.cmd_push_constants(
                self.cmd_buf,
                self.csc.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes,
            );
            self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.cmd_buf);
        }
    }

    /// [`record_csc`] over the planar (PyroWave) pass — always 8-bit, no MSB packing.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    unsafe fn record_csc_planar(
        &self,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        color: pf_client_core::video::ColorDesc,
    ) {
        // The planar pass exists whenever a PyroWave frame reached us (checked at bind).
        let Some(planar) = self.csc_planar.as_ref() else {
            return;
        };
        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        unsafe {
            self.device.cmd_begin_render_pass(
                self.cmd_buf,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(planar.render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                planar.pipeline,
            );
            self.device.cmd_set_viewport(
                self.cmd_buf,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                self.cmd_buf,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                self.cmd_buf,
                vk::PipelineBindPoint::GRAPHICS,
                planar.pipeline_layout,
                0,
                &[planar.desc_set],
                &[],
            );
            // An HDR (PQ) pyrowave session carries P010-style 10-bit studio codes MSB-packed
            // into 16-bit planes (design/pyrowave-444-hdr.md §2.2) — same sampling scale as
            // the P010 path; SDR sessions are plain 8-bit BT.709 limited. Depth follows the
            // colour contract (negotiation couples 10-bit ⟺ PQ for this codec).
            let (depth, msb_packed) = if color.is_pq() {
                (10, true)
            } else {
                (8, false)
            };
            let rows = csc_rows(color, depth, msb_packed);
            // Mode 1 = PQ→SDR tonemap (PQ stream without an HDR10 surface); mode 0 passes
            // the transfer through — identical to the NV12 arm above.
            let mode = if color.is_pq() && !self.hdr_active {
                1.0f32
            } else {
                0.0
            };
            let peak = std::env::var("PUNKTFUNK_TONEMAP_PEAK")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(4.9); // ≈1000 nits over the 203-nit reference
            let mut pc = [0f32; 16];
            pc[..12].copy_from_slice(bytemuck_rows(&rows));
            pc[12] = mode;
            pc[13] = peak;
            let bytes = std::slice::from_raw_parts(pc.as_ptr().cast::<u8>(), 64);
            self.device.cmd_push_constants(
                self.cmd_buf,
                planar.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                bytes,
            );
            self.device.cmd_draw(self.cmd_buf, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(self.cmd_buf);
        }
    }

    /// Per-plane views over a Vulkan-Video frame's multiplanar image — the CSC pass's
    /// exact sampling contract (the frames pool was created MUTABLE_FORMAT for this).
    /// See [`vkframe_plane_formats`] for the accepted pool formats.
    fn vkframe_plane_views(&self, f: &VkVideoFrame) -> Result<[vk::ImageView; 2]> {
        let Some((luma_fmt, chroma_fmt)) = vkframe_plane_formats(f.vk_format) else {
            bail!(
                "Vulkan-Video pool format {} unsupported (expected 2-plane 4:2:0 or 4:4:4, \
                 8/10-bit — 3-plane layouts need a third CSC binding)",
                f.vk_format
            );
        };
        // img[0] is creation-constant (only the sync fields need the frames lock).
        let image = vk::Image::from_raw(
            // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned
            // by this type and live for the call, and every builder struct is a local that
            // outlives it.
            unsafe { (*(f.vkframe as *const pf_ffvk::AVVkFrame)).img[0] } as u64,
        );
        let make = |aspect: vk::ImageAspectFlags, format: vk::Format| {
            // SAFETY: per the Vulkan contract above - a create/allocate call on the live device,
            // over builder structs that are locals outliving the call; the handle it returns is
            // owned by the value being built here.
            unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(aspect)
                                .level_count(1)
                                .layer_count(1),
                        ),
                    None,
                )
            }
            .context("vk-frame plane view")
        };
        let luma = make(vk::ImageAspectFlags::PLANE_0, luma_fmt)?;
        let chroma = match make(vk::ImageAspectFlags::PLANE_1, chroma_fmt) {
            Ok(v) => v,
            Err(e) => {
                // SAFETY: per the Vulkan contract above - this destroys objects this type owns,
                // and the GPU is known idle for them (the fence/queue-wait on the path here, or
                // the swapchain being retired), which is the obligation that makes a destroy sound
                // rather than the handle merely being non-null.
                unsafe { self.device.destroy_image_view(luma, None) };
                return Err(e);
            }
        };
        Ok([luma, chroma])
    }
}

/// The (luma, chroma) per-plane view formats for a Vulkan-Video pool format, or `None`
/// when this presenter can't sample it (the caller bails; the decoder demotes to
/// software — never a black screen).
///
/// The decision table IS the desktop 4:4:4 display contract, so it's a pure function
/// with a test pinning it:
/// - 2-plane 4:2:0, 8-bit (NV12-layout) and 10-bit (P010/X6) — the classic pair.
/// - 2-plane 4:4:4, 8- and 10-bit — what NVIDIA's Vulkan Video reports for HEVC RExt
///   full-chroma decode (semi-planar, like all NVDEC output). The CSC shader already
///   handles the full-size chroma plane (its 4:2:0 siting correction self-disables when
///   the plane widths match), so accepting the format here is all hardware 4:4:4 needs.
/// - 3-plane 4:4:4 stays rejected: the CSC pass samples exactly two planes (luma +
///   interleaved chroma); a triplanar pool needs a third binding + shader variant. No
///   supported driver reports it for HEVC decode today — revisit when one does.
fn vkframe_plane_formats(raw: i32) -> Option<(vk::Format, vk::Format)> {
    let eight = (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM);
    let ten = (
        vk::Format::R10X6_UNORM_PACK16,
        vk::Format::R10X6G10X6_UNORM_2PACK16,
    );
    [
        (vk::Format::G8_B8R8_2PLANE_420_UNORM, eight),
        (vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16, ten),
        (vk::Format::G8_B8R8_2PLANE_444_UNORM, eight),
        (vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16, ten),
    ]
    .into_iter()
    .find_map(|(f, planes)| (f.as_raw() == raw).then_some(planes))
}

/// Flatten the 3×vec4 rows for the push-constant block.
fn bytemuck_rows(rows: &[[f32; 4]; 3]) -> &[f32] {
    // SAFETY: [[f32;4];3] is 12 contiguous f32s.
    unsafe { std::slice::from_raw_parts(rows.as_ptr().cast::<f32>(), 12) }
}

/// The live sync state of an `AVVkFrame`, snapshotted under the frames lock.
struct VkFrameSync {
    image: u64,
    semaphore: u64,
    sem_value: u64,
    layout: i32,
    queue_family: u32,
}

/// Lock the frame and read its live sync state (the presenter's submit must wait
/// `sem_value` and signal `sem_value + 1`). The lock is held until [`unlock_vkframe`].
// bindgen's enum repr is target-dependent (u32 Linux/clang, i32 MSVC) — the layout cast
// is required on one platform and a no-op on the other.
#[allow(clippy::unnecessary_cast)]
fn lock_vkframe(f: &VkVideoFrame) -> VkFrameSync {
    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this type
    // and live for the call, and every builder struct is a local that outlives it.
    unsafe {
        let lock: unsafe extern "C" fn(*mut pf_ffvk::AVHWFramesContext, *mut pf_ffvk::AVVkFrame) =
            std::mem::transmute(f.lock_frame);
        let fc = f.frames_ctx as *mut pf_ffvk::AVHWFramesContext;
        let vkf = f.vkframe as *mut pf_ffvk::AVVkFrame;
        lock(fc, vkf);
        VkFrameSync {
            image: (*vkf).img[0] as u64,
            semaphore: (*vkf).sem[0] as u64,
            sem_value: (*vkf).sem_value[0],
            layout: (*vkf).layout[0] as i32,
            queue_family: (*vkf).queue_family[0],
        }
    }
}

/// Write the post-submission state back (FFmpeg waits these on its next use of the
/// frame) and release the lock. On a failed submit only the lock is released.
fn unlock_vkframe(f: &VkVideoFrame, sync: &VkFrameSync, submitted: bool, graphics_qf: u32) {
    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this type
    // and live for the call, and every builder struct is a local that outlives it.
    unsafe {
        let vkf = f.vkframe as *mut pf_ffvk::AVVkFrame;
        if submitted {
            (*vkf).sem_value[0] = sync.sem_value + 1;
            (*vkf).layout[0] =
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw() as pf_ffvk::VkImageLayout;
            if sync.queue_family != vk::QUEUE_FAMILY_IGNORED {
                (*vkf).queue_family[0] = graphics_qf;
            }
        }
        let unlock: unsafe extern "C" fn(*mut pf_ffvk::AVHWFramesContext, *mut pf_ffvk::AVVkFrame) =
            std::mem::transmute(f.unlock_frame);
        unlock(f.frames_ctx as *mut pf_ffvk::AVHWFramesContext, vkf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool-format decision table (what this presenter can sample → what stays on the
    /// hardware path, everything else demotes to software decode). Pinned so a format
    /// added or dropped here is a deliberate act, not a drive-by.
    #[test]
    fn vkframe_pool_format_decision_table() {
        let eight = Some((vk::Format::R8_UNORM, vk::Format::R8G8_UNORM));
        let ten = Some((
            vk::Format::R10X6_UNORM_PACK16,
            vk::Format::R10X6G10X6_UNORM_2PACK16,
        ));
        // 2-plane 4:2:0, both depths — the classic pair.
        let f = |fmt: vk::Format| vkframe_plane_formats(fmt.as_raw());
        assert_eq!(f(vk::Format::G8_B8R8_2PLANE_420_UNORM), eight);
        assert_eq!(
            f(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16),
            ten
        );
        // 2-plane 4:4:4, both depths — hardware full-chroma (NVIDIA RExt decode). Same
        // per-plane view formats; the full-size chroma plane is the shader's business.
        assert_eq!(f(vk::Format::G8_B8R8_2PLANE_444_UNORM), eight);
        assert_eq!(
            f(vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16),
            ten
        );
        // 3-plane 4:4:4 and 2-plane 4:2:2: real formats a driver could report, NOT
        // sampleable by the two-binding CSC — they must demote, not corrupt.
        assert_eq!(f(vk::Format::G8_B8_R8_3PLANE_444_UNORM), None);
        assert_eq!(f(vk::Format::G8_B8R8_2PLANE_422_UNORM), None);
        assert_eq!(f(vk::Format::G16_B16R16_2PLANE_444_UNORM), None);
        // Garbage never maps.
        assert_eq!(vkframe_plane_formats(0), None);
        assert_eq!(vkframe_plane_formats(-1), None);
    }
}
