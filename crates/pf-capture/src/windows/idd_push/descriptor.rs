//! Off-thread CCD sampler for the virtual target's live HDR flag and active resolution.
//!
//! `QueryDisplayConfig` (twice per sample) serializes on the session-global display-config
//! lock. A stall of tens of milliseconds on the capture thread misses frames, so
//! [`DescriptorPoller`] samples on its own thread and publishes a snapshot. The capture
//! loop's per-frame cost is one uncontended mutex read.
//!
//! Last-known-good per field: a `None` query — the target briefly missing from the active-path
//! list during a topology re-probe — keeps the previous value. `seq` advances only when at
//! least one query succeeded, so the consumer's debounce counts observations, never misses.
//!
//! Pin: [`DescriptorPoller::snapshot`]. Consumer: `poll_display_hdr` in the IDD-push capturer.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayDescriptor {
    pub(super) hdr: bool,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) struct DescriptorPoller {
    snap: Arc<Mutex<(DisplayDescriptor, u64)>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DescriptorPoller {
    /// 250 ms. Two-strikes debounce in `poll_display_hdr` acts on a real flip in two samples (~½ s).
    const INTERVAL: Duration = Duration::from_millis(250);
    /// 50 ms. A healthy CCD sample is sub-millisecond; above this the display-config lock is held.
    const SLOW: Duration = Duration::from_millis(50);

    pub(super) fn spawn(
        ccd: pf_win_display::win_display::CcdTargetKey,
        initial: DisplayDescriptor,
    ) -> Self {
        let snap = Arc::new(Mutex::new((initial, 0u64)));
        let stop = Arc::new(AtomicBool::new(false));
        let (snap_t, stop_t) = (snap.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("pf-idd-desc-poll".into())
            .spawn(move || {
                let mut last = initial;
                let mut seq = 0u64;
                let mut last_slow_log: Option<Instant> = None;
                while !stop_t.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    let (hdr, res) = (
                            pf_win_display::win_display::advanced_color_enabled(ccd),
                            pf_win_display::win_display::active_resolution(ccd),
                        );
                    let took = t.elapsed();
                    if took >= Self::SLOW
                        && last_slow_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
                    {
                        last_slow_log = Some(Instant::now());
                        tracing::warn!(
                            took_ms = took.as_millis() as u64,
                            target = %ccd,
                            "slow display-descriptor poll — something is holding the Windows \
                             display-config lock (topology churn / display-poller software); on \
                             a host with periodic stream hitches, correlate this cadence"
                        );
                    }
                    if hdr.is_some() || res.is_some() {
                        if let Some(hdr) = hdr {
                            last.hdr = hdr;
                        }
                        if let Some((width, height)) = res {
                            last.width = width;
                            last.height = height;
                        }
                        seq += 1;
                        *snap_t.lock().unwrap() = (last, seq);
                    }
                    // `park_timeout`, not `sleep`: `Drop` unparks so join does not wait out INTERVAL.
                    std::thread::park_timeout(Self::INTERVAL);
                }
            })
            .map_err(|e| {
                // Not fatal: `seq` stays 0, so the capture loop never follows a mid-session flip.
                tracing::warn!(error = %e, "IDD push: descriptor-poller thread failed to spawn — mid-session HDR/mode changes won't be followed");
            })
            .ok();
        Self { snap, stop, thread }
    }

    pub(super) fn snapshot(&self) -> (DisplayDescriptor, u64) {
        *self.snap.lock().unwrap()
    }
}

impl Drop for DescriptorPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
