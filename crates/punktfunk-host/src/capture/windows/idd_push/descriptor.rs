//! Off-thread display-descriptor polling (plan §W4, carved out of the IDD-push capturer): the
//! live HDR state + active resolution of the virtual target, sampled off the capture loop via CCD.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::*;

/// Creates + owns the shared ring; yields the driver's frames as [`FramePayload::D3d11`].
/// The display descriptor the capture loop follows: live HDR state + active resolution of the
/// virtual target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayDescriptor {
    pub(super) hdr: bool,
    pub(super) width: u32,
    pub(super) height: u32,
}

/// Off-thread poller for [`DisplayDescriptor`]. The CCD queries behind it (`QueryDisplayConfig`,
/// twice per sample) serialize on the session-global display-configuration lock, which display-
/// topology events and third-party display-poller software (the SteelSeries-GG class) can hold
/// for tens-to-hundreds of milliseconds at a time. Polled inline — the old design — that stall
/// landed ON the capture/encode thread: a periodic frame hitch on an otherwise healthy host, and
/// invisible in any log. Now a dedicated thread samples every [`Self::INTERVAL`] and publishes a
/// snapshot; the capture thread's per-frame cost is one uncontended mutex read, and a slow CCD
/// sample is *measured and logged* instead of silently stalling the stream.
///
/// Failure policy is last-known-good, per field: a transient CCD failure — including the target
/// briefly missing from the active-path list during a topology re-probe — keeps the previous
/// value instead of reading as `hdr = false` (the old behavior, which on an HDR session turned
/// every blip into TWO ring recreates: false, then true again a poll later). `seq` bumps only
/// when at least one query succeeded, so the consumer's debounce counts real observations, never
/// failures.
pub(super) struct DescriptorPoller {
    /// Latest merged sample + its sequence number; the poller holds the lock only to copy it.
    snap: Arc<Mutex<(DisplayDescriptor, u64)>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DescriptorPoller {
    /// Poll cadence — the old inline throttle. With the consumer's two-strikes debounce on top, a
    /// real "Use HDR" flip or mode-set is acted on within ~2 samples (≈ ½ s).
    const INTERVAL: Duration = Duration::from_millis(250);
    /// A sample slower than this means something is sitting on the display-config lock (topology
    /// churn / display-poller software) — the disturbance class behind periodic virtual-display
    /// stream hitches. Logged (rate-limited) so an affected host self-diagnoses.
    const SLOW: Duration = Duration::from_millis(50);

    pub(super) fn spawn(target_id: u32, initial: DisplayDescriptor) -> Self {
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
                    // SAFETY: both are read-only CCD queries taking only a copy of the plain `u32`
                    // target id (see their own SAFETY docs); nothing is borrowed across the calls.
                    let (hdr, res) = unsafe {
                        (
                            crate::win_display::advanced_color_enabled(target_id),
                            crate::win_display::active_resolution(target_id),
                        )
                    };
                    let took = t.elapsed();
                    if took >= Self::SLOW
                        && last_slow_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
                    {
                        last_slow_log = Some(Instant::now());
                        tracing::warn!(
                            took_ms = took.as_millis() as u64,
                            target_id,
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
                    // Park (not sleep) so `drop` wakes the thread immediately via `unpark`.
                    std::thread::park_timeout(Self::INTERVAL);
                }
            })
            .map_err(|e| {
                // Degraded, not fatal: the session streams, it just never follows a mid-session
                // HDR flip / mode-set (seq stays 0 → the consumer sees no changes).
                tracing::warn!(error = %e, "IDD push: descriptor-poller thread failed to spawn — mid-session HDR/mode changes won't be followed");
            })
            .ok();
        Self { snap, stop, thread }
    }

    /// The latest sample (lock held only for the copy — the poller writes at 4 Hz).
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
