//! What a swapchain brings with it: its images, one render semaphore per image, the overlay's
//! views and framebuffers, the extent and format. [`Targets::build`] makes the set whole or
//! destroys what it made; the presenter swaps it in whole and destroys the old set after.
//!
//! `extent` and `format` are the reconfig model's: committed ahead of a rebuild, which builds
//! at them, and kept while the native lane owns the window so the resume builds there.

use super::overlay_pipe::OverlayTargets;
use super::OverlayPipe;
use anyhow::{anyhow, Result};
use ash::vk;

#[derive(Default)]
pub(super) struct Targets {
    /// Null before the first build and while the lane owns the window.
    pub(super) swapchain: vk::SwapchainKHR,
    pub(super) images: Vec<vk::Image>,
    /// Render-finished, one per image. Present consumes them on the image's schedule; one
    /// shared semaphore can still be held by a previous present.
    pub(super) render_sems: Vec<vk::Semaphore>,
    pub(super) overlay: OverlayTargets,
    pub(super) extent: vk::Extent2D,
    pub(super) format: vk::SurfaceFormatKHR,
}

impl Targets {
    /// Create the swapchain `info` names, then its images, overlay targets and semaphores. A
    /// failure past the create destroys everything made here. The create retires
    /// `info.old_swapchain` whether or not it succeeds; the caller keeps and destroys it.
    /// `hint` is appended to a create failure.
    ///
    /// # Safety
    /// Nothing in flight names `info.old_swapchain`: the caller has quiesced its submit, idled
    /// the queue and drained the present waiter.
    pub(super) unsafe fn build(
        device: &ash::Device,
        swap_d: &ash::khr::swapchain::Device,
        info: &vk::SwapchainCreateInfoKHR,
        overlay: &OverlayPipe,
        hint: impl FnOnce() -> String,
    ) -> Result<Targets> {
        // SAFETY: CREATE — `device` and `info.surface` are live, `info` outlives the call; the
        // caller's contract makes retiring `info.old_swapchain` legal.
        let swapchain = unsafe { swap_d.create_swapchain(info, None) }.map_err(|e| {
            anyhow!(
                "vkCreateSwapchainKHR: {e} ({}x{}, {:?} / {:?}, {:?}, {} images){}",
                info.image_extent.width,
                info.image_extent.height,
                info.image_format,
                info.image_color_space,
                info.present_mode,
                info.min_image_count,
                hint()
            )
        })?;
        let mut t = Targets {
            swapchain,
            extent: info.image_extent,
            format: vk::SurfaceFormatKHR {
                format: info.image_format,
                color_space: info.image_color_space,
            },
            ..Targets::default()
        };
        // SAFETY: `t.swapchain` was created above on `device`.
        match unsafe { t.fill(device, swap_d, overlay) } {
            Ok(()) => Ok(t),
            Err(e) => {
                // SAFETY: never submitted or presented: nothing in flight names any of it.
                unsafe { t.destroy(device, swap_d) };
                Err(e)
            }
        }
    }

    /// The images, the overlay targets on them, one semaphore per image. A failure leaves what
    /// was made in `self`, for [`Targets::destroy`].
    ///
    /// # Safety
    /// `self.swapchain` is live on `device`.
    unsafe fn fill(
        &mut self,
        device: &ash::Device,
        swap_d: &ash::khr::swapchain::Device,
        overlay: &OverlayPipe,
    ) -> Result<()> {
        // SAFETY: the caller's contract: the swapchain is live and owned here.
        self.images = unsafe { swap_d.get_swapchain_images(self.swapchain) }?;
        self.overlay.fill(
            device,
            overlay,
            &self.images,
            self.format.format,
            self.extent,
        )?;
        for _ in 0..self.images.len() {
            // SAFETY: CREATE — `device` is live; the create-info is a local that outlives the call.
            let sem =
                unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
            self.render_sems.push(sem);
        }
        Ok(())
    }

    /// The handles out, the extent and format kept: what the presenter holds while the lane
    /// owns the window, or once it tears down.
    pub(super) fn take(&mut self) -> Targets {
        let kept = Targets {
            extent: self.extent,
            format: self.format,
            ..Targets::default()
        };
        std::mem::replace(self, kept)
    }

    /// Every handle; null ones are skipped.
    ///
    /// # Safety
    /// Nothing in flight names them: the submit fence, the queue and the present waiter are
    /// drained, or none was ever submitted.
    pub(super) unsafe fn destroy(
        mut self,
        device: &ash::Device,
        swap_d: &ash::khr::swapchain::Device,
    ) {
        self.overlay.destroy(device);
        // SAFETY: DESTROY — the caller's contract; the views above went before their images.
        unsafe {
            for s in self.render_sems.drain(..) {
                device.destroy_semaphore(s, None);
            }
            if self.swapchain != vk::SwapchainKHR::null() {
                swap_d.destroy_swapchain(self.swapchain, None);
            }
        }
    }
}
