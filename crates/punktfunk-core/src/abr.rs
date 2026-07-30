//! Adaptive bitrate: the client-side AIMD controller behind the "Automatic" bitrate setting.
//!
//! Runs inside [`crate::client`]'s data-plane pump on the same 750 ms cadence as the adaptive-FEC
//! [`crate::quic::LossReport`], deciding when to ask the host for a different encoder bitrate via
//! [`crate::quic::SetBitrate`]. Division of labour with adaptive FEC: **FEC answers fast, random
//! loss** (Wi-Fi bursts, RF noise — recoverable redundancy is the right tool); **bitrate answers
//! persistent congestion** (the link simply can't carry the rate — more FEC only adds load). The
//! controller therefore reacts to *sustained* signals only:
//!
//! - **unrecoverable frames** — loss exceeded the FEC budget (the stream visibly froze/recovered);
//! - **heavy loss** — a window whose shard loss is beyond what FEC should be left to absorb alone;
//! - **one-way-delay rise** — capture→received latency (host-clock skew corrected) climbing above
//!   its rolling baseline: standing queue growth, the *pre-loss* signature of a saturated link
//!   (bufferbloat) — this is the early-warning signal loss-based control lacks;
//! - **a jump-to-live flush** — the pump discarded its backlog, the strongest "we were behind"
//!   evidence there is;
//! - **host-encode-latency rise** — the host's per-AU 0xCF `encode_us` climbing above its rolling
//!   baseline: the ENCODER falling behind its frame budget (the compute knee), the one failure a
//!   fat LAN never surfaces as loss/OWD/decode. Paired with the host's own climb refusal (a
//!   behind-cadence host acks climbs at the current rate) and short-ack cap learning
//!   ([`BitrateController::on_ack`]), this is what stops an Automatic session from driving the
//!   encoder off a cliff the network could carry.
//!
//! AIMD shape: a SEVERE window (an unrecoverable frame, a flush, ≥6 % loss, or a decode-latency
//! excursion far past baseline) backs off ×0.7 immediately; ordinary congestion
//! (heavy-but-recoverable loss, an OWD rise, a decode rise) needs two consecutive bad windows.
//! Recovery is two-mode: **slow start** — until the first congestion signal the rate DOUBLES each
//! clean window (cooldown-paced), which is how an Automatic session climbs from the conservative
//! start to the [`set_ceiling`](BitrateController::set_ceiling) measured by the startup
//! link-capacity probe in seconds instead of minutes — then classic additive recovery (+~6 %
//! after ~4.5 s clean, ceilinged). Changes are rate-limited (each one costs the IDR the host's
//! rebuilt encoder opens with) and the whole controller disables itself against a host that never
//! answers [`crate::quic::BitrateChanged`] (an older build that ignores unknown control messages).
//!
//! Climbs are additionally **evidence-gated**. The target is only a *promise* to the encoder —
//! how many bits it actually emits depends on the content — so on calm content (a menu, an idle
//! desktop) every window looks clean while proving nothing: the decoder was never exposed to the
//! target rate. Ungated, the climb drifts the target into territory the pipeline has never
//! carried, and the first motion spike becomes the first real test — which it fails, overloading
//! the decoder for the two-window backoff latency. So (a) a clean window only counts toward a
//! climb when its actual delivered throughput came close to the current target, and (b) no climb
//! steps past a modest headroom over the session's *proven* throughput — the highest windowed
//! rate the decoder demonstrably digested with flat decode latency, kept as a high-water mark
//! (never decayed: calm periods neither raise nor lower a validated target, so the encoder keeps
//! its headroom and answers returning motion instantly). The cost is a one-time paced ramp during
//! the session's first loaded stretch; capacity that later *shrinks* (thermal throttling) is the
//! reactive decode signal's job, as before.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Never ask for less than this — below it the stream is unusable anyway and the floor keeps a
/// mis-measured window from cratering the session.
const FLOOR_KBPS: u32 = 5_000;
/// Consecutive bad windows before an ORDINARY decrease — one window can be a scheduler blip or a
/// single Wi-Fi scan; two in a row (1.5 s) is a condition. A SEVERE window skips the wait.
const BAD_WINDOWS_TO_DECREASE: u32 = 2;
/// Window shard loss at/above which ONE window is enough to back off — 6 % is past any
/// blip/retry tail, and every 750 ms spent there is visible damage. Unrecoverable frames and
/// jump-to-live flushes are severe for the same reason.
const SEVERE_LOSS_PPM: u32 = 60_000;
/// Consecutive clean windows before probing back up in congestion-avoidance mode (~4.5 s at the
/// 750 ms cadence): recovery stays slower than backoff, classic AIMD. (Slow start ignores this —
/// it doubles on every cooled clean window until the first congestion signal.)
const CLEAN_WINDOWS_TO_INCREASE: u32 = 6;
/// Minimum gap between requested changes — every accepted change costs an encoder rebuild + IDR
/// on the host today (in-place reconfigure is planned), and back-to-back steps would outrun the
/// ack/effect round trip.
const CHANGE_COOLDOWN: Duration = Duration::from_millis(1500);
/// Window shard loss beyond which the window counts bad even without an unrecoverable frame:
/// 2 % sustained is congestion territory, not the random tail FEC exists for.
const HEAVY_LOSS_PPM: u32 = 20_000;
/// Decode-recovery KEYFRAME asks in one window at/above which the window is bad: the decoder
/// asked for a fresh picture twice inside 750 ms — it is being overdriven (or repeatedly
/// wedged), whatever loss_ppm says. This is the signal the RX-9070 field trace exposed: 14
/// requests in 2 s at ~300 Mbps with ZERO loss, and the controller kept the rate because no
/// loss/OWD/latency signal moved. RFI asks are deliberately NOT counted — they are the routine
/// loss-recovery mechanism and loss_ppm already prices them in.
const RECOVERY_KF_BAD: u32 = 2;
/// One window at/above this many keyframe asks is SEVERE (skips the two-window confirmation):
/// the emitters throttle at 100 ms, so 4+ inside a window means the decoder spent most of it
/// unable to produce pictures — the user is already watching the damage.
const RECOVERY_KF_SEVERE: u32 = 4;
/// How far the window's mean one-way delay may sit above the rolling baseline before it counts
/// as queue growth. 25 ms is far beyond jitter at any streamable frame rate.
const OWD_RISE_US: i64 = 25_000;
/// How far the window's mean *decode-stage* latency (client hand-off → decoder output, reported by
/// the embedder) may sit above its rolling baseline before it counts as the decoder falling behind.
/// This is the signal the network-side ones can't see: on a fast LAN a mobile HW decoder saturates
/// long before the link does, backlogging frames INSIDE the decoder where loss/OWD never register —
/// so without this the controller slow-starts straight to the link ceiling and parks there, choking
/// the decoder. A rising decode latency ends the climb and (sustained) backs the rate off to the
/// real decode limit. Local, low-noise signal (no network jitter), so a tighter threshold than OWD:
/// 15 ms of standing decode queue is unambiguous backlog at any streamable frame rate.
const DECODE_RISE_US: i64 = 15_000;
/// Decode-stage latency this far above baseline is SEVERE — back off after ONE window instead of
/// two. 45 ms of standing decode queue is several frames of backlog at any streamable rate; the
/// user is already watching the spike-overload damage, and every extra window spent confirming it
/// is 750 ms more of it.
const DECODE_SEVERE_US: i64 = 45_000;
/// A clean window counts toward a CLIMB only when its actual delivered throughput reached
/// `actual × UTILIZATION_DEN ≥ target × UTILIZATION_NUM` (¾ of the current target). Below that
/// the encoder wasn't constrained by the target, so the window is evidence of nothing — climbing
/// on it just parks the target deeper into unvalidated territory (the settled-calm-then-spike
/// failure). At/above it the pipeline genuinely carried ~the target rate and survived.
const UTILIZATION_NUM: u64 = 3;
const UTILIZATION_DEN: u64 = 4;
/// A climb may step at most this far (×1.5) past the proven-throughput high-water mark: the next
/// target stays within a bounded experiment over what the decoder has demonstrably digested,
/// rather than doubling blind. Utilization-gated climbs guarantee `proven ≥ ¾ × current`, so the
/// cap always leaves ≥ ~12 % of climbing room — the two gates can't deadlock.
const PROVEN_HEADROOM_NUM: u32 = 3;
const PROVEN_HEADROOM_DEN: u32 = 2;
/// How far the window's mean HOST-ENCODE latency (the 0xCF `HostStages::encode_us` the host
/// already ships per AU) may rise above its rolling baseline before the window is bad. This is
/// the down-driver for the ENCODER's compute knee — the failure loss/OWD/decode are all blind
/// to: on a fat LAN the controller can climb to a rate the link carries fine but the ASIC
/// can't encode inside the frame budget (4K120 HEVC at ~800 Mbps ≈ 9.3 ms against 8.33), and
/// the only symptom is encode time. Baseline-RELATIVE on purpose: an escalated host reports
/// encode_us inflated by its retrieve-queue depth (~a frame), so an absolute budget threshold
/// would read permanently-red and drive the rate to the floor; a rise above the session's own
/// baseline survives that offset. ~half a 120 Hz frame budget of standing rise is real.
const ENCODE_RISE_US: i64 = 4_000;
/// Host-encode latency this far above baseline (≈1.5 × a 120 Hz budget) is SEVERE — the encode
/// queue is growing past the knee; skip the two-window confirmation.
const ENCODE_SEVERE_US: i64 = 12_000;
/// Clean windows parked at the learned [`host cap`](BitrateController::host_cap_kbps) before
/// re-probing above it (~60 s at the 750 ms tick). A cadence-refusal cap is scene-dependent
/// evidence, not a spec limit — without a re-probe, one heavy scene would cap the whole
/// session. A still-standing limit just re-teaches itself in two short acks, which the host
/// pre-clamps without touching the encoder — the re-probe costs no rebuild, no IDR.
const CAP_REPROBE_WINDOWS: u32 = 80;
/// Rolling window (in 750 ms report windows, ~30 s) whose minimum mean is the OWD baseline.
/// Long enough to remember the uncongested floor, short enough to follow genuine path changes.
const BASELINE_WINDOWS: usize = 40;
/// Requests sent without a single [`crate::quic::BitrateChanged`] ack before concluding the host
/// predates bitrate renegotiation and going quiet for the rest of the session.
const MAX_UNACKED: u32 = 3;

