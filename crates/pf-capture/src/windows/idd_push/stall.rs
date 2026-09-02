//! Capture-stall detection for IDD-push: holes in DWM frame delivery while
//! the desktop was composing.
//!
//! [`StallWatch`] gates on recent active flow, classifies each hole from
//! driver telemetry + probes + ETW, and feeds a metronome so periodic stalls
//! self-diagnose. Damage-idle holes (still cursor on a dwm-only desktop) are
//! real delivery gaps but are excluded from the beat.
//!
//! Evidence: `design/` disturbance-immunity verdict matrix. Tests sit with
//! the capturer's stall suite.

use super::*;

/// A hole in DWM delivery that opened after recent active compose ([`StallWatch`]).
///
/// The metronome is not fed here. [`StallWatch::report`] feeds it after
/// classification so a damage-idle hole never advances the display-hardware beat.
pub(super) struct Stall {
    pub(super) gap: Duration,
}

/// One degraded stretch, closed by [`StallWatch::take_recovery`].
///
/// Per-hole stall lines gate on prior active flow, so a sustained slow phase
/// logs only the first hole; this summary is the stretch's remaining line.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Recovery {
    pub(super) degraded: Duration,
    /// Stall-sized holes (≥ [`StallWatch::STALL_MIN`]).
    pub(super) holes: u32,
    pub(super) hole_time: Duration,
    pub(super) worst: Duration,
}

/// Open degraded stretch; closed into [`Recovery`].
struct Episode {
    started: Instant,
    last_hole_end: Instant,
    holes: u32,
    hole_time: Duration,
    worst: Duration,
}

/// Driver telemetry for one stall window (v2 header tail:
/// `pf_driver_proto::frame::SharedHeader`), sampled between the last pre-gap
/// frame and the frame that ended the stall.
pub(super) struct StallEvidence {
    /// Delta of `offered_total` in the window. `None` = pre-telemetry driver (no tail).
    pub(super) offered_delta: Option<u64>,
    /// Max `now − heartbeat` over the window, in milliseconds.
    pub(super) max_heartbeat_age_ms: u64,
    pub(super) probes: Option<ProbeWindow>,
    /// DxgKrnl DDI summary for the window. `None` when the ETW session is unavailable.
    pub(super) etw: Option<String>,
    /// Present-vs-queue counts ([`EtwWatch::window_report`]). Presents flowing
    /// while the queue starves = OS dropped composed frames; both silent =
    /// content stopped. `None` when the ETW session is unavailable.
    pub(super) etw_counts: Option<super::dxgkrnl_etw::EtwWindowCounts>,
    /// Cursor travel during the hole (px, |dx|+|dy|). `Some(0)` = nothing to
    /// compose (damage-idle); `Some(n>0)` = damage existed and DWM composed
    /// none of it. `None` = never sampled. The stall-ending frame's own move
    /// is not counted (capturer fold-on-next-call sampler).
    pub(super) cursor_moved_px: Option<u32>,
}

/// Per-leg maxima from `probes::ProbeEngine::window` across one stall.
/// Every field is `None` when that probe is absent; absence is never guessed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ProbeWindow {
    pub(super) fence_max_us: Option<u64>,
    /// Longest span with no `DwmGetCompositionTimingInfo` `cRefresh` advance (µs).
    pub(super) dwm_tick_frozen_us: Option<u64>,
    /// Longest span with no `cFrame` advance (µs). Advisory only: on Win11
    /// `DWM_TIMING_INFO.cFrame` is refresh-synthesized and ticks without
    /// composes. [`classify`] ignores it.
    pub(super) dwm_frame_frozen_us: Option<u64>,
    pub(super) dwm_flush_max_us: Option<u64>,
    /// Worst `D3DKMTGetScanLine` call latency (µs). Blocking here convicts the KMD.
    pub(super) scanline_max_us: Option<u64>,
    /// Physical head present. Exclusive topology leaves only our IDD; latency
    /// still counts, scanline values do not.
    pub(super) scanline_physical: bool,
    /// Worst high-res sleeper overshoot (µs). DPC-storm / CPU-starvation discriminator.
    pub(super) cpu_max_overshoot_us: Option<u64>,
}

