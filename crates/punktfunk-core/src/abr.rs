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
//!   encoder off a cliff the network could carry. It is also the one signal that can fire for a
//!   reason the rate cannot fix (contention on the host's GPU), so it stands itself down when
//!   backing off stops helping, and a later clean run re-probes it — see
//!   [`ENCODE_NOOP_BACKOFFS_TO_DISARM`].
//!
//! AIMD shape: a SEVERE window (an unrecoverable frame, a flush, ≥6 % loss, or a decode-latency
//! excursion far past baseline) backs off ×0.7 immediately; ordinary congestion
//! (heavy-but-recoverable loss, an OWD rise, a decode rise) needs two consecutive bad windows.
//! Recovery is two-mode: **slow start** — until the first congestion signal each clean window
//! asks for double the current rate, bounded (like every climb) by the proven-throughput
//! headroom below, so the step a loaded session actually takes is ×1.5 over what it last
//! delivered; either way it climbs from the conservative start to the
//! [`set_ceiling`](BitrateController::set_ceiling) measured by the startup link-capacity probe
//! in seconds rather than minutes — then classic additive recovery (+~6 % after ~4.5 s clean,
//! ceilinged). Changes are rate-limited (each one costs the IDR the host's
//! rebuilt encoder opens with) and the whole controller disables itself against a host that never
//! answers [`crate::quic::BitrateChanged`] (an older build that ignores unknown control messages).
//! Standing limits are LEARNED rather than re-poked: two identical short host acks latch the
//! encoder's ceiling (`host_cap_kbps`), two consecutive decode-severe backoffs at a similar rate
//! latch the client decoder's knee (`decode_cap_kbps`) — and both re-probe slowly
//! ([`CAP_REPROBE_WINDOWS_MIN`]) so neither latch outlives the condition that taught it.
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
///
/// A FRAME BUDGET, not a fixed duration — the two constants here are the 120 Hz values, used
/// only until [`set_frame_budget`](BitrateController::set_frame_budget) supplies the session's
/// own (see [`BitrateController::encode_thresholds`]). Left absolute they encode a 120 Hz
/// assumption into every session: at 60 Hz one frame is 16.7 ms, so an ordinary one-frame encode
/// hiccup clears the SEVERE tier and takes the immediate ×0.7 where the same hiccup at 120 Hz
/// (8.3 ms) does not even reach it. That asymmetry is a field report — a 1440p60 session ratcheted
/// to the floor while 1440p120 sessions on the same host and client climbed to their shape ceiling.
const ENCODE_RISE_US: i64 = 4_000;
/// Host-encode latency this far above baseline (≈1.5 × a 120 Hz budget) is SEVERE — the encode
/// queue is growing past the knee; skip the two-window confirmation. Frame-budget-scaled like
/// [`ENCODE_RISE_US`].
const ENCODE_SEVERE_US: i64 = 12_000;
/// Consecutive encode-attributed backoffs that did NOT bring host encode time down before the
/// encode down-driver is disarmed for the session.
///
/// The signal's whole premise is that encode time is a function of the rate the controller can
/// actuate: it exists to find the encoder's compute knee, where cutting the rate cuts the work.
/// When the rise comes from something else on the GPU — a game saturating the card, which is
/// exactly when the host is also behind cadence — the premise is false. The backoff changes
/// nothing, the signal fires again, and [`on_ack`](BitrateController::on_ack)'s baseline re-seed
/// erases the evidence that nothing improved, so the controller ratchets to the floor pulling the
/// one lever that cannot work (the field case: 57 → 5 Mbps over ten minutes with zero packet loss,
/// zero keyframe asks and a flat decoder).
///
/// So: remember the level each encode-attributed backoff fired at, and when the next one fires no
/// lower, count it. Two in a row means the rate is not what is driving encode time here — stop
/// letting it drive. Same shape as the clock-flush detector's
/// [`crate::client::frame_channel::NOOP_CLOCK_FLUSHES_TO_DISARM`]: a signal whose remedy is
/// demonstrably doing nothing should stand down rather than repeat forever.
///
/// Two, not one: a single pair of backoffs at a similar level is also what a real knee looks like
/// while the rate is still above it, and the knee is the case this signal was built for.
///
/// And a stand-down, never a permanent disarm. Nothing this controller learns from evidence is
/// permanent — both caps re-probe, and the clock-flush detector was itself changed from "off for
/// the session" to re-armable for exactly this reason. GPU contention is transient by nature (the
/// game exits to a menu, the shader storm ends), while what it silences is the only signal that
/// can descend when the encoder is past its knee on a link that shows nothing. So a clean run
/// re-arms it on the [`CAP_REPROBE_WINDOWS_MIN`] ladder, doubling each time the silence is
/// immediately re-earned. The loss, OWD, decode and keyframe signals keep their full power
/// throughout, and the host's own climb refusal stays the backstop for a genuine knee.
const ENCODE_NOOP_BACKOFFS_TO_DISARM: u32 = 2;
/// Clean windows parked at a learned cap before re-probing above it, and the ceiling that
/// interval backs off to.
///
/// A learned cap is EVIDENCE, not a spec limit: the host's short ack means "not right now",
/// which covers both its encoder's codec-level ceiling (durable) and a climb refused while
/// encode is behind cadence (transient, and routinely latched during slow start at the
/// conservative 20 Mbps default). The client cannot tell those apart from the ack alone, so the
/// re-probe is what keeps a transient from becoming the session's ceiling — and a flat ~60 s
/// clock at +12.5 % made that escape take upwards of twenty minutes to cross the gap to a
/// probe-measured link ceiling, which is indistinguishable from never.
///
/// So: probe again after 12 s, and DOUBLE the interval each time the lift is immediately
/// re-learned at the same value (see [`on_ack`](BitrateController::on_ack)). A transient is out
/// in one interval; a standing limit settles into a slow poll instead of a permanent one. The
/// re-probe itself is nearly free either way — a still-standing limit re-teaches itself in two
/// short acks, which the host pre-clamps without touching the encoder: no rebuild, no IDR.
/// The [`decode cap`](BitrateController::decode_cap_kbps) re-probes on the same schedule for
/// the same reason: the decoder's knee moves with content and thermals.
const CAP_REPROBE_WINDOWS_MIN: u32 = 16;
const CAP_REPROBE_WINDOWS_MAX: u32 = 128;
/// Two decode-driven backoffs latch the
/// [`decode cap`](BitrateController::decode_cap_kbps) only when their pre-backoff rates agree
/// within ±1/8: the decoder's knee is a RATE, so repeated chokes at the same rate are its
/// signature — two unrelated events (a Wi-Fi flush at 300 Mbps, a decode spike at 500) share
/// no knee and must not teach one. Each sample must come from a rate the controller CLIMBED
/// back to (`climb_since_backoff`) — the knee's real signature is choke, recover, re-climb,
/// choke again at the same place, and only backoffs at a climbed-to rate can agree within the
/// band (a cascade's second backoff sits at ×0.7 of the first: outside it by construction).
const DECODE_CAP_SIMILAR_DIV: u32 = 8;
/// A deciding window that DELIVERED under `current / STARVED_DELIVERY_DIV` is STARVED: the
/// stream barely flowed (a host-side capture stall, an outage, a mid-window pause), so whatever
/// distress the window carries — a flush, a keyframe-ask burst — is starvation-shaped, not
/// rate-shaped, and the decoder decoded almost nothing at the nominal rate. Such a window may
/// still back off on what the CLIENT saw — loss, a flush, a dropped frame mean the same thing
/// however little flowed, and real damage deserves the safe response — but two things it must
/// never do. It must never be a decode-knee sample, and it must never carry the HOST-ENCODE
/// signal: `encode_us` is averaged over the AUs of the window, so when almost none flowed the
/// mean describes whatever interrupted them rather than the cost of encoding at this rate (see
/// the withholding in [`BitrateController::on_window`]). Latching `current_kbps` off a starved
/// window teaches a phantom decoder cap at
/// whatever rate the stall interrupted (the periodic-capture-stall field case: every 5 s cycle
/// offers another pair of "backoffs" at the same rate — a bogus latch that then fights the
/// re-probe ladder for minutes). Deliberately far below the ×¾ utilization bar climbs require:
/// the band between them is ambiguous and keeps today's behavior.
const STARVED_DELIVERY_DIV: u32 = 4;
/// Rolling window (in 750 ms report windows, ~30 s) whose minimum mean is the OWD baseline.
/// Long enough to remember the uncongested floor, short enough to follow genuine path changes.
const BASELINE_WINDOWS: usize = 40;
/// Windows a rolling baseline must hold before the signal it feeds may fire. A baseline is a
/// rolling MINIMUM, so a single sample IS the baseline — and if that one window landed on calm
/// content, ordinary content variance clears the rise threshold by itself. That hole is not
/// theoretical: [`on_ack`](BitrateController::on_ack) deliberately CLEARS the encode baseline
/// after every decrease we ourselves asked for, so the encode down-driver re-armed on a
/// one-sample floor each time — a calm re-seed window followed by a motion scene reads as
/// `ENCODE_RISE_US` of "congestion", backs off, clears again, and ratchets to the floor on a
/// link that was never the problem. Four windows (3 s) of evidence before any of the three
/// latency signals may fire costs a little reaction latency at session start and buys a floor
/// that means something.
const BASELINE_MIN_WINDOWS: usize = 4;
/// Requests sent without a single [`crate::quic::BitrateChanged`] ack before concluding the host
/// predates bitrate renegotiation and going quiet for the rest of the session.
const MAX_UNACKED: u32 = 3;

