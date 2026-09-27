//! On-glass present stamps via `VK_KHR_present_wait`.
//!
//! `vkQueuePresentKHR` return is CPU submit, not vblank. A waiter thread
//! blocks in `vkWaitForPresentKHR` until the image is visible and stamps that.
//! Given the submit's timeline value it first stamps when our own GPU work was
//! done, which splits the compositor's share from ours.
//!
//! [`PresentTimer::drain`] before `vkDestroySwapchainKHR` and before any
//! `vkCreateSwapchainKHR` that names the live swapchain as `oldSwapchain` —
//! that create externally-synchronises the old handle and can retire it under
//! a parked waiter. 250 ms wait cap: ids complete in submission order (a
//! MAILBOX-replaced id completes with the present that replaced it); a wait
//! only outlives that cap when the pipeline is already wedged.
//!
//! `vkWaitForPresentKHR`, `vkAcquireNextImageKHR` and `vkQueuePresentKHR` each
//! externally synchronize the swapchain. Every such call holds
//! [`PresentTimer::swapchain_guard`]'s lock for one call of at most [`SLICE_NS`];
//! the waiter waits in slices and lets a waiting presenter go first.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ash::vk;

/// Longest single wait on the swapchain. 1 ms bounds how long a present waits for the
/// waiter, and a driver with millisecond timeouts still blocks rather than spins.
pub(crate) const SLICE_NS: u64 = 1_000_000;

/// Host sync for the swapchain between the presenter thread and the waiter.
#[derive(Default)]
struct SwapchainSync {
    lock: Mutex<()>,
    /// The presenter is blocked on `lock`; the waiter backs off before its next slice.
    presenter_waiting: AtomicBool,
}

pub(crate) struct PresentedSample {
    /// Capture stamp (host clock) — the e2e latency anchor.
    pub pts_ns: u64,
    /// Decode-complete stamp (client clock) — the display-stage anchor.
    pub decoded_ns: u64,
    /// `vkQueuePresentKHR` return (client clock) — pace/latch split:
    /// submitted−decoded is pipeline, displayed−submitted is the vsync latch.
    pub submitted_ns: u64,
    /// Our GPU work for this present finished (client clock). 0 when not waited.
    pub gpu_done_ns: u64,
    /// `vkWaitForPresentKHR` completion: the image is visible (client clock).
    pub displayed_ns: u64,
}

struct Job {
    swapchain: vk::SwapchainKHR,
    present_id: u64,
    /// The submit's timeline signal, waited before the present.
    done: Option<(vk::Semaphore, u64)>,
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
}

/// Run-loop wake (SDL event push), shared with the waiter thread.
type WakeSlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// Upstream keeps one frame in flight, so queue depth stays ~1.
pub(crate) struct PresentTimer {
    tx: Option<mpsc::Sender<Job>>,
    /// Enqueued but unfinished — drain barrier and the glass gate's in-flight count.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    /// After each wait. The run loop installs an SDL wake so a gate reopen
    /// never waits out the event-loop timeout.
    wake: WakeSlot,
    sync: Arc<SwapchainSync>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// `vkWaitForPresentKHR` for up to 250 ms in [`SLICE_NS`] calls, each under the swapchain
/// lock. 250 ms: ids complete in order; longer means the pipeline is wedged.
fn wait_sliced(
    wait_d: &ash::khr::present_wait::Device,
    sync: &SwapchainSync,
    job: &Job,
) -> ash::prelude::VkResult<()> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        // Sleep, not spin: a boosted waiter could starve the presenter on a shared core.
        while sync.presenter_waiting.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_micros(50));
        }
        let r = {
            let _swapchain = sync.lock.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `job.swapchain` stays live for this call — enqueue runs while the
            // swapchain exists, and `drain`/Drop wait it out first. The lock above is the
            // swapchain's host sync against the presenter's acquire and present.
            unsafe { wait_d.wait_for_present(job.swapchain, job.present_id, SLICE_NS) }
        };
        match r {
            Err(vk::Result::TIMEOUT) if Instant::now() < deadline => {}
            r => return r,
        }
    }
}

impl PresentTimer {
    pub(crate) fn spawn(wait_d: ash::khr::present_wait::Device, device: ash::Device) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let sync = Arc::new(SwapchainSync::default());
        let (pending_t, results_t, wake_t, sync_t) =
            (pending.clone(), results.clone(), wake.clone(), sync.clone());
        let join = std::thread::Builder::new()
            .name("pf-present-wait".into())
            .spawn(move || {
                // The on-glass stamp is taken at wake; scheduler delay reads as latch.
                pf_client_core::audio_rt::boost_and_log("present-wait");
                while let Ok(job) = rx.recv() {
                    let mut gpu_done_ns = 0;
                    if let Some((sem, value)) = job.done {
                        let semaphores = [sem];
                        let values = [value];
                        let info = vk::SemaphoreWaitInfo::default()
                            .semaphores(&semaphores)
                            .values(&values);
                        // SAFETY: `sem` is the presenter's timeline semaphore, alive until
                        // teardown, which drains this thread first.
                        if unsafe { device.wait_semaphores(&info, 250_000_000) }.is_ok() {
                            gpu_done_ns = pf_client_core::session::now_ns();
                        }
                    }
                    let r = wait_sliced(&wait_d, &sync_t, &job);
                    if r.is_ok() {
                        let displayed_ns = pf_client_core::session::now_ns();
                        results_t.lock().unwrap().push(PresentedSample {
                            pts_ns: job.pts_ns,
                            decoded_ns: job.decoded_ns,
                            submitted_ns: job.submitted_ns,
                            gpu_done_ns,
                            displayed_ns,
                        });
                    }
                    // Wait failed: no sample. The frame still showed, or the loop
                    // is about to find out — do not poison the stats window.
                    pending_t.fetch_sub(1, Ordering::AcqRel);
                    // Wake after the count dropped so the run loop sees the
                    // post-completion state. The callback is an SDL event push
                    // and must not reenter this type.
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
            sync,
            join: Some(join),
        }
    }

    /// The swapchain's host sync on the presenter thread: hold it across one acquire or
    /// present call. Waits out at most one [`SLICE_NS`] wait of the waiter.
    pub(crate) fn swapchain_guard(&self) -> MutexGuard<'_, ()> {
        self.sync.presenter_waiting.store(true, Ordering::Release);
        let guard = self
            .sync
            .lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.sync.presenter_waiting.store(false, Ordering::Release);
        guard
    }

    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// Undisplayed presents, including waits that will end SUBOPTIMAL/TIMEOUT.
    /// Those resolve within 250 ms, past the gate's 100 ms stale force-open.
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn enqueue(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        done: Option<(vk::Semaphore, u64)>,
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
                    done,
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

    /// Wait until no wait still names a swapchain. Required before
    /// `vkDestroySwapchainKHR` / `oldSwapchain` create. Capped at 250 ms.
    pub(crate) fn drain(&self) {
        while self.pending.load(Ordering::Acquire) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
        std::mem::take(&mut *self.results.lock().unwrap())
    }
}

impl Drop for PresentTimer {
    fn drop(&mut self) {
        // Dropping `tx` ends recv; join waits out any in-flight 250 ms wait.
        self.tx.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
