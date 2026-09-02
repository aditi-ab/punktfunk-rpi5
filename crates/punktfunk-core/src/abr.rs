//! Adaptive bitrate: AIMD controller for the Automatic bitrate setting.
//!
//! Runs in [`crate::client`]'s data-plane pump on the 750 ms cadence shared with
//! [`crate::quic::LossReport`]. FEC absorbs short random loss; this controller
//! asks the host for a different encoder rate via [`crate::quic::SetBitrate`]
//! when congestion persists.
//!
//! Severe windows (unrecoverable frame, flush, ≥6 % loss, deep decode or encode
//! rise, keyframe storm) back off ×0.7 immediately. Ordinary congestion needs
//! two consecutive bad windows. Recovery is slow start (double, bounded by
//! proven-throughput headroom) then additive (+~6 % after ~4.5 s). Each change
//! rebuilds the encoder (IDR); silence after [`MAX_UNACKED`] unanswered requests.
//!
//! Caps are learned: two identical short host acks latch `host_cap_kbps`; two
//! similar decode-driven backoffs latch `decode_cap_kbps`. Both re-probe on
//! [`CAP_REPROBE_WINDOWS_MIN`]. Climbs require utilization (delivered ≈ target)
//! and stay within ×1.5 of the windowed proven mark. Tests in this module pin
//! the contract.

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
/// One report window, in µs — the pump's 750 ms `ADAPT_REPORT_INTERVAL`.
const WINDOW_US: i64 = 750_000;
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
/// Decode-stage rise (hand-off → output) that marks the decoder falling
/// behind. Loss/OWD never see a queue inside the decoder; 15 ms of standing
/// decode queue is unambiguous at any streamable frame rate.
const DECODE_RISE_US: i64 = 15_000;
/// Decode-stage rise that is severe (one window). 45 ms is several frames of
/// backlog; waiting a second window is 750 ms more of visible damage.
const DECODE_SEVERE_US: i64 = 45_000;
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

