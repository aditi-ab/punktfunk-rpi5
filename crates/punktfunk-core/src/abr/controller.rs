//! The AIMD controller: one decision per report window.
//!
//! Severe windows (unrecoverable frame, flush, ≥6 % loss, deep decode or
//! encode rise, keyframe storm) back off ×0.7 immediately. Ordinary
//! congestion needs two consecutive bad windows. Recovery is slow start
//! (double, bounded by proven-throughput headroom) then additive (+~6 % after
//! ~4.5 s). Each change rebuilds the encoder (IDR); silence after
//! [`MAX_UNACKED`] unanswered requests.
//!
//! Caps are learned: two identical short host acks latch `host_cap_kbps`; two
//! similar decode-driven backoffs latch `decode_cap_kbps`, and so does decode
//! headroom on a clean window (park at 80 % of the frame budget, retreat a
//! notch at 90 %, each step kept only if the decoder's latency follows the
//! rate). Both re-probe on [`CAP_REPROBE_WINDOWS_MIN`]. Climbs require
//! utilization (delivered ≈ target) and stay within ×1.5 of the windowed
//! proven mark. Tests in this module pin the contract.

use super::sample::{self, WindowActivity, WindowSample};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Floor so a mis-measured window cannot crater the session. 2 Mbps: a thin
/// link is better served soft than lossy. First descent below
/// [`LOW_RATE_WARN_KBPS`] logs once.
const FLOOR_KBPS: u32 = 2_000;
/// One-shot quality warning. 5 Mbps was the old floor; riding under it is the
/// new territory worth flagging.
const LOW_RATE_WARN_KBPS: u32 = 5_000;
/// Fully-idle windows (every AU a host-marked repeat) before the next active
/// window re-arms slow start. 4 × 750 ms ≈ 3 s of stillness.
const IDLE_WINDOWS_TO_REARM: u32 = 4;
/// Fewest active frames before the fps-normalized utilization gate may climb.
/// Two stray frames would prorate the target to almost nothing.
const MIN_ACTIVE_FRAMES_TO_CLIMB: u32 = 4;
/// Windows per proven-throughput bucket (~30 s). The mark is the max of the
/// current and previous buckets so a regime minutes gone cannot license a
/// doubling.
const PROVEN_BUCKET_WINDOWS: u32 = 40;
/// Consecutive ordinary-bad windows before a decrease. One 750 ms window can
/// be a scheduler blip; 1.5 s is a condition. Severe skips the wait.
const BAD_WINDOWS_TO_DECREASE: u32 = 2;
/// Shard loss at which one window backs off. 6 % is past any retry tail;
/// 750 ms spent there is visible damage.
const SEVERE_LOSS_PPM: u32 = 60_000;
/// Clean windows before an additive climb (~4.5 s). Slow start ignores this
/// and doubles on every cooled clean window.
const CLEAN_WINDOWS_TO_INCREASE: u32 = 6;
/// Minimum gap between requests. Each accepted change rebuilds the encoder
/// and opens with an IDR; back-to-back steps outrun the ack RTT.
const CHANGE_COOLDOWN: Duration = Duration::from_millis(1500);
/// Shard loss that marks a window bad without an unrecoverable frame. 2 %
/// sustained is congestion, not the random tail FEC exists for.
const HEAVY_LOSS_PPM: u32 = 20_000;
/// Decode-recovery keyframe asks that mark a window bad. Two asks in 750 ms
/// means the decoder is overdriven, whatever `loss_ppm` says. RFI asks are
/// not counted — `loss_ppm` already prices them.
const RECOVERY_KF_BAD: u32 = 2;
/// Keyframe asks that make one window severe. Emitters throttle at 100 ms, so
/// 4+ in 750 ms means most of the window produced no pictures.
const RECOVERY_KF_SEVERE: u32 = 4;
/// One-way-delay rise above the rolling baseline that counts as queue growth.
/// 25 ms is far beyond jitter at any streamable frame rate.
const OWD_RISE_US: i64 = 25_000;
/// Decode-stage rise (received → decoded) that marks the decoder falling
/// behind, when no frame budget is known. With one, half a budget: the stage
/// includes the queue, so that is half a frame of standing backlog.
const DECODE_RISE_US: i64 = 15_000;
/// Severe decode rise without a frame budget. With one, 1.5 budgets: several
/// frames of backlog, and a second window is 750 ms more of visible damage.
const DECODE_SEVERE_US: i64 = 45_000;
/// Decode headroom, judged on clean full-rate windows against the frame
/// budget. Received → decoded includes the queue wait, so a mean near the
/// period is a decoder with no slack: the next jitter is a missed vsync. At
/// 80 % climbs park; at 90 % the rate retreats a notch, and the decoder's
/// answer decides whether the retreat stands.
const DECODE_HOLD_PCT: i64 = 80;
const DECODE_RETREAT_PCT: i64 = 90;
/// A retreat or a cap lift is answered when the decode mean moves by this
/// much of the budget over [`DECODE_VERDICT_WINDOWS`] clean windows at the
/// new rate. A pipelined decoder sits above the period with no queue; its
/// latency does not follow the rate, and the headroom driver stands down.
const DECODE_ANSWER_PCT: i64 = 5;
const DECODE_VERDICT_WINDOWS: u32 = 2;
/// Windows a step may wait for its new rate before the verdict is dropped.
const DECODE_PROBE_MAX_AGE: u32 = 16;
/// A window is full-rate for the headroom judgement at ≥ ¾ of the refresh's
/// frames. Fewer frames share the same bits, so a low-fps decode mean
/// overstates the full-rate load.
const DECODE_FULL_RATE_NUM: i64 = 3;
const DECODE_FULL_RATE_DEN: i64 = 4;
/// Climb credit requires `actual × DEN ≥ target × NUM` (¾ of target). Below
/// that the encoder was not constrained, so the window proves nothing.
const UTILIZATION_NUM: u64 = 3;
const UTILIZATION_DEN: u64 = 4;
/// Climb may step at most ×1.5 past proven throughput. Utilization guarantees
/// `proven ≥ ¾ × current`, so the two gates cannot deadlock.
const PROVEN_HEADROOM_NUM: u32 = 3;
const PROVEN_HEADROOM_DEN: u32 = 2;
/// Host-encode rise (`0xCF` `encode_us`) that marks the encoder past its
/// compute knee. Relative, not absolute: an escalated host inflates
/// `encode_us` by ~a frame of retrieve-queue. 4 ms ≈ half a 120 Hz frame
/// until [`BitrateController::set_frame_budget`] supplies the session budget.
const ENCODE_RISE_US: i64 = 4_000;
/// Host-encode rise that is severe (≈1.5 × a 120 Hz budget). Scaled with
/// [`ENCODE_RISE_US`] once a frame budget is known.
const ENCODE_SEVERE_US: i64 = 12_000;
/// Consecutive encode-attributed backoffs that did not bring host encode time
/// down, after which the encode down-driver stands down.
///
/// Encode time is treated as a function of rate. GPU contention breaks that,
/// and [`on_ack`](BitrateController::on_ack) re-seeds the baseline so the
/// ratchet never notices. Two no-ops — not one: a real knee still above the
/// rate looks like a single pair. Same shape as
/// [`crate::client::frame_channel::NOOP_CLOCK_FLUSHES_TO_DISARM`]. A clean
/// run re-arms on the [`CAP_REPROBE_WINDOWS_MIN`] ladder.
const ENCODE_NOOP_BACKOFFS_TO_DISARM: u32 = 2;
/// Clean windows parked at a learned cap before re-probing above it, and the
/// ceiling that interval backs off to.
///
/// A short ack means "not right now" — durable encoder ceiling or a transient
/// cadence refusal. The client cannot tell, so it probes again after 16
/// windows (~12 s) and doubles the interval each time the lift is immediately
/// re-learned. A still-standing limit re-teaches itself in two short acks
/// with no encoder rebuild. Decode cap uses the same clock.
const CAP_REPROBE_WINDOWS_MIN: u32 = 16;
const CAP_REPROBE_WINDOWS_MAX: u32 = 128;
/// Two decode-driven backoffs latch [`decode_cap_kbps`] only when their
/// pre-backoff rates agree within ±1/8. A cascade's second backoff sits at
/// ×0.7 of the first — outside the band by construction — so only a
/// climbed-to rate (`climb_since_backoff`) can sample the knee.
const DECODE_CAP_SIMILAR_DIV: u32 = 8;
/// A deciding window that delivered under `current / 4` is starved: the
/// stream barely flowed, so distress is stall-shaped, not rate-shaped. It
/// may still back off on what the client saw, but it must not sample a decode
/// knee and must not carry host-encode (`encode_us` averaged over almost no
/// AUs describes the interruption). Far below the ×¾ climb bar; the band
/// between them stays ambiguous on purpose.
const STARVED_DELIVERY_DIV: u32 = 4;
/// Rolling window (~30 s at 750 ms) whose minimum mean is the latency
/// baseline. Long enough to remember the uncongested floor.
const BASELINE_WINDOWS: usize = 40;
/// Samples a rolling-min baseline must hold before its signal may fire. One
/// sample *is* the min; a calm seed plus ordinary variance reads as rise.
/// [`on_ack`](BitrateController::on_ack) clears the encode baseline after
/// every decrease we asked for, so four windows (3 s) is the floor.
const BASELINE_MIN_WINDOWS: usize = 4;
/// Unacked [`crate::quic::SetBitrate`] requests before the host is treated as
/// predating renegotiation and the controller goes quiet.
const MAX_UNACKED: u32 = 3;