/// Operator escape hatch: `PUNKTFUNK_ABR_MAX_MBPS` (megabits/second, the
/// `PUNKTFUNK_PYROWAVE_MAX_MBPS` convention) caps the climb ceiling however it is learned.
/// The startup link-capacity probe MEASURES the ceiling, and
/// [`set_ceiling`](BitrateController::set_ceiling)'s deliberate monotonicity makes an inflated
/// measurement permanent for the session — a link that mis-measures (a bursty middlebox, a
/// queue-flattered interval) needs a knob that binds regardless of what any probe claims.
/// `PUNKTFUNK_ABR_PROBE_KBPS` is NOT that knob: it only shrinks the burst target, not what the
/// measurement may conclude. Unset/0/garbage → no cap. Read once per controller, at
/// construction.
fn ceiling_cap_from_env() -> Option<u32> {
    std::env::var("PUNKTFUNK_ABR_MAX_MBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&m| m > 0)
        .map(|m| m.saturating_mul(1_000))
}

/// The most bitrate this stream's SHAPE could plausibly use, in kbps — the backstop the
/// probe-measured link ceiling has never had.
///
/// The measured ceiling is pure link capacity (`delivered × 0.7`) with no term for what is being
/// carried, and the utilization gate cannot supply one: a hardware encoder in CBR mode genuinely
/// fills whatever target it is handed, so "the encoder could not use the rate" never fires. The
/// field session climbed to 657 Mbps for 1440p120 — 1.49 bits per pixel, some 3× beyond any rate
/// an inter-coded stream benefits from — and reaching for it drove the client's decode latency
/// from 0.8 ms to 10 ms.
///
/// Deliberately generous. This is a bound on the absurd, not a quality opinion: it is set well
/// above what anyone actually runs, so it should never bind on a real session, and where it does
/// bind [`BitrateController::set_ceiling`] says so in the log. A session with an explicit bitrate,
/// and every PyroWave session, is outside the controller entirely and never reaches here.
///
/// It is NOT the answer to "how much is enough" — that is content-dependent and only the encoder
/// knows it (at minimum QP more bits buy nothing). This is the part that works without new
/// telemetry.
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
    // Milli-bits per pixel, so the whole computation stays in integers. H.264 is the least
    // efficient of the three and is allowed correspondingly more.
    let milli_bpp: u64 = match codec {
        crate::quic::CODEC_H264 => 1_000,
        _ => 750,
    };
    // 10-bit carries 25 % more sample depth; 4:4:4 carries twice the chroma of 4:2:0, which is
    // half again as many samples overall.
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
    // bits/s = pixel_rate × bpp; kbps = that / 1000. The milli- factor and the kbps divisor
    // cancel, so this is just pixel_rate × milli_bpp / 1_000_000.
    u32::try_from(pixel_rate.saturating_mul(milli_bpp) / 1_000_000).unwrap_or(u32::MAX)
}

/// Score one window's latency sample against its rolling-min baseline, then record it.
///
/// Shared by all three latency signals (OWD, client decode, host encode) — same shape, different
/// thresholds. `mean` is `None` when nobody reports the signal (no clock handshake, an embedder
/// that doesn't measure decode, a host that ships no stage timings); the signal is then simply
/// absent rather than clean, so it can neither mark a window bad nor teach a baseline.
///
/// The baseline is the minimum of the PRIOR windows — this window is compared before it is
/// recorded, so a rising window can't drag its own floor up with it — and only counts once
/// [`BASELINE_MIN_WINDOWS`] of them exist. Returns `(rise, severe)`; pass `i64::MAX` for
/// `severe_us` on a signal with no severe tier.
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
    /// `false` = permanently off (explicit user bitrate, an old host, or ack silence).
    enabled: bool,
    /// The rate we believe the host encodes at (updated by acks; requests are not assumed).
    current_kbps: u32,
    /// The climb ceiling: the negotiated start rate until the startup link-capacity probe
    /// raises it via [`set_ceiling`](Self::set_ceiling) — that measurement is what lets an
    /// Automatic session scale past its conservative start.
    ceiling_kbps: u32,
    /// The `PUNKTFUNK_ABR_MAX_MBPS` cap in kbps (see [`ceiling_cap_from_env`]), injected at
    /// construction so tests exercise the clamp without touching the process environment.
    /// `None` = no cap.
    ceiling_cap_kbps: Option<u32>,
    /// What this stream's SHAPE could plausibly use (see [`stream_ceiling_kbps`]), set once the
    /// session's mode and codec are known. `None` = never set, i.e. exactly the old behavior.
    /// Bounds only what [`set_ceiling`](Self::set_ceiling) LEARNS: the negotiated start rate is a
    /// number the host resolved on purpose and is left alone.
    stream_cap_kbps: Option<u32>,
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
    /// This session's frame budget in µs (one refresh interval), the unit the encode thresholds
    /// are expressed in — see [`encode_thresholds`](Self::encode_thresholds). `None` = the mode
    /// was never plumbed in, and the 120 Hz constants stand exactly as before.
    frame_budget_us: Option<i64>,
    /// The window mean host-encode latency (µs) that drove the last encode-attributed backoff;
    /// `0` = none yet, or the streak was broken by a backoff something else drove.
    encode_backoff_us: i64,
    /// Consecutive encode-attributed backoffs after which encode time did NOT come down (see
    /// [`ENCODE_NOOP_BACKOFFS_TO_DISARM`]).
    encode_noop_backoffs: u32,
    /// The encode down-driver is stood down: its rises are not answering the rate, so they
    /// neither mark a window bad nor teach a baseline. Lifted by a clean run (see
    /// `encode_reprobe_after`) or a mode switch — never permanent, like every other piece of
    /// evidence-learned state here.
    encode_disarmed: bool,
    /// Clean windows since the stand-down, against `encode_reprobe_after`.
    encode_disarm_clean_windows: u32,
    /// Clean windows the stand-down must survive before the signal is re-armed. Doubles each
    /// time a re-armed signal is immediately silenced again, so a standing contention settles
    /// into a slow poll instead of thrashing ([`CAP_REPROBE_WINDOWS_MIN`]).
    encode_reprobe_after: u32,
    /// A stand-down has been lifted at least once, so the next one is re-silencing something the
    /// re-probe already tried — the trigger for backing that clock off.
    encode_rearmed: bool,
    /// The host-taught rate cap (§ABR overdrive): latched when the host acks BELOW what we
    /// asked twice consecutively at the same value — its encoder's codec-level ceiling, or a
    /// climb refusal while host encode can't hold cadence. Kept apart from `ceiling_kbps` so
    /// the probe-measured link authority survives a mode switch's reset. Slowly re-probed
    /// ([`CAP_REPROBE_WINDOWS_MIN`]) so scene-dependent evidence can't cap the session forever.
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
    /// Clean windows spent parked at the learned cap (the re-probe clock) and the interval it is
    /// counting toward — [`CAP_REPROBE_WINDOWS_MIN`], doubled toward
    /// [`CAP_REPROBE_WINDOWS_MAX`] each time a lift is immediately re-learned.
    cap_probe_windows: u32,
    cap_reprobe_after: u32,
    /// The client-decoder rate cap, mirroring [`host_cap_kbps`](Self::host_cap_kbps) for the
    /// OTHER end of the pipe: latched when two CONSECUTIVE backoffs carried decode-severe
    /// evidence (a deep decode-latency excursion, or a jump-to-live flush — in the
    /// decoder-saturation regime the flushed backlog formed BEHIND a decoder that stopped
    /// keeping up) at a similar pre-backoff rate. Without it a decoder knee below the link
    /// ceiling is a permanent 30–60 s sawtooth: every ×0.7 backoff re-climbs toward a ceiling
    /// the decoder can't hold, and each cycle costs a flush plus a dropped-frame burst (the
    /// 1440p120 HEVC field case: knee ~490 Mbps under a ~658 Mbps ceiling). Slowly re-probed
    /// on the [`CAP_REPROBE_WINDOWS_MIN`] clock, exactly like the host cap, so a decoder that
    /// recovers (lighter content, thermal headroom) climbs again — the latch is never
    /// permanent.
    decode_cap_kbps: Option<u32>,
    /// The previous decode-driven backoff's pre-backoff rate (0 = the last backoff wasn't
    /// decode-driven): the reference the next one must land near ([`DECODE_CAP_SIMILAR_DIV`])
    /// to latch the cap — one spurious flush teaches nothing.
    decode_backoff_kbps: u32,
    /// Decode-flagged windows in the CURRENT bad-window streak. The ordinary two-window backoff
    /// path is the decoder knee's most common presentation (a standing 15–45 ms decode rise —
    /// deep enough to hurt, not deep enough for the severe tier), and judging decode evidence
    /// from the FINAL window alone threw that attribution away: the backoff the decode signal
    /// itself caused then RESET the knee streak. Counted per bad window, cleared with the streak.
    streak_decode_windows: u32,
    /// Whether `current_kbps` has RISEN (via an ack — ours or a host-initiated re-target) since
    /// the last backoff. A knee sample is only meaningful for a rate the controller climbed to
    /// or held; a backoff that fires while the previous backoff's damage is still draining
    /// samples a rate the decoder never choked at (the host acks a ×0.7 request in ~100 ms, so
    /// a cascade's second backoff ALWAYS sits at the already-reduced rate — dissimilar to the
    /// knee by construction, 0.7 < 7/8). Such a backoff neither samples nor erases the
    /// reference.
    climb_since_backoff: bool,
    /// Clean windows spent parked at the learned decode cap (its re-probe clock), and that
    /// clock's own backoff interval — same schedule as the host cap's.
    decode_cap_probe_windows: u32,
    decode_cap_reprobe_after: u32,
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
    /// The last ceiling-clamp target asked for (0 = none). A session running ABOVE its effective
    /// ceiling is asked down to it exactly once per distinct target — a host that answers higher
    /// has said it cannot go there, and re-asking every cooldown only costs reconfigures.
    ceiling_ask_kbps: u32,
}

