//! True on-glass present timing via `VK_KHR_present_wait` (latency plan T0.2).
//!
//! The render loop's `displayed` stamp is taken when `vkQueuePresentKHR` *returns* — CPU
//! submit time, excluding the presentation engine's queue and the vblank latch, so the
//! HUD's `display` stage under-reports by up to a refresh (and hides a silent-FIFO
//! standing queue entirely). When the device offers `VK_KHR_present_id` +
//! `VK_KHR_present_wait`, each present carries a monotonically increasing id and a
//! dedicated waiter thread blocks in `vkWaitForPresentKHR` — which completes when the
//! image is actually visible — stamping the REAL on-glass time off the render loop.
//!
//! Lifecycle: `vkWaitForPresentKHR` requires the swapchain to stay alive for the call's
//! duration, so [`PresentTimer::drain`] must run before any `vkDestroySwapchainKHR`
//! (recreate and teardown both do). Waits carry a 250 ms cap: presentation ids complete
//! in submission order (a MAILBOX-replaced image's id completes with the present that
//! replaced it), so a wait only outlives that cap when the pipeline is already wedged —
//! the timeout keeps the drain bounded rather than wedging a resize with it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use ash::vk;

/// One presented frame's identity + the true on-glass stamp the waiter filled in.
pub(crate) struct PresentedSample {
    /// The frame's capture stamp (host clock) — the e2e anchor.
    pub pts_ns: u64,
    /// Decode-complete stamp (client clock) — the display-stage anchor.
    pub decoded_ns: u64,
    /// `vkWaitForPresentKHR` completion = the image is visible (client clock).
    pub displayed_ns: u64,
}

struct Job {
    swapchain: vk::SwapchainKHR,
    present_id: u64,
    pts_ns: u64,
    decoded_ns: u64,
}

/// The waiter: a channel-fed thread turning (swapchain, present-id) pairs into
/// [`PresentedSample`]s. One frame in flight upstream keeps the queue depth ~1.
pub(crate) struct PresentTimer {
    tx: Option<mpsc::Sender<Job>>,
    /// Jobs enqueued but not yet finished — the drain barrier for swapchain teardown.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PresentTimer {
    pub(crate) fn spawn(wait_d: ash::khr::present_wait::Device) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let (pending_t, results_t) = (pending.clone(), results.clone());
        let join = std::thread::Builder::new()
            .name("pf-present-wait".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // 250 ms cap — see the module doc's lifecycle note.
                    let r = unsafe {
                        wait_d.wait_for_present(job.swapchain, job.present_id, 250_000_000)
                    };
                    if r.is_ok() {
                        let displayed_ns = pf_client_core::session::now_ns();
                        results_t.lock().unwrap().push(PresentedSample {
                            pts_ns: job.pts_ns,
                            decoded_ns: job.decoded_ns,
                            displayed_ns,
                        });
                    }
                    // SUBOPTIMAL/TIMEOUT/DEVICE_LOST: no sample; the frame still showed
                    // (or the loop is about to find out) — never poison the window.
                    pending_t.fetch_sub(1, Ordering::AcqRel);
                }
            })
            .expect("spawn pf-present-wait");
        PresentTimer {
            tx: Some(tx),
            pending,
            results,
            join: Some(join),
        }
    }

    /// Hand a successfully submitted present to the waiter.
    pub(crate) fn enqueue(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        pts_ns: u64,
        decoded_ns: u64,
    ) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            if tx
                .send(Job {
                    swapchain,
                    present_id,
                    pts_ns,
                    decoded_ns,
                })
                .is_err()
            {
                self.pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Block until no wait references any swapchain — REQUIRED before
    /// `vkDestroySwapchainKHR`. Bounded by the waiter's own 250 ms wait cap.
    pub(crate) fn drain(&self) {
        while self.pending.load(Ordering::Acquire) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Take the window's completed samples (called at the 1 s stats fold).
    pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
        std::mem::take(&mut *self.results.lock().unwrap())
    }
}

impl Drop for PresentTimer {
    fn drop(&mut self) {
        // Close the channel, let in-flight waits finish (bounded), then join.
        self.tx.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