/// One decision per report window; `Some(kbps)` = send a [`crate::quic::SetBitrate`].
pub(crate) struct BitrateController {
    /// `false` = permanently off (explicit user bitrate, an old host, or ack silence).
    enabled: bool,
    /// The rate we believe the host encodes at (updated by acks; requests are not assumed).
    current_kbps: u32,
    /// The climb ceiling: the negotiated start rate until the startup link-capacity probe
    /// raises it via [`set_ceiling`](Self::set_ceiling) — that measurement is what lets an
    /// Automatic session scale past its conservative start.
    ceiling_kbps: u32,
    floor_kbps: u32,
    /// Slow start: true until the first congestion signal — clean windows DOUBLE the rate
    /// (cooldown-paced) instead of the +6 % additive step.
    probing: bool,
    /// Recent window mean OWDs (µs); the rolling min is the uncongested baseline.
    owd_means: VecDeque<i64>,
    /// Recent window mean decode-stage latencies (µs); the rolling min is the decoder's
    /// keeping-up baseline. Empty on embedders that don't report decode latency (the decode
    /// signal is then simply absent — identical to the pre-decode-signal behavior).
    decode_means: VecDeque<i64>,
    /// Recent window mean host-encode latencies (µs, from the 0xCF datagrams); rolling-min
    /// baseline like the decode signal. Cleared whenever OUR OWN rate decrease changes the
    /// encode regime (see [`on_ack`](Self::on_ack)) and on a mode switch.
    encode_means: VecDeque<i64>,
    /// The host-taught rate cap (§ABR overdrive): latched when the host acks BELOW what we
    /// asked twice consecutively at the same value — its encoder's codec-level ceiling, or a
    /// climb refusal while host encode can't hold cadence. Kept apart from `ceiling_kbps` so
    /// the probe-measured link authority survives a mode switch's reset. Slowly re-probed
    /// ([`CAP_REPROBE_WINDOWS`]) so scene-dependent evidence can't cap the session forever.
    host_cap_kbps: Option<u32>,
    /// The rate the last [`request`](Self::request) asked for — the reference an ack is judged
    /// short against. Taken (not kept) by the ack, so one request is judged at most once.
    last_requested_kbps: Option<u32>,
    /// Consecutive short-ack streak: the value and how many times in a row it was acked. Two
    /// identical short acks latch [`host_cap_kbps`](Self::host_cap_kbps) — one can be a
    /// transient (a failed host rebuild keeping the old rate); the host's resolves are
    /// deterministic min()s, so a persistent limit reproduces exactly.
    short_ack_kbps: u32,
    short_acks: u32,
    /// Clean windows spent parked at the learned cap (the re-probe clock).
    cap_probe_windows: u32,
    /// Proven throughput: the session's highest windowed ACTUAL delivered rate seen with flat
    /// decode latency — the known-good high-water mark climbs are bounded against. Never decays;
    /// shrinking capacity (thermals, a heavier scene) is the reactive decode signal's job. On
    /// embedders without a decode signal this is just the delivered high-water mark — weaker
    /// evidence, but the same bound.
    proven_kbps: u32,
    bad_windows: u32,
    clean_windows: u32,
    last_change: Option<Instant>,
    /// Requests since the last ack — reaching [`MAX_UNACKED`] disables the controller.
    unacked: u32,
}

