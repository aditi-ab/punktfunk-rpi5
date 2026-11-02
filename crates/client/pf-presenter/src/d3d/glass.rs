//! The D3D11 backend's glass clock (planning `design/windows-d3d11-presenter.md` D13, D14).
//!
//! DXGI frame statistics stamp each claimed present: `GetFrameStatistics` names the newest
//! present on glass by its count (`GetLastPresentCount` right after its `Present`) and the
//! vblank it reached, in QPC ticks. [`stats_to_samples`] turns one read into samples: the
//! present the statistics name is shown at their vblank, older pending ones were replaced
//! before a refresh took them (`unshown`), newer ones wait. Under tearing no vblank stamps a
//! present: it reads as shown when our GPU work for it is done, an estimate (`glass=est`).
//!
//! One waiter thread owns every wait: the frame-latency waitable object (each signal is one
//! present the swapchain takes without blocking; `Swapchain::ready` takes it from the count,
//! and the signal wakes the run loop), the GPU-done event of the context's fence, and the
//! statistics poll while claimed presents are pending. A resize retires the chain through
//! `glass::lifecycle_step`: its pending presents count as unshown, the refresh baseline restarts.
//!
//! The rules (statistics to samples, the QPC clock, the refresh span) are pure, built and
//! tested on every platform; the waiter is Windows.

/// A claimed present the statistics have not settled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    /// `GetLastPresentCount` after its `Present`: the count the statistics name it by.
    pub(crate) count: u32,
    pub(crate) pts_ns: u64,
    pub(crate) decoded_ns: u64,
    /// The `Present` call returned (session clock).
    pub(crate) submitted_ns: u64,
    /// The fence value the context signals after its work; 0: not waited.
    pub(crate) gpu_value: u64,
    /// That value completed (session clock); 0 until then.
    pub(crate) gpu_done_ns: u64,
    /// Presented with tearing: no vblank stamps it.
    pub(crate) tearing: bool,
}

/// The `DXGI_FRAME_STATISTICS` fields the clock reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameStats {
    /// The count of the newest present on glass.
    pub(crate) present_count: u32,
    /// The vblank that present reached the glass at.
    pub(crate) present_refresh_count: u32,
    /// The vblank `sync_qpc` stamps, which can be later than the present's.
    pub(crate) sync_refresh_count: u32,
    pub(crate) sync_qpc: i64,
}

/// QPC ticks on the session clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QpcClock {
    /// Ticks per second.
    pub(crate) freq: u64,
    /// The session clock minus QPC, in ns.
    pub(crate) offset_ns: i128,
}

impl QpcClock {
    /// From one QPC read between two session-clock reads; `None` for a zero frequency.
    pub(crate) fn calibrate(freq: u64, before_ns: u64, qpc: i64, after_ns: u64) -> Option<Self> {
        let mid = before_ns + after_ns.saturating_sub(before_ns) / 2;
        let offset_ns = i128::from(mid) - ticks_ns(i128::from(qpc), freq)?;
        Some(QpcClock { freq, offset_ns })
    }

    /// `qpc` on the session clock; `None` outside its range.
    pub(crate) fn session_ns(self, qpc: i64) -> Option<u64> {
        u64::try_from(ticks_ns(i128::from(qpc), self.freq)? + self.offset_ns).ok()
    }
}

/// `ticks` QPC ticks in ns; `None` for a zero frequency.
pub(crate) fn ticks_ns(ticks: i128, freq: u64) -> Option<i128> {
    (freq > 0).then(|| ticks * 1_000_000_000 / i128::from(freq))
}

/// QPC ticks per refresh between two reads a vblank apart or more.
pub(crate) fn refresh_span(prev: FrameStats, now: FrameStats) -> Option<u64> {
    let refreshes = now
        .sync_refresh_count
        .checked_sub(prev.sync_refresh_count)
        .filter(|&n| n > 0)?;
    let ticks = u64::try_from(now.sync_qpc.checked_sub(prev.sync_qpc)?).ok()?;
    Some(ticks / u64::from(refreshes))
}

/// A pending present this old settles with no sample, as a present-wait past its cap.
pub(crate) const STALE_NS: u64 = 250_000_000;