impl std::fmt::Display for ProbeWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |v: Option<u64>| match v {
            Some(us) => format!("{:.0}ms", us as f64 / 1_000.0),
            None => "absent".to_string(),
        };
        write!(
            f,
            "fence={} dwm_tick_frozen={} dwm_frames_frozen={} dwm_flush={} scanline={}({}) \
             cpu_overshoot={}",
            ms(self.fence_max_us),
            ms(self.dwm_tick_frozen_us),
            ms(self.dwm_frame_frozen_us),
            ms(self.dwm_flush_max_us),
            ms(self.scanline_max_us),
            if self.scanline_physical {
                "physical"
            } else {
                "virtual"
            },
            ms(self.cpu_max_overshoot_us),
        )
    }
}

/// Class a stall's combined evidence supports: [`attribute`]'s driver verdict
/// refined by the probe window. Verdict matrix: `design/` (disturbance immunity).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum StallClass {
    /// Drain worker starved (CPU / MMCSS / dead WUDFHost).
    OursWorker,
    /// Composed and offered, never consumable (ring / publish / consume).
    OursDelivery,
    /// Engine-liveness fences stalled with the hole: the adapter froze
    /// (link train, power transition, mux).
    AdapterFreeze,
    /// Engines alive, DWM tick frozen (DDC / vendor lock / win32k display-config queue).
    CompositorBlocked,
    /// No swapchain presents from any process across the hole. A one-off is
    /// a hitch; a frozen presenter looks the same. Probes run at the host's
    /// elevated GPU priority, so normal-band starvation reads healthy.
    /// Repeated holes under load do not exonerate the display path.
    ContentSilence,
    /// Presents flowed through the hole while the virtual display's kernel queue
    /// (`BltQueueAddEntry`) starved: OS dropped composed frames before our swap-chain.
    FrameGeneration,
    /// Content-silence with dwm.exe-only flow and a still cursor: nothing was dirty.
    /// An input/hand pause, not a display stall. Excluded from the metronome and
    /// both repeated-stall warns.
    DamageIdle,
    /// Pre-telemetry driver and/or probes absent.
    Unattributed,
}

impl std::fmt::Display for StallClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OursWorker => "OURS-worker (drain thread starved)",
            Self::OursDelivery => "OURS-delivery (ring/publish/consume lost composed frames)",
            Self::AdapterFreeze => {
                "CLASS-1 adapter freeze (engines stalled below the OS — link/power/mux servicing)"
            }
            Self::CompositorBlocked => {
                "CLASS-2 compositor blocked (engines alive, DWM tick frozen — vendor lock / DDC)"
            }
            Self::ContentSilence => {
                "CONTENT-SILENCE (no swapchain presents from any process across the hole — the content stopped presenting; a one-off is a game hitch/menu, but REPEATED holes under load can equally be the display stack freezing the presenter)"
            }
            Self::FrameGeneration => {
                "FRAME-GENERATION (presents FLOWED while the virtual display's kernel queue starved — the OS display path dropped composed frames)"
            }
            Self::DamageIdle => {
                "DAMAGE-IDLE (dwm-only flow and the cursor sat still through the hole — nothing was dirty, so DWM correctly composed nothing; an input/hand pause, not a display stall)"
            }
            Self::Unattributed => "UNATTRIBUTED (insufficient telemetry)",
        })
    }
}

/// Driver GPU-priority lever (`PFVD_NO_RT_GPU` opt-out; default REALTIME).
/// Reads this process's env; WUDFHost resolves the same variable. An env
/// edited after either process started is stale until restart. Rides every
/// stall-triage line so an A/B is interpretable; lowering priority masks
/// this class at most.
pub(super) fn rt_gpu_driver_posture() -> &'static str {
    if std::env::var_os("PFVD_NO_RT_GPU").is_some() {
        "off (PFVD_NO_RT_GPU)"
    } else {
        "REALTIME (default)"
    }
}

pub(super) fn rt_gpu_host_posture() -> &'static str {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => "off",
        Some("normal") => "normal",
        Some("high") => "high",
        _ => "REALTIME (default)",
    }
}

/// Presents across the hole that acquit the content. Matches [`attribute`]'s
/// offered-frames bar and [`StallWatch::RECENT`]: a caret blink or stall-ending
/// frame stays under 8; a game presenting through the hole clears it.
const PRESENTS_ACQUIT_CONTENT: u32 = 8;

