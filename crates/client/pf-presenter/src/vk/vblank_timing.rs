//! Glass stamps without `VK_KHR_present_wait`: a waiter on the window's output vblank.
//!
//! A driver without `VK_KHR_present_wait` (AMD on Windows) leaves the panel unmeasured.
//! `IDXGIOutput::WaitForVBlank` returns once per refresh of the monitor under the window; a
//! FIFO present is taken to be on glass at the first vblank after its GPU work was done,
//! one present per vblank. Under MAILBOX or IMMEDIATE the newest ready present takes that
//! vblank and the ones before it are replaced. An estimate, labelled `glass=est`: it feeds
//! the ledger and the VRR verdict, never the latch grid, and the glass gate only while the
//! panel runs at its mode rate. Under variable refresh the vblanks follow the presents; the
//! waiter publishes their spacing, the one direct reading of the panel's refresh this path
//! has.
//!
//! A GPU-done stage ahead of the vblank loop stamps when our own work finished, as the
//! present-wait waiter does, so the display split tells our render from the flip.
//!
//! Never touches the swapchain, so no drain before a swapchain teardown. It reads the
//! presenter's `done_sem` only, which outlives it: the presenter drops this waiter first.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ash::vk;
use windows::Win32::dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC};

use super::present_timing::PresentedSample;

enum Msg {
    Present(Job),
    /// The window moved to another monitor: follow it.
    Retarget(isize),
}

struct Job {
    done: Option<(vk::Semaphore, u64)>,
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
    /// Our GPU work finished (client clock); 0 when there was nothing to wait.
    gpu_done_ns: u64,
    queued: Instant,
    /// MAILBOX/IMMEDIATE: a newer ready present replaces this one at the vblank.
    latest_wins: bool,
}

impl Job {
    /// Ready for a flip: the GPU done, or the submit when nothing was waited.
    fn ready_ns(&self) -> u64 {
        self.gpu_done_ns.max(self.submitted_ns)
    }
}

/// A present whose GPU work never signals within this leaves the count without a
/// sample, like a present-wait past its cap. The gate force-opens at 100 ms anyway.
const STALE: Duration = Duration::from_millis(250);

/// Stamps each present's GPU completion in submission order and hands it to the vblank
/// loop. A wait past [`STALE`] is a wedged pipeline: that present leaves the count unsampled.
fn stamp_gpu_done(
    device: ash::Device,
    rx: mpsc::Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
    pending: Arc<AtomicUsize>,
    wake: WakeSlot,
) {
    while let Ok(msg) = rx.recv() {
        let Msg::Present(mut job) = msg else {
            if tx.send(msg).is_err() {
                return;
            }
            continue;
        };
        if let Some((sem, value)) = job.done {
            let semaphores = [sem];
            let values = [value];
            let info = vk::SemaphoreWaitInfo::default()
                .semaphores(&semaphores)
                .values(&values);
            // SAFETY: the presenter's timeline semaphore, alive until the presenter drops
            // this waiter, which joins the thread first.
            if unsafe { device.wait_semaphores(&info, STALE.as_nanos() as u64) }.is_err() {
                pending.fetch_sub(1, Ordering::AcqRel);
                if let Some(cb) = wake.lock().unwrap().as_ref() {
                    cb();
                }
                continue;
            }
            job.gpu_done_ns = pf_client_core::session::now_ns();
        }
        if tx.send(Msg::Present(job)).is_err() {
            return;
        }
    }
}

type WakeSlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

pub(crate) struct VblankTimer {
    tx: Option<mpsc::Sender<Msg>>,
    /// Enqueued and not yet on glass: the glass gate's in-flight count.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    /// Median spacing of the last [`REFRESH_SPANS`] vblanks; 0 until measured.
    refresh_ns: Arc<AtomicU64>,
    wake: WakeSlot,
    stop: Arc<AtomicBool>,
    /// The GPU-done stage, then the vblank loop.
    join: Vec<std::thread::JoinHandle<()>>,
}