/// What one statistics read settles of the pending presents, oldest first.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Settled {
    /// `(present, displayed_ns, exact)`: on glass then; `exact` when its own vblank stamps it.
    pub(crate) shown: Vec<(Pending, u64, bool)>,
    /// Presents a newer one replaced before a refresh took them.
    pub(crate) unshown: u32,
    /// How many of the pending presents, from the front, settled.
    pub(crate) count: usize,
}

enum Verdict {
    Shown(u64, bool),
    Unshown,
    /// Settled with no sample.
    Dropped,
    Wait,
}

/// One read over `pending`, oldest first. `stats` is `None` while none arrive; `refresh_qpc`
/// is the measured refresh in QPC ticks, 0 until measured. It settles a prefix, so samples
/// leave in present order. A tearing present is shown when its GPU work is done, or at its
/// present return when that is not waited. The present the statistics name is shown at their
/// vblank, stepped back by the refresh when the stamp is a later vblank's (no sample without a
/// measured refresh); an older one is unshown; a newer one waits. A wait past [`STALE_NS`]
/// settles with no sample.
pub(crate) fn stats_to_samples(
    pending: &[Pending],
    stats: Option<(FrameStats, QpcClock)>,
    refresh_qpc: u64,
    now_ns: u64,
) -> Settled {
    let mut out = Settled::default();
    for p in pending {
        match verdict(p, stats, refresh_qpc) {
            Verdict::Shown(ns, exact) => out.shown.push((*p, ns, exact)),
            Verdict::Unshown => out.unshown += 1,
            Verdict::Dropped => {}
            Verdict::Wait if now_ns.saturating_sub(p.submitted_ns) > STALE_NS => {}
            Verdict::Wait => break,
        }
        out.count += 1;
    }
    out
}