/// Fold driver verdict, probe window, and ETW present-vs-queue counts into a
/// class. A leg covers the hole when its worst reading is ≥ half the gap
/// (same bar as [`attribute`]).
///
/// Compose-silence splits on ETW only (`DWM_TIMING_INFO.cFrame` is
/// refresh-synthesized — [`ProbeWindow::dwm_frame_frozen_us`]): presents
/// flowing = FRAME-GENERATION; none = CONTENT-SILENCE. No witness stays
/// UNATTRIBUTED.
pub(super) fn classify(
    gap: Duration,
    verdict: &StallVerdict,
    probes: Option<&ProbeWindow>,
    etw_counts: Option<&super::dxgkrnl_etw::EtwWindowCounts>,
    cursor_moved_px: Option<u32>,
) -> StallClass {
    match verdict {
        StallVerdict::WorkerStalled => return StallClass::OursWorker,
        StallVerdict::DeliveryLeg => return StallClass::OursDelivery,
        StallVerdict::ComposeSilence | StallVerdict::NoTelemetry => {}
    }
    let Some(p) = probes else {
        return StallClass::Unattributed;
    };
    let half_gap_us = (gap.as_micros() as u64) / 2;
    let covers = |v: Option<u64>| v.is_some_and(|us| us >= half_gap_us);
    if covers(p.fence_max_us) {
        return StallClass::AdapterFreeze;
    }
    if covers(p.dwm_tick_frozen_us) || covers(p.dwm_flush_max_us) {
        return StallClass::CompositorBlocked;
    }
    // Only the driver's E_PENDING pins silence on the present path. Without
    // it (pre-telemetry) the delivery leg is equally possible.
    if matches!(verdict, StallVerdict::ComposeSilence) {
        match etw_counts {
            Some(c) if c.present_history => {
                if c.presents >= PRESENTS_ACQUIT_CONTENT {
                    StallClass::FrameGeneration
                } else if c.flow_dwm_only && cursor_moved_px == Some(0) {
                    // dwm-only + still cursor: nothing to compose (input pause).
                    // A moved cursor stays CONTENT-SILENCE. Both required: a
                    // game is never demoted; a missing witness (`None`) is not
                    // treated as still.
                    StallClass::DamageIdle
                } else {
                    StallClass::ContentSilence
                }
            }
            // No present witness (session refused / DXGI enable failed /
            // renumbered events): silence cannot be pinned to either side.
            _ => StallClass::Unattributed,
        }
    } else {
        StallClass::Unattributed
    }
}

/// Attribution from driver telemetry alone, before the probe/ETW matrix.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StallVerdict {
    /// Pre-telemetry driver: host observation only.
    NoTelemetry,
    /// Drain heartbeat silent for a large share of the hole (CPU/MMCSS or dead WUDFHost).
    WorkerStalled,
    /// Worker drained E_PENDING: DWM composed nothing. Below capture; probes + ETW split it.
    ComposeSilence,
    /// Frames composed and offered, none consumable — our publish/ring/consume leg.
    DeliveryLeg,
}

impl std::fmt::Display for StallVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoTelemetry => "pre-telemetry driver (no verdict)",
            Self::WorkerStalled => "driver-worker-stalled (heartbeat silent) — host CPU/MMCSS or a dead WUDFHost, NOT the display path",
            Self::ComposeSilence => "compose-silence (driver drained E_PENDING) — DWM composed nothing; the disturbance is below capture",
            Self::DeliveryLeg => "delivery-leg (frames were composed + offered but never consumable) — OUR ring/publish/consume leg",
        })
    }
}

/// Fold one stall window into a [`StallVerdict`].
///
/// Heartbeat ever `max(gap/2, 250 ms)` stale convicts the worker (cadence is
/// ≤16 ms, so 250 ms is starvation; gap/2 scales long holes). `offered_delta
/// ≥ 8` acquits DWM — same sustained-flow bar as [`StallWatch::RECENT`].
pub(super) fn attribute(gap: Duration, evidence: &StallEvidence) -> StallVerdict {
    let Some(offered) = evidence.offered_delta else {
        return StallVerdict::NoTelemetry;
    };
    let gap_ms = gap.as_millis() as u64;
    if evidence.max_heartbeat_age_ms >= (gap_ms / 2).max(250) {
        StallVerdict::WorkerStalled
    } else if offered >= 8 {
        StallVerdict::DeliveryLeg
    } else {
        StallVerdict::ComposeSilence
    }
}

