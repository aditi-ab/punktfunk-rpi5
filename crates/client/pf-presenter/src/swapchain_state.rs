//! The swapchain's per-frame bookkeeping beside the presenter's Vulkan handles: the submit
//! in flight and the image held between acquire and submit.
//!
//! Pure: no Vulkan. Who owns the window is the reconfig model's (`vk/reconfig_model.rs`);
//! the stamp asks and which glass clock stamps the display are the glass clock's
//! (`vk/glass.rs`). Pinned by the table tests below. The order inside a present (import,
//! acquire, retire) is `vk/present.rs`'s.

pub(crate) struct SwapchainState {
    /// Submit fence has work pending. Wait before recording; also what makes the single
    /// staging buffer safe to overwrite.
    pub(crate) submitted: bool,
    /// Swapchain image acquired and not yet submitted: the non-blocking probe's, or one an
    /// error left behind. Its `acquire_sem` signal is pending until a batch waits it.
    pub(crate) acquired: Option<u32>,
}

impl SwapchainState {
    pub(crate) fn new() -> SwapchainState {
        SwapchainState {
            submitted: false,
            acquired: None,
        }
    }

    /// A submit waited the held image's `acquire_sem` and named the fence.
    pub(crate) fn on_submit(&mut self) {
        self.submitted = true;
        self.acquired = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submit_consumes_the_held_image() {
        let mut s = SwapchainState::new();
        s.acquired = Some(2);
        s.on_submit();
        assert!(s.submitted);
        assert_eq!(s.acquired, None);
    }
}