impl BitrateController {
    /// `start_kbps` is the Welcome-resolved session bitrate when the user chose Automatic, or `0`
    /// to build a permanently-disabled controller (explicit bitrate / an old host that didn't
    /// echo one — no known ceiling to work against).
    pub(crate) fn new(start_kbps: u32) -> Self {
        BitrateController {
            enabled: start_kbps > 0,
            current_kbps: start_kbps,
            ceiling_kbps: start_kbps,
            floor_kbps: FLOOR_KBPS.min(start_kbps.max(1)),
            probing: true,
            owd_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            decode_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            encode_means: VecDeque::with_capacity(BASELINE_WINDOWS),
            host_cap_kbps: None,
            last_requested_kbps: None,
            short_ack_kbps: 0,
            short_acks: 0,
            cap_probe_windows: 0,
            proven_kbps: 0,
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
            unacked: 0,
        }
    }

    /// Raise the climb ceiling to a measured link capacity (the startup speed-test probe's
    /// delivered throughput with headroom already subtracted by the caller). Without this call
    /// the ceiling stays the negotiated start rate — exactly the old behavior. Never lowers:
    /// a congested-moment measurement must not shrink authority below what was negotiated
    /// (descent is the congestion signals' job).
    pub(crate) fn set_ceiling(&mut self, kbps: u32) {
        if self.enabled && kbps > self.ceiling_kbps {
            self.ceiling_kbps = kbps;
        }
    }