fn verdict(p: &Pending, stats: Option<(FrameStats, QpcClock)>, refresh_qpc: u64) -> Verdict {
    if p.tearing {
        return match (p.gpu_done_ns, p.gpu_value) {
            (0, 0) => Verdict::Shown(p.submitted_ns, false),
            (0, _) => Verdict::Wait,
            (done, _) => Verdict::Shown(done, false),
        };
    }
    let Some((s, clock)) = stats else {
        return Verdict::Wait;
    };
    match p.count.cmp(&s.present_count) {
        std::cmp::Ordering::Greater => Verdict::Wait,
        std::cmp::Ordering::Less => Verdict::Unshown,
        std::cmp::Ordering::Equal => {
            let behind = s.sync_refresh_count.saturating_sub(s.present_refresh_count);
            let back = i64::from(behind).saturating_mul(i64::try_from(refresh_qpc).unwrap_or(0));
            let qpc = (behind == 0 || back > 0).then(|| s.sync_qpc.saturating_sub(back));
            match qpc.and_then(|q| clock.session_ns(q)) {
                Some(ns) => Verdict::Shown(ns, behind == 0),
                None => Verdict::Dropped,
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use waiter::Glass;

#[cfg(windows)]
mod waiter {
    use super::{refresh_span, stats_to_samples, ticks_ns, FrameStats, Pending, QpcClock};
    use crate::d3d::OwnedHandle;
    use crate::glass::{lifecycle_step, Act, Event, Lifecycle};
    use crate::vk::PresentedSample;
    use anyhow::{bail, Context as _, Result};
    use pf_client_core::session::now_ns;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;
    use windows::Win32::d3d11::ID3D11Fence;
    use windows::Win32::dxgi::{IDXGISwapChain2, DXGI_FRAME_STATISTICS};
    use windows::Win32::handleapi::DuplicateHandle;
    use windows::Win32::processthreadsapi::GetCurrentProcess;
    use windows::Win32::profileapi::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::synchapi::{CreateEventW, SetEvent, WaitForMultipleObjects};
    use windows::Win32::winnt::{DUPLICATE_SAME_ACCESS, HANDLE};

    /// The sleep between reads while presents are pending: a quarter of a 240 Hz refresh, so
    /// each present on glass is read before a successor replaces it.
    const POLL: Duration = Duration::from_millis(1);
    /// Refresh spans per published median, as the vblank waiter.
    const SPANS: usize = 16;
    /// `WaitForMultipleObjects` answers the index of the handle that signalled.
    const STOP: u32 = 0;
    const SIGNAL: u32 = 1;
    const GPU: u32 = 2;
    const INFINITE: u32 = u32::MAX;

    /// What the waiter and the presenter thread share.
    struct Shared {
        ledger: Mutex<Ledger>,
        /// Frame-latency signals the waiter took that `Swapchain::ready` has not.
        slots: Arc<AtomicU32>,
        /// The last read returned statistics.
        stats: AtomicBool,
        /// The published refresh in ns; 0 until measured.
        refresh_ns: AtomicU64,
        wake: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl Shared {
        fn ledger(&self) -> MutexGuard<'_, Ledger> {
            self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    #[derive(Default)]
    struct Ledger {
        /// Claimed presents, oldest first.
        pending: Vec<Pending>,
        samples: Vec<PresentedSample>,
        unshown: u32,
        /// The last statistics read; `None` after a restart or a read without them.
        prev: Option<FrameStats>,
        spans: Vec<u64>,
        /// The published refresh in QPC ticks; 0 until measured.
        refresh_qpc: u64,
    }

    /// What the waiter reads, moved to its thread.
    struct Waits {
        chain: IDXGISwapChain2,
        /// `None` without D3D11 fences: no present is waited.
        fence: Option<ID3D11Fence>,
        /// Stop, the frame-latency object, the GPU-done event, all owned by [`Glass`].
        handles: [HANDLE; 3],
        freq: u64,
    }

    // SAFETY: DXGI objects and D3D11 device children are free-threaded with atomic reference
    // counts; the waiter calls only `GetFrameStatistics` and `GetCompletedValue`, queries that
    // send no window message. The handles are valid on any thread while open, and
    // `Glass::drop` joins the thread before they close.
    unsafe impl Send for Waits {}

    /// The waiter thread and the lifecycle of the chain it reads.
    pub(crate) struct Glass {
        shared: Arc<Shared>,
        state: Lifecycle,
        /// The chain's size at the last claim; a claim at another size follows a resize.
        extent: (u32, u32),
        generation: u64,
        stop: OwnedHandle,
        _waitable: OwnedHandle,
        gpu: OwnedHandle,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Glass {
        /// The waiter over `chain`'s frame-latency object `waitable` (its own duplicate),
        /// `fence`'s completions, if any, and the chain's statistics. `slots` is the count
        /// `Swapchain::ready` takes from; the first signal is taken here, so the first present
        /// never races the thread's start.
        pub(crate) fn spawn(
            chain: IDXGISwapChain2,
            waitable: HANDLE,
            fence: Option<ID3D11Fence>,
            slots: Arc<AtomicU32>,
            extent: (u32, u32),
        ) -> Result<Glass> {
            let mut dup: HANDLE = std::ptr::null_mut();
            let mut freq = 0i64;
            // SAFETY: `waitable` is the swapchain's open handle; the duplicate and both events
            // are owned by the `OwnedHandle`s built below, which close each once. QPC writes
            // the local `freq`.
            let (dup_ok, stop, gpu) = unsafe {
                let process = GetCurrentProcess();
                let ok = DuplicateHandle(
                    process,
                    waitable,
                    process,
                    &mut dup,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS as u32,
                )
                .as_bool();
                let _ = QueryPerformanceFrequency(std::ptr::from_mut(&mut freq).cast());
                let event = || CreateEventW(None, false, false, windows::core::PCWSTR::null());
                (ok, OwnedHandle(event()), OwnedHandle(event()))
            };
            let waitable = OwnedHandle(if dup_ok { dup } else { std::ptr::null_mut() });
            if waitable.0.is_null() || stop.0.is_null() || gpu.0.is_null() {
                bail!("create the glass waiter's handles");
            }
            let shared = Arc::new(Shared {
                ledger: Mutex::default(),
                slots,
                stats: AtomicBool::new(false),
                refresh_ns: AtomicU64::new(0),
                wake: Mutex::new(None),
            });
            // SAFETY: a zero-timeout poll of the duplicate just made.
            if unsafe { WaitForMultipleObjects(&[waitable.0], false, 0) } == 0 {
                shared.slots.fetch_add(1, Ordering::AcqRel);
            }
            let waits = Waits {
                chain,
                fence,
                handles: [stop.0, waitable.0, gpu.0],
                freq: u64::try_from(freq).unwrap_or(0),
            };
            let thread_shared = shared.clone();
            let join = std::thread::Builder::new()
                .name("pf-dxgi-glass".into())
                .spawn(move || {
                    // The GPU-done stamp is taken at wake; scheduler delay would read as ours.
                    pf_client_core::audio_rt::boost_and_log("dxgi-glass");
                    run(&thread_shared, &waits);
                })
                .context("spawn pf-dxgi-glass")?;
            let mut glass = Glass {
                shared,
                state: Lifecycle::Unarmed { stamps: true },
                extent,
                generation: 0,
                stop,
                _waitable: waitable,
                gpu,
                join: Some(join),
            };
            glass.step(Event::Created(0));
            Ok(glass)
        }

        /// Step the lifecycle and perform its act: a retired chain's pending presents count
        /// as unshown, and its refresh baseline goes.
        fn step(&mut self, e: Event) {
            let (next, act) = lifecycle_step(self.state, e);
            self.state = next;
            let mut l = self.shared.ledger();
            match act {
                Act::Drain => {
                    l.unshown += l.pending.len() as u32;
                    l.pending.clear();
                }
                Act::Disarm => {
                    l.prev = None;
                    l.spans.clear();
                    l.refresh_qpc = 0;
                    self.shared.refresh_ns.store(0, Ordering::Relaxed);
                }
                Act::Arm(_) | Act::Keep => {}
            }
        }

        /// Claim the present just made at `extent`. A new size means the chain was resized
        /// since the last claim, which retires it first.
        pub(crate) fn claim(&mut self, p: Pending, extent: (u32, u32)) {
            if extent != self.extent {
                self.extent = extent;
                self.generation += 1;
                self.step(Event::Retiring);
                self.step(Event::Drained);
                self.step(Event::Created(self.generation));
            }
            self.shared.ledger().pending.push(p);
        }

        /// The event the fence sets for the waiter as a claimed present's work completes.
        pub(crate) fn gpu_event(&self) -> HANDLE {
            self.gpu.0
        }

        /// The last read returned statistics.
        pub(crate) fn stats_arrive(&self) -> bool {
            self.shared.stats.load(Ordering::Relaxed)
        }

        /// The median vblank spacing the statistics show, once measured.
        pub(crate) fn refresh_ns(&self) -> Option<u64> {
            Some(self.shared.refresh_ns.load(Ordering::Relaxed)).filter(|&ns| ns > 0)
        }

        pub(crate) fn outstanding(&self) -> usize {
            self.shared.ledger().pending.len()
        }

        pub(crate) fn take_unshown(&self) -> u32 {
            std::mem::take(&mut self.shared.ledger().unshown)
        }

        pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
            std::mem::take(&mut self.shared.ledger().samples)
        }

        pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
            *self
                .shared
                .wake
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(cb);
        }
    }

    impl Drop for Glass {
        fn drop(&mut self) {
            // SAFETY: `stop` is this value's open event; its fields close only after the join.
            let _ = unsafe { SetEvent(self.stop.0) };
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Take each frame-latency signal, read after every wake, and wake the run loop on a
    /// signal or a settled present. While presents are pending it reads every [`POLL`];
    /// otherwise it sleeps until a handle signals.
    fn run(shared: &Shared, w: &Waits) {
        loop {
            let pending = !shared.ledger().pending.is_empty();
            // SAFETY: the handles stay open until `Glass::drop` has joined this thread.
            let r = unsafe {
                WaitForMultipleObjects(&w.handles, false, if pending { 0 } else { INFINITE })
            };
            let mut wake = false;
            match r {
                STOP => return,
                SIGNAL => {
                    shared.slots.fetch_add(1, Ordering::AcqRel);
                    wake = true;
                }
                GPU => {}
                // A timeout, or a failed wait: sleep, never spin.
                _ => std::thread::sleep(POLL),
            }
            wake |= read(shared, w);
            if wake {
                if let Some(cb) = shared
                    .wake
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    cb();
                }
            }
        }
    }

    /// Stamp finished GPU work, read the statistics, settle what they show and measure the
    /// refresh. `true` when a present settled.
    fn read(shared: &Shared, w: &Waits) -> bool {
        let done = match &w.fence {
            // SAFETY: a COM query on the fence `w` holds.
            Some(f) => unsafe { f.GetCompletedValue() },
            None => 0,
        };
        let gpu_ns = now_ns();
        let mut raw = DXGI_FRAME_STATISTICS::default();
        // SAFETY: a COM query on the chain `w` holds, writing the local `raw`.
        let read = unsafe { w.chain.GetFrameStatistics(&mut raw) }.is_ok();
        // SAFETY: every bit pattern of the union is a valid `i64`.
        let sync_qpc = unsafe { raw.SyncQPCTime.QuadPart };
        let stats = (read && raw.PresentCount != 0 && sync_qpc != 0).then_some(FrameStats {
            present_count: raw.PresentCount,
            present_refresh_count: raw.PresentRefreshCount,
            sync_refresh_count: raw.SyncRefreshCount,
            sync_qpc,
        });
        shared.stats.store(stats.is_some(), Ordering::Relaxed);
        let mut qpc = 0i64;
        let before = now_ns();
        // SAFETY: writes the local `qpc`.
        let _ = unsafe { QueryPerformanceCounter(std::ptr::from_mut(&mut qpc).cast()) };
        let now = now_ns();
        let clock = QpcClock::calibrate(w.freq, before, qpc, now);
        let mut l = shared.ledger();
        for p in l.pending.iter_mut() {
            if p.gpu_value != 0 && p.gpu_value <= done && p.gpu_done_ns == 0 {
                p.gpu_done_ns = gpu_ns;
            }
        }
        if let Some(span) = l.prev.zip(stats).and_then(|(a, b)| refresh_span(a, b)) {
            l.spans.push(span);
            if l.spans.len() == SPANS {
                l.spans.sort_unstable();
                l.refresh_qpc = l.spans[SPANS / 2];
                l.spans.clear();
                let ns = ticks_ns(i128::from(l.refresh_qpc), w.freq);
                let ns = ns.and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
                shared.refresh_ns.store(ns, Ordering::Relaxed);
            }
        }
        l.prev = stats;
        let settled = stats_to_samples(&l.pending, stats.zip(clock), l.refresh_qpc, now);
        l.unshown += settled.unshown;
        l.pending.drain(..settled.count);
        l.samples
            .extend(
                settled
                    .shown
                    .into_iter()
                    .map(|(p, displayed_ns, exact)| PresentedSample {
                        present_id: u64::from(p.count),
                        pts_ns: p.pts_ns,
                        decoded_ns: p.decoded_ns,
                        submitted_ns: p.submitted_ns,
                        gpu_done_ns: p.gpu_done_ns,
                        displayed_ns,
                        exact,
                    }),
            );
        settled.count > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz QPC, the session clock 1 s ahead: tick t reads as t × 100 ns + 1 s.
    const CLOCK: QpcClock = QpcClock {
        freq: 10_000_000,
        offset_ns: 1_000_000_000,
    };
    const SUBMITTED: u64 = 1_000_000_000;
    const NOW: u64 = SUBMITTED + 100_000_000;

    fn fifo(count: u32) -> Pending {
        Pending {
            count,
            submitted_ns: SUBMITTED,
            ..Default::default()
        }
    }

    fn tearing(count: u32, gpu_value: u64, gpu_done_ns: u64) -> Pending {
        Pending {
            tearing: true,
            gpu_value,
            gpu_done_ns,
            ..fifo(count)
        }
    }

    fn stats(count: u32, present: u32, sync: u32, qpc: i64) -> Option<(FrameStats, QpcClock)> {
        let s = FrameStats {
            present_count: count,
            present_refresh_count: present,
            sync_refresh_count: sync,
            sync_qpc: qpc,
        };
        Some((s, CLOCK))
    }

    type Row = (
        &'static str,
        Vec<Pending>,
        Option<(FrameStats, QpcClock)>,
        u64,
        u64,
        Vec<(u32, u64, bool)>,
        u32,
        usize,
    );

    #[test]
    fn statistics_settle_the_presents_they_name() {
        // (name, pending, stats, refresh_qpc, now, shown (count, displayed_ns, exact),
        //  unshown, settled)
        let rows: Vec<Row> = vec![
            (
                "the named present is shown at its vblank, the offset applied once",
                vec![fifo(5)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![(5, 1_005_000_000, true)],
                0,
                1,
            ),
            (
                "a present newer than the statistics waits",
                vec![fifo(6)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![],
                0,
                0,
            ),
            (
                "a gap in the count marks the skipped presents unshown",
                vec![fifo(3), fifo(4), fifo(5)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![(5, 1_005_000_000, true)],
                2,
                3,
            ),
            (
                "presents after the named one wait",
                vec![fifo(5), fifo(6)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![(5, 1_005_000_000, true)],
                0,
                1,
            ),
            (
                "a later vblank's stamp steps back by the measured refresh",
                vec![fifo(5)],
                stats(5, 100, 102, 1_000_000),
                166_667,
                NOW,
                vec![(5, 1_066_666_600, false)],
                0,
                1,
            ),
            (
                "a later vblank's stamp without a measured refresh leaves no sample",
                vec![fifo(5)],
                stats(5, 100, 102, 1_000_000),
                0,
                NOW,
                vec![],
                0,
                1,
            ),
            (
                "without statistics a vblank-bound present waits",
                vec![fifo(5)],
                None,
                0,
                NOW,
                vec![],
                0,
                0,
            ),
            (
                "a tearing present is shown when its GPU work is done",
                vec![tearing(5, 9, 1_002_000_000)],
                None,
                0,
                NOW,
                vec![(5, 1_002_000_000, false)],
                0,
                1,
            ),
            (
                "a tearing present not waited is shown at its present return",
                vec![tearing(5, 0, 0)],
                None,
                0,
                NOW,
                vec![(5, SUBMITTED, false)],
                0,
                1,
            ),
            (
                "a tearing present waits for its GPU work",
                vec![tearing(5, 9, 0)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![],
                0,
                0,
            ),
            (
                "a waiting present holds the ones behind it",
                vec![tearing(4, 9, 0), fifo(5)],
                stats(5, 100, 100, 50_000),
                0,
                NOW,
                vec![],
                0,
                0,
            ),
            (
                "a present past the stale cap settles with no sample",
                vec![fifo(6), fifo(7)],
                stats(5, 100, 100, 50_000),
                0,
                SUBMITTED + STALE_NS + 1,
                vec![],
                0,
                2,
            ),
        ];
        for (name, pending, s, refresh_qpc, now, shown, unshown, count) in rows {
            let got = stats_to_samples(&pending, s, refresh_qpc, now);
            let got_shown: Vec<_> = got
                .shown
                .iter()
                .map(|(p, ns, e)| (p.count, *ns, *e))
                .collect();
            assert_eq!(got_shown, shown, "{name}");
            assert_eq!(got.unshown, unshown, "{name}");
            assert_eq!(got.count, count, "{name}");
        }
    }

    #[test]
    fn qpc_ticks_read_on_the_session_clock() {
        // (name, freq, before_ns, qpc, after_ns, stamp, session_ns)
        let rows = [
            (
                "10 MHz",
                10_000_000,
                2_000_000_000,
                10_000_000,
                2_000_000_000,
                20_000_000,
                Some(3_000_000_000),
            ),
            (
                "19.2 MHz, the Snapdragon timer",
                19_200_000,
                5_000_000_000,
                19_200_000,
                5_000_000_000,
                28_800_000,
                Some(5_500_000_000),
            ),
            (
                "the read sits between the two session reads",
                10_000_000,
                1_000,
                0,
                3_000,
                0,
                Some(2_000),
            ),
            (
                "a stamp before the session epoch reads nothing",
                10_000_000,
                0,
                10_000_000,
                0,
                0,
                None,
            ),
            (
                "ticks past the session clock's range read nothing",
                10_000_000,
                0,
                0,
                0,
                i64::MAX,
                None,
            ),
        ];
        for (name, freq, before, qpc, after, stamp, want) in rows {
            let got =
                QpcClock::calibrate(freq, before, qpc, after).and_then(|c| c.session_ns(stamp));
            assert_eq!(got, want, "{name}");
        }
        assert_eq!(QpcClock::calibrate(0, 0, 0, 0), None, "a zero frequency");
    }

    #[test]
    fn the_refresh_is_the_vblank_spacing_between_reads() {
        let at = |sync: u32, qpc: i64| FrameStats {
            present_count: 1,
            present_refresh_count: sync,
            sync_refresh_count: sync,
            sync_qpc: qpc,
        };
        let rows = [
            (
                "one vblank apart",
                at(100, 1_000_000),
                at(101, 1_166_667),
                Some(166_667),
            ),
            (
                "three vblanks apart",
                at(100, 1_000_000),
                at(103, 1_500_000),
                Some(166_666),
            ),
            (
                "the same vblank",
                at(100, 1_000_000),
                at(100, 1_000_000),
                None,
            ),
            (
                "a counter that went back",
                at(100, 1_000_000),
                at(5, 1_500_000),
                None,
            ),
        ];
        for (name, prev, now, want) in rows {
            assert_eq!(refresh_span(prev, now), want, "{name}");
        }
    }
}