/// Upper bound on bitrate this stream's shape could use, in kbps.
///
/// The probe-measured ceiling is pure link capacity (`delivered × 0.7`) with
/// no term for pixels. A CBR encoder fills whatever target it is handed, so
/// utilization never supplies one. Deliberately generous: a bound on the
/// absurd, not a quality opinion. Explicit-bitrate and PyroWave sessions
/// never reach here.
pub(crate) fn stream_ceiling_kbps(
    width: u32,
    height: u32,
    refresh_hz: u32,
    codec: u8,
    bit_depth: u8,
    chroma_format: u8,
) -> u32 {
    let pixel_rate = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(refresh_hz.max(1) as u64);
    if pixel_rate == 0 {
        return u32::MAX;
    }
    // Milli-bits per pixel so the arithmetic stays integer. H.264 is the
    // least efficient of the three and is allowed correspondingly more.
    let milli_bpp: u64 = match codec {
        crate::quic::CODEC_H264 => 1_000,
        _ => 750,
    };
    // 10-bit is 25 % more sample depth; 4:4:4 is twice the chroma of 4:2:0
    // → half again as many samples overall.
    let milli_bpp = if bit_depth >= 10 {
        milli_bpp * 5 / 4
    } else {
        milli_bpp
    };
    let milli_bpp = if chroma_format == crate::quic::CHROMA_IDC_444 {
        milli_bpp * 3 / 2
    } else {
        milli_bpp
    };
    // bits/s = pixel_rate × bpp; kbps = that / 1000. The milli- factor and
    // the kbps divisor cancel: pixel_rate × milli_bpp / 1_000_000.
    u32::try_from(pixel_rate.saturating_mul(milli_bpp) / 1_000_000).unwrap_or(u32::MAX)
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

/// One decision per report window; `Some(kbps)` = send a [`crate::quic::SetBitrate`].
pub(crate) struct BitrateController {
    /// `false` = permanently off (explicit bitrate, old host, or ack silence).
    enabled: bool,
    /// Host-acked encoder rate. Requests are not assumed applied.
    current_kbps: u32,
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
    encode_disarmed: bool,
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
    /// Highest clean delivered rate of the current/previous
    /// [`PROVEN_BUCKET_WINDOWS`] buckets. Shrinking capacity is the reactive
    /// decode signal's job.
    proven_cur_kbps: u32,
    proven_prev_kbps: u32,
    proven_bucket_windows: u32,
    /// Consecutive fully-idle windows. First active window after
    /// [`IDLE_WINDOWS_TO_REARM`] re-arms slow start.
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
    fn with_ceiling_cap(start_kbps: u32, ceiling_cap_kbps: Option<u32>) -> Self {
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

    /// One report window; `Some(kbps)` is the rate to request. `None` on an
    /// argument means that signal is absent (`active_frames: None` = older host,
    /// legacy wall-clock arithmetic).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_window(
        &mut self,
        now: Instant,
        dropped: u64,
        loss_ppm: u32,
        owd_mean_us: Option<i64>,
        decode_mean_us: Option<i64>,
        encode_mean_us: Option<i64>,
        actual_kbps: u32,
        flushed: bool,
        recovery_kf: u32,
        active_frames: Option<u32>,
    ) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        if self.unacked >= MAX_UNACKED {
            // Host never answered: older build. Quiet, don't spam unknown.
            self.enabled = false;
            tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            return None;
        }
        // Repeat-only window: stillness, not this rate. Skip baselines, climb
        // credit, re-probe, encode. Loss/flush/drop keep full power. `None`
        // (older host) is never idle.
        let idle = active_frames == Some(0);
        if idle {
            self.idle_windows = self.idle_windows.saturating_add(1);
        } else {
            // First active window after a real idle stretch: re-arm slow start
            // and clear the cooldown so this window can climb.
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
        let owd_mean_us = owd_mean_us.filter(|_| !idle);
        let decode_mean_us = decode_mean_us.filter(|_| !idle);
        // No severe OWD tier: a standing queue is congestion, not visible
        // damage, so it always takes the two-window path.
        let (owd_bad, _) = score_baseline(&mut self.owd_means, owd_mean_us, OWD_RISE_US, i64::MAX);
        // Decode rise ends slow start immediately; a far-past-baseline
        // excursion is severe (one window).
        let (decode_bad, decode_severe) = score_baseline(
            &mut self.decode_means,
            decode_mean_us,
            DECODE_RISE_US,
            DECODE_SEVERE_US,
        );
        // Starved: encode_us is not a measurement here. `current_kbps` does
        // not move in this function, so the backoff block reads the same value.
        let starved =
            (actual_kbps as u64) * (STARVED_DELIVERY_DIV as u64) < self.current_kbps as u64;
        // Encode: the only signal that can descend on a clean LAN. Withheld
        // when starved (mean describes the interruption), disarmed, or idle.
        // Passed as absent so it cannot teach the baseline either. Loss,
        // flush, and drop keep full power.
        let (encode_rise_us, encode_severe_us) = self.encode_thresholds();
        let encode_usable = !starved && !self.encode_disarmed && !idle;
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
            // Any congestion ends slow start until a later idle-onset re-arm.
            self.probing = false;
        } else if idle {
            // Neutral: stillness is neither climb credit nor a cleared streak.
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
            } else if !idle && self.current_kbps >= cap.saturating_sub(cap / 16) {
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
            // Same `!idle` rule as the host-cap clock.
            } else if !idle && self.current_kbps >= cap.saturating_sub(cap / 16) {
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
            } else if !idle {
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
        // Climbs need a utilized clean window and stay within ×1.5 of proven.
        // Frame-driven sources never reach ¾ of the wall-clock target, so the
        // ¾ gate is prorated by active/expected, floored at
        // [`MIN_ACTIVE_FRAMES_TO_CLIMB`]. Older host: wall-clock arithmetic.
        let legacy_utilized =
            actual_kbps as u64 * UTILIZATION_DEN >= self.current_kbps as u64 * UTILIZATION_NUM;
        // Prorate utilization AND proven-headroom together or they deadlock
        // (a 35 fps source's wall-clock wire rate never exceeds ~39 % of target).
        let proration = match (active_frames, self.frame_budget_us) {
            (Some(n), Some(budget_us))
                if budget_us > 0 && n > 0 && (n as i64) < WINDOW_US / budget_us =>
            {
                Some((n as u64, ((WINDOW_US / budget_us).max(1)) as u64))
            }
            _ => None,
        };
        let utilized = match (active_frames, proration) {
            (Some(0), _) => false,
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
            out = c.on_window(
                ticks(start, i),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            );
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
            c.on_window(
                now,
                5,
                900_000,
                Some(500_000),
                None,
                None,
                1_000_000,
                true,
                0,
                None,
            ),
            None
        );
    }

    #[test]
    fn two_ordinary_bad_windows_step_down_multiplicatively() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // 2–6 % loss is ordinary: one window is a blip.
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                25_000,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        // Second consecutive ordinary-bad window: ×0.7.
        assert_eq!(
            c.on_window(
                ticks(start, 1),
                0,
                25_000,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Still bad after cooldown: another ×0.7 from the acked rate.
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                0,
                25_000,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 7),
                0,
                25_000,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(9_800)
        );
    }

    #[test]
    fn severe_window_backs_off_immediately() {
        // Unrecoverable frame skips the two-window wait…
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            Some(14_000)
        );
        // …and so does a jump-to-live flush.
        let mut c = BitrateController::new(20_000);
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                0,
                None,
                None,
                None,
                1_000_000,
                true,
                0,
                None
            ),
            Some(14_000)
        );
        // …and ≥6 % window loss.
        let mut c = BitrateController::new(20_000);
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                80_000,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(14_000)
        );
    }

    #[test]
    fn cooldown_blocks_back_to_back_steps() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Tick 1 = 750 ms, inside cooldown; tick 2 = 1.5 s, fires.
        assert_eq!(
            c.on_window(
                ticks(start, 1),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 2),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            Some(9_800)
        );
    }

    #[test]
    fn floor_is_never_crossed() {
        let mut c = BitrateController::new(2_500);
        let start = Instant::now();
        // ×0.7 of 2500 = 1750 < floor → 2000.
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            Some(2_000)
        );
        c.on_ack(2_000);
        // At the floor, further bad windows request nothing.
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 7),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            None
        );
    }

    #[test]
    fn sustained_clean_recovers_toward_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
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
            if let Some(k) = c.on_window(
                ticks(start, i),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
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
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(40_000)
        );
        c.on_ack(40_000);
        // Severe: immediate ×0.7, slow start over.
        assert_eq!(
            c.on_window(
                ticks(start, 2),
                1,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(28_000)
        );
        c.on_ack(28_000);
        // Next climb is additive, after 6 clean windows.
        let mut next = None;
        for i in 3..12 {
            next = c.on_window(
                ticks(start, i),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            );
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
            c.on_window(
                Instant::now(),
                0,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            None
        );
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(10_000); // below the negotiated start → ignored
        assert_eq!(c.ceiling_kbps, 20_000);
    }

    /// Bound cuts an absurd probe ceiling and must not trim a session anyone runs.
    #[test]
    fn the_stream_bound_cuts_the_absurd_and_spares_the_ordinary() {
        use crate::quic::{CHROMA_IDC_420, CHROMA_IDC_444, CODEC_H264, CODEC_HEVC};

        // 1440p120 HEVC Main10 4:2:0: bound must sit under the ~460 Mbps decode knee.
        let field = stream_ceiling_kbps(2560, 1440, 120, CODEC_HEVC, 10, CHROMA_IDC_420);
        assert!(
            field < 657_000,
            "the bound must actually bind on the field case, got {field}"
        );
        assert!(
            field < 460_000,
            "and land under the decode knee this session found, got {field}"
        );

        // 1080p60 HEVC 8-bit: 80–100 Mbps sessions must keep headroom.
        let ordinary = stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_420);
        assert!(
            ordinary >= 90_000,
            "an ordinary 1080p60 session must keep its headroom, got {ordinary}"
        );

        // H.264, 10-bit, and 4:4:4 are each allowed more.
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_H264, 8, CHROMA_IDC_420) > ordinary,
            "H.264 is allowed more than HEVC"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 10, CHROMA_IDC_420) > ordinary,
            "10-bit is allowed more than 8-bit"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_444) > ordinary,
            "4:4:4 is allowed more than 4:2:0"
        );
        // Degenerate mode must not bound at zero.
        assert_eq!(
            stream_ceiling_kbps(0, 0, 0, CODEC_HEVC, 8, CHROMA_IDC_420),
            u32::MAX
        );
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    None,
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        // +40 ms OWD, zero loss: two windows → back off.
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(50_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 5),
                0,
                0,
                Some(52_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    Some(8_000),
                    None,
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        // +30 ms decode, zero loss, flat OWD: two windows → ×0.7.
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(10_000),
                Some(38_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 5),
                0,
                0,
                Some(10_000),
                Some(40_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_storm_alone_is_a_congestion_signal() {
        // Pristine link, no latency signal, two kf asks per window: ordinary-bad.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                2,
                None,
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 1),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                2,
                None,
            ),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_saturation_is_severe() {
        // Emitters throttle at 100 ms: 4+ asks in 750 ms is severe.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                4,
                None,
            ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    None,
                    1_000_000,
                    false,
                    1,
                    None,
                ),
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
            if let Some(k) = c.on_window(
                ticks(start, i * 2),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
                last = k;
                c.on_ack(k);
            }
        }
        assert_eq!(last, 300_000, "slow start should reach the probed ceiling");
        // +30 ms decode: climb stops.
        assert_eq!(
            c.on_window(
                ticks(start, 20),
                0,
                0,
                Some(10_000),
                Some(38_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        // Second backed-up window: ×0.7, not park at the link ceiling.
        assert_eq!(
            c.on_window(
                ticks(start, 22),
                0,
                0,
                Some(10_000),
                Some(40_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    Some(mean),
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None,
                "window {i} fired off a baseline of fewer than {BASELINE_MIN_WINDOWS} samples"
            );
        }
        // With 4 samples, a sustained rise still backs off.
        assert_eq!(
            c.on_window(
                ticks(start, 8),
                0,
                0,
                Some(10_000),
                None,
                Some(20_000),
                1_000_000,
                false,
                0,
                None,
            ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    Some(8_000),
                    None,
                    2_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        // First utilized window: ×1.5 over proven 18 000 → 27 000, not 2× to 40 000.
        assert_eq!(
            c.on_window(
                ticks(start, 12),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                18_000,
                false,
                0,
                None,
            ),
            Some(27_000)
        );
        // Zero active frames never authorizes a climb, whatever delivered claims.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        c.set_frame_budget(60);
        for i in 0..12 {
            assert_eq!(
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    Some(8_000),
                    None,
                    18_000,
                    false,
                    0,
                    Some(0),
                ),
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
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                None,
                None,
                8_000,
                false,
                0,
                Some(26),
            ),
            Some(30_923)
        );
        // Under [`MIN_ACTIVE_FRAMES_TO_CLIMB`]: not utilized.
        let mut d = BitrateController::new(20_000);
        d.set_stream_cap(100_000);
        d.set_ceiling(60_000);
        d.set_frame_budget(90);
        assert_eq!(
            d.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                None,
                None,
                900,
                false,
                0,
                Some(3),
            ),
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
            let mut decision = None;
            // Four active windows at 30 ms OWD.
            for i in 0..4 {
                let r = c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(30_000),
                    None,
                    None,
                    2_000,
                    false,
                    0,
                    marking.then_some(45),
                );
                assert_eq!(r, None);
            }
            // Keepalive at 1 ms OWD: marking host trains nothing; legacy trains min to 1 ms.
            for i in 4..10 {
                let r = c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(1_000),
                    None,
                    None,
                    200,
                    false,
                    0,
                    marking.then_some(0),
                );
                assert_eq!(r, None);
            }
            // Motion at the warmup's 30 ms OWD. First decision is the verdict
            // (cooldown silences the second).
            for i in 10..12 {
                let r = c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(30_000),
                    None,
                    None,
                    18_000,
                    false,
                    0,
                    marking.then_some(45),
                );
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
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                18_000,
                false,
                0,
                Some(45)
            ),
            Some(14_000)
        );
        c.on_ack(14_000);
        // …one clean window proves 14 000…
        assert_eq!(
            c.on_window(
                ticks(start, 1),
                0,
                0,
                None,
                None,
                None,
                14_000,
                false,
                0,
                Some(45),
            ),
            None
        );
        // …then ≥ [`IDLE_WINDOWS_TO_REARM`] idle windows.
        for i in 2..6 {
            assert_eq!(
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    None,
                    None,
                    None,
                    200,
                    false,
                    0,
                    Some(0)
                ),
                None
            );
        }
        // Onset: ×1.5 over proven 14 000 → 21 000, not additive 14 876.
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                0,
                0,
                None,
                None,
                None,
                14_000,
                false,
                0,
                Some(45),
            ),
            Some(21_000)
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
            c.on_window(
                ticks(start, 0),
                0,
                0,
                None,
                None,
                None,
                20_000,
                false,
                0,
                Some(45),
            ),
            Some(30_000)
        );
        // Severe inside cooldown scores but decides nothing; second backs off.
        assert_eq!(
            c.on_window(
                ticks(start, 1),
                1,
                0,
                None,
                None,
                None,
                20_000,
                false,
                0,
                Some(45)
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 2),
                1,
                0,
                None,
                None,
                None,
                20_000,
                false,
                0,
                Some(45)
            ),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Idle 2 × [`PROVEN_BUCKET_WINDOWS`]: both buckets rotate away.
        for i in 3..3 + 2 * PROVEN_BUCKET_WINDOWS {
            assert_eq!(
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    None,
                    None,
                    None,
                    200,
                    false,
                    0,
                    Some(0)
                ),
                None
            );
        }
        // Onset delivers 14 000 → 21 000, not 28 000 off the stale 20 000 mark.
        assert_eq!(
            c.on_window(
                ticks(start, 3 + 2 * PROVEN_BUCKET_WINDOWS),
                0,
                0,
                None,
                None,
                None,
                14_000,
                false,
                0,
                Some(45),
            ),
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
            c.on_window(
                ticks(start, 0),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
            Some(4_200)
        );
        assert!(c.low_rate_warned, "the descent below 5 000 warns");
        c.on_ack(4_200);
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None
            ),
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
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                20_000,
                false,
                0,
                None,
            ),
            Some(30_000)
        );
        c.on_ack(30_000);
        // Delivers 30 000 → next step 45 000, not 60 000.
        assert_eq!(
            c.on_window(
                ticks(start, 2),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                30_000,
                false,
                0,
                None,
            ),
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
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                20_000,
                false,
                0,
                None,
            ),
            Some(30_000)
        );
        c.on_ack(30_000);
        // Long calm stretch (2 % utilization): stay silent. Keep proven headroom.
        for i in 2..30 {
            assert_eq!(
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    Some(4_000),
                    None,
                    600,
                    false,
                    0,
                    None,
                ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    Some(8_000),
                    None,
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        // 52 ms over 8 ms baseline: immediate ×0.7. 30 ms still takes two.
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(10_000),
                Some(60_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ),
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
            let _ = c.on_window(
                ticks(start, 20 + i),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            );
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
            if let Some(k) = c.on_window(
                ticks(start, tick),
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
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
                let _ = c.on_window(
                    ticks(start, tick),
                    0,
                    0,
                    Some(10_000),
                    None,
                    None,
                    1_000_000,
                    false,
                    0,
                    None,
                );
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    Some(7_000),
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(10_000),
                None,
                Some(11_500),
                1_000_000,
                false,
                0,
                None,
            ),
            None
        );
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                0,
                0,
                Some(10_000),
                None,
                Some(12_000),
                1_000_000,
                false,
                0,
                None,
            ),
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
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    Some(7_000),
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(10_000),
                None,
                Some(20_000),
                1_000_000,
                false,
                0,
                None,
            ),
            Some(14_000)
        );
    }

    #[test]
    fn rate_decrease_rebases_the_encode_baseline() {
        // Our own decrease must rebase encode; old baseline would train-fire.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        for i in 0..4 {
            let _ = c.on_window(
                ticks(start, i),
                0,
                0,
                Some(10_000),
                None,
                Some(7_000),
                1_000_000,
                false,
                0,
                None,
            );
        }
        let _ = c.on_window(
            ticks(start, 4),
            0,
            0,
            Some(10_000),
            None,
            Some(12_000),
            1_000_000,
            false,
            0,
            None,
        );
        assert_eq!(
            c.on_window(
                ticks(start, 6),
                0,
                0,
                Some(10_000),
                None,
                Some(12_500),
                1_000_000,
                false,
                0,
                None,
            ),
            Some(14_000)
        );
        // After rebase, 15 ms against the old 7 ms floor must read clean.
        c.on_ack(14_000);
        for i in 8..11 {
            assert_eq!(
                c.on_window(
                    ticks(start, i),
                    0,
                    0,
                    Some(10_000),
                    None,
                    Some(15_000),
                    1_000_000,
                    false,
                    0,
                    None,
                ),
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
            if let Some(k) = c.on_window(
                at,
                0,
                0,
                Some(10_000),
                None,
                Some(7_000),
                1_000_000,
                false,
                0,
                None,
            ) {
                c.on_ack(k);
            }
        }
        let at = ticks(start, *tick);
        *tick += 1;
        c.on_window(
            at,
            0,
            0,
            Some(10_000),
            None,
            Some(level),
            1_000_000,
            false,
            0,
            None,
        )
    }

    /// `n` clean windows with no encode sample; ack any climb.
    fn clean_run(c: &mut BitrateController, start: Instant, tick: &mut u32, n: u32) {
        for _ in 0..n {
            let at = ticks(start, *tick);
            *tick += 1;
            if let Some(k) = c.on_window(
                at,
                0,
                0,
                Some(10_000),
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
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
            .on_window(at, 0, 0, Some(10_000), None, None, 1_000_000, true, 0, None)
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
            hz60.on_window(
                at,
                0,
                0,
                Some(10_000),
                None,
                Some(excursion),
                1_000_000,
                false,
                0,
                None,
            ),
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
                c.on_window(
                    at,
                    0,
                    0,
                    Some(10_000),
                    None,
                    Some(7_000),
                    1_000_000,
                    false,
                    0,
                    None,
                ),
                None
            );
        }
        // Encode excursion + flush: flush is the explanation, streak resets.
        let at = ticks(start, tick);
        assert_eq!(
            c.on_window(
                at,
                0,
                0,
                Some(10_000),
                None,
                Some(20_000),
                1_000_000,
                true,
                0,
                None,
            ),
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
            c.on_window(
                at,
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                2_000,
                false,
                0,
                None
            ),
            None
        );
    }

    /// Climb to `target` on fully-utilized windows, acking each step. 600-window bound.
    fn climb_to(c: &mut BitrateController, start: Instant, tick: &mut u32, target: u32) {
        for _ in 0..600 {
            if c.current_kbps >= target {
                return;
            }
            if let Some(k) = c.on_window(
                ticks(start, *tick),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
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
        let r = c.on_window(
            ticks(start, *tick),
            0,
            0,
            Some(10_000),
            Some(60_000),
            None,
            c.current_kbps,
            false,
            0,
            None,
        );
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
        let r = c.on_window(
            ticks(start, *tick),
            0,
            0,
            None,
            None,
            None,
            c.current_kbps / 10,
            true,
            RECOVERY_KF_SEVERE,
            None,
        );
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
                c.on_window(
                    ticks(start, t),
                    0,
                    0,
                    Some(3_500),
                    Some(200),
                    Some(2_800),
                    10_000,
                    false,
                    0,
                    None,
                ),
                None
            );
            t += 1;
        }
        assert!(
            c.probing,
            "slow start is still armed going into the rebuild"
        );

        let verdict = c.on_window(
            ticks(start, t),
            0,
            0,
            Some(15_711),
            Some(129),
            Some(15_063),
            390,
            false,
            0,
            None,
        );
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
        let verdict = c.on_window(
            ticks(start, t),
            0,
            0,
            Some(3_600),
            Some(210),
            Some(15_063),
            20_000,
            false,
            0,
            None,
        );
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
            if let Some(k) = c.on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                1_000_000,
                false,
                0,
                None,
            ) {
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
            .on_window(
                ticks(start, t),
                0,
                0,
                None,
                None,
                None,
                490_000,
                true,
                0,
                None,
            )
            .expect("flush must back off");
        assert_eq!(r1, 350_000);
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r1);
        // …loss-driven backoff at the re-climbed rate breaks the streak…
        climb_to(&mut c, start, &mut t, 460_000);
        t += 2;
        let r2 = c
            .on_window(
                ticks(start, t),
                1,
                0,
                None,
                None,
                None,
                c.current_kbps,
                false,
                0,
                None,
            )
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
            .on_window(
                ticks(start, t),
                0,
                0,
                None,
                None,
                None,
                c.current_kbps,
                true,
                0,
                None,
            )
            .expect("flush must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r3);
        // …dissimilar climbed-to rates share no knee.
        let dissimilar_target = c.current_kbps + 20_000;
        climb_to(&mut c, start, &mut t, dissimilar_target);
        t += 2;
        let _ = c
            .on_window(
                ticks(start, t),
                0,
                0,
                None,
                None,
                None,
                c.current_kbps,
                true,
                0,
                None,
            )
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
            let _ = c.on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                490_000,
                false,
                0,
                None,
            );
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
        let _ = c.on_window(
            ticks(start, t),
            0,
            HEAVY_LOSS_PPM,
            Some(10_000),
            Some(8_000),
            None,
            15_000,
            false,
            0,
            None,
        );
        t += 1;
        // First sample: flush + 40 ms decode.
        climb_to(&mut c, start, &mut t, 417_277);
        let first = c.current_kbps;
        t += 2;
        let r1 = c
            .on_window(
                ticks(start, t),
                0,
                0,
                Some(8_313),
                Some(40_087),
                None,
                first,
                true,
                1,
                None,
            )
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
            c.on_window(
                ticks(start, t),
                0,
                0,
                Some(6_877),
                Some(26_474),
                None,
                second,
                false,
                0,
                None,
            ),
            None,
            "the first bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(
                ticks(start, t),
                0,
                0,
                Some(6_877),
                Some(26_474),
                None,
                second,
                false,
                0,
                None,
            ),
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
            .on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                Some(43_305),
                None,
                r1,
                true,
                1,
                None,
            )
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
            .on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                None,
                None,
                300_000,
                false,
                RECOVERY_KF_SEVERE,
                None,
            )
            .expect("keyframe storm must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, 280_000);
        let rate = c.current_kbps;
        t += 2;
        let _ = c
            .on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                None,
                None,
                rate,
                false,
                RECOVERY_KF_SEVERE,
                None,
            )
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
            .on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                None,
                None,
                300_000,
                false,
                RECOVERY_KF_SEVERE,
                None,
            )
            .expect("clean storm must back off");
        t += 1;
        assert_eq!(c.decode_backoff_kbps, 300_000);
        c.on_ack(r1);
        climb_to(&mut c, start, &mut t, 280_000);
        t += 2;
        let _ = c
            .on_window(
                ticks(start, t),
                0,
                SEVERE_LOSS_PPM,
                Some(10_000),
                None,
                None,
                c.current_kbps,
                false,
                RECOVERY_KF_SEVERE,
                None,
            )
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
            c.on_window(
                ticks(start, t),
                0,
                0,
                Some(40_000),
                Some(8_000),
                None,
                490_000,
                false,
                0,
                None,
            ),
            None,
            "one OWD-bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(
                ticks(start, t),
                0,
                0,
                Some(10_000),
                Some(26_000),
                None,
                490_000,
                false,
                0,
                None,
            ),
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
            if c.on_window(
                ticks(start, i),
                1,
                0,
                None,
                None,
                None,
                1_000_000,
                false,
                0,
                None,
            )
            .is_some()
            {
                sent += 1;
            }
            i += 1;
        }
        assert_eq!(sent, MAX_UNACKED);
    }
}