/// Capture-stall watch. A hole counts only when [`Self::RECENT`] pre-gap
/// frames all fit in [`Self::ACTIVE_SPAN`] — an idle desktop goes quiet
/// with no damage.
///
/// Reported stalls feed a [`pf_frame::metronome::Metronome`] after
/// classification so periodic DWM holes self-diagnose. Encode/network
/// causes stay with the recovery-cadence detector. The caller logs.
pub(super) struct StallWatch {
    /// Last [`Self::RECENT`] fresh-frame instants — activity-gate history.
    recent: std::collections::VecDeque<Instant>,
    cadence: pf_frame::metronome::Metronome,
    /// Session stall count and how many carried a coinciding OS display event.
    seen: u32,
    with_os_events: u32,
    /// Per-verdict counts in [`StallVerdict`] order. The metronomic WARN prints
    /// the session, not just the stall that tripped the beat.
    verdicts: [u32; 4],
    /// Per-class counts in [`StallClass`] declaration order.
    classes: [u32; 8],
    /// Open stretch; every stall-sized hole feeds it until sustained flow returns.
    episode: Option<Episode>,
    pending_recovery: Option<Recovery>,
    /// Reported-stall instants in [`Self::RATE_WINDOW`]. The metronome needs a
    /// stable period; this arm covers aperiodic bursts.
    rate_window: std::collections::VecDeque<Instant>,
    /// Last rate-arm WARN; spacing is [`Self::RATE_REWARN`].
    last_rate_warn: Option<Instant>,
}

impl StallWatch {
    /// Pre-gap frames that must be tight for active flow. Stalls are then spaced
    /// ≥ this many frame times — no extra log rate limit.
    const RECENT: usize = 8;
    /// Span the RECENT pre-gap frames must fit. 8 frames in 400 ms is 7 intervals
    /// ≈ 17.5 fps: a 30 fps-capped game passes; idle-desktop damage does not.
    const ACTIVE_SPAN: Duration = Duration::from_millis(400);
    /// Smallest stall hole. ~9 missed frames at 60 Hz; above encode/present jitter.
    const STALL_MIN: Duration = Duration::from_millis(150);
    /// Hole that is a content stop, not a stretch. Closes an open episode first so
    /// a quit-to-idle pause never folds into the tally.
    const EPISODE_BREAK: Duration = Duration::from_secs(10);
    /// Below this, the episode dissolves; the single stall's report already covers it.
    const EPISODE_MIN_HOLES: u32 = 2;
    /// Window for the aperiodic repeated-stall WARN.
    const RATE_WINDOW: Duration = Duration::from_secs(60);
    /// Stalls in [`Self::RATE_WINDOW`] that trip the rate WARN. Above a busy
    /// desktop's ~1/min; under a degraded session's dozens.
    const RATE_MIN_STALLS: usize = 3;
    /// Rate-arm re-WARN spacing. Metronomic arms pace via the metronome.
    const RATE_REWARN: Duration = Duration::from_secs(300);

    pub(super) fn new() -> Self {
        Self {
            recent: std::collections::VecDeque::with_capacity(Self::RECENT + 1),
            cadence: pf_frame::metronome::Metronome::new(),
            seen: 0,
            with_os_events: 0,
            verdicts: [0; 4],
            classes: [0; 8],
            episode: None,
            pending_recovery: None,
            rate_window: std::collections::VecDeque::new(),
            last_rate_warn: None,
        }
    }