impl BitrateController {
    /// `start_kbps` is the Welcome-resolved session bitrate when the user chose Automatic, or `0`
    /// to build a permanently-disabled controller (explicit bitrate / an old host that didn't
    /// echo one — no known ceiling to work against).
    pub(crate) fn new(start_kbps: u32) -> Self {
        Self::with_ceiling_cap(start_kbps, ceiling_cap_from_env())
    }

    /// [`new`](Self::new) with the `PUNKTFUNK_ABR_MAX_MBPS` cap injected — the seam the unit
    /// tests use so the clamp's behavior never depends on the test process's environment.
    fn with_ceiling_cap(start_kbps: u32, ceiling_cap_kbps: Option<u32>) -> Self {
        BitrateController {
            enabled: start_kbps > 0,
            current_kbps: start_kbps,
            // The env cap binds the NEGOTIATED ceiling too, not just probe-learned ones. It is
            // the only lever an Automatic session gives the operator (Automatic is precisely
            // "no explicit bitrate"), so a start rate above it has to come down rather than
            // stand as a ceiling the user asked not to reach — see the clamp-down step in
            // [`on_window`](Self::on_window).
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
            // The negotiated start rate was held, not drained to — the first backoff ever is a
            // legitimate knee sample.
            climb_since_backoff: true,
            decode_cap_probe_windows: 0,
            decode_cap_reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            proven_kbps: 0,
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
            unacked: 0,
            ceiling_ask_kbps: 0,
        }
    }

    /// Raise the climb ceiling to a measured link capacity (the startup speed-test probe's
    /// delivered throughput with headroom already subtracted by the caller). Without this call
    /// the ceiling stays the negotiated start rate — exactly the old behavior. Never lowers:
    /// a congested-moment measurement must not shrink authority below what was negotiated
    /// (descent is the congestion signals' job). The `PUNKTFUNK_ABR_MAX_MBPS` cap clamps HERE
    /// — the one funnel every learned ceiling passes through — so it binds no matter how the
    /// ceiling was learned; monotonicity is precisely why the user needs it (one inflated
    /// measurement is otherwise permanent for the session).
    pub(crate) fn set_ceiling(&mut self, kbps: u32) {
        let measured = kbps;
        let kbps = kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.stream_cap_kbps.unwrap_or(u32::MAX));
        if self.enabled && kbps < measured {
            // Say so when it binds. A cap that silently trims what the link offered is exactly
            // the kind of thing nobody reports and everybody wonders about, so a field log should
            // carry both numbers.
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

    /// Teach the controller what this session's mode and codec could plausibly use (see
    /// [`stream_ceiling_kbps`]). Bounds future LEARNED ceilings at the same funnel as the
    /// operator's env cap.
    ///
    /// The FIRST set is the session's negotiated shape and keeps the founding semantics — a
    /// negotiated start rate above it stands, the host resolved that number (pinned by
    /// `the_stream_bound_clamps_a_learned_ceiling_only`). A RE-set is a mode switch, and
    /// there a DROP in pixel rate also rebinds the already-standing ceiling: `set_ceiling`
    /// clamps only at learn time and deliberately never lowers, so a 4K-learned ceiling
    /// would otherwise stand over a 720p stream with only the reactive loss/decode signals
    /// to bound the climb (review §2.1).
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

    /// Teach the controller this session's refresh rate, so the encode thresholds can be sized in
    /// FRAME BUDGETS rather than the 120 Hz durations they were calibrated at (see
    /// [`ENCODE_RISE_US`]). Ignored for a nonsense rate — the defaults are the old behavior, which
    /// is the right answer when the mode is not known.
    pub(crate) fn set_frame_budget(&mut self, refresh_hz: u32) {
        if refresh_hz > 0 {
            self.frame_budget_us = Some(1_000_000 / refresh_hz as i64);
        }
    }

    /// `(rise, severe)` for the host-encode signal: half a frame budget and one and a half of
    /// them, the shape [`ENCODE_RISE_US`] documents, against this session's actual budget.
    ///
    /// Scales with the SESSION REFRESH, not with the rate the source actually delivers. A game
    /// rendering below refresh stretches the real budget further still (the host stretches its own
    /// cadence deadline by exactly that, `cadence_budget`), so a sub-refresh source can still
    /// present a one-frame hiccup above the severe tier — that residue is what
    /// [`ENCODE_NOOP_BACKOFFS_TO_DISARM`] is for. Deliberately not chased here: the client would
    /// have to infer the source period from arrival cadence, which is the same jitter the signal
    /// is trying to read through.
    fn encode_thresholds(&self) -> (i64, i64) {
        match self.frame_budget_us {
            Some(budget) => (budget / 2, budget * 3 / 2),
            None => (ENCODE_RISE_US, ENCODE_SEVERE_US),
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
                        // Re-learning a cap we had already lifted means the limit is STANDING,
                        // not the transient the re-probe exists to escape — back its clock off
                        // (see [`CAP_REPROBE_WINDOWS_MIN`]) so a hard encoder ceiling settles
                        // into a slow poll instead of two pointless acks every 12 s. A first
                        // latch starts the clock fast, because that is the case that matters.
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
                    // GRANTED in full at or above the learned cap: the limit that taught it is
                    // gone, and we have the host's own word for it. Drop the cap outright rather
                    // than keep crawling up in +12.5 % re-probe steps — for a cap latched from a
                    // transient (a host briefly behind cadence) that crawl is the entire
                    // remaining cost of the transient, and it is measured in minutes.
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
                // The rate ROSE — whatever the pipeline chokes on next, it will choke at a rate
                // it was driven up to: a fresh knee sample (see `climb_since_backoff`). An ack'd
                // decrease deliberately does not arm this — the drain after a backoff is not a
                // knee encounter.
                self.climb_since_backoff = true;
            }
            self.current_kbps = kbps;
            // The host may run ABOVE our climb ceiling, and be right to: it sends an unsolicited
            // `BitrateChanged` when a rebuild re-resolves an Automatic rate for what it actually
            // encodes (a 1080p session mirroring a 4K panel resolves ~3× higher), and that is
            // the host's own Automatic answer, not a climb we asked for. Let the ceiling follow
            // — `set_ceiling` only ever raises, and still clamps to the operator's
            // `PUNKTFUNK_ABR_MAX_MBPS`, which is what must bind here if anything does. Without
            // this the ceiling stays at the stale negotiated rate and the step-down below
            // immediately drags the host back off the rate it just chose. A no-op for ordinary
            // acks: we never request above the effective ceiling in the first place.
            self.set_ceiling(kbps);
        }
        self.unacked = 0;
    }

