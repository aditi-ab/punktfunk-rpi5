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
//! (recreate and teardown both do) — AND before a `vkCreateSwapchainKHR` that names the
//! live swapchain as `oldSwapchain`, which externally-synchronises it and lets the driver
//! retire it under a parked waiter. Stating only the destroy half is how the drain came to
//! sit after the create, which is a Windows `VK_ERROR_UNKNOWN` on an F11 mode change.
//! Waits carry a 250 ms cap: presentation ids complete
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
    /// `vkQueuePresentKHR`-return stamp (client clock) — the pace/latch split point:
    /// `submitted − decoded` is our pipeline, `displayed − submitted` the vsync latch.
    pub submitted_ns: u64,
    /// `vkWaitForPresentKHR` completion = the image is visible (client clock).
    pub displayed_ns: u64,
}

struct Job {
    swapchain: vk::SwapchainKHR,
    present_id: u64,
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
}

/// The run loop's wake callback (an SDL event push), shared with the waiter thread.
type WakeSlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// The waiter: a channel-fed thread turning (swapchain, present-id) pairs into
/// [`PresentedSample`]s. One frame in flight upstream keeps the queue depth ~1.
pub(crate) struct PresentTimer {
    tx: Option<mpsc::Sender<Job>>,
    /// Jobs enqueued but not yet finished — the drain barrier for swapchain teardown,
    /// and the glass gate's "undisplayed presents in flight" count.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    /// Called by the waiter after each completed wait (sample or not) — the run loop
    /// installs an SDL wake here so a gate reopen / smoothness slot never waits out the
    /// event-loop timeout.
    wake: WakeSlot,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PresentTimer {
    pub(crate) fn spawn(wait_d: ash::khr::present_wait::Device) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let (pending_t, results_t, wake_t) = (pending.clone(), results.clone(), wake.clone());
        let join = std::thread::Builder::new()
            .name("pf-present-wait".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // 250 ms cap — see the module doc's lifecycle note.
                    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are
                    // owned by this type and live for the call, and every builder struct is a
                    // local that outlives it.
                    let r = unsafe {
                        wait_d.wait_for_present(job.swapchain, job.present_id, 250_000_000)
                    };
                    if r.is_ok() {
                        let displayed_ns = pf_client_core::session::now_ns();
                        results_t.lock().unwrap().push(PresentedSample {
                            pts_ns: job.pts_ns,
                            decoded_ns: job.decoded_ns,
                            submitted_ns: job.submitted_ns,
                            displayed_ns,
                        });
                    }
                    // SUBOPTIMAL/TIMEOUT/DEVICE_LOST: no sample; the frame still showed
                    // (or the loop is about to find out) — never poison the window.
                    pending_t.fetch_sub(1, Ordering::AcqRel);
                    // Wake the run loop AFTER the count dropped: what it observes on
                    // wake is the post-completion state (the gate may now be open).
                    // Called under the slot lock — the callback is a bare SDL event
                    // push and never reenters this type.
                    if let Some(cb) = wake_t.lock().unwrap().as_ref() {
                        cb();
                    }
                }
            })
            .expect("spawn pf-present-wait");
        PresentTimer {
            tx: Some(tx),
            pending,
            results,
            wake,
            join: Some(join),
        }
    }

    /// Install the run loop's wake callback (an SDL event push — thread-safe by design).
    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// Presents handed to the waiter and not yet resolved to glass — the glass gate's
    /// budget count. (Also counts a wait that will end SUBOPTIMAL/TIMEOUT; those resolve
    /// within the 250 ms cap, far past the gate's own 100 ms stale force-open.)
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Hand a successfully submitted present to the waiter.
    pub(crate) fn enqueue(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        pts_ns: u64,
        decoded_ns: u64,
        submitted_ns: u64,
    ) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            if tx
                .send(Job {
                    swapchain,
                    present_id,
                    pts_ns,
                    decoded_ns,
                    submitted_ns,
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