    /// Record a reported stall at `now`. `Some(count)` when the rate WARN is due
    /// (≥ [`Self::RATE_MIN_STALLS`] in [`Self::RATE_WINDOW`], [`Self::RATE_REWARN`]
    /// spacing).
    pub(super) fn note_for_rate_warn(&mut self, now: Instant) -> Option<usize> {
        self.rate_window.push_back(now);
        while let Some(front) = self.rate_window.front() {
            if now.duration_since(*front) > Self::RATE_WINDOW {
                self.rate_window.pop_front();
            } else {
                break;
            }
        }
        if self.rate_window.len() < Self::RATE_MIN_STALLS {
            return None;
        }
        if self
            .last_rate_warn
            .is_some_and(|t| now.duration_since(t) < Self::RATE_REWARN)
        {
            return None;
        }
        self.last_rate_warn = Some(now);
        Some(self.rate_window.len())
    }

    /// Per-verdict log token. Array is [`StallVerdict`] order; the string prints
    /// worker, compose-silence, delivery-leg, no-telemetry.
    fn verdict_tally(&self) -> String {
        format!(
            "worker-stalled {}, compose-silence {}, delivery-leg {}, no-telemetry {}",
            self.verdicts[1], self.verdicts[2], self.verdicts[3], self.verdicts[0]
        )
    }

    /// Per-class log token in [`StallClass`] order.
    fn class_tally(&self) -> String {
        format!(
            "ours-worker {}, ours-delivery {}, adapter-freeze {}, compositor-blocked {}, \
             content-silence {}, frame-generation {}, damage-idle {}, unattributed {}",
            self.classes[0],
            self.classes[1],
            self.classes[2],
            self.classes[3],
            self.classes[4],
            self.classes[5],
            self.classes[6],
            self.classes[7]
        )
    }

    /// Drop flow history. A ring-recreate gap is self-inflicted; without this the
    /// first post-recreate frame reads as a stall. Open episodes still close —
    /// those holes predate the recreate.
    pub(super) fn reset(&mut self) {
        self.recent.clear();
        self.close_episode();
    }

    /// Close the open episode into [`Self::pending_recovery`] if past the noise bar.
    fn close_episode(&mut self) {
        if let Some(ep) = self.episode.take() {
            if ep.holes >= Self::EPISODE_MIN_HOLES {
                self.pending_recovery = Some(Recovery {
                    degraded: ep.last_hole_end.duration_since(ep.started),
                    holes: ep.holes,
                    hole_time: ep.hole_time,
                    worst: ep.worst,
                });
            }
        }
    }

    /// Take a closed stretch if one is waiting. Call after every
    /// [`Self::note_fresh`] / [`Self::reset`]: closure rides a non-stall frame.
    pub(super) fn take_recovery(&mut self) -> Option<Recovery> {
        self.pending_recovery.take()
    }

    /// Record a fresh driver frame at `now`. `Some` iff it ended a stall.
    pub(super) fn note_fresh(&mut self, now: Instant) -> Option<Stall> {
        let was_active = self.recent.len() == Self::RECENT
            && self
                .recent
                .back()
                .zip(self.recent.front())
                .is_some_and(|(b, f)| b.duration_since(*f) <= Self::ACTIVE_SPAN);
        let gap = self.recent.back().map(|last| now.duration_since(*last));
        self.recent.push_back(now);
        if self.recent.len() > Self::RECENT {
            self.recent.pop_front();
        }
        let gap = gap?;
        if gap >= Self::EPISODE_BREAK {
            // Content stopped (quit / long idle). Summarize the stretch; do not
            // fold a legitimate pause into the tally.
            self.close_episode();
        }
        if gap >= Self::STALL_MIN {
            match &mut self.episode {
                // Accumulate every stall-sized hole. The activity gate below
                // quiets per-hole reports (pre-gap spans the slow frames).
                Some(ep) => {
                    ep.holes += 1;
                    ep.hole_time += gap;
                    ep.worst = ep.worst.max(gap);
                    ep.last_hole_end = now;
                }
                None if was_active => {
                    self.episode = Some(Episode {
                        started: now - gap,
                        last_hole_end: now,
                        holes: 1,
                        hole_time: gap,
                        worst: gap,
                    });
                }
                None => {}
            }
        } else if was_active {
            // [`Self::RECENT`] tight frames: the stretch is over.
            self.close_episode();
        }
        if !was_active || gap < Self::STALL_MIN {
            return None;
        }
        // Metronome is fed in [`Self::report`] after classification. A
        // damage-idle hole must not advance the display-hardware beat.
        Some(Stall { gap })
    }