/// `PUNKTFUNK_ABR_MAX_MBPS` (megabits/second) caps the climb ceiling however
/// it is learned. [`set_ceiling`](BitrateController::set_ceiling) never
/// lowers, so one inflated probe is otherwise permanent.
/// `PUNKTFUNK_ABR_PROBE_KBPS` only shrinks the burst target, not the
/// conclusion. Unset/0/garbage → no cap. Read once, at construction.
fn ceiling_cap_from_env() -> Option<u32> {
    std::env::var("PUNKTFUNK_ABR_MAX_MBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&m| m > 0)
        .map(|m| m.saturating_mul(1_000))
}

/// Score one window's latency against its rolling-min baseline, then record it.
///
/// Shared by OWD, client decode, and host encode. `mean` is `None` when nobody
/// reports the signal — absent, not clean, so it neither marks bad nor teaches
/// a baseline. Compared against PRIOR windows before recording, and only after
/// [`BASELINE_MIN_WINDOWS`]. Pass `i64::MAX` for `severe_us` on a signal with
/// no severe tier.
fn score_baseline(
    means: &mut VecDeque<i64>,
    mean: Option<i64>,
    rise_us: i64,
    severe_us: i64,
) -> (bool, bool) {
    let Some(mean) = mean else {
        return (false, false);
    };
    let base = (means.len() >= BASELINE_MIN_WINDOWS)
        .then(|| means.iter().min().copied())
        .flatten();
    let over = |t: i64| base.is_some_and(|b| mean > b.saturating_add(t));
    if means.len() == BASELINE_WINDOWS {
        means.pop_front();
    }
    means.push_back(mean);
    (over(rise_us), over(severe_us))
}

/// A headroom step awaiting the decoder's answer at its new rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodeProbe {
    kind: DecodeProbeKind,
    /// Rate before the step; restored when the step proves nothing or hurts.
    from_kbps: u32,
    /// Decode mean at `from_kbps`.
    ref_us: i64,
    /// Clean full-rate windows at the new rate, and their decode sum.
    windows: u32,
    sum_us: i64,
    /// Windows since the step, at any rate. Bounds a lift the climb never took.
    age: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeProbeKind {
    Retreat,
    Lift,
}

/// One decision per report window; `Some(kbps)` = send a [`crate::quic::SetBitrate`].
pub(crate) struct BitrateController {
    /// `false` = permanently off (explicit bitrate, old host, or ack silence).
    enabled: bool,
    /// Host-acked encoder rate. Requests are not assumed applied.
    pub(super) current_kbps: u32,
    /// Climb ceiling: negotiated start until [`set_ceiling`](Self::set_ceiling)
    /// raises it from the startup probe.
    ceiling_kbps: u32,
    /// `PUNKTFUNK_ABR_MAX_MBPS` in kbps, injected so tests never touch the env.
    /// `None` = no cap.
    ceiling_cap_kbps: Option<u32>,
    /// [`stream_ceiling_kbps`] for this mode/codec. Bounds only what
    /// [`set_ceiling`](Self::set_ceiling) learns; the negotiated start stands.
    stream_cap_kbps: Option<u32>,
    floor_kbps: u32,
    /// Slow start until the first congestion signal.
    probing: bool,
    owd_means: VecDeque<i64>,
    /// Empty when the embedder does not report decode latency (signal absent).
    decode_means: VecDeque<i64>,
    /// Cleared on our own rate decrease ([`on_ack`](Self::on_ack)) and on a
    /// mode switch — the encode regime changed.
    encode_means: VecDeque<i64>,
    /// One refresh interval, µs. `None` = the 120 Hz [`ENCODE_RISE_US`] defaults.
    frame_budget_us: Option<i64>,
    /// Mean encode_us that drove the last encode-attributed backoff; `0` = none
    /// or the streak was broken by a backoff something else drove.
    encode_backoff_us: i64,
    encode_noop_backoffs: u32,
    /// Encode rises are not answering the rate. Lifted by a clean run or a
    /// mode switch — never permanent.
    pub(super) encode_disarmed: bool,
    encode_disarm_clean_windows: u32,
    /// Clean windows the stand-down must survive. Doubles each time a re-armed
    /// signal is immediately silenced again.
    encode_reprobe_after: u32,
    /// A stand-down has been lifted once, so the next one backs the clock off.
    encode_rearmed: bool,
    /// Two identical short acks latch this. Kept apart from `ceiling_kbps` so a
    /// mode switch does not drop probe-measured link authority.
    host_cap_kbps: Option<u32>,
    /// Last [`request`](Self::request). Taken (not kept) by the ack, so one
    /// request is judged at most once.
    last_requested_kbps: Option<u32>,
    /// Two identical short acks latch [`host_cap_kbps`](Self::host_cap_kbps).
    /// One can be a failed rebuild keeping the old rate.
    short_ack_kbps: u32,
    short_acks: u32,
    /// Re-probe clock: [`CAP_REPROBE_WINDOWS_MIN`], doubled toward
    /// [`CAP_REPROBE_WINDOWS_MAX`] each time a lift is immediately re-learned.
    cap_probe_windows: u32,
    cap_reprobe_after: u32,
    /// Two consecutive decode-driven backoffs at a similar rate. Without it a
    /// decoder knee below the link ceiling is a 30–60 s sawtooth. Re-probed on
    /// the [`CAP_REPROBE_WINDOWS_MIN`] clock.
    decode_cap_kbps: Option<u32>,
    /// Previous decode-driven backoff's pre-backoff rate (`0` = last backoff
    /// was not decode-driven). One spurious flush teaches nothing.
    decode_backoff_kbps: u32,
    /// Decode-flagged windows in the current bad streak. The deciding window
    /// alone would drop the first ordinary-bad window's attribution.
    streak_decode_windows: u32,
    /// `current_kbps` has risen (ack) since the last backoff. A cascade's
    /// second backoff sits at ×0.7 — not a knee sample, and must not erase
    /// the reference.
    climb_since_backoff: bool,
    decode_cap_probe_windows: u32,
    decode_cap_reprobe_after: u32,
    /// A retreat or a cap lift waiting for the decoder's answer.
    decode_probe: Option<DecodeProbe>,
    /// Decode latency did not follow the rate: hold and retreat stand down
    /// until a clean run re-probes them. Same shape as the encode stand-down.
    decode_headroom_disarmed: bool,
    decode_headroom_clean_windows: u32,
    decode_headroom_reprobe_after: u32,
    decode_headroom_rearmed: bool,
    /// Highest clean delivered rate of the current/previous
    /// [`PROVEN_BUCKET_WINDOWS`] buckets. Shrinking capacity is the reactive
    /// decode signal's job.
    proven_cur_kbps: u32,
    proven_prev_kbps: u32,
    proven_bucket_windows: u32,
    /// Consecutive fully-idle windows. First active window after
    /// [`IDLE_WINDOWS_TO_REARM`] re-arms slow start. Empty windows do not count.
    idle_windows: u32,
    low_rate_warned: bool,
    bad_windows: u32,
    clean_windows: u32,
    last_change: Option<Instant>,
    /// Reaching [`MAX_UNACKED`] disables the controller.
    unacked: u32,
    /// Last ceiling-clamp target asked (`0` = none). Asked once per distinct
    /// target: a host that answers higher cannot go there.
    ceiling_ask_kbps: u32,
}

impl BitrateController {
    /// `start_kbps` is the Welcome-resolved Automatic rate, or `0` for a
    /// permanently-disabled controller (explicit bitrate / old host).
    pub(crate) fn new(start_kbps: u32) -> Self {
        Self::with_ceiling_cap(start_kbps, ceiling_cap_from_env())
    }

    /// [`new`](Self::new) with the env cap injected so tests never touch the process env.
    pub(super) fn with_ceiling_cap(start_kbps: u32, ceiling_cap_kbps: Option<u32>) -> Self {
        BitrateController {
            enabled: start_kbps > 0,
            current_kbps: start_kbps,
            // The env cap binds the negotiated start too. Automatic has no
            // explicit bitrate, so a start above the cap must come down — see
            // the clamp-down step in [`on_window`](Self::on_window).
            ceiling_kbps: start_kbps.min(ceiling_cap_kbps.unwrap_or(u32::MAX)),
            ceiling_cap_kbps,
            stream_cap_kbps: None,
            floor_kbps: FLOOR_KBPS.min(start_kbps.max(1)),
            probing: true,
            owd_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            decode_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            encode_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            frame_budget_us: None,
            encode_backoff_us: 0,
            encode_noop_backoffs: 0,
            encode_disarmed: false,
            encode_disarm_clean_windows: 0,
            encode_reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            encode_rearmed: false,
            host_cap_kbps: None,
            last_requested_kbps: None,
            short_ack_kbps: 0,
            short_acks: 0,
            cap_probe_windows: 0,
            cap_reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            decode_cap_kbps: None,
            decode_backoff_kbps: 0,
            streak_decode_windows: 0,
            // Negotiated start was held, not drained to — the first backoff
            // is a legitimate knee sample.
            climb_since_backoff: true,
            decode_cap_probe_windows: 0,
            decode_cap_reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            decode_probe: None,
            decode_headroom_disarmed: false,
            decode_headroom_clean_windows: 0,
            decode_headroom_reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            decode_headroom_rearmed: false,
            proven_cur_kbps: 0,
            proven_prev_kbps: 0,
            proven_bucket_windows: 0,
            idle_windows: 0,
            low_rate_warned: false,
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
            unacked: 0,
            ceiling_ask_kbps: 0,
        }
    }

    /// Raise the climb ceiling to a measured link capacity (caller already
    /// subtracted headroom). Never lowers: a congested-moment measurement must
    /// not shrink authority below what was negotiated. The env cap clamps here
    /// — the one funnel every learned ceiling passes through.
    pub(crate) fn set_ceiling(&mut self, kbps: u32) {
        let measured = kbps;
        let kbps = kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.stream_cap_kbps.unwrap_or(u32::MAX));
        if self.enabled && kbps < measured {
            // Log both numbers when it binds; a silent trim is undiagnosable.
            tracing::info!(
                measured_kbps = measured,
                bounded_kbps = kbps,
                "adaptive bitrate: link ceiling bounded by what this stream can use"
            );
        }
        if self.enabled && kbps > self.ceiling_kbps {
            self.ceiling_kbps = kbps;
        }
    }

    /// Bound future learned ceilings (same funnel as the env cap). The first
    /// set leaves a negotiated start above it standing. A re-set is a mode
    /// switch: a drop in pixel rate rebinds the standing ceiling because
    /// [`set_ceiling`](Self::set_ceiling) never lowers.
    pub(crate) fn set_stream_cap(&mut self, kbps: u32) {
        let mode_switch = self.stream_cap_kbps.is_some();
        self.stream_cap_kbps = Some(kbps);
        if mode_switch && self.enabled && self.ceiling_kbps > kbps {
            tracing::info!(
                ceiling_kbps = self.ceiling_kbps,
                stream_cap_kbps = kbps,
                "adaptive bitrate: ceiling rebound to the switched mode's stream shape"
            );
            self.ceiling_kbps = kbps;
        }
    }

    /// Size encode thresholds in frame budgets, not the 120 Hz [`ENCODE_RISE_US`]
    /// durations. Ignored for a nonsense rate — the defaults stand.
    pub(crate) fn set_frame_budget(&mut self, refresh_hz: u32) {
        if refresh_hz > 0 {
            self.frame_budget_us = Some(1_000_000 / refresh_hz as i64);
        }
    }

    /// `(rise, severe)`: half a frame budget and 1.5 of them, against this
    /// session's refresh. Not the source's delivered fps — inferring that from
    /// arrival cadence is the jitter the signal is trying to read through.
    /// Residue is [`ENCODE_NOOP_BACKOFFS_TO_DISARM`].
    fn encode_thresholds(&self) -> (i64, i64) {
        match self.frame_budget_us {
            Some(budget) => (budget / 2, budget * 3 / 2),
            None => (ENCODE_RISE_US, ENCODE_SEVERE_US),
        }
    }

    /// Decode `(rise, severe)` in the same budgets. The stage includes the
    /// queue, so half a budget of rise is half a frame of standing backlog at
    /// any refresh; the absolute defaults stand until a budget is known.
    fn decode_thresholds(&self) -> (i64, i64) {
        match self.frame_budget_us {
            Some(budget) => (budget / 2, budget * 3 / 2),
            None => (DECODE_RISE_US, DECODE_SEVERE_US),
        }
    }

    /// A cap lift that lost the decoder its headroom: back to the cap, and
    /// the next lift waits twice as long.
    fn undo_decode_lift(&mut self, p: DecodeProbe, decode_us: i64) {
        self.decode_cap_reprobe_after = self
            .decode_cap_reprobe_after
            .saturating_mul(2)
            .min(CAP_REPROBE_WINDOWS_MAX);
        self.decode_cap_kbps = Some(p.from_kbps);
        self.decode_cap_probe_windows = 0;
        tracing::info!(
            cap_kbps = p.from_kbps,
            decode_us,
            was_us = p.ref_us,
            reprobe_after_windows = self.decode_cap_reprobe_after,
            "adaptive bitrate: decode cap lift undone — the decoder lost its headroom"
        );
    }

    /// Decode headroom on a clean full-rate window. First the verdict on a
    /// step in flight, then the bands: park at [`DECODE_HOLD_PCT`], retreat a
    /// notch at [`DECODE_RETREAT_PCT`]. `Some` is a rate to request.
    fn judge_decode_headroom(&mut self, mean_us: i64, budget_us: i64, now: Instant) -> Option<u32> {
        let answer_us = budget_us * DECODE_ANSWER_PCT / 100;
        if let Some(mut p) = self.decode_probe {
            let at_new_rate = match p.kind {
                DecodeProbeKind::Retreat => self.current_kbps < p.from_kbps,
                DecodeProbeKind::Lift => self.current_kbps > p.from_kbps,
            };
            p.age += 1;
            if at_new_rate {
                p.windows += 1;
                p.sum_us += mean_us;
            }
            self.decode_probe = Some(p);
            if p.windows < DECODE_VERDICT_WINDOWS {
                if p.age >= DECODE_PROBE_MAX_AGE {
                    self.decode_probe = None; // the step never reached its rate
                }
                return None;
            }
            self.decode_probe = None;
            let mean = p.sum_us / i64::from(p.windows);
            match p.kind {
                DecodeProbeKind::Retreat if p.ref_us - mean >= answer_us => {
                    tracing::info!(
                        from_kbps = p.from_kbps,
                        at_kbps = self.current_kbps,
                        decode_us = mean,
                        was_us = p.ref_us,
                        "adaptive bitrate: decode latency followed the rate down — retreat kept"
                    );
                    // Fresh evidence; the bands below judge this window too.
                }
                DecodeProbeKind::Retreat => {
                    // Latency is not a function of the rate here: pipelined
                    // decoder, or something else on the SoC. Give the rate
                    // back and stand down; the cap ladder still probes up.
                    self.decode_headroom_disarmed = true;
                    self.decode_headroom_clean_windows = 0;
                    self.decode_headroom_reprobe_after = if self.decode_headroom_rearmed {
                        self.decode_headroom_reprobe_after
                            .saturating_mul(2)
                            .min(CAP_REPROBE_WINDOWS_MAX)
                    } else {
                        CAP_REPROBE_WINDOWS_MIN
                    };
                    self.decode_cap_kbps = Some(p.from_kbps);
                    self.decode_cap_probe_windows = 0;
                    tracing::info!(
                        restore_kbps = p.from_kbps,
                        decode_us = mean,
                        was_us = p.ref_us,
                        rearm_after_windows = self.decode_headroom_reprobe_after,
                        "adaptive bitrate: decode latency did not follow the rate — restoring it \
                         and standing the headroom driver down (a pipelined decoder sits above \
                         the period with no queue)"
                    );
                    self.ceiling_ask_kbps = p.from_kbps;
                    return self.request(p.from_kbps, now);
                }
                DecodeProbeKind::Lift => {
                    let worse = mean - p.ref_us >= answer_us
                        || mean * 100 / budget_us >= DECODE_RETREAT_PCT;
                    if worse {
                        self.undo_decode_lift(p, mean);
                        self.ceiling_ask_kbps = p.from_kbps;
                        return self.request(p.from_kbps, now);
                    }
                    tracing::info!(
                        at_kbps = self.current_kbps,
                        decode_us = mean,
                        was_us = p.ref_us,
                        "adaptive bitrate: decode cap lift held — the decoder kept its headroom"
                    );
                }
            }
        }
        if self.decode_headroom_disarmed {
            self.decode_headroom_clean_windows += 1;
            if self.decode_headroom_clean_windows >= self.decode_headroom_reprobe_after {
                self.decode_headroom_disarmed = false;
                self.decode_headroom_rearmed = true;
                self.decode_headroom_clean_windows = 0;
                tracing::debug!(
                    after_windows = self.decode_headroom_reprobe_after,
                    "adaptive bitrate: re-arming the decode headroom driver after a clean run"
                );
            }
            return None;
        }
        let pct = mean_us * 100 / budget_us;
        if pct >= DECODE_RETREAT_PCT && self.current_kbps > self.floor_kbps {
            let from = self.current_kbps;
            let next = (from - from / 8).max(self.floor_kbps);
            self.decode_cap_kbps = Some(next);
            self.decode_cap_probe_windows = 0;
            self.decode_probe = Some(DecodeProbe {
                kind: DecodeProbeKind::Retreat,
                from_kbps: from,
                ref_us: mean_us,
                windows: 0,
                sum_us: 0,
                age: 0,
            });
            tracing::info!(
                from_kbps = from,
                to_kbps = next,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: the decoder has no headroom — retreating a notch, kept only \
                 if its latency follows"
            );
            self.ceiling_ask_kbps = next;
            return self.request(next, now);
        }
        if pct >= DECODE_HOLD_PCT && self.decode_cap_kbps.is_none_or(|c| c > self.current_kbps) {
            self.decode_cap_kbps = Some(self.current_kbps);
            self.decode_cap_probe_windows = 0;
            tracing::info!(
                cap_kbps = self.current_kbps,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: decode headroom exhausted — parking here (re-probed after a \
                 clean run)"
            );
        }
        None
    }

    /// Host [`crate::quic::BitrateChanged`]: the clamp is authoritative, and any
    /// ack proves the host renegotiates. Two identical short acks latch
    /// [`host_cap_kbps`](Self::host_cap_kbps); one can be a failed rebuild.
    pub(crate) fn on_ack(&mut self, kbps: u32) {
        if kbps > 0 {
            if kbps < self.current_kbps {
                // Our own decrease changes the encode-time regime. Judging the
                // new regime against the old baseline would train-fire.
                self.encode_means.clear();
            }
            if let Some(req) = self.last_requested_kbps.take() {
                if kbps < req {
                    if self.short_ack_kbps == kbps {
                        self.short_acks += 1;
                    } else {
                        self.short_ack_kbps = kbps;
                        self.short_acks = 1;
                    }
                    if self.short_acks >= 2 && self.host_cap_kbps.is_none_or(|c| kbps < c) {
                        // Re-learning a lifted cap means the limit is standing.
                        // First latch starts the clock fast; later ones double.
                        self.cap_reprobe_after = if self.host_cap_kbps.is_some() {
                            self.cap_reprobe_after
                                .saturating_mul(2)
                                .min(CAP_REPROBE_WINDOWS_MAX)
                        } else {
                            CAP_REPROBE_WINDOWS_MIN
                        };
                        tracing::info!(
                            cap_kbps = kbps,
                            reprobe_after_windows = self.cap_reprobe_after,
                            "adaptive bitrate: host cap learned (encoder ceiling or cadence \
                             refusal) — climbs stop here until it lifts"
                        );
                        self.host_cap_kbps = Some(kbps.max(self.floor_kbps));
                        self.cap_probe_windows = 0;
                    }
                } else {
                    self.short_acks = 0;
                    // Granted at or above the learned cap: drop it. Crawling
                    // +12.5 % is the remaining cost of a transient latch.
                    if self.host_cap_kbps.is_some_and(|c| kbps >= c) {
                        tracing::info!(
                            granted_kbps = kbps,
                            "adaptive bitrate: host granted a climb at the learned cap — the \
                             limit has lifted, dropping it"
                        );
                        self.host_cap_kbps = None;
                        self.cap_probe_windows = 0;
                        self.cap_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
                    }
                }
            }
            if kbps > self.current_kbps {
                // Rate rose: next choke is at a climbed-to rate. An acked
                // decrease does not arm this — drain is not a knee encounter.
                self.climb_since_backoff = true;
            }
            self.current_kbps = kbps;
            // Unsolicited `BitrateChanged` can sit above our ceiling (host
            // re-resolved Automatic for what it encodes). Follow it; env cap
            // still binds. Without this, the step-down drags the host back.
            self.set_ceiling(kbps);
        }
        self.unacked = 0;
    }

    /// Drop mode-scoped learned state. Encoder/decoder knees and rolling
    /// baselines are properties of the mode; a baseline from the old mode is a
    /// floor the new one clears on the first window. Probe-measured
    /// `ceiling_kbps` (a link property) survives. Proven throughput re-earns.
    pub(crate) fn on_mode_switch(&mut self) {
        self.host_cap_kbps = None;
        self.short_acks = 0;
        self.cap_probe_windows = 0;
        self.cap_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
        self.decode_cap_kbps = None;
        self.decode_backoff_kbps = 0;
        self.streak_decode_windows = 0;
        self.climb_since_backoff = true;
        self.decode_cap_probe_windows = 0;
        self.decode_cap_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
        self.decode_probe = None;
        self.decode_headroom_disarmed = false;
        self.decode_headroom_clean_windows = 0;
        self.decode_headroom_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
        self.decode_headroom_rearmed = false;
        self.owd_means.clear();
        self.decode_means.clear();
        self.encode_means.clear();
        // Encode work per frame changed with the mode. Re-arm; the caller
        // re-sizes the frame budget alongside this.
        self.encode_disarmed = false;
        self.encode_backoff_us = 0;
        self.encode_noop_backoffs = 0;
        self.encode_disarm_clean_windows = 0;
        self.encode_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
        self.encode_rearmed = false;
        self.proven_cur_kbps = 0;
        self.proven_prev_kbps = 0;
        self.proven_bucket_windows = 0;
        self.idle_windows = 0;
    }

    /// Max clean delivered rate of the current and previous buckets (~30–60 s).
    fn proven(&self) -> u32 {
        self.proven_cur_kbps.max(self.proven_prev_kbps)
    }

    /// Decide whether this 750 ms window should ask for a new encoder rate.
    ///
    /// `Some(kbps)` is the request. OWD, decode, and encode `None` mean that
    /// signal is absent. Activity is three-way: [`WindowActivity::Unmarked`]
    /// (older host, wall-clock), [`WindowActivity::Empty`] (no AU arrived;
    /// quiet like idle, does not count toward re-arm), and
    /// [`WindowActivity::Active`] including `0` for observed stillness.
    pub(crate) fn on_window(&mut self, w: &WindowSample) -> Option<u32> {
        let WindowSample {
            now,
            dropped,
            loss_ppm,
            owd_mean_us,
            decode_mean_us,
            encode_mean_us,
            actual_kbps,
            flushed,
            recovery_kf,
            activity,
        } = *w;
        if !self.enabled {
            return None;
        }
        if self.unacked >= MAX_UNACKED {
            // Host never answered: older build. Quiet, don't spam unknown.
            self.enabled = false;
            tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            return None;
        }
        // Repeat-only: stillness. Empty: no AU, same neutrality, not a
        // re-arm count. Unmarked (older host) is never idle.
        let quiet = activity.quiet();
        if activity.idle() {
            self.idle_windows = self.idle_windows.saturating_add(1);
        } else if !quiet {
            // First active window after a real idle stretch: re-arm slow
            // start and clear the cooldown so this window can climb.
            if self.idle_windows >= IDLE_WINDOWS_TO_REARM && !self.probing {
                self.probing = true;
                self.last_change = None;
                tracing::debug!(
                    idle_windows = self.idle_windows,
                    proven_kbps = self.proven(),
                    "adaptive bitrate: motion onset after an idle stretch — slow start re-armed"
                );
            }
            self.idle_windows = 0;
        }
        // Bucket clock ticks on idle windows too: decay is about time.
        self.proven_bucket_windows += 1;
        if self.proven_bucket_windows >= PROVEN_BUCKET_WINDOWS {
            self.proven_bucket_windows = 0;
            self.proven_prev_kbps = self.proven_cur_kbps;
            self.proven_cur_kbps = 0;
        }
        // Keepalive OWD/decode would train the rolling min on the quietest
        // traffic, so the first motion window reads as congestion.
        let owd_mean_us = owd_mean_us.filter(|_| !quiet);
        let decode_mean_us = decode_mean_us.filter(|_| !quiet);
        // No severe OWD tier: a standing queue is congestion, not visible
        // damage, so it always takes the two-window path.
        let (owd_bad, _) = score_baseline(&mut self.owd_means, owd_mean_us, OWD_RISE_US, i64::MAX);
        // Decode rise ends slow start immediately; a far-past-baseline
        // excursion is severe (one window). Sized in frame budgets.
        let (decode_rise_us, decode_severe_us) = self.decode_thresholds();
        let (decode_bad, decode_severe) = score_baseline(
            &mut self.decode_means,
            decode_mean_us,
            decode_rise_us,
            decode_severe_us,
        );
        // Starved: encode_us is not a measurement here. `current_kbps` does
        // not move in this function, so the backoff block reads the same value.
        let starved =
            (actual_kbps as u64) * (STARVED_DELIVERY_DIV as u64) < self.current_kbps as u64;
        // Encode: the only signal that can descend on a clean LAN. Withheld
        // when starved (mean describes the interruption), disarmed, or quiet.
        // Passed as absent so it cannot teach the baseline either. Loss,
        // flush, and drop keep full power.
        let (encode_rise_us, encode_severe_us) = self.encode_thresholds();
        let encode_usable = !starved && !self.encode_disarmed && !quiet;
        let (encode_bad, encode_severe) = score_baseline(
            &mut self.encode_means,
            encode_mean_us.filter(|_| encode_usable),
            encode_rise_us,
            encode_severe_us,
        );
        // Severe: one window. Ordinary congestion: two consecutive.
        let severe = dropped > 0
            || flushed
            || loss_ppm >= SEVERE_LOSS_PPM
            || decode_severe
            || encode_severe
            || recovery_kf >= RECOVERY_KF_SEVERE;
        let bad = severe
            || loss_ppm >= HEAVY_LOSS_PPM
            || owd_bad
            || decode_bad
            || encode_bad
            || recovery_kf >= RECOVERY_KF_BAD;
        // Proven mark: scored after the verdict, gated on the whole of it.
        // Damaged windows overstate delivered (stall drain, flush queue, FEC
        // surge); those bytes arriving is not climb authority.
        if !bad && actual_kbps > self.proven_cur_kbps {
            self.proven_cur_kbps = actual_kbps;
        }
        if bad {
            self.bad_windows += 1;
            if decode_bad {
                // Counted here: backoff only sees the final window, and the
                // cooldown eats the first ordinary-bad window.
                self.streak_decode_windows += 1;
            }
            self.clean_windows = 0;
            self.decode_headroom_clean_windows = 0;
            // A lift that ran into damage failed; a retreat learns nothing here.
            match self.decode_probe.take() {
                Some(p) if p.kind == DecodeProbeKind::Lift && self.current_kbps > p.from_kbps => {
                    self.undo_decode_lift(p, decode_mean_us.unwrap_or(p.ref_us));
                }
                _ => {}
            }
            // Any congestion ends slow start until a later idle-onset re-arm.
            self.probing = false;
        } else if quiet {
            // Neutral: stillness and empty are neither climb credit nor a cleared streak.
        } else {
            self.clean_windows += 1;
            self.bad_windows = 0;
            self.streak_decode_windows = 0;
        }
        // Host-cap re-probe: after a clean run parked at the cap, lift +12.5 %.
        // A still-standing limit re-latches from the next short-ack pair.
        if let Some(cap) = self.host_cap_kbps {
            if bad {
                self.cap_probe_windows = 0;
            // Re-probe accrues from clean loaded windows, not stillness.
            } else if !quiet && self.current_kbps >= cap.saturating_sub(cap / 16) {
                self.cap_probe_windows += 1;
                if self.cap_probe_windows >= self.cap_reprobe_after {
                    self.cap_probe_windows = 0;
                    let lifted = cap.saturating_add(cap / 8).min(self.ceiling_kbps);
                    if lifted > cap {
                        tracing::debug!(
                            from_kbps = cap,
                            to_kbps = lifted,
                            "adaptive bitrate: re-probing above the learned host cap"
                        );
                        self.host_cap_kbps = Some(lifted);
                    }
                }
            }
        }
        // Decode cap: same clock. Knee is content/thermals evidence; a
        // still-standing knee re-latches from the next decode-driven pair.
        if let Some(cap) = self.decode_cap_kbps {
            if bad {
                self.decode_cap_probe_windows = 0;
            // Same `!quiet` rule as the host-cap clock.
            } else if !quiet && self.current_kbps >= cap.saturating_sub(cap / 16) {
                self.decode_cap_probe_windows += 1;
                if self.decode_cap_probe_windows >= self.decode_cap_reprobe_after {
                    self.decode_cap_probe_windows = 0;
                    let lifted = cap.saturating_add(cap / 8).min(self.ceiling_kbps);
                    if lifted > cap {
                        tracing::debug!(
                            from_kbps = cap,
                            to_kbps = lifted,
                            "adaptive bitrate: re-probing above the learned decode cap"
                        );
                        self.decode_cap_kbps = Some(lifted);
                        // The decoder's answer above the cap decides the lift.
                        self.decode_probe = decode_mean_us.map(|ref_us| DecodeProbe {
                            kind: DecodeProbeKind::Lift,
                            from_kbps: cap,
                            ref_us,
                            windows: 0,
                            sum_us: 0,
                            age: 0,
                        });
                    }
                }
            }
        }
        // Encode stand-down re-probes on the same clock. GPU contention ends;
        // a too-eager re-arm costs one ×0.7, a permanent silence costs the
        // knee protection.
        if self.encode_disarmed {
            if bad {
                self.encode_disarm_clean_windows = 0;
            // Quiet because nothing needed encoding proves nothing about the knee.
            } else if !quiet {
                self.encode_disarm_clean_windows += 1;
                if self.encode_disarm_clean_windows >= self.encode_reprobe_after {
                    self.encode_disarmed = false;
                    self.encode_rearmed = true;
                    self.encode_disarm_clean_windows = 0;
                    // Fresh baseline, no streak: the old firing level is stale.
                    self.encode_backoff_us = 0;
                    self.encode_noop_backoffs = 0;
                    self.encode_means.clear();
                    tracing::debug!(
                        after_windows = self.encode_reprobe_after,
                        "adaptive bitrate: re-arming the encode down-driver after a clean run"
                    );
                }
            }
        }
        let cooled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= CHANGE_COOLDOWN);
        if !cooled {
            return None;
        }
        if (self.bad_windows >= BAD_WINDOWS_TO_DECREASE || (severe && self.bad_windows >= 1))
            && self.current_kbps > self.floor_kbps
        {
            // Decode evidence: severe in this window; every window in the
            // ordinary streak was decode-flagged; kf-storm without heavy loss;
            // flush only if decode is bad or the signal is absent (flat decode
            // + flush is a network event).
            let decode_evidence = decode_severe
                || self.streak_decode_windows >= BAD_WINDOWS_TO_DECREASE
                || (recovery_kf >= RECOVERY_KF_BAD && loss_ppm < HEAVY_LOSS_PPM)
                || (flushed && (decode_bad || decode_mean_us.is_none()));
            if !self.climb_since_backoff {
                // Drain after ×0.7 (~100 ms ack): not a knee sample. Neither
                // latch nor erase the reference.
                tracing::debug!(
                    at_kbps = self.current_kbps,
                    reference_kbps = self.decode_backoff_kbps,
                    "adaptive bitrate: backoff without an intervening climb — draining the \
                     previous choke, not a knee sample"
                );
            } else if starved {
                // Starved: same as drain — neither latch nor erase.
                tracing::debug!(
                    at_kbps = self.current_kbps,
                    actual_kbps,
                    reference_kbps = self.decode_backoff_kbps,
                    "adaptive bitrate: backoff in a starved window (delivery a fraction of \
                     the target) — starvation-shaped distress, not a knee sample"
                );
            } else if decode_evidence {
                let rate = self.current_kbps;
                let similar = self.decode_backoff_kbps > 0
                    && rate.abs_diff(self.decode_backoff_kbps)
                        <= self.decode_backoff_kbps / DECODE_CAP_SIMILAR_DIV;
                // Latch just under the choke rate: a cap on the knee authorizes
                // climbing straight back into it. 1/16 is inside the ±1/8 band.
                let knee = rate.saturating_sub(rate / 16).max(self.floor_kbps);
                if similar && self.decode_cap_kbps.is_none_or(|c| knee < c) {
                    // Standing-vs-transient clock, same as the host cap.
                    self.decode_cap_reprobe_after = if self.decode_cap_kbps.is_some() {
                        self.decode_cap_reprobe_after
                            .saturating_mul(2)
                            .min(CAP_REPROBE_WINDOWS_MAX)
                    } else {
                        CAP_REPROBE_WINDOWS_MIN
                    };
                    tracing::info!(
                        cap_kbps = knee,
                        choked_at_kbps = rate,
                        reprobe_after_windows = self.decode_cap_reprobe_after,
                        "adaptive bitrate: decode cap learned (decoder knee) — climbs stop \
                         here until it lifts"
                    );
                    self.decode_cap_kbps = Some(knee);
                    self.decode_cap_probe_windows = 0;
                }
                self.decode_backoff_kbps = rate;
            } else {
                self.decode_backoff_kbps = 0;
            }
            // Encode attribution: judged from this firing level, not the
            // baseline (`on_ack` re-seeded it). Loss/flush/drop explain the
            // backoff without the encoder.
            let encode_attributed = (encode_severe || encode_bad)
                && dropped == 0
                && !flushed
                && loss_ppm < HEAVY_LOSS_PPM;
            if let Some(mean) = encode_mean_us.filter(|_| encode_attributed) {
                if self.encode_backoff_us > 0
                    && mean >= self.encode_backoff_us.saturating_sub(encode_rise_us)
                {
                    // Fired again no lower: the ×0.7 in between did nothing.
                    self.encode_noop_backoffs += 1;
                    if self.encode_noop_backoffs >= ENCODE_NOOP_BACKOFFS_TO_DISARM {
                        // Re-silencing a lifted stand-down: standing contention,
                        // back the clock off.
                        self.encode_reprobe_after = if self.encode_rearmed {
                            self.encode_reprobe_after
                                .saturating_mul(2)
                                .min(CAP_REPROBE_WINDOWS_MAX)
                        } else {
                            CAP_REPROBE_WINDOWS_MIN
                        };
                        self.encode_disarmed = true;
                        self.encode_disarm_clean_windows = 0;
                        self.encode_means.clear();
                        tracing::info!(
                            at_kbps = self.current_kbps,
                            encode_mean_us = mean,
                            noop_backoffs = self.encode_noop_backoffs,
                            rearm_after_windows = self.encode_reprobe_after,
                            "adaptive bitrate: host encode time is not answering the rate — \
                             standing the encode down-driver down until a clean run re-probes it \
                             (loss, OWD, decode and keyframe signals keep driving)"
                        );
                    }
                } else {
                    self.encode_noop_backoffs = 0;
                }
                self.encode_backoff_us = mean;
            } else {
                // Something else drove this one: encode streak is broken.
                self.encode_backoff_us = 0;
                self.encode_noop_backoffs = 0;
            }
            self.climb_since_backoff = false;
            let next = ((self.current_kbps as u64 * 7 / 10) as u32).max(self.floor_kbps);
            // First descent below the old 5 Mbps floor: log once.
            if next < LOW_RATE_WARN_KBPS && !self.low_rate_warned {
                self.low_rate_warned = true;
                tracing::warn!(
                    at_kbps = next,
                    "adaptive bitrate: the link sustains only a very low rate — expect \
                     visibly soft video until it recovers (floor: 2 Mbps)"
                );
            }
            self.bad_windows = 0;
            self.streak_decode_windows = 0;
            return self.request(next, now);
        }
        // Decode headroom: a clean, loaded, full-rate window with the signal.
        // Loss, flush and starvation are the link's story, not the decoder's.
        let full_rate = match (activity, self.frame_budget_us) {
            (WindowActivity::Active(n), Some(budget_us)) if budget_us > 0 => {
                i64::from(n) * DECODE_FULL_RATE_DEN
                    >= (sample::WINDOW_US / budget_us) * DECODE_FULL_RATE_NUM
            }
            _ => true,
        };
        if let (Some(mean_us), Some(budget_us)) = (decode_mean_us, self.frame_budget_us) {
            if budget_us > 0 && !bad && !quiet && !starved && full_rate {
                if let Some(kbps) = self.judge_decode_headroom(mean_us, budget_us, now) {
                    return Some(kbps);
                }
            }
        }
        // Climbs need a utilized clean window and stay within ×1.5 of proven.
        // Frame-driven sources never reach ¾ of the wall-clock target, so the
        // ¾ gate is prorated by active/expected, floored at
        // [`MIN_ACTIVE_FRAMES_TO_CLIMB`]. Older host: wall-clock arithmetic.
        let legacy_utilized =
            actual_kbps as u64 * UTILIZATION_DEN >= self.current_kbps as u64 * UTILIZATION_NUM;
        // Prorate utilization AND proven-headroom together or they deadlock
        // (a 35 fps source's wall-clock wire rate never exceeds ~39 % of target).
        let proration = match (activity, self.frame_budget_us) {
            (WindowActivity::Active(n), Some(budget_us))
                if budget_us > 0 && n > 0 && (n as i64) < sample::WINDOW_US / budget_us =>
            {
                Some((n as u64, ((sample::WINDOW_US / budget_us).max(1)) as u64))
            }
            _ => None,
        };
        let utilized = match (activity, proration) {
            (WindowActivity::Active(0) | WindowActivity::Empty, _) => false,
            (_, Some((n, expected))) => {
                n >= MIN_ACTIVE_FRAMES_TO_CLIMB as u64
                    && actual_kbps as u64 * UTILIZATION_DEN * expected
                        >= self.current_kbps as u64 * UTILIZATION_NUM * n
            }
            // Full-rate source, older host, or unknown refresh: wall-clock.
            _ => legacy_utilized,
        };
        // Probe = link, short acks = encoder, decode cap = client decoder.
        let eff_ceiling = self
            .ceiling_kbps
            .min(self.host_cap_kbps.unwrap_or(u32::MAX))
            .min(self.decode_cap_kbps.unwrap_or(u32::MAX));
        // Above the env/policy ceiling with no congestion: step down once per
        // distinct target. A host that answers higher cannot go there.
        let ceiling_target = eff_ceiling.max(self.floor_kbps);
        if self.current_kbps > ceiling_target && self.ceiling_ask_kbps != ceiling_target {
            tracing::info!(
                from_kbps = self.current_kbps,
                to_kbps = ceiling_target,
                "adaptive bitrate: session rate is above the configured ceiling — stepping down"
            );
            self.ceiling_ask_kbps = ceiling_target;
            return self.request(ceiling_target, now);
        }
        // Proven bounds projected wire rate at ×1.5. Frame-driven: invert the
        // same proration so a utilized window still has ≥ ~12 % climb room.
        let proven_wire_cap =
            self.proven().saturating_mul(PROVEN_HEADROOM_NUM) / PROVEN_HEADROOM_DEN;
        let proven_target_cap = match proration {
            Some((n, expected)) => {
                u32::try_from(proven_wire_cap as u64 * expected / n).unwrap_or(u32::MAX)
            }
            None => proven_wire_cap,
        };
        let cap = eff_ceiling.min(proven_target_cap);
        if self.current_kbps < eff_ceiling && utilized && cap > self.current_kbps {
            // Slow start: double every cooled clean window. Else +~6 % after
            // a sustained clean run.
            if self.probing && self.clean_windows >= 1 {
                let next = self.current_kbps.saturating_mul(2).min(cap);
                self.clean_windows = 0;
                return self.request(next, now);
            }
            if self.clean_windows >= CLEAN_WINDOWS_TO_INCREASE {
                let next = (self.current_kbps + self.current_kbps / 16 + 1).min(cap);
                self.clean_windows = 0;
                return self.request(next, now);
            }
        }
        None
    }

    fn request(&mut self, kbps: u32, now: Instant) -> Option<u32> {
        self.last_change = Some(now);
        self.unacked += 1;
        self.last_requested_kbps = Some(kbps);
        // Ack is authoritative. A lost request recomputes from the same base.
        Some(kbps)
    }

    /// Control queue was full: undo request bookkeeping. [`MAX_UNACKED`]
    /// detects a host that doesn't answer; counting a message never sent
    /// retires the controller. Also keeps a later unsolicited ack from being
    /// judged short against a rate we never asked for.
    pub(crate) fn on_request_dropped(&mut self) {
        self.unacked = self.unacked.saturating_sub(1);
        self.last_requested_kbps = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pump's 750 ms tick; 5× is past [`CHANGE_COOLDOWN`].
    const TICK: Duration = Duration::from_millis(750);

    fn ticks(start: Instant, n: u32) -> Instant {
        start + TICK * n
    }

    /// `n` clean fully-loaded windows (1 Gb/s) so utilization and proven never bind.
    fn run_clean(c: &mut BitrateController, start: Instant, from: u32, n: u32) -> Option<u32> {
        let mut out = None;
        for i in from..from + n {
            out = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            });
            if out.is_some() {
                return out;
            }
        }
        out
    }

    #[test]
    fn disabled_when_not_automatic_or_old_host() {
        // start 0 = explicit bitrate or a host that didn't echo one.
        let mut c = BitrateController::new(0);
        let now = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 5,
                loss_ppm: 900_000,
                owd_mean_us: Some(500_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(now)
            }),
            None
        );
    }

    #[test]
    fn two_ordinary_bad_windows_step_down_multiplicatively() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // 2–6 % loss is ordinary: one window is a blip.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            None
        );
        // Second consecutive ordinary-bad window: ×0.7.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 1))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Still bad after cooldown: another ×0.7 from the acked rate.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 7))
            }),
            Some(9_800)
        );
    }

    #[test]
    fn severe_window_backs_off_immediately() {
        // Unrecoverable frame skips the two-window wait…
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        // …and so does a jump-to-live flush.
        let mut c = BitrateController::new(20_000);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        // …and ≥6 % window loss.
        let mut c = BitrateController::new(20_000);
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 80_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn cooldown_blocks_back_to_back_steps() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Tick 1 = 750 ms, inside cooldown; tick 2 = 1.5 s, fires.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(9_800)
        );
    }

    #[test]
    fn floor_is_never_crossed() {
        let mut c = BitrateController::new(2_500);
        let start = Instant::now();
        // ×0.7 of 2500 = 1750 < floor → 2000.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(2_000)
        );
        c.on_ack(2_000);
        // At the floor, further bad windows request nothing.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 7))
            }),
            None
        );
    }

    #[test]
    fn sustained_clean_recovers_toward_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Slow start is over: 6 clean windows → +~6 % (14000 + 14000/16 + 1 = 14876).
        let up = run_clean(&mut c, start, 2, 7);
        assert_eq!(up, Some(14_876));
        c.on_ack(14_876);
        // At the ceiling, clean windows stay quiet.
        c.on_ack(20_000);
        assert_eq!(run_clean(&mut c, start, 40, 20), None);
    }

    #[test]
    fn slow_start_doubles_to_a_probed_ceiling_then_stops() {
        let mut c = BitrateController::new(20_000);
        // Probe measured ~430 Mbps delivered → ×0.7 ceiling.
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Cooled clean windows double until the ceiling, then quiet.
        let mut got = Vec::new();
        for i in 0..14 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            }) {
                c.on_ack(k);
                got.push(k);
            }
        }
        assert_eq!(got, vec![40_000, 80_000, 160_000, 300_000]);
    }

    #[test]
    fn first_congestion_ends_slow_start_for_good() {
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(40_000)
        );
        c.on_ack(40_000);
        // Severe: immediate ×0.7, slow start over.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(28_000)
        );
        c.on_ack(28_000);
        // Next climb is additive, after 6 clean windows.
        let mut next = None;
        for i in 3..12 {
            next = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            });
            if next.is_some() {
                assert!(i >= 8, "additive climb must wait for the clean run");
                break;
            }
        }
        assert_eq!(next, Some(29_751)); // 28000 + 28000/16 + 1
    }

    #[test]
    fn set_ceiling_is_ignored_when_disabled_and_never_lowers() {
        let mut c = BitrateController::new(0);
        c.set_ceiling(1_000_000);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 1_000_000,
                ..WindowSample::at(Instant::now())
            }),
            None
        );
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(10_000); // below the negotiated start → ignored
        assert_eq!(c.ceiling_kbps, 20_000);
    }

    /// Stream bound clamps learned ceilings only; a host-resolved start stands.
    #[test]
    fn the_stream_bound_clamps_a_learned_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000, "a learned ceiling is bounded");

        // Never set: no stream bound.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 657_000);

        // Negotiated start above the bound stands.
        let mut c = BitrateController::new(300_000);
        c.set_stream_cap(100_000);
        assert_eq!(c.ceiling_kbps, 300_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 300_000,
            "and a learned ceiling under it never lowers what was negotiated"
        );

        // Tighter of env and stream caps wins.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 50_000,
            "the env cap still binds when it is tighter"
        );
    }

    /// Mode switch re-teaches the stream cap both ways: upswitch opens room,
    /// downswitch rebinds because [`BitrateController::set_ceiling`] never lowers.
    #[test]
    fn a_mode_switch_reteaches_the_stream_cap_both_ways() {
        // 1080p on a fat link: ceiling bound at the 1080p shape.
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000);

        // Upswitch to 4K: new shape allows more; probe measurement may re-authorize.
        c.on_mode_switch();
        c.set_stream_cap(400_000);
        assert_eq!(
            c.ceiling_kbps, 100_000,
            "an upswitch alone raises nothing — authority still needs a measurement"
        );
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 400_000,
            "the 4K shape no longer pins the session to the 1080p bound"
        );

        // Downswitch to 720p: re-taught cap rebinds; `set_ceiling` never lowers.
        c.on_mode_switch();
        c.set_stream_cap(42_000);
        assert_eq!(
            c.ceiling_kbps, 42_000,
            "a downswitch rebinds the already-learned ceiling"
        );

        // Disabled controller (explicit bitrate) is untouched.
        let mut d = BitrateController::new(0);
        d.set_stream_cap(100_000);
        d.set_stream_cap(42_000);
        assert_eq!(d.ceiling_kbps, 0);
    }

    #[test]
    fn owd_rise_alone_is_a_congestion_signal() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // ~10 ms OWD baseline.
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // +40 ms OWD, zero loss: two windows → back off.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(50_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(52_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 5))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn decode_latency_rise_alone_is_a_congestion_signal() {
        // Pristine link; only decode latency is rising.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // ~8 ms decode baseline.
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // +30 ms decode, zero loss, flat OWD: two windows → ×0.7.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(38_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(40_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 5))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_storm_alone_is_a_congestion_signal() {
        // Pristine link, no latency signal, two kf asks per window: ordinary-bad.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 2,
                ..WindowSample::at(ticks(start, 0))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 2,
                ..WindowSample::at(ticks(start, 1))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_saturation_is_severe() {
        // Emitters throttle at 100 ms: 4+ asks in 750 ms is severe.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 4,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn a_single_keyframe_ask_is_not_congestion() {
        // One kf ask is not congestion, even in a row.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    recovery_kf: 1,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }

    #[test]
    fn decode_latency_caps_the_slow_start_climb() {
        // Fat link, decoder saturates below it.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // First [`BASELINE_MIN_WINDOWS`] teach the decode baseline.
        let mut last = 0;
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i * 2))
            }) {
                last = k;
                c.on_ack(k);
            }
        }
        assert_eq!(last, 300_000, "slow start should reach the probed ceiling");
        // +30 ms decode: climb stops.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(38_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 20))
            }),
            None
        );
        // Second backed-up window: ×0.7, not park at the link ceiling.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(40_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 22))
            }),
            Some(210_000)
        );
    }

    #[test]
    fn one_calm_window_is_not_a_baseline() {
        // Our own decrease clears the encode baseline. One sample must not arm.
        let mut c = BitrateController::new(100_000);
        let start = Instant::now();
        // One 3 ms seed, then 12 ms: past [`ENCODE_RISE_US`], but no baseline yet.
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            let mean = if i == 0 { 3_000 } else { 12_000 };
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(mean),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None,
                "window {i} fired off a baseline of fewer than {BASELINE_MIN_WINDOWS} samples"
            );
        }
        // With 4 samples, a sustained rise still backs off.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 8))
            }),
            Some(70_000)
        );
    }

    #[test]
    fn unloaded_clean_windows_never_authorize_a_climb() {
        // Calm, under-target delivery: no climb credit.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        for i in 0..12 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 2_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // First utilized window: ×1.5 over proven 18 000 → 27 000, not 2× to 40 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 18_000,
                ..WindowSample::at(ticks(start, 12))
            }),
            Some(27_000)
        );
        // Zero active frames never authorizes a climb, whatever delivered claims.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        c.set_frame_budget(60);
        for i in 0..12 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 18_000,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None,
                "an idle window must never climb"
            );
        }
    }

    /// Frame-driven source: utilization and proven-headroom prorate together
    /// so a 35 fps source on 90 Hz is not stuck in a wall-clock dead band.
    #[test]
    fn a_frame_driven_source_climbs_at_its_own_fps() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(90); // 11 111 µs budget → 67 expected frames / window
        let start = Instant::now();
        // 26/67 frames, 8 000 kbps vs prorated 7 761: utilized. Proven headroom
        // 8 000×1.5×67/26 = 30 923, not wall-clock 12 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 8_000,
                activity: WindowActivity::Active(26),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_923)
        );
        // Under [`MIN_ACTIVE_FRAMES_TO_CLIMB`]: not utilized.
        let mut d = BitrateController::new(20_000);
        d.set_stream_cap(100_000);
        d.set_ceiling(60_000);
        d.set_frame_budget(90);
        assert_eq!(
            d.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 900,
                activity: WindowActivity::Active(3),
                ..WindowSample::at(ticks(start, 0))
            }),
            None,
            "three stray frames are not a utilized window"
        );
    }

    /// Same history: non-marking host trains OWD on keepalive and backs off
    /// at motion; marking host trains nothing and climbs.
    #[test]
    fn idle_windows_train_no_baselines() {
        let start = Instant::now();
        let run = |marking: bool| -> Option<u32> {
            let mut c = BitrateController::new(20_000);
            c.set_ceiling(300_000);
            c.set_frame_budget(60);
            // A host that marks repeats reports the new-content count; an
            // older one reports nothing but arrivals.
            let act = |n: u32| {
                if marking {
                    WindowActivity::Active(n)
                } else {
                    WindowActivity::Unmarked
                }
            };
            let mut decision = None;
            // Four active windows at 30 ms OWD.
            for i in 0..4 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(30_000),
                    actual_kbps: 2_000,
                    activity: act(45),
                    ..WindowSample::at(ticks(start, i))
                });
                assert_eq!(r, None);
            }
            // Keepalive at 1 ms OWD: marking host trains nothing; legacy trains min to 1 ms.
            for i in 4..10 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(1_000),
                    actual_kbps: 200,
                    activity: act(0),
                    ..WindowSample::at(ticks(start, i))
                });
                assert_eq!(r, None);
            }
            // Motion at the warmup's 30 ms OWD. First decision is the verdict
            // (cooldown silences the second).
            for i in 10..12 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(30_000),
                    actual_kbps: 18_000,
                    activity: act(45),
                    ..WindowSample::at(ticks(start, i))
                });
                decision = decision.or(r);
            }
            decision
        };
        // Legacy: 30 ms vs 1 ms baseline → ×0.7.
        assert_eq!(run(false), Some(14_000));
        // Marking: 30 ms is normal; first utilized window climbs to 27 000.
        assert_eq!(run(true), Some(27_000));
    }

    /// Motion onset after a real idle stretch re-arms slow start, bounded by
    /// ×1.5 over the windowed proven mark.
    #[test]
    fn motion_onset_rearms_slow_start_bounded_by_the_windowed_proven() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        // Severe window ends slow start…
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        // …one clean window proves 14 000…
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        // …then ≥ [`IDLE_WINDOWS_TO_REARM`] idle windows.
        for i in 2..6 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // Onset: ×1.5 over proven 14 000 → 21 000, not additive 14 876.
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(21_000)
        );
    }

    /// Five empty windows are not an idle stretch. [`CLEAN_WINDOWS_TO_INCREASE`]
    /// is 6, so five quiet windows plus one active would additive-climb if
    /// Empty were credited as clean (the older-host `None` path).
    #[test]
    fn empty_windows_do_not_rearm_abr_slow_start() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        for i in 1..=5 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Empty,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 6))
            }),
            None,
            "a blackout is not stillness and cannot authorize a climb"
        );
    }

    /// Empty is neutral on the idle counter: it neither fills nor clears it.
    #[test]
    fn empty_windows_do_not_count_toward_idle_rearm() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        for i in 2..6 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 200,
                activity: WindowActivity::Empty,
                ..WindowSample::at(ticks(start, 6))
            }),
            None,
            "empty is not motion onset"
        );
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 7))
            }),
            Some(21_000),
            "empty must not clear a real idle stretch"
        );
    }

    /// Idle stretch outlives both proven buckets: onset doubles over what it
    /// just delivered, not the stale pre-idle mark.
    #[test]
    fn the_proven_mark_decays_with_its_buckets() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        // Prove 20 000; slow start asks for the bounded double (mark matters)…
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        // Severe inside cooldown scores but decides nothing; second backs off.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Idle 2 × [`PROVEN_BUCKET_WINDOWS`]: both buckets rotate away.
        for i in 3..3 + 2 * PROVEN_BUCKET_WINDOWS {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // Onset delivers 14 000 → 21 000, not 28 000 off the stale 20 000 mark.
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 3 + 2 * PROVEN_BUCKET_WINDOWS))
            }),
            Some(21_000)
        );
    }

    /// One-shot warning on first descent below the old 5 Mbps floor.
    #[test]
    fn the_low_rate_warning_fires_once_below_the_old_floor() {
        let mut c = BitrateController::new(6_000);
        let start = Instant::now();
        assert!(!c.low_rate_warned);
        // 6000 × 0.7 = 4200: under the old floor, over the new one.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(4_200)
        );
        assert!(c.low_rate_warned, "the descent below 5 000 warns");
        c.on_ack(4_200);
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(2_940)
        );
        assert!(c.low_rate_warned, "…exactly once");
    }

    #[test]
    fn slow_start_steps_stay_within_proven_headroom() {
        // Each slow-start step is ×1.5 over delivered, not a blind 2×.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Full-target delivery: proven 20 000 → cap 30 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 20_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        c.on_ack(30_000);
        // Delivers 30 000 → next step 45 000, not 60 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 30_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(45_000)
        );
    }

    #[test]
    fn calm_period_keeps_the_validated_target() {
        // Validated target is not surrendered when the scene goes calm.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 20_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        c.on_ack(30_000);
        // Long calm stretch (2 % utilization): stay silent. Keep proven headroom.
        for i in 2..30 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(4_000),
                    actual_kbps: 600,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }

    #[test]
    fn deep_decode_excursion_is_severe() {
        // Decode rise >45 ms is already overload: one window.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // 52 ms over 8 ms baseline: immediate ×0.7. 30 ms still takes two.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(60_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn two_identical_short_acks_latch_the_host_cap() {
        // Two identical short acks latch the host cap; climbs stop poking it.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        // One short ack is not a cap.
        c.on_ack(794_000);
        assert!(c.host_cap_kbps.is_none());
        // Second identical short ack: latch.
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        // Parked at the cap: no more requests.
        assert_eq!(run_clean(&mut c, start, 20, 12), None);
    }

    #[test]
    fn one_short_ack_is_a_transient_not_a_cap() {
        // One short ack (failed rebuild) must not latch.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(400_000); // failed rebuild kept the old rate
        assert!(c.host_cap_kbps.is_none());
        // Full grant: streak broken, no cap.
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(800_000));
        c.on_ack(800_000);
        assert!(c.host_cap_kbps.is_none());
    }

    #[test]
    fn mode_switch_clears_the_learned_cap() {
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        // Mode-scoped cap drops; probe-measured link ceiling survives.
        c.on_mode_switch();
        assert!(c.host_cap_kbps.is_none());
        assert_eq!(c.ceiling_kbps, 1_400_000);
    }

    #[test]
    fn learned_cap_reprobes_after_a_sustained_clean_run() {
        // After a clean run parked at the cap, lift one step.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        // First re-probe is the fast interval.
        assert_eq!(c.cap_reprobe_after, CAP_REPROBE_WINDOWS_MIN);
        for i in 0..CAP_REPROBE_WINDOWS_MIN {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 20 + i))
            });
        }
        assert_eq!(c.host_cap_kbps, Some(794_000 + 794_000 / 8));
    }

    #[test]
    fn a_transient_refusal_does_not_pin_the_session() {
        // Transient cadence refusal at the start rate must not pin the session.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        let mut tick = 0u32;
        let mut windows_pinned = 0u32;
        // Two refused climbs at the same rate latch 20 Mbps.
        for _ in 0..2 {
            let k = run_clean(&mut c, start, tick, 4).expect("slow start should ask to climb");
            tick += 4;
            assert!(k > 20_000);
            c.on_ack(20_000);
        }
        assert_eq!(c.host_cap_kbps, Some(20_000));
        // Host recovered; grant whatever the re-probe asks.
        while c.current_kbps < 150_000 && windows_pinned < 400 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, tick))
            }) {
                c.on_ack(k);
            }
            tick += 1;
            windows_pinned += 1;
        }
        assert!(
            c.current_kbps >= 150_000,
            "still pinned at {} after {windows_pinned} windows",
            c.current_kbps
        );
        // 40 windows × 750 ms ≈ 30 s.
        assert!(
            windows_pinned <= 40,
            "took {windows_pinned} windows (~{} s) to escape a transient refusal",
            windows_pinned * 3 / 4
        );
        // Disproven cap is gone, not nudged.
        assert!(c.host_cap_kbps.is_none());
    }

    #[test]
    fn a_host_retarget_above_the_ceiling_raises_it() {
        // Unsolicited host re-target above the negotiated rate must raise the ceiling.
        let mut c = BitrateController::new(20_000);
        assert_eq!(c.ceiling_kbps, 20_000);
        c.on_ack(60_000); // unsolicited, no request outstanding
        assert_eq!(c.current_kbps, 60_000);
        assert_eq!(c.ceiling_kbps, 60_000);
        let start = Instant::now();
        // No step-down.
        assert_eq!(run_clean(&mut c, start, 0, 4), None);
        // Env cap still outranks the host retarget.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.on_ack(60_000);
        assert_eq!(c.ceiling_kbps, 50_000);
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
    }

    #[test]
    fn a_standing_cap_backs_its_reprobe_clock_off() {
        // Standing encoder ceiling: each re-learn doubles the re-probe interval.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.cap_reprobe_after, CAP_REPROBE_WINDOWS_MIN);
        // Park, lift, refuse at the same value: standing, so the clock doubles.
        let mut tick = 20;
        for round in 0..3 {
            let before = c.cap_reprobe_after;
            for _ in 0..before {
                let _ = c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, tick))
                });
                tick += 1;
            }
            let lifted = c.host_cap_kbps.expect("cap should still be latched");
            assert!(lifted > 794_000, "round {round}: the re-probe never lifted");
            // Host clamps the lift back to its real ceiling.
            c.last_requested_kbps = Some(lifted);
            c.on_ack(794_000);
            assert_eq!(c.host_cap_kbps, Some(794_000));
            assert_eq!(
                c.cap_reprobe_after,
                (before * 2).min(CAP_REPROBE_WINDOWS_MAX)
            );
        }
    }

    #[test]
    fn host_encode_latency_rise_backs_off() {
        // Only host encode time moves: two risen windows → ×0.7.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(11_500),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(12_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn deep_encode_excursion_is_severe() {
        // ≈1.5 frame budgets over baseline: severe, one window.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn rate_decrease_rebases_the_encode_baseline() {
        // Our own decrease must rebase encode; old baseline would train-fire.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(7_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            });
        }
        let _ = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            encode_mean_us: Some(12_000),
            actual_kbps: 1_000_000,
            ..WindowSample::at(ticks(start, 4))
        });
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(12_500),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(14_000)
        );
        // After rebase, 15 ms against the old 7 ms floor must read clean.
        c.on_ack(14_000);
        for i in 8..11 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(15_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }

    /// Re-seed the baseline `on_ack` cleared, then present `level`. Four seed
    /// windows stay under [`CLEAN_WINDOWS_TO_INCREASE`].
    fn encode_choke(
        c: &mut BitrateController,
        start: Instant,
        tick: &mut u32,
        level: i64,
    ) -> Option<u32> {
        for _ in 0..BASELINE_MIN_WINDOWS {
            let at = ticks(start, *tick);
            *tick += 1;
            // Ack a climb if taken so tests with headroom still work.
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(7_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(at)
            }) {
                c.on_ack(k);
            }
        }
        let at = ticks(start, *tick);
        *tick += 1;
        c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            encode_mean_us: Some(level),
            actual_kbps: 1_000_000,
            ..WindowSample::at(at)
        })
    }

    /// `n` clean windows with no encode sample; ack any climb.
    fn clean_run(c: &mut BitrateController, start: Instant, tick: &mut u32, n: u32) {
        for _ in 0..n {
            let at = ticks(start, *tick);
            *tick += 1;
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(at)
            }) {
                c.on_ack(k);
            }
        }
    }

    /// Encode-attributed backoffs at a level ×0.7 never moves, until stand-down.
    fn disarm_encode(c: &mut BitrateController, start: Instant, tick: &mut u32) {
        for _ in 0..=ENCODE_NOOP_BACKOFFS_TO_DISARM {
            let verdict = encode_choke(c, start, tick, 20_000);
            c.on_ack(verdict.expect("an unanswered encode rise must back off"));
        }
        assert!(c.encode_disarmed);
    }

    #[test]
    fn a_stood_down_encode_signal_re_arms_after_a_clean_run() {
        // Stand-down is evidence: a clean run must re-arm the encode signal.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode_reprobe_after, CAP_REPROBE_WINDOWS_MIN);

        // One window short of the run is not enough.
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(c.encode_disarmed);
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode_disarmed);

        // Re-armed: a fresh excursion backs off.
        assert!(encode_choke(&mut c, start, &mut tick, 40_000).is_some());
    }

    #[test]
    fn a_standing_contention_backs_the_re_arm_clock_off() {
        // Re-silenced after re-arm: standing, so the clock doubles. Start
        // high enough that two ratchets stay above [`FLOOR_KBPS`].
        let mut c = BitrateController::new(200_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN);
        assert!(!c.encode_disarmed);
        // Re-armed; contention still there.
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode_reprobe_after, CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_bad_window_restarts_the_re_arm_run() {
        // A spoiled window says nothing about the encoder; restart the run.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        let at = ticks(start, tick);
        tick += 1;
        // Flush: severe ×0.7 and resets the clean run.
        assert!(c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(at)
            })
            .is_some());
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(c.encode_disarmed, "the spoiled window must restart the run");
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode_disarmed);
    }

    #[test]
    fn the_encode_thresholds_follow_the_session_frame_budget() {
        // Same physical hiccup: severe at 120 Hz, ordinary at 60 Hz when
        // thresholds follow the session frame budget.
        let excursion = 23_700; // 7 ms baseline + ~one 60 Hz frame
        let mut hz120 = BitrateController::new(20_000);
        hz120.set_frame_budget(120);
        let mut tick = 0;
        let start = Instant::now();
        assert_eq!(
            encode_choke(&mut hz120, start, &mut tick, excursion),
            Some(14_000),
            "at 120 Hz that is ~2.8 frame budgets over baseline — severe, one window"
        );

        let mut hz60 = BitrateController::new(20_000);
        hz60.set_frame_budget(60);
        let mut tick = 0;
        assert_eq!(
            encode_choke(&mut hz60, start, &mut tick, excursion),
            None,
            "the same excursion is ~1 frame budget at 60 Hz — bad, but not severe"
        );
        // Second window still backs off: re-scaled, not weakened.
        let at = ticks(start, tick + 1);
        assert_eq!(
            hz60.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(excursion),
                actual_kbps: 1_000_000,
                ..WindowSample::at(at)
            }),
            Some(14_000)
        );
    }

    #[test]
    fn unactuatable_encode_rises_disarm_the_down_driver() {
        // GPU contention holds encode time up; `on_ack` re-seeds the baseline,
        // so only the firing level notices the backoffs are no-ops.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;

        // First choke is a legitimate knee sample.
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(14_000));
        c.on_ack(14_000);
        // Fires again no lower. One no-op is not a verdict: a real knee looks like this.
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(9_800));
        c.on_ack(9_800);
        assert_eq!(c.encode_noop_backoffs, 1);
        assert!(!c.encode_disarmed);
        // Twice: rate is not the lever. This backoff still lands; then stand-down.
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(6_860));
        c.on_ack(6_860);
        assert!(c.encode_disarmed);

        // Same excursion no longer moves the rate…
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), None);
        // …and the session climbs out instead of parking.
        c.set_ceiling(200_000);
        assert!(
            run_clean(&mut c, start, tick, 8).is_some_and(|k| k > 6_860),
            "a disarmed encode signal must not keep the session pinned"
        );
    }

    #[test]
    fn an_encode_backoff_that_helps_keeps_the_down_driver_armed() {
        // ×0.7 that actually drops encode time must not disarm.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        assert_eq!(encode_choke(&mut c, start, &mut tick, 40_000), Some(14_000));
        c.on_ack(14_000);
        assert_eq!(encode_choke(&mut c, start, &mut tick, 22_000), Some(9_800));
        c.on_ack(9_800);
        assert_eq!(c.encode_noop_backoffs, 0);
        assert!(!c.encode_disarmed);
    }

    #[test]
    fn a_network_driven_backoff_breaks_the_encode_streak() {
        // Network distress with elevated encode time must not count toward disarm.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(14_000));
        c.on_ack(14_000);
        assert_eq!(c.encode_backoff_us, 20_000);
        // Re-seed the encode baseline…
        for _ in 0..BASELINE_MIN_WINDOWS {
            let at = ticks(start, tick);
            tick += 1;
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(at)
                }),
                None
            );
        }
        // Encode excursion + flush: flush is the explanation, streak resets.
        let at = ticks(start, tick);
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(at)
            }),
            Some(9_800)
        );
        assert_eq!(c.encode_backoff_us, 0);
        assert_eq!(c.encode_noop_backoffs, 0);
        assert!(!c.encode_disarmed);
    }

    #[test]
    fn env_max_mbps_caps_every_learned_ceiling() {
        // Injected 50 Mbps env cap outranks an 886 Mbps probe.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_ceiling(886_312);
        assert_eq!(c.ceiling_kbps, 50_000);
        // Measurement under the cap stands.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_ceiling(40_000);
        assert_eq!(c.ceiling_kbps, 40_000);
        // Climb honors it: 20→40→50, then quiet.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_ceiling(886_312);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(40_000));
        c.on_ack(40_000);
        assert_eq!(run_clean(&mut c, start, 2, 1), Some(50_000));
        c.on_ack(50_000);
        assert_eq!(run_clean(&mut c, start, 4, 20), None);
    }

    #[test]
    fn a_session_above_the_env_cap_steps_down_to_it_once() {
        // Env cap binds the negotiated start, not only probe-learned ceilings.
        let mut c = BitrateController::with_ceiling_cap(100_000, Some(50_000));
        assert_eq!(c.ceiling_kbps, 50_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
        // Host answers higher: cannot go there; do not re-ask every cooldown.
        c.on_ack(80_000);
        assert_eq!(run_clean(&mut c, start, 2, 20), None);
        // Same clamped target: no new ask.
        c.set_ceiling(90_000);
        assert_eq!(run_clean(&mut c, start, 24, 20), None);
    }

    fn calm_window(c: &mut BitrateController, at: Instant) {
        // Calm, unutilized: seed baselines, decide nothing.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 2_000,
                ..WindowSample::at(at)
            }),
            None
        );
    }

    /// Climb to `target` on fully-utilized windows, acking each step. 600-window bound.
    fn climb_to(c: &mut BitrateController, start: Instant, tick: &mut u32, target: u32) {
        for _ in 0..600 {
            if c.current_kbps >= target {
                return;
            }
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, *tick))
            }) {
                c.on_ack(k);
            }
            *tick += 1;
        }
        panic!(
            "no climb to {target} within 600 windows (stuck at {})",
            c.current_kbps
        );
    }

    /// One decode-severe window at the current rate. Steps past cooldown first.
    fn choke(c: &mut BitrateController, start: Instant, tick: &mut u32) -> Option<u32> {
        *tick += 2;
        let r = c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(60_000),
            actual_kbps: c.current_kbps,
            ..WindowSample::at(ticks(start, *tick))
        });
        *tick += 1;
        r
    }

    /// Choke, ack the ×0.7, re-climb, choke inside ±1/8. Returns the latched cap.
    fn latch_knee(c: &mut BitrateController, start: Instant, tick: &mut u32) -> u32 {
        for _ in 0..4 {
            calm_window(c, ticks(start, *tick));
            *tick += 1;
        }
        let knee = c.current_kbps;
        let r1 = choke(c, start, tick).expect("first choke must back off");
        assert!(c.decode_cap_kbps.is_none(), "one event must not latch");
        c.on_ack(r1);
        climb_to(c, start, tick, knee - knee / DECODE_CAP_SIMILAR_DIV);
        let rate = c.current_kbps;
        let r2 = choke(c, start, tick).expect("re-climb choke must back off");
        assert_eq!(c.decode_cap_kbps, Some(rate - rate / 16));
        c.on_ack(r2);
        rate - rate / 16
    }

    /// Stall-shaped: current/10 delivered, flush + kf-storm. Severe, but starved.
    fn stall_choke(c: &mut BitrateController, start: Instant, tick: &mut u32) -> Option<u32> {
        *tick += 2;
        let r = c.on_window(&WindowSample {
            actual_kbps: c.current_kbps / 10,
            flushed: true,
            recovery_kf: RECOVERY_KF_SEVERE,
            ..WindowSample::at(ticks(start, *tick))
        });
        *tick += 1;
        r
    }

    #[test]
    fn capture_stall_windows_never_latch_a_decode_cap() {
        // Repeated stall-shaped backoffs at the same rate must not latch a knee.
        let mut c = BitrateController::new(240_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        climb_to(&mut c, start, &mut t, 400_000);
        let at = c.current_kbps;
        let r1 = stall_choke(&mut c, start, &mut t).expect("stall damage still backs off");
        assert!(
            c.decode_cap_kbps.is_none(),
            "one starved window must not latch"
        );
        assert_eq!(
            c.decode_backoff_kbps, 0,
            "a starved window is not a knee sample — no reference recorded"
        );
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, at - at / DECODE_CAP_SIMILAR_DIV);
        let r2 = stall_choke(&mut c, start, &mut t).expect("second stall edge backs off too");
        c.on_ack(r2);
        assert!(
            c.decode_cap_kbps.is_none(),
            "a starved pair at the same rate must not latch a phantom knee"
        );
    }

    #[test]
    fn starved_window_preserves_the_knee_reference() {
        // Real knee, then stall, then re-climb choke: stall neither latches nor erases.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        let knee = c.current_kbps;
        let r1 = choke(&mut c, start, &mut t).expect("real choke backs off");
        assert_eq!(
            c.decode_backoff_kbps, knee,
            "real choke records the reference"
        );
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, knee - knee / DECODE_CAP_SIMILAR_DIV);
        let r2 = stall_choke(&mut c, start, &mut t).expect("stall edge backs off");
        assert_eq!(
            c.decode_backoff_kbps, knee,
            "the starved window must not erase the real reference"
        );
        assert!(c.decode_cap_kbps.is_none(), "and must not latch against it");
        c.on_ack(r2);
        climb_to(&mut c, start, &mut t, knee - knee / DECODE_CAP_SIMILAR_DIV);
        let rate = c.current_kbps;
        choke(&mut c, start, &mut t).expect("genuine re-climb choke backs off");
        assert_eq!(
            c.decode_cap_kbps,
            Some(rate - rate / 16),
            "the genuine pair still latches around the starved interruption"
        );
    }

    /// Starved window: encode_us is not a measurement of encode cost. Withheld;
    /// the window decides nothing. The same excursion at full delivery still
    /// backs off.
    #[test]
    fn a_starved_window_cannot_back_off_on_host_encode_time_alone() {
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_000);
        let start = Instant::now();
        let mut t = 0;
        // Half-utilized: encode samples count, no climb, `current_kbps` stays.
        for _ in 0..BASELINE_MIN_WINDOWS {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(3_500),
                    decode_mean_us: Some(200),
                    encode_mean_us: Some(2_800),
                    actual_kbps: 10_000,
                    ..WindowSample::at(ticks(start, t))
                }),
                None
            );
            t += 1;
        }
        assert!(
            c.probing,
            "slow start is still armed going into the rebuild"
        );

        let verdict = c.on_window(&WindowSample {
            owd_mean_us: Some(15_711),
            decode_mean_us: Some(129),
            encode_mean_us: Some(15_063),
            actual_kbps: 390,
            ..WindowSample::at(ticks(start, t))
        });
        t += 1;
        assert_eq!(
            verdict, None,
            "a host-local rebuild must not move the rate: nothing was lost and nothing was slow"
        );
        assert_eq!(c.current_kbps, 20_000, "and the rate is untouched");
        assert!(
            c.probing,
            "nor may it retire slow start — recovery would crawl at +6 % per six windows"
        );

        // Same encode excursion at full delivery is still severe.
        let verdict = c.on_window(&WindowSample {
            owd_mean_us: Some(3_600),
            decode_mean_us: Some(210),
            encode_mean_us: Some(15_063),
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, t))
        });
        assert!(
            verdict.is_some_and(|k| k < 20_000),
            "a real encode excursion at full delivery still backs off, got {verdict:?}"
        );
    }

    #[test]
    fn decode_cap_latches_when_the_reclimb_chokes_at_the_same_knee() {
        // Choke, recover, re-climb, choke inside the band: latch.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        latch_knee(&mut c, start, &mut t);
        // Climbs must stop at the knee, not the 900 Mbps link ceiling.
        let mut max_req = 0;
        for _ in 0..62 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, t))
            }) {
                // Cap in force at decision time. Re-probe may lift it; the link
                // ceiling must not.
                assert!(
                    k <= c.decode_cap_kbps.unwrap(),
                    "climb past the decode cap: {k}"
                );
                max_req = max_req.max(k);
                c.on_ack(k);
            }
            t += 1;
        }
        assert!(
            max_req < 600_000,
            "the decode knee stopped binding: climbed to {max_req}"
        );
    }

    #[test]
    fn a_single_flush_or_dissimilar_backoffs_never_latch_a_decode_cap() {
        // Lone flush at a climbed-to rate backs off but teaches nothing…
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let r1 = c
            .on_window(&WindowSample {
                actual_kbps: 490_000,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        assert_eq!(r1, 350_000);
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r1);
        // …loss-driven backoff at the re-climbed rate breaks the streak…
        climb_to(&mut c, start, &mut t, 460_000);
        t += 2;
        let r2 = c
            .on_window(&WindowSample {
                dropped: 1,
                actual_kbps: c.current_kbps,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("loss must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(
            c.decode_backoff_kbps, 0,
            "a climbed-to non-decode backoff must reset the knee reference"
        );
        c.on_ack(r2);
        // …next flush is a first decode event again — still no latch…
        climb_to(&mut c, start, &mut t, 460_000);
        t += 2;
        let r3 = c
            .on_window(&WindowSample {
                actual_kbps: c.current_kbps,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r3);
        // …dissimilar climbed-to rates share no knee.
        let dissimilar_target = c.current_kbps + 20_000;
        climb_to(&mut c, start, &mut t, dissimilar_target);
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                actual_kbps: c.current_kbps,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        assert!(c.decode_cap_kbps.is_none());
    }

    #[test]
    fn decode_cap_reprobes_after_a_sustained_clean_run() {
        // After a clean run parked at the cap, lift +12.5 %.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let knee = latch_knee(&mut c, start, &mut t);
        // Host parks at the knee (unsolicited re-target).
        c.on_ack(knee);
        for _ in 0..CAP_REPROBE_WINDOWS_MIN {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            });
            t += 1;
        }
        assert_eq!(c.decode_cap_kbps, Some(knee + knee / 8));
    }

    #[test]
    fn mode_switch_clears_the_decode_cap() {
        // Decode cap is mode-scoped; probe-measured link ceiling survives.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let _ = latch_knee(&mut c, start, &mut t);
        c.on_mode_switch();
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(c.ceiling_kbps, 900_000);
    }

    #[test]
    fn ordinary_decode_bad_window_pairs_latch_the_knee_field_trace() {
        // Ordinary two-window decode rise (15–45 ms) must latch, not reset the streak.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_788);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        // One heavy-loss window ends slow start so the climb is additive.
        let _ = c.on_window(&WindowSample {
            loss_ppm: HEAVY_LOSS_PPM,
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(8_000),
            actual_kbps: 15_000,
            ..WindowSample::at(ticks(start, t))
        });
        t += 1;
        // First sample: flush + 40 ms decode.
        climb_to(&mut c, start, &mut t, 417_277);
        let first = c.current_kbps;
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(8_313),
                decode_mean_us: Some(40_087),
                actual_kbps: first,
                flushed: true,
                recovery_kf: 1,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush choke must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(c.decode_backoff_kbps, first);
        c.on_ack(r1);
        // Two consecutive ~26 ms decode-bad windows: ordinary path, latch.
        climb_to(&mut c, start, &mut t, 440_000);
        let second = c.current_kbps;
        t += 2;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(6_877),
                decode_mean_us: Some(26_474),
                actual_kbps: second,
                ..WindowSample::at(ticks(start, t))
            }),
            None,
            "the first bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(6_877),
                decode_mean_us: Some(26_474),
                actual_kbps: second,
                ..WindowSample::at(ticks(start, t))
            }),
            Some(((second as u64 * 7 / 10) as u32).max(FLOOR_KBPS))
        );
        assert_eq!(
            c.decode_cap_kbps,
            Some(second - second / 16),
            "two decode-bad windows are knee evidence"
        );
    }

    #[test]
    fn cascade_backoffs_neither_sample_nor_erase_the_knee_reference() {
        // Drain backoff at the already-reduced rate must neither latch nor
        // erase; the re-climb choke latches against the original sample.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        let r1 = choke(&mut c, start, &mut t).expect("knee choke must back off");
        assert_eq!(c.decode_backoff_kbps, 500_000);
        c.on_ack(r1);
        t += 2;
        let r2 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(43_305),
                actual_kbps: r1,
                flushed: true,
                recovery_kf: 1,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("drain flush must back off");
        t += 1;
        assert!(
            c.decode_cap_kbps.is_none(),
            "a drain backoff must not latch"
        );
        assert_eq!(
            c.decode_backoff_kbps, 500_000,
            "…nor erase the knee reference"
        );
        c.on_ack(r2);
        climb_to(&mut c, start, &mut t, 460_000);
        let rate = c.current_kbps;
        choke(&mut c, start, &mut t).expect("re-climb choke must back off");
        assert_eq!(c.decode_cap_kbps, Some(rate - rate / 16));
    }

    #[test]
    fn keyframe_storms_on_a_clean_link_latch_the_knee() {
        // Kf-storm on a clean link (no decode latency) is decode evidence.
        let mut c = BitrateController::new(300_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 300_000,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("keyframe storm must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, 280_000);
        let rate = c.current_kbps;
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: rate,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("second storm must back off");
        assert_eq!(c.decode_cap_kbps, Some(rate - rate / 16));
    }

    #[test]
    fn keyframe_storms_with_real_loss_teach_no_knee() {
        // Same storm with heavy loss is network-attributed: reset, no latch.
        let mut c = BitrateController::new(300_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 300_000,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("clean storm must back off");
        t += 1;
        assert_eq!(c.decode_backoff_kbps, 300_000);
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, 280_000);
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                loss_ppm: SEVERE_LOSS_PPM,
                owd_mean_us: Some(10_000),
                actual_kbps: c.current_kbps,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("lossy storm must back off");
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(
            c.decode_backoff_kbps, 0,
            "a loss-attributed storm must reset the knee reference"
        );
    }

    #[test]
    fn a_mixed_streak_without_decode_attribution_is_no_knee_evidence() {
        // Mixed streak (one OWD, one decode): not a knee sample.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(40_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            }),
            None,
            "one OWD-bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(26_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            }),
            Some(350_000)
        );
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(
            c.decode_backoff_kbps, 0,
            "a mixed-attribution backoff must reset the knee reference"
        );
    }

    #[test]
    fn ack_silence_disables_the_controller() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut sent = 0;
        let mut i = 0;
        // Never ack: exactly [`MAX_UNACKED`] requests, then silence.
        while i < 60 {
            if c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            })
            .is_some()
            {
                sent += 1;
            }
            i += 1;
        }
        assert_eq!(sent, MAX_UNACKED);
    }
    /// One clean, utilized, full-rate 120 Hz window with a given decode mean.
    fn loaded(c: &mut BitrateController, at: Instant, decode_us: i64) -> Option<u32> {
        c.on_window(&WindowSample {
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(decode_us),
            actual_kbps: c.current_kbps,
            activity: WindowActivity::Active(90),
            ..WindowSample::at(at)
        })
    }

    /// A 120 Hz controller with room to climb and seeded baselines.
    fn seeded_120(start_kbps: u32) -> (BitrateController, Instant, u32) {
        let mut c = BitrateController::new(start_kbps);
        c.set_ceiling(400_000);
        c.set_frame_budget(120);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        (c, start, t)
    }

    /// Clean windows at `decode_us` until the controller asks for a rate, acked.
    fn until_request(
        c: &mut BitrateController,
        start: Instant,
        t: &mut u32,
        decode_us: i64,
        max: u32,
    ) -> Option<u32> {
        for _ in 0..max {
            let r = loaded(c, ticks(start, *t), decode_us);
            *t += 1;
            if let Some(k) = r {
                c.on_ack(k);
                return Some(k);
            }
        }
        None
    }

    #[test]
    fn decode_headroom_parks_a_clean_climb() {
        // 7 000 µs of 8 333: 84 % — inside the hold band. No climb, cap at the rate.
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..12 {
            assert_eq!(
                loaded(&mut c, ticks(start, t), 7_000),
                None,
                "window {t} climbed"
            );
            t += 1;
        }
        assert_eq!(c.decode_cap_kbps, Some(100_000));
        assert_eq!(c.current_kbps, 100_000);
    }

    #[test]
    fn a_lifted_decode_cap_is_held_when_headroom_stays() {
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..4 {
            assert_eq!(loaded(&mut c, ticks(start, t), 7_000), None);
            t += 1;
        }
        // The ladder lifts the parked cap +12.5 % and the climb takes it.
        let lift = until_request(&mut c, start, &mut t, 7_000, 40).expect("ladder lift");
        assert_eq!(lift, 112_500);
        // Flat latency above the old cap: the lift stands.
        assert_eq!(until_request(&mut c, start, &mut t, 7_000, 6), None);
        assert_eq!(c.current_kbps, 112_500);
        assert_eq!(c.decode_cap_kbps, Some(112_500));
        assert!(c.decode_probe.is_none(), "verdict must have been reached");
    }

    #[test]
    fn a_lift_is_undone_when_the_decoder_loses_headroom() {
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..4 {
            assert_eq!(loaded(&mut c, ticks(start, t), 7_000), None);
            t += 1;
        }
        assert_eq!(
            until_request(&mut c, start, &mut t, 7_000, 40),
            Some(112_500)
        );
        // 7 700 µs: +700 over the reference (≥ 5 % of the budget) and 92 %.
        let back = until_request(&mut c, start, &mut t, 7_700, 6).expect("lift undone");
        assert_eq!(back, 100_000);
        assert_eq!(c.decode_cap_kbps, Some(100_000));
        assert_eq!(c.decode_cap_reprobe_after, CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_decoder_without_headroom_retreats_and_keeps_it_when_latency_follows() {
        let (mut c, start, mut t) = seeded_120(100_000);
        // 7 800 µs: 93 % — retreat one notch, not the ×0.7 congestion backoff.
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        assert_eq!(r, 87_500);
        assert_eq!(c.decode_cap_kbps, Some(87_500));
        c.on_ack(r);
        // Latency followed (−1 300 µs): the retreat stands, nothing else moves.
        assert_eq!(until_request(&mut c, start, &mut t, 6_500, 8), None);
        assert_eq!(c.current_kbps, 87_500);
        assert!(!c.decode_headroom_disarmed);
    }

    #[test]
    fn a_flat_decoder_gets_its_rate_back_and_the_driver_stands_down() {
        let (mut c, start, mut t) = seeded_120(100_000);
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        c.on_ack(r);
        // Same latency at the lower rate: not a function of the rate.
        let restore = until_request(&mut c, start, &mut t, 7_800, 6).expect("restore");
        assert_eq!(restore, 100_000);
        assert!(c.decode_headroom_disarmed);
        assert_eq!(c.decode_cap_kbps, Some(100_000));
        // Stood down: the same 93 % no longer retreats.
        assert_eq!(until_request(&mut c, start, &mut t, 7_800, 10), None);
        assert_eq!(c.current_kbps, 100_000);
    }

    #[test]
    fn decode_thresholds_follow_the_frame_budget() {
        // +6 000 µs over the baseline: half a 120 Hz budget (4 166) is a rise,
        // the 15 ms no-budget default is not.
        let mut hz120 = BitrateController::new(100_000);
        hz120.set_frame_budget(120);
        let mut plain = BitrateController::new(100_000);
        let start = Instant::now();
        for i in 0..5 {
            assert_eq!(loaded(&mut hz120, ticks(start, i), 3_000), None);
            assert_eq!(loaded(&mut plain, ticks(start, i), 3_000), None);
        }
        loaded(&mut hz120, ticks(start, 5), 9_000);
        loaded(&mut plain, ticks(start, 5), 9_000);
        assert!(!hz120.probing, "a budget-sized rise ends slow start");
        assert!(plain.probing, "below the absolute default it is not a rise");
    }

    #[test]
    fn frame_driven_windows_do_not_judge_headroom() {
        // 30 of 90 expected frames: bigger frames per budget, so 93 % here
        // says nothing about the full-rate load. Climbs are still allowed.
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..6 {
            let r = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(7_800),
                actual_kbps: c.current_kbps,
                activity: WindowActivity::Active(30),
                ..WindowSample::at(ticks(start, t))
            });
            t += 1;
            if let Some(k) = r {
                assert!(k > c.current_kbps, "only climbs, never a retreat");
                c.on_ack(k);
            }
        }
        assert_eq!(c.decode_cap_kbps, None);
    }
}