/// Vblank spacings per published median: a tenth of a second at 165 Hz.
const REFRESH_SPANS: usize = 16;

/// The DXGI output whose monitor is `monitor`, on any adapter: a hybrid box scans out on
/// one of them.
fn output_for(monitor: isize) -> Option<IDXGIOutput> {
    // SAFETY: plain factory creation; the interface is owned by this scope.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    for a in 0.. {
        // SAFETY: COM enumeration on the live factory; an error ends the walk.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            // SAFETY: COM enumeration on the adapter just returned; an error ends the walk.
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else {
                break;
            };
            let mut desc = DXGI_OUTPUT_DESC::default();
            // SAFETY: a COM read into the local descriptor, checked before it is used.
            if unsafe { output.GetDesc(&mut desc) }.is_ok() && desc.Monitor as isize == monitor {
                return Some(output);
            }
        }
    }
    None
}

/// 1 ms: no panel refreshes faster, so an answer inside it is no vblank.
const MIN_VBLANK_NS: u64 = 1_000_000;

#[allow(clippy::too_many_arguments)]
fn run(
    monitor: isize,
    rx: mpsc::Receiver<Msg>,
    ready_tx: mpsc::Sender<bool>,
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    refresh_ns: Arc<AtomicU64>,
    wake: WakeSlot,
    stop: Arc<AtomicBool>,
) {
    // COM objects stay on this thread; the spawner only learns whether one was found.
    let mut output = output_for(monitor);
    let _ = ready_tx.send(output.is_some());
    if output.is_none() {
        return;
    }
    let mut queue: VecDeque<Job> = VecDeque::new();
    let mut last_vblank_ns = 0u64;
    let mut spans: Vec<u64> = Vec::with_capacity(REFRESH_SPANS);
    while !stop.load(Ordering::Acquire) {
        // SAFETY: a COM call on the live output; it blocks until the monitor's next vblank.
        let answered = output
            .as_ref()
            .is_some_and(|o| unsafe { o.WaitForVBlank() }.is_ok());
        if !answered {
            // Output gone (mode change, undock) until a retarget: keep the queue moving.
            output = None;
            std::thread::sleep(Duration::from_millis(16));
        }
        let vblank_ns = pf_client_core::session::now_ns();
        // An answer inside a millisecond (a sleeping or duplicated output) is no vblank: it
        // would spin this boosted thread and read a microsecond "refresh".
        let instant = answered
            && last_vblank_ns != 0
            && vblank_ns.saturating_sub(last_vblank_ns) < MIN_VBLANK_NS;
        if instant {
            std::thread::sleep(Duration::from_millis(1));
        }
        let waited = answered && !instant;
        if !waited {
            spans.clear();
            refresh_ns.store(0, Ordering::Relaxed);
        } else if last_vblank_ns != 0 {
            spans.push(vblank_ns.saturating_sub(last_vblank_ns));
            if spans.len() == REFRESH_SPANS {
                spans.sort_unstable();
                refresh_ns.store(spans[REFRESH_SPANS / 2], Ordering::Relaxed);
                spans.clear();
            }
        }
        last_vblank_ns = if answered { vblank_ns } else { 0 };
        // Presents handed over during the wait are candidates for this vblank too.
        loop {
            match rx.try_recv() {
                Ok(Msg::Present(j)) => queue.push_back(j),
                Ok(Msg::Retarget(m)) => {
                    if let Some(o) = output_for(m) {
                        output = Some(o);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        // Replaced presents never reach the glass: settle them without a sample.
        let mut replaced = 0;
        while queue.len() > 1 && queue[0].latest_wins && queue[1].ready_ns() < vblank_ns {
            queue.pop_front();
            replaced += 1;
        }
        // One present per vblank, in submission order: the first vblank after its GPU
        // work was done. Work that finished inside the driver's flip deadline reads one
        // vblank early.
        let settled = match queue.front() {
            Some(j) if j.ready_ns() < vblank_ns => {
                let j = queue.pop_front().expect("front checked");
                results.lock().unwrap().push(PresentedSample {
                    present_id: 0,
                    pts_ns: j.pts_ns,
                    decoded_ns: j.decoded_ns,
                    submitted_ns: j.submitted_ns,
                    gpu_done_ns: j.gpu_done_ns,
                    displayed_ns: vblank_ns,
                    exact: false,
                });
                true
            }
            Some(j) if j.queued.elapsed() > STALE => {
                queue.pop_front();
                true
            }
            _ => false,
        };
        let settled = replaced + usize::from(settled);
        if settled > 0 {
            pending.fetch_sub(settled, Ordering::AcqRel);
            if let Some(cb) = wake.lock().unwrap().as_ref() {
                cb();
            }
        }
    }
}

impl VblankTimer {
    /// `None` when no DXGI output holds the window's `monitor`: better no clock than a
    /// gate that never reopens.
    pub(crate) fn spawn(device: ash::Device, monitor: isize) -> Option<Self> {
        let (tx, gpu_rx) = mpsc::channel::<Msg>();
        let (gpu_tx, rx) = mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let refresh_ns = Arc::new(AtomicU64::new(0));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (pending_g, wake_g) = (pending.clone(), wake.clone());
        let gpu = std::thread::Builder::new()
            .name("pf-gpu-done".into())
            .spawn(move || {
                // The stamp is taken at wake; scheduler delay would read as our render.
                pf_client_core::audio_rt::boost_and_log("gpu-done");
                stamp_gpu_done(device, gpu_rx, gpu_tx, pending_g, wake_g);
            })
            .expect("spawn pf-gpu-done");
        let (pending_t, results_t, wake_t, stop_t) =
            (pending.clone(), results.clone(), wake.clone(), stop.clone());
        let refresh_t = refresh_ns.clone();
        let vblank = std::thread::Builder::new()
            .name("pf-vblank".into())
            .spawn(move || {
                // The stamp is taken at wake; scheduler delay would read as latch.
                pf_client_core::audio_rt::boost_and_log("vblank");
                run(
                    monitor, rx, ready_tx, pending_t, results_t, refresh_t, wake_t, stop_t,
                );
            })
            .expect("spawn pf-vblank");
        let found = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false);
        let timer = VblankTimer {
            tx: Some(tx),
            pending,
            results,
            refresh_ns,
            wake,
            stop,
            join: vec![gpu, vblank],
        };
        found.then_some(timer)
    }

    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// The output's measured refresh spacing; 0 until a median exists.
    pub(crate) fn refresh_ns(&self) -> u64 {
        self.refresh_ns.load(Ordering::Relaxed)
    }

    /// Presents not yet on glass by the estimate, plus any that will drop out stale.
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn enqueue(
        &self,
        done: Option<(vk::Semaphore, u64)>,
        pts_ns: u64,
        decoded_ns: u64,
        submitted_ns: u64,
        latest_wins: bool,
    ) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            let job = Job {
                done,
                pts_ns,
                decoded_ns,
                submitted_ns,
                gpu_done_ns: 0,
                queued: Instant::now(),
                latest_wins,
            };
            if tx.send(Msg::Present(job)).is_err() {
                self.pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Follow the window to `monitor` (a display change).
    pub(crate) fn retarget(&self, monitor: isize) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Retarget(monitor));
        }
    }

    pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
        std::mem::take(&mut *self.results.lock().unwrap())
    }
}

impl Drop for VblankTimer {
    fn drop(&mut self) {
        // Dropping `tx` ends the GPU stage after at most one [`STALE`] wait; the vblank
        // loop checks `stop` every vblank, so the joins wait out one refresh past that.
        self.stop.store(true, Ordering::Release);
        self.tx.take();
        for j in self.join.drain(..) {
            let _ = j.join();
        }
    }
}