    /// The host's [`crate::quic::BitrateChanged`] ack: its clamp is authoritative for what the
    /// encoder now targets, and any ack proves the host renegotiates (resets the silence counter).
    ///
    /// A SHORT ack (below what we asked) is the host telling us about a limit the network
    /// signals can't see — its encoder's codec-level ceiling, or a climb refusal while encode
    /// can't hold cadence. Two consecutive short acks at the SAME value latch it as
    /// [`host_cap_kbps`](Self::host_cap_kbps), stopping the AIMD sawtooth from re-poking a
    /// limit the host already refused; ONE is not enough — a failed host rebuild also acks
    /// short once, and latching a transient would cap the session on a hiccup.
    pub(crate) fn on_ack(&mut self, kbps: u32) {
        if kbps > 0 {
            if kbps < self.current_kbps {
                // Our own decrease changes the encode-time regime (less work per frame; on an
                // escalated host the queue offset shifts too) — judging the new regime against
                // the old baseline would train-fire the encode down-driver. Re-seed it.
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
                        tracing::info!(
                            cap_kbps = kbps,
                            "adaptive bitrate: host cap learned (encoder ceiling or cadence \
                             refusal) — climbs stop here until it lifts"
                        );
                        self.host_cap_kbps = Some(kbps.max(self.floor_kbps));
                        self.cap_probe_windows = 0;
                    }
                } else {
                    self.short_acks = 0;
                }
            }
            self.current_kbps = kbps;
        }
        self.unacked = 0;
    }

    /// An accepted mode switch: the encoder's ceiling and compute knee are properties of the
    /// MODE (4K120 caps where 1080p60 never would) — drop the mode-scoped learned state. The
    /// probe-measured `ceiling_kbps` (a LINK property) survives.
    pub(crate) fn on_mode_switch(&mut self) {
        self.host_cap_kbps = None;
        self.short_acks = 0;
        self.cap_probe_windows = 0;
        self.encode_means.clear();
    }

    /// Feed one report window; returns the rate to request now, if any. `dropped` = frames that
    /// went FEC-unrecoverable in the window, `loss_ppm` the window's [`crate::quic::LossReport`]
    /// figure, `owd_mean_us` the window's mean skew-corrected capture→received latency (`None`
    /// without a clock handshake), `decode_mean_us` the window's mean client decode-stage latency
    /// (`None` on an embedder that doesn't report it — the signal is then absent),
    /// `encode_mean_us` the window's mean HOST encode-stage latency (from the per-AU 0xCF
    /// datagrams; `None` on an old host that doesn't send them), `actual_kbps` the window's
    /// ACTUAL delivered throughput (wire bytes received ÷ window — what the pipeline really
    /// carried, as opposed to the target it was allowed; feeds the utilization climb gate and
    /// the proven-throughput high-water mark), `flushed` = the pump's jump-to-live fired in the
    /// window, `recovery_kf` = decode-recovery keyframe asks the client sent in the window (see
    /// [`RECOVERY_KF_BAD`]).
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
    ) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        if self.unacked >= MAX_UNACKED {
            // The host never answered: an older build. Go quiet instead of spamming a message it
            // logs as unknown every few seconds.
            self.enabled = false;
            tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            return None;
        }
        // OWD: compare against the rolling-min baseline of PRIOR windows (so a rising window
        // doesn't drag its own baseline up), then record it.
        let owd_bad = match owd_mean_us {
            Some(mean) => {
                let bad = self
                    .owd_means
                    .iter()
                    .min()
                    .is_some_and(|&base| mean > base + OWD_RISE_US);
                if self.owd_means.len() == BASELINE_WINDOWS {
                    self.owd_means.pop_front();
                }
                self.owd_means.push_back(mean);
                bad
            }
            None => false,
        };
        // Decode-stage latency: same rolling-min-baseline treatment as OWD, but measuring the
        // CLIENT'S decoder rather than the link. A rise means the decoder is backlogging frames —
        // the bottleneck the network signals are blind to. Marking the window bad both ends slow
        // start (so the climb stops the moment decode latency lifts, instead of doubling on into
        // the link ceiling) and, sustained, drives the ×0.7 backoff down to the real decode limit.
        // An excursion far past baseline is SEVERE: the decoder is deep in spike-overload and the
        // user is watching it — skip the two-window confirmation.
        let (decode_bad, decode_severe) = match decode_mean_us {
            Some(mean) => {
                let base = self.decode_means.iter().min().copied();
                let bad = base.is_some_and(|b| mean > b + DECODE_RISE_US);
                let severe = base.is_some_and(|b| mean > b + DECODE_SEVERE_US);
                if self.decode_means.len() == BASELINE_WINDOWS {
                    self.decode_means.pop_front();
                }
                self.decode_means.push_back(mean);
                (bad, severe)
            }
            None => (false, false),
        };
        // Host-encode latency: the same rolling-min-baseline treatment, measuring the HOST'S
        // encoder — the compute-knee down-driver (see [`ENCODE_RISE_US`]). This is the only
        // signal that can push an already-too-high rate back under the knee: the host refuses
        // further climbs while behind cadence, but nothing else ever DESCENDS on a clean LAN.
        let (encode_bad, encode_severe) = match encode_mean_us {
            Some(mean) => {
                let base = self.encode_means.iter().min().copied();
                let bad = base.is_some_and(|b| mean > b + ENCODE_RISE_US);
                let severe = base.is_some_and(|b| mean > b + ENCODE_SEVERE_US);
                if self.encode_means.len() == BASELINE_WINDOWS {
                    self.encode_means.pop_front();
                }
                self.encode_means.push_back(mean);
                (bad, severe)
            }
            None => (false, false),
        };
        // The proven-throughput high-water mark: this window's delivered rate is now demonstrably
        // digestible (decode latency stayed flat while it was carried). Loss doesn't disqualify —
        // the bytes that DID arrive still went through the decoder; what loss means for the rate
        // is the bad/severe machinery's business.
        if !decode_bad && actual_kbps > self.proven_kbps {
            self.proven_kbps = actual_kbps;
        }
        // SEVERE = the user already saw damage (an unrecoverable frame, a jump-to-live flush, a
        // deep decode-latency excursion, a window spent begging for keyframes) or loss far past
        // any blip — one window is enough. Ordinary congestion (heavy-but-recoverable loss, an
        // OWD rise, a decode-latency rise, repeated keyframe asks) still needs two consecutive
        // windows.
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
        if bad {
            self.bad_windows += 1;
            self.clean_windows = 0;
            // Any congestion signal ends slow start for good — from here on, climbs are additive.
            self.probing = false;
        } else {
            self.clean_windows += 1;
            self.bad_windows = 0;
        }
        // The learned host cap re-probe (see [`CAP_REPROBE_WINDOWS`]): after ~60 s of clean
        // windows parked at the cap, lift it one step (+12.5 %, ceiling-bounded) so a
        // scene-dependent refusal can't quietly cap the whole session — a still-standing limit
        // just re-latches from the next pair of short acks, at zero encoder cost.
        if let Some(cap) = self.host_cap_kbps {
            if bad {
                self.cap_probe_windows = 0;
            } else if self.current_kbps >= cap.saturating_sub(cap / 16) {
                self.cap_probe_windows += 1;
                if self.cap_probe_windows >= CAP_REPROBE_WINDOWS {
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
        let cooled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= CHANGE_COOLDOWN);
        if !cooled {
            return None;
        }
        if (self.bad_windows >= BAD_WINDOWS_TO_DECREASE || (severe && self.bad_windows >= 1))
            && self.current_kbps > self.floor_kbps
        {
            let next = ((self.current_kbps as u64 * 7 / 10) as u32).max(self.floor_kbps);
            self.bad_windows = 0;
            return self.request(next, now);
        }
        // Climbs only fire off a UTILIZED clean window (actual delivered ≥ ¾ of the target — the
        // target was genuinely tested, not idling under calm content) and step at most ×1.5 past
        // the proven high-water mark. Calm windows still count as clean (clean_windows keeps
        // accumulating — the network is healthy), they just can't authorize a climb; the first
        // utilized window after a long-enough clean run climbs immediately.
        let utilized =
            actual_kbps as u64 * UTILIZATION_DEN >= self.current_kbps as u64 * UTILIZATION_NUM;
        // The effective ceiling folds in the host-taught cap: the probe measured the LINK, but
        // the host's short acks measured the ENCODER — whichever binds first is the limit.
        let eff_ceiling = self
            .ceiling_kbps
            .min(self.host_cap_kbps.unwrap_or(u32::MAX));
        let cap = eff_ceiling
            .min(self.proven_kbps.saturating_mul(PROVEN_HEADROOM_NUM) / PROVEN_HEADROOM_DEN);
        if self.current_kbps < eff_ceiling && utilized && cap > self.current_kbps {
            // Slow start: double on every cooled clean window until the first congestion signal
            // (this is how an Automatic session reaches a probe-measured ceiling in seconds).
            // Congestion avoidance: +~6 % after a sustained clean run.
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
        // `current_kbps` is NOT updated here — the host's ack is authoritative. A lost/ignored
        // request just recomputes from the same base next time (and counts toward MAX_UNACKED).
        Some(kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window cadence matching the pump's 750 ms tick, safely past the change cooldown when
    /// stepped 5× between decisions.
    const TICK: Duration = Duration::from_millis(750);

    fn ticks(start: Instant, n: u32) -> Instant {
        start + TICK * n
    }

    /// Drive `n` clean windows, asserting no decision fires before the clean threshold. Windows
    /// are fully loaded (1 Gb/s actual) so neither the utilization gate nor the proven cap binds.
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
            );
            if out.is_some() {
                return out;
            }
        }
        out
    }

    #[test]
    fn disabled_when_not_automatic_or_old_host() {
        // start 0 = explicit user bitrate or a host that didn't echo one → permanently off.
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
                0
            ),
            None
        );
    }

    #[test]
    fn two_ordinary_bad_windows_step_down_multiplicatively() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // Heavy-but-recoverable loss (2–6 %) is ORDINARY: one window is a blip — no reaction.
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
                0
            ),
            None
        );
        // The second consecutive bad window backs off ×0.7.
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
                0
            ),
            Some(14_000)
        );
        c.on_ack(14_000);
        // Still bad after the cooldown → another ×0.7 step from the ACKED rate.
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
                0
            ),
            None
        ); // bad #1 again
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
                0
            ),
            Some(9_800)
        );
    }

    #[test]
    fn severe_window_backs_off_immediately() {
        // An unrecoverable frame (the user SAW a freeze) skips the two-window wait…
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(ticks(start, 0), 1, 0, None, None, None, 1_000_000, false, 0),
            Some(14_000)
        );
        // …and so does a jump-to-live flush.
        let mut c = BitrateController::new(20_000);
        assert_eq!(
            c.on_window(ticks(start, 0), 0, 0, None, None, None, 1_000_000, true, 0),
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn cooldown_blocks_back_to_back_steps() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(ticks(start, 0), 1, 0, None, None, None, 1_000_000, false, 0),
            Some(14_000)
        );
        c.on_ack(14_000);
        // A severe window INSIDE the 1.5 s cooldown (tick 1 = 750 ms) → held; at the cooldown
        // boundary (tick 2 = 1.5 s) it fires.
        assert_eq!(
            c.on_window(ticks(start, 1), 1, 0, None, None, None, 1_000_000, false, 0),
            None
        );
        assert_eq!(
            c.on_window(ticks(start, 2), 1, 0, None, None, None, 1_000_000, false, 0),
            Some(9_800)
        );
    }

    #[test]
    fn floor_is_never_crossed() {
        let mut c = BitrateController::new(6_000);
        let start = Instant::now();
        // ×0.7 of 6000 = 4200 < floor → clamped to 5000.
        assert_eq!(
            c.on_window(ticks(start, 0), 1, 0, None, None, None, 1_000_000, false, 0),
            Some(5_000)
        );
        c.on_ack(5_000);
        // At the floor, further bad windows request nothing.
        assert_eq!(
            c.on_window(ticks(start, 6), 1, 0, None, None, None, 1_000_000, false, 0),
            None
        );
        assert_eq!(
            c.on_window(ticks(start, 7), 1, 0, None, None, None, 1_000_000, false, 0),
            None
        );
    }

    #[test]
    fn sustained_clean_recovers_toward_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(ticks(start, 0), 1, 0, None, None, None, 1_000_000, false, 0),
            Some(14_000)
        );
        c.on_ack(14_000);
        // The backoff ended slow start → additive recovery: 6 clean windows → one +~6 % step
        // (14000 + 14000/16 + 1 = 14876).
        let up = run_clean(&mut c, start, 2, 7);
        assert_eq!(up, Some(14_876));
        c.on_ack(14_876);
        // Fully recovered → clean windows at the ceiling stay quiet (never probe past it).
        c.on_ack(20_000);
        assert_eq!(run_clean(&mut c, start, 40, 20), None);
    }

    #[test]
    fn slow_start_doubles_to_a_probed_ceiling_then_stops() {
        let mut c = BitrateController::new(20_000);
        // The startup link-capacity probe measured ~430 Mbps delivered → ×0.7 ceiling.
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Every cooled clean window doubles until the ceiling caps the climb, then quiet.
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
                0
            ),
            Some(40_000)
        );
        c.on_ack(40_000);
        // Severe window → immediate ×0.7, and slow start is over.
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
                0
            ),
            Some(28_000)
        );
        c.on_ack(28_000);
        // Clean again — but the next climb is additive, after the 6-window clean run.
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
            c.on_window(Instant::now(), 0, 0, None, None, None, 1_000_000, false, 0),
            None
        );
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(10_000); // below the negotiated start → ignored
        assert_eq!(c.ceiling_kbps, 20_000);
    }

    #[test]
    fn owd_rise_alone_is_a_congestion_signal() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // Establish a ~10 ms baseline over a few clean windows.
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
                    0
                ),
                None
            );
        }
        // Delay climbs 40 ms above baseline with ZERO loss — bufferbloat. Two windows → back off.
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
                0
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn decode_latency_rise_alone_is_a_congestion_signal() {
        // The link is pristine (zero loss, flat OWD) but the client's decoder is falling behind —
        // the LAN-vs-mobile-decoder case. Only the decode signal can catch it.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        // A ~8 ms decode baseline over a few clean windows.
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
                    0
                ),
                None
            );
        }
        // Decode latency climbs 30 ms above baseline with ZERO loss and flat OWD: the decoder is
        // backlogging. Two windows → back off ×0.7, exactly like an OWD rise.
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
                0
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_storm_alone_is_a_congestion_signal() {
        // The RX-9070 field shape: pristine link (zero loss, flat OWD), no latency signal — but
        // the decoder keeps begging for keyframes. Two asks per window is ordinary-bad: two
        // consecutive windows back off ×0.7.
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
                2
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
                2
            ),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_saturation_is_severe() {
        // The emitters throttle at 100 ms, so 4+ asks in one 750 ms window means the decoder
        // spent most of it unable to produce pictures — one window is enough.
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
                4
            ),
            Some(14_000)
        );
    }

    #[test]
    fn a_single_keyframe_ask_is_not_congestion() {
        // A lone hiccup's recovery ask must not read as congestion — windows carrying one ask
        // stay clean (no backoff however many in a row).
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
                    1
                ),
                None
            );
        }
    }

    #[test]
    fn decode_latency_caps_the_slow_start_climb() {
        // A fat link (probe measured ~300 Mbps) but a decoder that saturates around the start rate.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // First clean window (decoder fine at 20 Mbps) → slow start doubles to 40.
        assert_eq!(
            c.on_window(
                ticks(start, 0),
                0,
                0,
                Some(10_000),
                Some(8_000),
                None,
                1_000_000,
                false,
                0
            ),
            Some(40_000)
        );
        c.on_ack(40_000);
        // At 40 Mbps the decoder starts backing up (30 ms over baseline): the window is bad, so the
        // climb stops here instead of doubling on toward the 300 Mbps link ceiling…
        assert_eq!(
            c.on_window(
                ticks(start, 2),
                0,
                0,
                Some(10_000),
                Some(38_000),
                None,
                1_000_000,
                false,
                0
            ),
            None
        );
        // …and a second backed-up window backs the rate off, settling at the decode limit rather
        // than choking the decoder at the link ceiling (the reported bug).
        assert_eq!(
            c.on_window(
                ticks(start, 4),
                0,
                0,
                Some(10_000),
                Some(40_000),
                None,
                1_000_000,
                false,
                0
            ),
            Some(28_000)
        );
    }

    #[test]
    fn unloaded_clean_windows_never_authorize_a_climb() {
        // Calm content: the network is pristine but the encoder emits a fraction of the target —
        // those windows prove nothing, so the target must NOT drift up (the settle-calm-then-
        // spike-overload bug this gate exists for).
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
                    0
                ),
                None
            );
        }
        // Motion arrives: the first utilized window climbs immediately (clean credit is already
        // banked), but only to ×1.5 over the proven high-water (18 000 delivered → 27 000), not a
        // blind doubling to 40 000.
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
                0
            ),
            Some(27_000)
        );
    }

    #[test]
    fn slow_start_steps_stay_within_proven_headroom() {
        // Under real load the climb proceeds, but each step is a bounded experiment: ×1.5 over
        // what was actually delivered and digested, never a blind 2× toward the link ceiling.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // The window delivered the full target (the encoder is constrained by it): proven 20 000
        // → the doubling is capped at 30 000.
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
                0
            ),
            Some(30_000)
        );
        c.on_ack(30_000);
        // The next loaded window delivers 30 000 → the next step is 45 000, not 60 000.
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
                0
            ),
            Some(45_000)
        );
    }

    #[test]
    fn calm_period_keeps_the_validated_target() {
        // A target validated under load is NOT surrendered when the scene goes calm: no
        // down-steps, no ceiling decay — the encoder keeps the proven headroom so returning
        // motion gets the full rate instantly instead of re-ramping every calm→action edge.
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
                0
            ),
            Some(30_000)
        );
        c.on_ack(30_000);
        // A long calm stretch (2 % utilization, decoder idle): the controller stays silent.
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
                    0
                ),
                None
            );
        }
    }

    #[test]
    fn deep_decode_excursion_is_severe() {
        // A motion spike that shoots decode latency far past baseline (>45 ms) is the overload
        // already happening — it must not wait out the two-window confirmation.
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
                    0
                ),
                None
            );
        }
        // 52 ms over the 8 ms baseline in ONE window → immediate ×0.7. (A 30 ms rise — see
        // decode_latency_rise_alone_is_a_congestion_signal — still takes the ordinary two.)
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn two_identical_short_acks_latch_the_host_cap() {
        // The 4K120 field failure: the encoder ceilings at ~794 Mbps while the link carries
        // more — the host acks short. TWO identical short acks teach the cap; climbs then stop
        // poking a limit the host already refused (the rebuild-storm driver).
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        // First short ack: current follows (authoritative), but one short ack is not a cap.
        c.on_ack(794_000);
        assert!(c.host_cap_kbps.is_none());
        // The next climb overshoots again and is short-acked at the SAME value: latch.
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        // Parked AT the learned cap, nothing left to climb to — no more requests.
        assert_eq!(run_clean(&mut c, start, 20, 12), None);
    }

    #[test]
    fn one_short_ack_is_a_transient_not_a_cap() {
        // A failed host rebuild acks short once (it kept the old rate) — latching THAT would
        // cap the session on a driver hiccup. The streak must survive only identical repeats.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(400_000); // rebuild failed, host kept the old rate
        assert!(c.host_cap_kbps.is_none());
        // The retry applies fully: streak broken, still no cap, full authority kept.
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
        // 4K120's ceiling means nothing at the new mode — the cap must not survive the switch
        // (the probe-measured link ceiling does).
        c.on_mode_switch();
        assert!(c.host_cap_kbps.is_none());
        assert_eq!(c.ceiling_kbps, 1_400_000);
    }

    #[test]
    fn learned_cap_reprobes_after_a_sustained_clean_run() {
        // A cadence-refusal cap is scene evidence, not a spec limit: after ~60 s parked clean
        // at the cap, lift one step so a one-time heavy scene can't cap the session forever. A
        // still-standing limit just re-latches from the next short-ack pair, at zero cost.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        for i in 0..CAP_REPROBE_WINDOWS {
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
            );
        }
        assert_eq!(c.host_cap_kbps, Some(794_000 + 794_000 / 8));
    }

    #[test]
    fn host_encode_latency_rise_backs_off() {
        // The compute knee: link pristine, client decoder fine — only HOST encode time moves
        // (the 4K120 case: ~9.3 ms against an 8.33 ms budget shows up nowhere else). Two risen
        // windows → ×0.7, exactly like an OWD/decode rise.
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
                    0
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
                0
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn deep_encode_excursion_is_severe() {
        // Encode time shooting ≈1.5 frame budgets over baseline = the queue is growing past
        // the knee right now — no two-window confirmation.
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
                    0
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn rate_decrease_rebases_the_encode_baseline() {
        // After OUR OWN decrease the encode regime legitimately changes (less work per frame;
        // an escalated host's reported encode_us also carries a queue offset) — the old
        // baseline must not train-fire repeated backoffs down to the floor.
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
                0
            ),
            Some(14_000)
        );
        // The decrease applies → rebase. The new regime's ~15 ms means (an escalated host's
        // queue offset) would be far over the OLD 7 ms baseline, but must now read clean.
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
                    0
                ),
                None
            );
        }
    }

    #[test]
    fn ack_silence_disables_the_controller() {
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut sent = 0;
        let mut i = 0;
        // Keep every window bad and never ack: exactly MAX_UNACKED requests, then silence.
        while i < 60 {
            if c.on_window(ticks(start, i), 1, 0, None, None, None, 1_000_000, false, 0)
                .is_some()
            {
                sent += 1;
            }
            i += 1;
        }
        assert_eq!(sent, MAX_UNACKED);
    }
}