    /// An accepted mode switch: the encoder's ceiling and compute knee are properties of the
    /// MODE (4K120 caps where 1080p60 never would) — drop the mode-scoped learned state. The
    /// decoder's knee is just as mode-scoped (pixel rate drives both ends of the codec), so
    /// the decode cap goes with it. The probe-measured `ceiling_kbps` (a LINK property)
    /// survives.
    ///
    /// Every rolling BASELINE is mode-scoped too, and for the same reason the encode one always
    /// was: a mode switch changes what "normal" costs at both ends of the pipe. 4K120 decodes
    /// and encodes far slower than 1080p60 and puts bigger frames on the wire, so a baseline
    /// learned under the old mode is a floor the new one clears on its very first window —
    /// [`DECODE_RISE_US`] is 15 µs-thousands, well inside the gap between those two modes. Left
    /// standing (only `encode_means` used to be cleared here), the ~30 s it takes
    /// [`BASELINE_WINDOWS`] to age out is ~30 s of every window scoring bad, which is a ×0.7
    /// backoff every other window: a switch UP in mode cratered the rate instead of raising it.
    /// `proven_kbps` goes with them — it is the mark climbs are bounded against, and throughput
    /// the OLD mode's decoder digested is not evidence about this one. It re-earns itself from
    /// the next window.
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
        // The encode down-driver's disarm is mode-scoped like everything else here: the new mode
        // is a different amount of encode work per frame, so a rate that could not move encode
        // time under the old one says nothing about this one. Re-arm and let it prove itself
        // again. (The caller re-sizes the frame budget for the new refresh alongside this.)
        self.encode_disarmed = false;
        self.encode_backoff_us = 0;
        self.encode_noop_backoffs = 0;
        self.encode_disarm_clean_windows = 0;
        self.encode_reprobe_after = CAP_REPROBE_WINDOWS_MIN;
        self.encode_rearmed = false;
        self.proven_kbps = 0;
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
        // doesn't drag its own baseline up), then record it. No severe tier — a standing queue is
        // congestion evidence, not visible damage, so it always takes the two-window path.
        let (owd_bad, _) = score_baseline(&mut self.owd_means, owd_mean_us, OWD_RISE_US, i64::MAX);
        // Decode-stage latency: same rolling-min-baseline treatment as OWD, but measuring the
        // CLIENT'S decoder rather than the link. A rise means the decoder is backlogging frames —
        // the bottleneck the network signals are blind to. Marking the window bad both ends slow
        // start (so the climb stops the moment decode latency lifts, instead of doubling on into
        // the link ceiling) and, sustained, drives the ×0.7 backoff down to the real decode limit.
        // An excursion far past baseline is SEVERE: the decoder is deep in spike-overload and the
        // user is watching it — skip the two-window confirmation.
        let (decode_bad, decode_severe) = score_baseline(
            &mut self.decode_means,
            decode_mean_us,
            DECODE_RISE_US,
            DECODE_SEVERE_US,
        );
        // STARVED (see [`STARVED_DELIVERY_DIV`]): the window carried under a quarter of the rate
        // it was allowed. Hoisted above the signal scoring because the encode signal below is not
        // merely inconvenient in such a window, it is not a measurement — see there. `current_kbps`
        // does not move inside this function, so this is the same value the backoff block reads.
        let starved =
            (actual_kbps as u64) * (STARVED_DELIVERY_DIV as u64) < self.current_kbps as u64;
        // Host-encode latency: the same rolling-min-baseline treatment, measuring the HOST'S
        // encoder — the compute-knee down-driver (see [`ENCODE_RISE_US`]). This is the only
        // signal that can push an already-too-high rate back under the knee: the host refuses
        // further climbs while behind cadence, but nothing else ever DESCENDS on a clean LAN.
        //
        // Withheld entirely in a STARVED window. `encode_us` is a per-AU host measurement averaged
        // over the window, so when almost no AUs flowed the mean is taken over the handful that
        // straddled whatever interrupted them — and their encode time carries that interruption,
        // not the cost of encoding at this rate. The field case: a 401 ms capture-ring and encoder
        // rebuild (an exclusive-topology eviction, entirely host-local) produced one window with
        // `encode_mean_us=15063` against a ~2800 baseline, `actual_kbps=390` against a 20 000
        // target, and `loss_ppm=0`. That cleared [`ENCODE_SEVERE_US`], took the one-window path,
        // and cost a ×0.7 plus slow start for the rest of the session — on a link that never
        // dropped a packet. Passed as absent rather than ignored so it cannot teach the rolling
        // baseline either: a sample that measures a stall is not evidence about anything.
        //
        // The other signals keep their full power here on purpose. Loss, a flush and a dropped
        // frame describe what reached the CLIENT, and they mean the same thing however little
        // flowed — the periodic-capture-stall case (see [`STARVED_DELIVERY_DIV`]) still backs off
        // on one window, as its tests require.
        //
        // Withheld the same way once the signal has DISARMED itself (see
        // [`ENCODE_NOOP_BACKOFFS_TO_DISARM`]): a rise the rate has twice failed to answer is not
        // evidence about the rate, so it must neither mark a window bad nor teach a baseline.
        let (encode_rise_us, encode_severe_us) = self.encode_thresholds();
        let encode_usable = !starved && !self.encode_disarmed;
        let (encode_bad, encode_severe) = score_baseline(
            &mut self.encode_means,
            encode_mean_us.filter(|_| encode_usable),
            encode_rise_us,
            encode_severe_us,
        );
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
        // The proven-throughput high-water mark: this window's delivered rate is now demonstrably
        // digestible — the pipeline carried it and NOTHING went wrong while it did. Scored after
        // the verdict and gated on the whole of it, not on decode alone: the mark never decays, so
        // one window is permanent authority over how far every later climb may step, and the
        // windows that overstate delivered throughput are exactly the damaged ones (a stall's
        // backlog draining in a single window, a flush's queue, the FEC surge that answers a loss
        // burst). "Loss doesn't disqualify, the bytes still arrived" was true about the bytes and
        // wrong about the conclusion drawn from them.
        if !bad && actual_kbps > self.proven_kbps {
            self.proven_kbps = actual_kbps;
        }
        if bad {
            self.bad_windows += 1;
            if decode_bad {
                // Per-window decode attribution for the streak (see `streak_decode_windows`) —
                // scored HERE because at backoff time only the final window's signals are in
                // scope, and on the two-window path the first bad window never even reaches a
                // decision (the cooldown eats it).
                self.streak_decode_windows += 1;
            }
            self.clean_windows = 0;
            // Any congestion signal ends slow start for good — from here on, climbs are additive.
            self.probing = false;
        } else {
            self.clean_windows += 1;
            self.bad_windows = 0;
            self.streak_decode_windows = 0;
        }
        // The learned host cap re-probe (see [`CAP_REPROBE_WINDOWS_MIN`]): after a clean run
        // parked at the cap, lift it one step (+12.5 %, ceiling-bounded) so a scene-dependent
        // refusal can't quietly cap the whole session — a still-standing limit just re-latches
        // from the next pair of short acks, at zero encoder cost, and backs the clock off.
        if let Some(cap) = self.host_cap_kbps {
            if bad {
                self.cap_probe_windows = 0;
            } else if self.current_kbps >= cap.saturating_sub(cap / 16) {
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
        // The decode cap re-probes on the same clock and for the same reason: the knee is
        // content- and thermals-dependent evidence, not a spec limit — a decoder that recovers
        // must get its headroom back, so the latch clears UPWARD through here rather than ever
        // being permanent. A still-standing knee re-latches from the next pair of
        // decode-driven backoffs.
        if let Some(cap) = self.decode_cap_kbps {
            if bad {
                self.decode_cap_probe_windows = 0;
            } else if self.current_kbps >= cap.saturating_sub(cap / 16) {
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
        // The encode down-driver's stand-down re-probes on the same clock, for the same reason
        // the two caps do: it is EVIDENCE, not a spec limit. What silenced it — a game
        // saturating the GPU, a shader-compile storm, another app on the card — is exactly the
        // sort of thing that ENDS mid-session, and what it silences is the only signal that can
        // descend when the encoder is genuinely past its compute knee on a link that shows
        // nothing. Left permanent, one contended stretch would strip that protection from every
        // later minute of the session, including the calm ones where a climb can reach a rate
        // the ASIC cannot hold.
        //
        // A clean run is the cheapest moment to ask again: nothing else is unhappy, so if the
        // rate still cannot move encode time, two more no-op backoffs stand it down again at a
        // bounded cost — while the doubling interval keeps a genuinely standing contention from
        // thrashing. The asymmetry decides it: a too-eager re-arm costs one ×0.7, a too-permanent
        // silence costs the knee protection outright.
        if self.encode_disarmed {
            if bad {
                self.encode_disarm_clean_windows = 0;
            } else {
                self.encode_disarm_clean_windows += 1;
                if self.encode_disarm_clean_windows >= self.encode_reprobe_after {
                    self.encode_disarmed = false;
                    self.encode_rearmed = true;
                    self.encode_disarm_clean_windows = 0;
                    // Re-arm on a FRESH baseline and with no streak carried over: the level the
                    // old backoffs fired at describes a regime that has since been clean for
                    // seconds, so it is not the reference the next one should be judged against.
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
            // Decode-cap learning (see [`decode_cap_kbps`](Self::decode_cap_kbps)): a backoff
            // with decode evidence remembers its pre-backoff rate; the next one at a similar
            // rate latches that rate as the decoder's knee. One event never latches (a spurious
            // flush must stay a one-off), and a decode-free backoff in between breaks the
            // streak — whatever it saw, it wasn't the same knee.
            //
            // Decode evidence, in order:
            // - a decode-SEVERE excursion in the deciding window;
            // - the ordinary two-window path where EVERY bad window was decode-flagged
            //   (`streak_decode_windows`) — the knee's most common presentation is a standing
            //   15–45 ms rise, below the severe tier, and the deciding window alone can't see
            //   that the streak it ends was decode's doing;
            // - a keyframe-ask storm without meaningful loss: a decoder begging for fresh
            //   pictures on a clean link is being overdriven, whatever its latency figure says
            //   (some decoders wedge rather than queue — the Steam Deck presentation). With
            //   real loss present the asks are network-attributed and teach nothing here;
            // - a flush, where the decode signal can't speak against it: on an embedder that
            //   reports decode latency, a flush with FLAT decode is a network event (a stall, a
            //   clock step) that drained a queue the decoder was keeping up with — teaching a
            //   "decoder knee" from it caps the session on the wrong end of the pipe. Where the
            //   signal is absent the flush is the only decoder-saturation evidence there is.
            let decode_evidence = decode_severe
                || self.streak_decode_windows >= BAD_WINDOWS_TO_DECREASE
                || (recovery_kf >= RECOVERY_KF_BAD && loss_ppm < HEAVY_LOSS_PPM)
                || (flushed && (decode_bad || decode_mean_us.is_none()));
            // `starved` (the deciding window barely flowed, so it says nothing about what the
            // decoder can hold at this rate) is now computed once at the top of the window — the
            // same predicate also governs the severe tier and slow start.
            if !self.climb_since_backoff {
                // Still draining the previous backoff: the host acks a ×0.7 request in ~100 ms,
                // so this window's rate is one the decoder never choked at while keeping up —
                // its distress is residue of the choke above. Not a knee sample either way:
                // neither latch against it nor let it erase the reference the real knee set.
                tracing::debug!(
                    at_kbps = self.current_kbps,
                    reference_kbps = self.decode_backoff_kbps,
                    "adaptive bitrate: backoff without an intervening climb — draining the \
                     previous choke, not a knee sample"
                );
            } else if starved {
                // Same "not a knee sample either way" treatment as the draining arm: neither
                // latch against a starved window nor let it erase the reference a real knee
                // set — the next genuine choke at that rate must still find its pair.
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
                // Latch just UNDER the rate that choked, not at it: the knee is the rate the
                // decoder could not hold, so a cap sitting exactly on it authorizes climbing
                // straight back into the failure — the sawtooth the cap exists to end, merely
                // slower. One sixteenth is inside the ±1/8 band the pair had to agree within,
                // so it costs nothing the evidence actually established.
                let knee = rate.saturating_sub(rate / 16).max(self.floor_kbps);
                if similar && self.decode_cap_kbps.is_none_or(|c| knee < c) {
                    // Same standing-vs-transient backoff as the host cap.
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
            // Encode attribution (see [`ENCODE_NOOP_BACKOFFS_TO_DISARM`]): did the LAST
            // encode-driven backoff buy anything? Judged from the level this one fires at, not
            // from the baseline — `on_ack` re-seeded that after the last decrease, so the firing
            // level is the only surviving record of what encode time did in between. Network
            // distress disqualifies the attribution: loss, a flush or a dropped frame explain the
            // backoff without the encoder, and cutting the rate genuinely is the remedy for those.
            let encode_attributed = (encode_severe || encode_bad)
                && dropped == 0
                && !flushed
                && loss_ppm < HEAVY_LOSS_PPM;
            if let Some(mean) = encode_mean_us.filter(|_| encode_attributed) {
                if self.encode_backoff_us > 0
                    && mean >= self.encode_backoff_us.saturating_sub(encode_rise_us)
                {
                    // Fired again no lower than last time: the ×0.7 in between did nothing.
                    self.encode_noop_backoffs += 1;
                    if self.encode_noop_backoffs >= ENCODE_NOOP_BACKOFFS_TO_DISARM {
                        // Re-silencing something the re-probe had already lifted means the
                        // contention is STANDING, not the transient the re-probe exists to ride
                        // out — back its clock off, exactly as both learned caps do.
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
                // Something else drove this one: the encode streak is broken, and the level the
                // next encode-driven backoff would have to beat no longer means anything.
                self.encode_backoff_us = 0;
                self.encode_noop_backoffs = 0;
            }
            self.climb_since_backoff = false;
            let next = ((self.current_kbps as u64 * 7 / 10) as u32).max(self.floor_kbps);
            self.bad_windows = 0;
            self.streak_decode_windows = 0;
            return self.request(next, now);
        }
        // Climbs only fire off a UTILIZED clean window (actual delivered ≥ ¾ of the target — the
        // target was genuinely tested, not idling under calm content) and step at most ×1.5 past
        // the proven high-water mark. Calm windows still count as clean (clean_windows keeps
        // accumulating — the network is healthy), they just can't authorize a climb; the first
        // utilized window after a long-enough clean run climbs immediately.
        let utilized =
            actual_kbps as u64 * UTILIZATION_DEN >= self.current_kbps as u64 * UTILIZATION_NUM;
        // The effective ceiling folds in both learned caps: the probe measured the LINK, the
        // host's short acks measured the ENCODER, and the decode cap measured the CLIENT
        // DECODER — whichever binds first is the limit.
        let eff_ceiling = self
            .ceiling_kbps
            .min(self.host_cap_kbps.unwrap_or(u32::MAX))
            .min(self.decode_cap_kbps.unwrap_or(u32::MAX));
        // Above the ceiling with nothing wrong: the session negotiated a rate the operator's
        // `PUNKTFUNK_ABR_MAX_MBPS` forbids (no congestion signal will ever find this — the link
        // is fine, the cap is a policy). Step straight to it rather than sitting above a limit
        // the user set, and never below the floor. Asked ONCE per distinct target: if the host
        // answers with something higher it has told us it cannot go there (its own floor, an
        // encoder minimum), and repeating the ask every cooldown would buy nothing but a
        // reconfigure each time.
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

    /// The decision [`on_window`](Self::on_window) returned never reached the wire (the control
    /// queue was full). Undo the request's bookkeeping: [`MAX_UNACKED`] exists to detect a HOST
    /// that doesn't answer, and counting a message we never sent toward it retires the
    /// controller for the session — with a log line blaming an "older host" that is not what
    /// happened. Clearing the pending request also keeps a later unsolicited ack from being
    /// judged short against a rate we never asked for.
    pub(crate) fn on_request_dropped(&mut self) {
        self.unacked = self.unacked.saturating_sub(1);
        self.last_requested_kbps = None;
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

    /// The stream bound must cut the field runaway and must NOT touch a session anyone
    /// actually runs. Both halves matter: a cap that silently trims a happy user is a
    /// regression nobody reports.
    #[test]
    fn the_stream_bound_cuts_the_absurd_and_spares_the_ordinary() {
        use crate::quic::{CHROMA_IDC_420, CHROMA_IDC_444, CODEC_H264, CODEC_HEVC};

        // The field session: 1440p120 HEVC Main10 4:2:0. The probe measured 939 Mbps and the
        // ceiling became 657 Mbps — 1.49 bits/pixel. The client's decode latency was still flat
        // (0.78 ms) at ~396 Mbps delivered and blew up to 10 ms by ~461 Mbps, so the bound has to
        // land below that knee to have helped.
        let field = stream_ceiling_kbps(2560, 1440, 120, CODEC_HEVC, 10, CHROMA_IDC_420);
        assert!(
            field < 657_000,
            "the bound must actually bind on the field case, got {field}"
        );
        assert!(
            field < 460_000,
            "and land under the decode knee this session found, got {field}"
        );

        // 1080p60 HEVC 8-bit: people do run 80-100 Mbps here and must not be trimmed.
        let ordinary = stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_420);
        assert!(
            ordinary >= 90_000,
            "an ordinary 1080p60 session must keep its headroom, got {ordinary}"
        );

        // H.264 needs more bits for the same picture, and 4:4:4 / 10-bit carry more samples.
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
        // A degenerate mode must not produce a bound of zero and strangle the session.
        assert_eq!(
            stream_ceiling_kbps(0, 0, 0, CODEC_HEVC, 8, CHROMA_IDC_420),
            u32::MAX
        );
    }

    /// The bound rides the same funnel as the operator's env cap, and binds only what the probe
    /// LEARNS — a rate the host resolved on purpose is left alone.
    #[test]
    fn the_stream_bound_clamps_a_learned_ceiling_only() {
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000, "a learned ceiling is bounded");

        // Never set → exactly the old behaviour.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 657_000);

        // A negotiated start rate above the bound stands: the host resolved that number.
        let mut c = BitrateController::new(300_000);
        c.set_stream_cap(100_000);
        assert_eq!(c.ceiling_kbps, 300_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 300_000,
            "and a learned ceiling under it never lowers what was negotiated"
        );

        // The tighter of the two caps wins.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 50_000,
            "the env cap still binds when it is tighter"
        );
    }

    /// Review §2.1: the stream-shape cap was computed once from the Welcome mode and never
    /// again — 1080p→4K kept a 1080p-sized climb ceiling, 4K→720p left an oversized one
    /// standing. A mode switch now re-teaches the cap: an upswitch opens room for the probe's
    /// measurement to authorize more, a downswitch rebinds the already-learned ceiling.
    #[test]
    fn a_mode_switch_reteaches_the_stream_cap_both_ways() {
        // 1080p session, probe measured a fat link: ceiling bound at the 1080p shape.
        let mut c = BitrateController::new(20_000);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000);

        // Switch UP to 4K: the new shape allows more, and the probe's measurement (already
        // taken this session) may re-authorize up to it.
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

        // Switch DOWN to 720p: the learned 4K ceiling must not stand over the small stream —
        // `set_ceiling` never lowers, so the re-taught cap is what rebinds it.
        c.on_mode_switch();
        c.set_stream_cap(42_000);
        assert_eq!(
            c.ceiling_kbps, 42_000,
            "a downswitch rebinds the already-learned ceiling"
        );

        // A disabled controller (explicit bitrate) is untouched by all of it.
        let mut d = BitrateController::new(0);
        d.set_stream_cap(100_000);
        d.set_stream_cap(42_000);
        assert_eq!(d.ceiling_kbps, 0);
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
        // A fat link (probe measured ~300 Mbps) but a decoder that saturates below it.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Slow start doubles while the decoder keeps up, and the first BASELINE_MIN_WINDOWS of
        // those windows are what teach the decode baseline (one sample is not a floor).
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
            ) {
                last = k;
                c.on_ack(k);
            }
        }
        assert_eq!(last, 300_000, "slow start should reach the probed ceiling");
        // Now the decoder starts backing up (30 ms over the learned baseline): the window is bad,
        // so the climb stops instead of parking at the link ceiling…
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
                0
            ),
            None
        );
        // …and a second backed-up window backs the rate off toward the real decode limit rather
        // than choking the decoder at the link ceiling (the reported bug).
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
                0
            ),
            Some(210_000)
        );
    }

    #[test]
    fn one_calm_window_is_not_a_baseline() {
        // The ratchet this guard exists to stop: our own decrease CLEARS the encode baseline, so
        // it re-seeds from whatever the next window happens to be. If that window is calm, the
        // ordinary content variance that follows reads as a rise, backs off, clears again — all
        // the way to the floor on a link that was never the problem. A single sample must not
        // arm the signal.
        let mut c = BitrateController::new(100_000);
        let start = Instant::now();
        // One calm 3 ms encode window, then windows 9 ms above it: far past ENCODE_RISE_US, and
        // sustained — yet no baseline exists to judge them against yet.
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
                    0
                ),
                None,
                "window {i} fired off a baseline of fewer than {BASELINE_MIN_WINDOWS} samples"
            );
        }
        // With a real baseline (min 3 ms over 4 windows) the signal works exactly as before: a
        // sustained rise past it still backs the rate off.
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
                0
            ),
            Some(70_000)
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
        // A cadence-refusal cap is scene evidence, not a spec limit: after a clean run parked at
        // the cap, lift one step so a one-time heavy scene can't cap the session forever. A
        // still-standing limit just re-latches from the next short-ack pair, at zero cost.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.host_cap_kbps, Some(794_000));
        // The FIRST re-probe is the fast one — a transient refusal must not cost the session
        // minutes to escape.
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
            );
        }
        assert_eq!(c.host_cap_kbps, Some(794_000 + 794_000 / 8));
    }

    #[test]
    fn a_transient_refusal_does_not_pin_the_session() {
        // The field failure this whole cap-escape change exists for. A host that escalates its
        // capture/encode pipeline once — a startup hitch is enough — used to refuse every climb
        // for the rest of the session; the client latched that refusal as a cap, at whatever
        // rate slow start had reached, which is routinely the 20 Mbps default. Escaping cost
        // +12.5 % per ~60 s: north of twenty minutes to reach a 300 Mbps link ceiling, which the
        // user experiences as "Automatic is broken".
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(300_000); // the startup probe measured a fat link
        let start = Instant::now();
        let mut tick = 0u32;
        let mut windows_pinned = 0u32;
        // Two refused climbs at the same rate → the cap latches at 20 Mbps.
        for _ in 0..2 {
            let k = run_clean(&mut c, start, tick, 4).expect("slow start should ask to climb");
            tick += 4;
            assert!(k > 20_000);
            c.on_ack(20_000); // "behind cadence — held at the current rate"
        }
        assert_eq!(c.host_cap_kbps, Some(20_000));
        // The host recovers immediately (its bucket drains; the escalation bought the headroom
        // it was for), but the client has no way to know that except by asking again. Drive
        // clean windows and grant whatever it asks for.
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
        // ~750 ms a window: this must be tens of seconds, not the old tens of minutes.
        assert!(
            windows_pinned <= 40,
            "took {windows_pinned} windows (~{} s) to escape a transient refusal",
            windows_pinned * 3 / 4
        );
        // And the disproven cap is gone, not merely nudged upward.
        assert!(c.host_cap_kbps.is_none());
    }

    #[test]
    fn a_host_retarget_above_the_ceiling_raises_it() {
        // The host sends an unsolicited `BitrateChanged` when a rebuild re-resolves an Automatic
        // rate for what it ACTUALLY encodes — a 1080p session mirroring a 4K panel resolves far
        // above the negotiated rate. That is the host's own Automatic answer, so the climb
        // ceiling has to follow it; otherwise the ceiling stays stale and the step-down drags
        // the host straight back off the rate it just chose.
        let mut c = BitrateController::new(20_000);
        assert_eq!(c.ceiling_kbps, 20_000);
        c.on_ack(60_000); // unsolicited: no request was outstanding
        assert_eq!(c.current_kbps, 60_000);
        assert_eq!(c.ceiling_kbps, 60_000);
        let start = Instant::now();
        // No step-down, and no spurious re-target of any kind.
        assert_eq!(run_clean(&mut c, start, 0, 4), None);
        // The operator's cap still outranks it — that is the one thing that must bind here.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.on_ack(60_000);
        assert_eq!(c.ceiling_kbps, 50_000);
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
    }

    #[test]
    fn a_standing_cap_backs_its_reprobe_clock_off() {
        // The other half of the re-probe: an encoder's real codec ceiling (794 Mbps, L6.2)
        // re-teaches itself every time the lift is tried. Escaping fast is right for a
        // transient and pointless here, so each re-learn doubles the interval — a hard limit
        // settles into a slow poll instead of two acks every 12 s for the whole session.
        let mut c = BitrateController::new(400_000);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000);
        assert_eq!(c.cap_reprobe_after, CAP_REPROBE_WINDOWS_MIN);
        // Each round: park clean at the cap until it re-probes upward, then have the host refuse
        // the lift at the same value again. That is a STANDING limit, so the clock doubles.
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
                );
                tick += 1;
            }
            let lifted = c.host_cap_kbps.expect("cap should still be latched");
            assert!(lifted > 794_000, "round {round}: the re-probe never lifted");
            // The host clamps the lift straight back to its real ceiling.
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

    /// One encode-attributed choke: re-seed the baseline `on_ack` cleared, then present `level`
    /// again — the shape of an encoder held up by something the last ×0.7 did nothing about.
    /// Four seed windows is under [`CLEAN_WINDOWS_TO_INCREASE`], so no cycle can climb its way
    /// out from under the test.
    fn encode_choke(
        c: &mut BitrateController,
        start: Instant,
        tick: &mut u32,
        level: i64,
    ) -> Option<u32> {
        for _ in 0..BASELINE_MIN_WINDOWS {
            let at = ticks(start, *tick);
            *tick += 1;
            // Seed windows are clean by construction; ack a climb if the controller takes one, so
            // the helper stays usable in tests that leave climb headroom below the ceiling.
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
        )
    }

    /// `n` clean windows carrying no encode sample, acking any climb the controller takes.
    fn clean_run(c: &mut BitrateController, start: Instant, tick: &mut u32, n: u32) {
        for _ in 0..n {
            let at = ticks(start, *tick);
            *tick += 1;
            if let Some(k) = c.on_window(at, 0, 0, Some(10_000), None, None, 1_000_000, false, 0) {
                c.on_ack(k);
            }
        }
    }

    /// Drive the field ratchet: encode-attributed backoffs at a level the ×0.7s never move, until
    /// the signal stands down.
    fn disarm_encode(c: &mut BitrateController, start: Instant, tick: &mut u32) {
        for _ in 0..=ENCODE_NOOP_BACKOFFS_TO_DISARM {
            let verdict = encode_choke(c, start, tick, 20_000);
            c.on_ack(verdict.expect("an unanswered encode rise must back off"));
        }
        assert!(c.encode_disarmed);
    }

    #[test]
    fn a_stood_down_encode_signal_re_arms_after_a_clean_run() {
        // The stand-down is EVIDENCE, not a spec limit, and what it answers — contention on the
        // host's GPU — is exactly the sort of thing that ends mid-session. Left permanent, one
        // contended stretch would strip the knee down-driver from every calm minute that follows,
        // including the ones where a climb can reach a rate the ASIC cannot hold.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode_reprobe_after, CAP_REPROBE_WINDOWS_MIN);

        // A short clean spell is not enough — the re-probe is a run, not a blip.
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(c.encode_disarmed);
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode_disarmed);

        // And it really drives again: a fresh excursion backs the rate off.
        assert!(encode_choke(&mut c, start, &mut tick, 40_000).is_some());
    }

    #[test]
    fn a_standing_contention_backs_the_re_arm_clock_off() {
        // A re-armed signal silenced again means the contention is STANDING, not the transient
        // the re-probe rides out. Same answer both caps give: poll it slowly rather than either
        // giving up forever or thrashing every twelve seconds.
        //
        // Started high enough that two full ratchets stay clear of the floor — a rate pinned at
        // `FLOOR_KBPS` stops backing off at all, which would starve the second stand-down of the
        // backoffs it is counted from.
        let mut c = BitrateController::new(200_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN);
        assert!(!c.encode_disarmed);
        // Re-armed, and the contention is still there.
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode_reprobe_after, CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_bad_window_restarts_the_re_arm_run() {
        // The re-probe wants a genuinely quiet stretch: a window the network spoiled says nothing
        // about whether the encoder would answer the rate now, so the run starts over.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        let at = ticks(start, tick);
        tick += 1;
        // A flush: severe, so it also costs a ×0.7 — and it resets the clean run behind it.
        assert!(c
            .on_window(at, 0, 0, Some(10_000), None, None, 1_000_000, true, 0)
            .is_some());
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(c.encode_disarmed, "the spoiled window must restart the run");
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode_disarmed);
    }

    #[test]
    fn the_encode_thresholds_follow_the_session_frame_budget() {
        // The 1440p60-vs-1440p120 field asymmetry. One frame of encode delay is 8.3 ms at 120 Hz
        // and 16.7 ms at 60 Hz, so against FIXED thresholds the 60 Hz session takes the immediate
        // ×0.7 for the same physical hiccup the 120 Hz one shrugs off. Sized in frame budgets,
        // both treat it the same way: ordinary, and confirmed by a second window.
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
        // Confirmed by a second window, it still backs off — the signal is not weakened, only
        // re-scaled.
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
                0
            ),
            Some(14_000)
        );
    }

    #[test]
    fn unactuatable_encode_rises_disarm_the_down_driver() {
        // The field ratchet (2026-08-22): a game saturating the GPU holds host encode time up,
        // the client reads it as the compute knee, and every ×0.7 changes nothing — 57 Mbps to
        // the floor over ten minutes with zero loss, zero keyframe asks and a flat decoder.
        // `on_ack` re-seeds the encode baseline after each decrease, so nothing in the signal
        // itself ever notices that the backoffs are not working. The firing LEVEL does.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;

        // First one is a legitimate knee sample — nothing has been learned yet.
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(14_000));
        c.on_ack(14_000);
        // Fires again no lower: the first ×0.7 bought nothing. One no-op is not a verdict — a
        // real knee still above the current rate looks exactly like this.
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(9_800));
        c.on_ack(9_800);
        assert_eq!(c.encode_noop_backoffs, 1);
        assert!(!c.encode_disarmed);
        // Twice in a row ⇒ the rate is not the lever. This backoff still lands (the window was
        // judged before the verdict), and it is the last one this signal drives until a clean run
        // re-probes it (`a_stood_down_encode_signal_re_arms_after_a_clean_run`).
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(6_860));
        c.on_ack(6_860);
        assert!(c.encode_disarmed);

        // The ratchet stops: the same excursion no longer moves the rate…
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), None);
        // …and the session climbs back out instead of parking at the floor.
        c.set_ceiling(200_000);
        assert!(
            run_clean(&mut c, start, tick, 8).is_some_and(|k| k > 6_860),
            "a disarmed encode signal must not keep the session pinned"
        );
    }

    #[test]
    fn an_encode_backoff_that_helps_keeps_the_down_driver_armed() {
        // The knee this signal was built for: the ×0.7 lands nearer it and encode time genuinely
        // comes down, so the next excursion is a fresh event rather than evidence that the rate
        // is the wrong lever. Nothing here may disarm.
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
        // Loss, a flush or a dropped frame explain a backoff without the encoder — and cutting
        // the rate genuinely IS the remedy for those. Such a window must not count toward the
        // disarm, even when encode time happens to be elevated in it too.
        let mut c = BitrateController::new(20_000);
        let start = Instant::now();
        let mut tick = 0;
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(14_000));
        c.on_ack(14_000);
        assert_eq!(c.encode_backoff_us, 20_000);
        // Re-seed so the encode signal is live again…
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
                    0
                ),
                None
            );
        }
        // …then a window carrying BOTH an encode excursion and a jump-to-live flush. The flush is
        // the explanation, so the encode streak resets rather than advancing toward a disarm.
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
                0
            ),
            Some(9_800)
        );
        assert_eq!(c.encode_backoff_us, 0);
        assert_eq!(c.encode_noop_backoffs, 0);
        assert!(!c.encode_disarmed);
    }

    #[test]
    fn env_max_mbps_caps_every_learned_ceiling() {
        // PUNKTFUNK_ABR_MAX_MBPS=50 (injected — `new` reads the env exactly once, at
        // construction): a probe "measuring" 886 Mbps (the divisor bug's field figure) must
        // not out-rank the user's cap…
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_ceiling(886_312);
        assert_eq!(c.ceiling_kbps, 50_000);
        // …while a measurement under the cap stands untouched.
        let mut c = BitrateController::with_ceiling_cap(20_000, Some(50_000));
        c.set_ceiling(40_000);
        assert_eq!(c.ceiling_kbps, 40_000);
        // And the climb honors it: slow start doubles 20→40, the capped ceiling truncates the
        // next step to 50, then quiet — never a request past the user's limit.
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
        // PUNKTFUNK_ABR_MAX_MBPS is the only lever an Automatic session gives the operator, and
        // it used to bind only ceilings the PROBE taught — so a session that negotiated a rate
        // above the cap simply ran above it forever. No congestion signal will ever find that:
        // the link is fine, the cap is policy.
        let mut c = BitrateController::with_ceiling_cap(100_000, Some(50_000));
        assert_eq!(c.ceiling_kbps, 50_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
        // Suppose the host answers HIGHER than asked (its own floor, an encoder minimum): that
        // is the host saying it cannot go there. Don't re-ask every cooldown forever.
        c.on_ack(80_000);
        assert_eq!(run_clean(&mut c, start, 2, 20), None);
        // A ceiling that MOVES is a new question, and gets asked once more.
        c.set_ceiling(90_000); // clamped to the 50 Mbps cap → still 50 000, no new ask
        assert_eq!(run_clean(&mut c, start, 24, 20), None);
    }

    fn calm_window(c: &mut BitrateController, at: Instant) {
        // One calm, unutilized window (2 Mb/s actual): seeds the latency baselines without
        // authorizing climbs, and must decide nothing.
        assert_eq!(
            c.on_window(at, 0, 0, Some(10_000), Some(8_000), None, 2_000, false, 0),
            None
        );
    }

    /// Drive clean, fully-utilized windows (1 Gb/s actual), acking every climb the controller
    /// asks for — a live host answers in ~100 ms — until `current_kbps` reaches `target`.
    /// Bounded so a climb-path regression fails loudly instead of spinning.
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

    /// One decode-SEVERE window (60 ms against the ~8 ms baseline) at the current rate — a
    /// knee choke. Steps past the change cooldown first so the decision can fire.
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
        );
        *tick += 1;
        r
    }

    /// The latch's only production-reachable shape: choke at the knee, the host ACKS the ×0.7
    /// (a live host answers in ~100 ms, so a cascade's second backoff always sits at the
    /// already-reduced rate — dissimilar by construction), the controller climbs back, and the
    /// re-climb chokes inside the ±1/8 band. Latches, acks the backoff, returns the cap.
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

    /// One capture-stall-shaped window at the current rate: almost nothing delivered
    /// (current/10), nothing decoded, no loss — but a jump-to-live flush and a keyframe-ask
    /// storm (the stall edge's damage signature). SEVERE, so it backs off; STARVED, so it must
    /// never be a knee sample.
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
        );
        *tick += 1;
        r
    }

    #[test]
    fn capture_stall_windows_never_latch_a_decode_cap() {
        // The periodic-capture-stall field case (RDNA4 standby-sink, 5 s cycle): every stall
        // edge offers another flush + kf-storm "backoff" at the SAME rate — without the starved
        // guard that pair latches a phantom decoder knee at whatever rate the display driver
        // happened to interrupt, and the session then fights the re-probe ladder for minutes.
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
        // A REAL knee sample, then a stall edge, then the genuine re-climb choke: the starved
        // window in the middle must neither latch nor ERASE the reference the real choke set —
        // the genuine pair must still find each other around it.
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

    /// The host-rebuild field window, verbatim from the 0.29 log: an exclusive-topology eviction
    /// rebuilt the capture ring and the encoder in place (401 ms, entirely host-local), and the
    /// client's report window straddled it — 390 kbps delivered against a 20 000 target, zero
    /// loss, no flush, and an encode mean of 15 063 µs against a ~2 800 baseline.
    ///
    /// That used to clear the severe encode tier and take the one-window path, costing a ×0.7 and
    /// slow start for the rest of the session on a link that never dropped a packet. The encode
    /// mean over a window in which almost nothing flowed is not a measurement of encode cost, so
    /// the signal is withheld and the window decides nothing.
    #[test]
    fn a_starved_window_cannot_back_off_on_host_encode_time_alone() {
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_000);
        let start = Instant::now();
        let mut t = 0;
        // Seed the latency baselines. Half-utilized on purpose: above the starved bar (a quarter
        // of target) so the encode samples count, below the climb bar (three quarters) so no step
        // fires and `current_kbps` stays put.
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
                    0
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

        // The signal itself must still work: the starved sample was withheld rather than folded
        // into the rolling minimum, so the SAME encode excursion in a window that actually
        // carried its rate is still severe, and still backs off on one window.
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
        );
        assert!(
            verdict.is_some_and(|k| k < 20_000),
            "a real encode excursion at full delivery still backs off, got {verdict:?}"
        );
    }

    #[test]
    fn decode_cap_latches_when_the_reclimb_chokes_at_the_same_knee() {
        // The 1440p120 field sawtooth: a decoder knee (~500 Mbps) well under the (inflated)
        // link ceiling — nothing ever LEARNED the knee, so every re-climb ended in a flush +
        // dropped-frame burst. Choke, recover, climb back, choke again inside the band: latch.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        latch_knee(&mut c, start, &mut t);
        // The latch applies; from here every climb must stop AT the knee — not the 900 Mbps
        // link ceiling the old sawtooth kept re-poking.
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
            ) {
                // Never past the cap in force when the decision was made. (A long clean run
                // legitimately re-probes that cap upward — `decode_cap_reprobes_after_a_
                // sustained_clean_run` owns that; here the point is that nothing climbs toward
                // the 900 Mbps LINK ceiling the old sawtooth kept re-poking.)
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
        // The latch's false-positive guards, every event at a rate the controller climbed to
        // or held (drain-time backoffs are no sample at all —
        // `cascade_backoffs_neither_sample_nor_erase_the_knee_reference` owns those). A lone
        // jump-to-live flush (a Wi-Fi clump can flush once at ANY rate) backs off but teaches
        // nothing…
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let r1 = c
            .on_window(ticks(start, t), 0, 0, None, None, None, 490_000, true, 0)
            .expect("flush must back off");
        assert_eq!(r1, 350_000);
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r1);
        // …a LOSS-driven backoff at the re-climbed rate breaks the streak (whatever choked
        // there, it wasn't the decoder — even inside the similarity band)…
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
            )
            .expect("loss must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(
            c.decode_backoff_kbps, 0,
            "a climbed-to non-decode backoff must reset the knee reference"
        );
        c.on_ack(r2);
        // …so the next flush counts as a FIRST decode event again — still no latch…
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
            )
            .expect("flush must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        c.on_ack(r3);
        // …and two decode events at DISSIMILAR climbed-to rates (~460 vs ~350 Mbps — no
        // common knee) must not latch either.
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
            )
            .expect("flush must back off");
        assert!(c.decode_cap_kbps.is_none());
    }

    #[test]
    fn decode_cap_reprobes_after_a_sustained_clean_run() {
        // The knee is content/thermals evidence, not a spec limit: after ~60 s parked clean at
        // the latched cap, it lifts one step (+12.5 %, ceiling-bounded) — the re-probe path is
        // how the latch clears (never permanent), and a still-standing knee just re-latches
        // from the next pair of decode-driven backoffs.
        let mut c = BitrateController::new(500_000);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let knee = latch_knee(&mut c, start, &mut t);
        // The host parks the session at the knee (an unsolicited re-target up to it — its
        // clamp is authoritative).
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
            );
            t += 1;
        }
        assert_eq!(c.decode_cap_kbps, Some(knee + knee / 8));
    }

    #[test]
    fn mode_switch_clears_the_decode_cap() {
        // A 1440p120 knee means nothing at the new mode's pixel rate — the decode cap must
        // not survive the switch (the probe-measured link ceiling does).
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
        // The 2026-08-03 780M field trace, numbers from the log. The knee's most common
        // presentation is a standing ~26 ms decode rise — deep enough for the ordinary
        // two-window backoff, below the 45 ms severe tier. Judging evidence from the deciding
        // window alone read those backoffs as decode-free and RESET the knee streak each
        // time; the session sawtoothed 220↔450 Mbps for its remaining minutes.
        let mut c = BitrateController::new(20_000);
        c.set_ceiling(657_788); // the log's probe ceiling
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        // A single heavy-loss window ends slow start (as the field session's startup hitch
        // did) so the climb below is the additive one the trace shows.
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
        );
        t += 1;
        // Choke #1 (00:35:56Z): flush + 40 ms decode at ~417 Mbps — evidence, first sample.
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
            )
            .expect("flush choke must back off");
        t += 1;
        assert!(c.decode_cap_kbps.is_none());
        assert_eq!(c.decode_backoff_kbps, first);
        c.on_ack(r1);
        // Choke #2 (00:36:32Z): TWO consecutive ~26 ms decode-bad windows at ~446 Mbps — the
        // ordinary two-window path, no flush, nothing severe. This is the backoff the old
        // evidence gate threw away.
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
                0
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
                0
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
        // Choke at the knee (reference set), the host acks the ×0.7 within ~100 ms, and the
        // drain flushes → a second backoff fires at the REDUCED rate. That rate is one the
        // decoder never choked at while keeping up — the old code overwrote the reference
        // with it (and could never latch from a cascade at all: ×0.7 sits outside the ±1/8
        // band by construction). A drain backoff must neither latch nor erase; the eventual
        // re-climb's choke latches against the ORIGINAL sample.
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
        // The Steam Deck presentation of the knee: an overdriven decoder that WEDGES instead
        // of queueing — decode latency reads absent-to-flat while the client begs for
        // keyframes with zero loss (the field traces: 14–19 asks at ~300 Mbps, loss_ppm=0).
        // The asks are the decode evidence.
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
            )
            .expect("second storm must back off");
        assert_eq!(c.decode_cap_kbps, Some(rate - rate / 16));
    }

    #[test]
    fn keyframe_storms_with_real_loss_teach_no_knee() {
        // The same storm WITH heavy loss is network-attributed (a lost reference forces
        // recovery asks; loss_ppm already prices that path): it must not latch, and it must
        // break the streak like any other non-decode backoff.
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
        // Two bad windows, only ONE decode-flagged (OWD carried the other): the backoff is
        // not decode-attributed — the reference must reset, not sample.
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
                0
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
                0
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