    /// Feed a classified stall into the metronome. `Some(mean period)` on a
    /// completed cycle. Damage-idle is not fed: an input pause on the user's
    /// cadence must not fabricate a display-disturbance beat.
    pub(super) fn cycle(&mut self, now: Instant, damage_idle: bool) -> Option<Duration> {
        if damage_idle {
            return None;
        }
        self.cadence.note(now)
    }
    /// Log a stall, correlate OS display events, and name the class once the
    /// cadence is metronomic.
    ///
    /// `now` is the frame that ended the stall (same instant as [`Self::note_fresh`])
    /// and bounds the event-correlation window. `evidence` is the capturer's
    /// sample for that window; [`attribute`] rides every stall line.
    pub(super) fn report(&mut self, stall: &Stall, now: Instant, evidence: &StallEvidence) {
        // Gap plus 300 ms lead-in: the causing OS event lands just before
        // DWM stops delivering.
        let window = stall.gap + Duration::from_millis(300);
        let events = now
            .checked_sub(window)
            .map(|from| pf_win_display::display_events::events_between(from, now))
            .unwrap_or_default();
        self.seen = self.seen.saturating_add(1);
        if !events.is_empty() {
            self.with_os_events = self.with_os_events.saturating_add(1);
        }
        let verdict = attribute(stall.gap, evidence);
        self.verdicts[match verdict {
            StallVerdict::NoTelemetry => 0,
            StallVerdict::WorkerStalled => 1,
            StallVerdict::ComposeSilence => 2,
            StallVerdict::DeliveryLeg => 3,
        }] += 1;
        let class = classify(
            stall.gap,
            &verdict,
            evidence.probes.as_ref(),
            evidence.etw_counts.as_ref(),
            evidence.cursor_moved_px,
        );
        self.classes[match class {
            StallClass::OursWorker => 0,
            StallClass::OursDelivery => 1,
            StallClass::AdapterFreeze => 2,
            StallClass::CompositorBlocked => 3,
            StallClass::ContentSilence => 4,
            StallClass::FrameGeneration => 5,
            StallClass::DamageIdle => 6,
            StallClass::Unattributed => 7,
        }] += 1;
        // Damage-idle is still a delivery hole (episode/recovery count it) but
        // not display-disturbance evidence: skip metronome, rate WARN, and
        // connected-inactive trial. The per-stall line still carries evidence.
        let damage_idle = class == StallClass::DamageIdle;
        let metronomic = self.cycle(now, damage_idle);
        // debug, not warn: a single hole is a legitimate content pause. The
        // reportable signal is the metronomic cycle below.
        tracing::debug!(
            gap_ms = stall.gap.as_millis() as u64,
            os_display_events = %pf_win_display::display_events::summarize(&events),
            verdict = %verdict,
            class = %class,
            probes = evidence.probes.as_ref().map(tracing::field::display),
            etw = evidence.etw.as_deref().unwrap_or("unavailable"),
            // Presents from any process vs virtual-display kernel-queue adds.
            // presents≥bar with adds≈0 = FRAME-GENERATION.
            etw_presents = evidence.etw_counts.map(|c| c.presents),
            etw_queue_adds = evidence.etw_counts.map(|c| c.queue_adds),
            // 0 on a dwm-only desktop = nothing to compose. >0 with no
            // presents = damage existed and DWM composed none of it.
            cursor_moved_px_during_gap = evidence.cursor_moved_px,
            flow_dwm_only = evidence.etw_counts.map(|c| c.flow_dwm_only),
            offered_during_gap = evidence.offered_delta,
            max_heartbeat_age_ms = evidence.max_heartbeat_age_ms,
            "IDD-push capture stall — the desktop was composing at speed, then the ring \
             delivered no frame for the gap; the class names the leg that lost them"
        );
        // Aperiodic 150+ ms holes still need the triage payload. Skip when
        // this stall completed a metronomic cycle (richer arms below) or is
        // damage-idle.
        if metronomic.is_none() && !damage_idle {
            if let Some(stalls_in_window) = self.note_for_rate_warn(now) {
                let suspects = pf_win_display::display_events::connected_inactive_physicals();
                let suspects = if suspects.is_empty() {
                    "none".to_string()
                } else {
                    suspects.join(", ")
                };
                tracing::warn!(
                    stalls_in_window = stalls_in_window as u64,
                    os_correlated = format!("{}/{}", self.with_os_events, self.seen),
                    connected_inactive = %suspects,
                    rt_gpu_driver = rt_gpu_driver_posture(),
                    rt_gpu_host = rt_gpu_host_posture(),
                    verdicts = %self.verdict_tally(),
                    classes = %self.class_tally(),
                    "capture stalls are REPEATING without a stable period — same triage as the \
                     metronomic class: a connected-but-inactive display's standby servicing \
                     (see connected_inactive), then display-poller software (the SteelSeries \
                     GG / SignalRGB class). Lowering the GPU-priority defaults \
                     (setx /M PFVD_NO_RT_GPU 1 / PUNKTFUNK_GPU_PRIORITY_CLASS=high) has \
                     quieted some AMD boxes but masks this class at most — attenuation, not \
                     attribution. A content-silence class tally does NOT exonerate the \
                     display stack — a frozen presenter reads identically (Flavor 3)"
                );
            }
        }
        if let Some(period) = metronomic {
            let suspects = pf_win_display::display_events::connected_inactive_physicals();
            let suspects = if suspects.is_empty() {
                "none".to_string()
            } else {
                suspects.join(", ")
            };
            let correlated = format!("{}/{}", self.with_os_events, self.seen);
            let verdict_tally = self.verdict_tally();
            let class_tally = self.class_tally();
            // ≥ half the stalls with a coinciding OS event: the cascade is
            // OS-visible. Otherwise it never surfaces above the driver.
            if self.with_os_events * 2 >= self.seen {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    verdicts = %verdict_tally,
                    classes = %class_tally,
                    "capture stalls are METRONOMIC and coincide with Windows monitor \
                     hot-plug/re-enumeration events — a connected display (or its \
                     cable/switch/AVR) re-probes the link on a timer and Windows re-reacts \
                     each time. Cures, best-first: that display's OSD 'auto input \
                     scan/detect' OFF (and on TVs: instant-on/quick-start + CEC off), \
                     unplug its cable at the GPU, an HPD-holding adapter/dummy plug, or \
                     keep it active while streaming; the pnp_disable_monitors policy axis \
                     suppresses the Windows-side reaction (see connected_inactive for the \
                     suspects)"
                );
            } else {
                // Both REALTIME GPU-priority levers (default on). The line
                // must say whether either is engaged so an A/B is interpretable.
                let rt_gpu_driver = rt_gpu_driver_posture();
                let rt_gpu_host = rt_gpu_host_posture();
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    rt_gpu_driver,
                    rt_gpu_host,
                    verdicts = %verdict_tally,
                    classes = %class_tally,
                    "capture stalls are METRONOMIC with NO coinciding OS display event — \
                     the disturbance is BELOW Windows (damage-idle holes — cursor \
                     stationary on a dwm-only desktop, i.e. input/hand pauses — are \
                     already excluded from this beat; see cursor_moved_px_during_gap on \
                     the per-stall lines). Suspects: the GPU driver servicing a \
                     connected-but-asleep sink (standby HPD/DDC/link probing), \
                     display-poller software (the SteelSeries-GG/SignalRGB class — \
                     correlate 'slow display-descriptor poll' lines), or the DWM present \
                     clock (try a different refresh rate). Lowering the GPU-priority \
                     defaults (setx /M PFVD_NO_RT_GPU 1 / \
                     PUNKTFUNK_GPU_PRIORITY_CLASS=high) has quieted some AMD boxes but \
                     masks this class at most — a quiet A/B is attenuation, not \
                     attribution. If connected_inactive lists a \
                     display, its standby servicing is a suspect — cursor motion through \
                     the holes is what convicts the display stack. For an external \
                     display: keep it active while streaming, disable its OSD auto input \
                     scan (TVs: instant-on/quick-start + CEC off), unplug it at the GPU, \
                     or use an HPD-holding adapter/dummy. For a LAPTOP PANEL: keep it \
                     active with `topology: primary` (the dark-but-connected-head \
                     hypothesis has no confirmed post-0.28 case — verify with the cursor \
                     witness before chasing it)"
                );
            }
        }
    }
}
