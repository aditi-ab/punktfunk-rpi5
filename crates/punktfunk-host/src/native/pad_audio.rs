//! Per-pad DualSense audio (the 0xD1 pad-audio plane): capture of the pad's own audio device —
//! Windows: WASAPI loopback of a pre-provisioned endpoint ([`crate::audio::pad_endpoint`]);
//! Linux: the per-pad PipeWire sink we mint (`crate::audio::pad_sink`) — → 4-ch de-interleave
//! into the speaker (front) and voice-coil haptics (back) pairs → per-kind silence gate →
//! stereo Opus (48 kHz, CBR, LowDelay)
//! → [`PAD_AUDIO_MAGIC`](punktfunk_core::quic::PAD_AUDIO_MAGIC) datagrams. One thread per
//! arriving pad, spawned/reaped by the input thread ([`super::input`]) as arrivals declare
//! renderers and pads leave. Modeled on the session audio thread ([`super::audio`]): the same
//! reopen-with-backoff on capture death, the same monotonic-seq-kept-across-reopens discipline,
//! the same power-of-two encode-warn throttle.

use super::*;

/// `kinds` bit for the haptics stream (bit N = wire kind N — the same packing the arrival's
/// audio-caps bits use, see [`punktfunk_core::input::decode_gamepad_arrival`]).
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(super) const KIND_BIT_HAPTICS: u8 = 1 << punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS;
/// `kinds` bit for the speaker stream.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(super) const KIND_BIT_SPEAKER: u8 = 1 << punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER;

/// Haptics frames are 5 ms (the session-audio cadence — haptics are felt latency); speaker
/// frames are 10 ms (speaker content tolerates the buffering for the coding efficiency). Both
/// are the wire contract's cadences (`punktfunk_core::quic::PAD_AUDIO_KIND_*`).
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const HAPTICS_FRAME_MS: u32 = 5;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const SPEAKER_FRAME_MS: u32 = 10;
/// Samples per frame (per channel) at 48 kHz: 240 / 480.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const HAPTICS_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * HAPTICS_FRAME_MS as usize / 1000;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const SPEAKER_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * SPEAKER_FRAME_MS as usize / 1000;
/// The capture's channel count — the pad endpoint is stamped quad (FL FR BL BR: front pair =
/// speaker, back pair = voice coils). Mirrors `pad_endpoint::PAD_CHANNELS` (Windows-gated, so
/// the pure splitter logic keeps its own copy).
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const CAP_CHANNELS: usize = 4;

/// Peak (absolute sample) at or above which a frame counts as signal — the gate OPENS on that
/// very frame (haptics are felt latency; the first active frame must ship). ≈ −60 dBFS.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const GATE_OPEN_PEAK: f32 = 1e-3;
/// How long the gate keeps sending after the last signal frame before it CLOSES (hangover):
/// long enough that a decaying haptic tail (and the client decoder's own tail) is never
/// clipped, short enough that an idle pad costs nothing in steady state.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const GATE_HANGOVER_MS: u32 = 250;

/// Per-kind Opus bitrate — a stereo voice-coil / pad-speaker pair needs far less than the
/// session plane's 128 kbps; 64 kbps CBR keeps every frame comfortably under one MTU.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const PAD_AUDIO_BITRATE: i32 = 64_000;

/// The per-kind silence gate — the steady-state-cost feature: an idle pad endpoint (games
/// rarely render pad audio) must cost ZERO encodes and ZERO datagrams, not a permanent 200 Hz
/// stream of coded silence. Opens the instant a frame carries signal ([`GATE_OPEN_PEAK`]);
/// closes only after [`GATE_HANGOVER_MS`] of continuous sub-threshold frames. Pure logic,
/// unit-tested below.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
struct SilenceGate {
    /// Consecutive sub-threshold frames that close the gate ([`GATE_HANGOVER_MS`] ÷ frame ms).
    hangover_frames: u32,
    /// Consecutive sub-threshold frames seen so far while open.
    quiet: u32,
    /// Starts closed: a pad no game ever renders into never opens (and never sends).
    open: bool,
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
impl SilenceGate {
    fn new(frame_ms: u32) -> SilenceGate {
        SilenceGate {
            hangover_frames: (GATE_HANGOVER_MS / frame_ms).max(1),
            quiet: 0,
            open: false,
        }
    }

    /// Feed one frame; `true` = encode + send it. Signal opens the gate on THIS frame; the
    /// frame that completes the hangover closes it and is itself suppressed (the client
    /// already has ~250 ms of ramped-out silence by then).
    fn feed(&mut self, frame: &[f32]) -> bool {
        if frame.iter().any(|s| s.abs() >= GATE_OPEN_PEAK) {
            self.open = true;
            self.quiet = 0;
        } else if self.open {
            self.quiet += 1;
            if self.quiet >= self.hangover_frames {
                self.open = false;
                self.quiet = 0;
            }
        }
        self.open
    }
}

/// One kind's send-admission + seq bookkeeping (pure logic — the capture thread wraps it with
/// the encoder and the datagram send). `seq` is monotonic per (pad, kind) and NEVER advances
/// while the gate is closed: frozen-seq = deliberate silence — the client tells silence from
/// loss by seq continuity (the mic-mute discipline, pf-client-core/src/audio.rs). It is also
/// kept across capture reopens (the session audio thread's discipline, audio.rs): the client
/// sees a gap, not a restart.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
struct LaneCtl {
    gate: SilenceGate,
    seq: u32,
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
impl LaneCtl {
    fn new(frame_ms: u32) -> LaneCtl {
        LaneCtl {
            gate: SilenceGate::new(frame_ms),
            seq: 0,
        }
    }

    /// Admit one frame: `Some(seq)` = encode + send it with this seq (advanced for the next);
    /// `None` = gated — do not send, do not advance. An encode failure AFTER admission leaves a
    /// one-frame seq gap, which the client conceals exactly like datagram loss.
    fn admit(&mut self, frame: &[f32]) -> Option<u32> {
        if !self.gate.feed(frame) {
            return None;
        }
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(seq)
    }
}

/// De-interleave one 4-ch block (FL FR BL BR) into its stereo pairs: `(front, back)` — front =
/// speaker (channels 0/1), back = voice-coil haptics (channels 2/3). A ragged tail (not a
/// multiple of 4 — the capturer only ever delivers whole frames) is dropped, never smeared
/// across channels.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn split_quad(block: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut front = Vec::with_capacity(block.len() / 2);
    let mut back = Vec::with_capacity(block.len() / 2);
    for s in block.chunks_exact(CAP_CHANNELS) {
        front.extend_from_slice(&s[..2]);
        back.extend_from_slice(&s[2..4]);
    }
    (front, back)
}

/// Accumulates interleaved 4-ch capture and cuts it into the wire contract's per-kind stereo
/// frames — haptics every 5 ms from the back pair, speaker every 10 ms from the front pair —
/// emitting ONLY the kinds enabled in `kinds` (a disabled kind is never even split out, so it
/// can never reach an encoder). Pure logic, unit-tested; the capture thread wraps it.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
struct PadFramer {
    kinds: u8,
    /// Raw interleaved 4-ch accumulation, drained in 5 ms blocks.
    acc: Vec<f32>,
    /// Front-pair stereo accumulation toward the next 10 ms speaker frame.
    front: Vec<f32>,
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
impl PadFramer {
    fn new(kinds: u8) -> PadFramer {
        PadFramer {
            kinds,
            acc: Vec::with_capacity(HAPTICS_FRAME_SAMPLES * CAP_CHANNELS * 4),
            front: Vec::new(),
        }
    }

    /// Feed one capture chunk; `emit(kind, stereo_frame)` fires for each completed frame
    /// (haptics first — it is the latency-critical pair).
    fn feed(&mut self, chunk: &[f32], mut emit: impl FnMut(u8, &[f32])) {
        self.acc.extend_from_slice(chunk);
        let block_len = HAPTICS_FRAME_SAMPLES * CAP_CHANNELS;
        while self.acc.len() >= block_len {
            let block: Vec<f32> = self.acc.drain(..block_len).collect();
            let (front, back) = split_quad(&block);
            if self.kinds & KIND_BIT_HAPTICS != 0 {
                emit(punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS, &back);
            }
            if self.kinds & KIND_BIT_SPEAKER != 0 {
                self.front.extend_from_slice(&front);
                let frame_len = SPEAKER_FRAME_SAMPLES * 2;
                while self.front.len() >= frame_len {
                    let frame: Vec<f32> = self.front.drain(..frame_len).collect();
                    emit(punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER, &frame);
                }
            }
        }
    }

    /// Drop the partial frames straddling a capture gap (reopen). The seq/gate state is NOT
    /// here — [`LaneCtl`] deliberately survives reopens, so the client sees a gap, not a
    /// restart.
    fn clear(&mut self) {
        self.acc.clear();
        self.front.clear();
    }
}

/// A running per-pad streamer. [`stop`](PadAudioHandle::stop) (or drop) flags the thread and
/// joins it; [`signal`](PadAudioHandle::signal) only flags — the input thread's teardown flags
/// every pad first so the joins overlap instead of serializing the capturer's worst-case ~5 s
/// quiet-endpoint recv timeout.
pub(super) struct PadAudioHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PadAudioHandle {
    /// Flag the streamer to wind down without waiting for it.
    pub(super) fn signal(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop + reap. Bounded by the capturer's ~5 s quiet-endpoint recv timeout in the worst
    /// case — the mid-session reap paths run this on a detached reaper thread for that reason
    /// (`input.rs::PadAudioSlots::stop`); session teardown affords it inline (the 10 s
    /// side-thread join grace covers it).
    pub(super) fn stop(mut self) {
        self.reap();
    }

    fn reap(&mut self) {
        self.signal();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A handle dropped without `stop()` (reaper-spawn failure) still winds its thread down.
impl Drop for PadAudioHandle {
    fn drop(&mut self) {
        self.reap();
    }
}

/// Whether this session's Welcome should advertise
/// [`HOST_CAP_PAD_AUDIO`](punktfunk_core::quic::HOST_CAP_PAD_AUDIO): the client asked
/// ([`CLIENT_CAP_PAD_AUDIO`](punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO)), the feature is on
/// (`PUNKTFUNK_PAD_AUDIO` != "0"), and the pad audio source exists — Windows: startup
/// provisioning published at least one endpoint (`pad_endpoint::provision_at_startup`; a
/// still-running provisioning reads as "none yet" and the next connect picks it up); Linux: a
/// PipeWire daemon is reachable (the per-pad sinks are minted lazily at spawn, so reachability
/// IS the existence question).
pub(super) fn host_cap(client_caps: u8) -> bool {
    let asked = client_caps & punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO != 0;
    #[cfg(target_os = "windows")]
    {
        // R5: a startup attempt that failed transiently leaves nothing latched, so retry here —
        // this is the first moment in a session's life that anyone asks whether pad audio exists.
        if asked {
            crate::audio::pad_endpoint::ensure_provisioned();
        }
        asked
            && std::env::var_os("PUNKTFUNK_PAD_AUDIO").is_none_or(|v| v != "0")
            && crate::audio::pad_endpoint::provisioned_endpoints()
                .is_some_and(|eps| !eps.is_empty())
    }
    #[cfg(target_os = "linux")]
    {
        asked
            && std::env::var_os("PUNKTFUNK_PAD_AUDIO").is_none_or(|v| v != "0")
            && crate::audio::pad_sink::pipewire_reachable()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        // No pad audio source on this host OS.
        let _ = asked;
        false
    }
}

/// Start the per-pad streamer toward `conn` for `pad`, streaming the kinds in `kinds` (bit 0 =
/// haptics, bit 1 = speaker — the arrival's audio-caps packing). `stop` is this handle's own
/// flag (fresh per spawn — pad streamers stop individually, not with the session). `None` when
/// the slot has no provisioned endpoint (provisioning failed or still running, or the slot is
/// past `PUNKTFUNK_PAD_AUDIO_SLOTS` — only 0..4 can ever have one) or the thread cannot spawn;
/// the pad itself keeps working either way, just without audio.
#[cfg(target_os = "windows")]
pub(super) fn spawn(
    conn: quinn::Connection,
    pad: u8,
    kinds: u8,
    _edge: bool,
    stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    if kinds & (KIND_BIT_HAPTICS | KIND_BIT_SPEAKER) == 0 {
        return None;
    }
    let Some(ep) = crate::audio::pad_endpoint::endpoint_for(pad) else {
        tracing::debug!(
            pad,
            "pad-audio arrival for a slot without a provisioned endpoint — not streaming"
        );
        return None;
    };
    if ep.endpoint_id.is_empty() {
        // The devnode-without-endpoint shape (`find`) — never in the provisioned set, but
        // cheap to refuse rather than spin the open/backoff loop on an empty id.
        return None;
    }
    if ep.needs_aeb_kick {
        // R4: this flag was computed on every path and consulted nowhere past startup. It means
        // the endpoint's stamps are STORED but not SERVED — the audio stack never picked up the
        // DualSense identity — and startup's one restart did not fix it. Opening anyway is worse
        // than refusing: `AUTOCONVERTPCM` makes a wrong-format endpoint initialize *successfully*,
        // so the stream runs, the logs look healthy, and the haptics/speaker pair is mis-routed
        // with nothing to point at. Decline, and say which reboot-shaped problem it is.
        tracing::warn!(
            pad,
            endpoint = %ep.endpoint_id,
            "pad endpoint stamps are stored but not served — the audio stack has not adopted the \
             DualSense identity (a reboot, or a manual AudioEndpointBuilder+Audiosrv restart, \
             clears it). Not streaming: the endpoint would open and mis-route."
        );
        return None;
    }
    let stop_t = stop.clone();
    let endpoint_id = ep.endpoint_id;
    let vis_id = endpoint_id.clone();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            // COM for the visibility flips (the capturer's opens run on their own thread).
            let _ = wasapi::initialize_mta();
            // The endpoint parks HIDDEN while no pad is attached — an idle visible "Wireless
            // Controller" speaker makes libScePad titles engage their DualSense-haptics path
            // against an endpoint nothing services (the 2026-08-14 Helldivers 2 field tank).
            // Show it for exactly this pad's lifetime, like a real DualSense arriving; the
            // capturer's open/backoff loop absorbs the moment audiosrv takes to re-activate.
            crate::audio::pad_endpoint::set_visibility(&vis_id, pad, true);
            pad_audio_thread(
                conn,
                pad,
                kinds,
                move || crate::audio::pad_endpoint::PadLoopbackCapturer::open(&endpoint_id),
                stop_t,
            );
            crate::audio::pad_endpoint::set_visibility(&vis_id, pad, false);
        }) {
        Ok(join) => Some(PadAudioHandle {
            stop,
            join: Some(join),
        }),
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio thread spawn failed — pad streams without audio");
            None
        }
    }
}

/// Linux: mint the pad's PipeWire sink lazily inside the streamer thread (the same
/// open-with-backoff loop the Windows capture rides — a PipeWire hiccup at arrival time starts
/// pad audio late, not never). `edge` picks the DualSense Edge identity for the sink. `None`
/// only for empty kinds, a slot past `PUNKTFUNK_PAD_AUDIO_SLOTS`, or a failed thread spawn;
/// the pad itself keeps working either way, just without audio.
#[cfg(target_os = "linux")]
pub(super) fn spawn(
    conn: quinn::Connection,
    pad: u8,
    kinds: u8,
    edge: bool,
    stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    if kinds & (KIND_BIT_HAPTICS | KIND_BIT_SPEAKER) == 0 {
        return None;
    }
    if pad >= crate::audio::pad_sink::pad_audio_slots() {
        tracing::debug!(
            pad,
            "pad-audio arrival past PUNKTFUNK_PAD_AUDIO_SLOTS — not streaming"
        );
        return None;
    }
    let stop_t = stop.clone();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            pad_audio_thread(
                conn,
                pad,
                kinds,
                move || crate::audio::pad_sink::PadSinkCapturer::open(pad, edge),
                stop_t,
            )
        }) {
        Ok(join) => Some(PadAudioHandle {
            stop,
            join: Some(join),
        }),
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio thread spawn failed — pad streams without audio");
            None
        }
    }
}

/// Stub — pad audio sources exist only behind the Windows and Linux virtual DualSense; other
/// hosts run pads without the audio side (and never advertise the cap, see [`host_cap`]).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(super) fn spawn(
    _conn: quinn::Connection,
    _pad: u8,
    _kinds: u8,
    _edge: bool,
    _stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    None
}

/// One enabled kind's encoder lane: admission/seq control + its stereo Opus encoder + the
/// power-of-two warn throttle (a stuck encoder would otherwise fail ~200 times a second).
#[cfg(any(target_os = "windows", target_os = "linux"))]
struct Lane {
    kind: u8,
    ctl: LaneCtl,
    enc: opus::Encoder,
    encode_errs: u64,
}

/// Build one stereo encoder per enabled kind: 48 kHz LowDelay hard-CBR like the session audio
/// plane ([`super::audio`]), at the pad plane's 64 kbps.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn build_lanes(kinds: u8) -> Result<Vec<Lane>, opus::Error> {
    let mut lanes = Vec::new();
    for (bit, kind, frame_ms) in [
        (
            KIND_BIT_HAPTICS,
            punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS,
            HAPTICS_FRAME_MS,
        ),
        (
            KIND_BIT_SPEAKER,
            punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER,
            SPEAKER_FRAME_MS,
        ),
    ] {
        if kinds & bit == 0 {
            continue;
        }
        let mut enc = opus::Encoder::new(
            crate::audio::SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )?;
        enc.set_bitrate(opus::Bitrate::Bits(PAD_AUDIO_BITRATE)).ok();
        enc.set_vbr(false).ok();
        lanes.push(Lane {
            kind,
            ctl: LaneCtl::new(frame_ms),
            enc,
            encode_errs: 0,
        });
    }
    Ok(lanes)
}

/// The per-pad streaming thread: capture of the pad's audio device (`open` builds the
/// platform's capturer — Windows loopback / Linux minted sink) → framer → per-kind gate/encode
/// → 0xD1 datagrams. Capture death reopens with the session-audio backoff
/// ([`INJECTOR_REOPEN_BACKOFF`], encoders + seq kept); a send error ends the thread (the
/// connection — the session — is gone).
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn pad_audio_thread<C: crate::audio::AudioCapturer>(
    conn: quinn::Connection,
    pad: u8,
    kinds: u8,
    open: impl Fn() -> anyhow::Result<C>,
    stop: Arc<AtomicBool>,
) {
    // Above-normal like the session send thread — this plane is silence-gated and tiny, but when
    // a pad speaker/haptics stream IS live it runs the same ≤10 ms cadence as session audio.
    crate::native::boost_thread_priority(false);
    let mut lanes = match build_lanes(kinds) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio opus encoder init failed — pad continues without audio");
            return;
        }
    };
    if lanes.is_empty() {
        return; // spawn() refuses kinds == 0 — belt and braces
    }
    let mut framer = PadFramer::new(kinds);
    // One Opus frame per datagram; 64 kbps CBR at ≤10 ms is ~80 bytes — sized with the session
    // plane's slack.
    let mut opus_buf = vec![0u8; 1500];
    // Reopen-with-backoff (the audio.rs discipline): a capture death (endpoint invalidated,
    // audio-engine restart) reopens instead of muting the pad for the rest of the session. The
    // first open ALSO rides this loop, so an open lost to endpoint churn starts late, not never.
    let mut capturer: Option<C> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    tracing::info!(
        pad,
        haptics = kinds & KIND_BIT_HAPTICS != 0,
        speaker = kinds & KIND_BIT_SPEAKER != 0,
        "pad audio streaming (0xD1, Opus 48 kHz, silence-gated)"
    );
    'session: while !stop.load(Ordering::SeqCst) {
        if capturer.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < INJECTOR_REOPEN_BACKOFF) {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            match open() {
                Ok(c) => {
                    if last_failed.take().is_some() {
                        tracing::info!(pad, "pad-audio capture reopened");
                    }
                    capturer = Some(c);
                    framer.clear(); // drop the partial frames straddling the gap
                }
                Err(e) => {
                    tracing::debug!(pad, error = %format!("{e:#}"), "pad-audio open failed — will retry");
                    last_failed = Some(std::time::Instant::now());
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
        }
        // An empty chunk is a QUIET endpoint (the capturer's idle timeout), not a death — keep
        // it; only a genuine Err (capture thread ended) drops the capturer for reopen.
        let chunk = match capturer.as_mut().unwrap().next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(pad, error = %format!("{e:#}"), "pad-audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        let mut session_gone = false;
        framer.feed(&chunk, |kind, frame| {
            if session_gone {
                return;
            }
            let Some(lane) = lanes.iter_mut().find(|l| l.kind == kind) else {
                return; // framer emits only enabled kinds — unreachable, but never panic here
            };
            // Gated = deliberate silence: no datagram AND a frozen seq (the client tells
            // silence from loss by seq continuity).
            let Some(seq) = lane.ctl.admit(frame) else {
                return;
            };
            let pts_ns = now_ns();
            match lane.enc.encode_float(frame, &mut opus_buf) {
                Ok(n) => {
                    let d = punktfunk_core::quic::encode_pad_audio_datagram(
                        pad,
                        kind,
                        seq,
                        pts_ns,
                        &opus_buf[..n],
                    );
                    if conn.send_datagram(d.into()).is_err() {
                        session_gone = true; // connection gone — the session is over
                    }
                }
                Err(e) => {
                    lane.encode_errs += 1;
                    if lane.encode_errs.is_power_of_two() {
                        tracing::warn!(
                            pad,
                            kind,
                            error = %e,
                            count = lane.encode_errs,
                            "pad-audio opus encode failed — dropping frame"
                        );
                    }
                }
            }
        });
        if session_gone {
            break 'session;
        }
    }
    // Dropping the capturer stops its WASAPI thread. Nothing to park: pad capture is per-pad,
    // per-session by design (unlike the session audio slot there is no cross-session reuse).
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};

    /// A stereo frame of `n` samples at a constant level.
    fn frame(level: f32, n: usize) -> Vec<f32> {
        vec![level; n * 2]
    }

    #[test]
    fn gate_opens_immediately_and_closes_after_hangover() {
        let mut g = SilenceGate::new(HAPTICS_FRAME_MS);
        // 250 ms of 5 ms frames.
        assert_eq!(g.hangover_frames, 50);
        // Closed from birth: an idle pad never sends.
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // A peak at exactly the threshold opens on THIS frame (haptics are felt latency).
        assert!(g.feed(&frame(GATE_OPEN_PEAK, HAPTICS_FRAME_SAMPLES)));
        // 49 quiet frames ride the hangover; the 50th completes 250 ms and is suppressed.
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // ... and stays closed.
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // Sub-threshold wiggle does not reopen; real signal does (negative peaks count).
        assert!(!g.feed(&frame(9e-4, HAPTICS_FRAME_SAMPLES)));
        assert!(g.feed(&frame(-0.5, HAPTICS_FRAME_SAMPLES)));
        // A loud frame mid-hangover rearms the full 250 ms.
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(g.feed(&frame(0.02, HAPTICS_FRAME_SAMPLES)));
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
    }

    #[test]
    fn gate_hangover_scales_with_frame_ms() {
        let mut g = SilenceGate::new(SPEAKER_FRAME_MS);
        assert_eq!(g.hangover_frames, 25); // 250 ms of 10 ms frames
        assert!(g.feed(&frame(0.1, SPEAKER_FRAME_SAMPLES)));
        for _ in 0..24 {
            assert!(g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
    }

    #[test]
    fn seq_freezes_while_gated_and_survives_reopen() {
        let mut lane = LaneCtl::new(HAPTICS_FRAME_MS);
        // Two audible frames: seq 0, 1.
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(0));
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(1));
        // The hangover is still sent (seq advances), then the gate closes and seq FREEZES —
        // deliberate silence the client tells from loss by continuity.
        for i in 0..49u32 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), Some(2 + i));
        }
        for _ in 0..500 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), None);
        }
        // A capture reopen resets ONLY the framer (PadFramer::clear) — LaneCtl is deliberately
        // untouched, so the next audible frame CONTINUES the sequence (gap, not restart).
        assert_eq!(lane.admit(&frame(0.9, HAPTICS_FRAME_SAMPLES)), Some(51));
    }

    #[test]
    fn splitter_exact_pairs() {
        // Interleave [FL FR BL BR] × 2 frames with distinct values everywhere.
        let quad = [0.0, 1.0, 2.0, 3.0, 10.0, 11.0, 12.0, 13.0];
        let (front, back) = split_quad(&quad);
        assert_eq!(front, [0.0, 1.0, 10.0, 11.0]);
        assert_eq!(back, [2.0, 3.0, 12.0, 13.0]);
        // A ragged tail (never produced by the capturer) is dropped, not smeared.
        let (front, back) = split_quad(&quad[..7]);
        assert_eq!((front.len(), back.len()), (2, 2));
    }

    #[test]
    fn framer_cuts_the_wire_cadence() {
        let mut f = PadFramer::new(KIND_BIT_HAPTICS | KIND_BIT_SPEAKER);
        let mut got: Vec<(u8, usize, f32)> = Vec::new();
        // 10 ms of capture (480 samples), fed in ragged chunks: exactly two 5 ms haptics
        // frames from the back pair, then one 10 ms speaker frame from the front pair.
        let mut quad = Vec::new();
        for _ in 0..2 * HAPTICS_FRAME_SAMPLES {
            quad.extend_from_slice(&[0.25, 0.25, -0.5, -0.5]);
        }
        for chunk in quad.chunks(101) {
            f.feed(chunk, |kind, frame| got.push((kind, frame.len(), frame[0])));
        }
        assert_eq!(
            got,
            vec![
                (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES, -0.5),
                (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES, -0.5),
                (PAD_AUDIO_KIND_SPEAKER, 2 * SPEAKER_FRAME_SAMPLES, 0.25),
            ]
        );
    }

    #[test]
    fn framer_masks_disabled_kinds() {
        // 20 ms of all-ones capture: 4 potential haptics frames, 2 potential speaker frames.
        let quad = vec![1.0f32; 4 * HAPTICS_FRAME_SAMPLES * CAP_CHANNELS];
        let mut kinds_seen = Vec::new();
        // Haptics-only: the front pair is never split out, let alone encoded.
        let mut f = PadFramer::new(KIND_BIT_HAPTICS);
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_HAPTICS; 4]);
        // Speaker-only: no haptics frames.
        let mut f = PadFramer::new(KIND_BIT_SPEAKER);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_SPEAKER; 2]);
        // kinds = 0 is never spawned, but the framer must still be total: nothing comes out.
        let mut f = PadFramer::new(0);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert!(kinds_seen.is_empty());
    }

    #[test]
    fn framer_clear_drops_partials_only() {
        let mut f = PadFramer::new(KIND_BIT_HAPTICS | KIND_BIT_SPEAKER);
        let mut emitted = 0;
        // 100 samples: no frame boundary reached yet.
        f.feed(&vec![0.1; 100 * CAP_CHANNELS], |_, _| emitted += 1);
        assert_eq!(emitted, 0);
        f.clear();
        // After the gap: exactly one haptics frame from 240 fresh samples — the 100 stale
        // samples are gone (they would skew every later frame boundary).
        f.feed(
            &vec![0.2; HAPTICS_FRAME_SAMPLES * CAP_CHANNELS],
            |kind, frame| {
                emitted += 1;
                assert_eq!(
                    (kind, frame.len()),
                    (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES)
                );
            },
        );
        assert_eq!(emitted, 1);
    }

    #[test]
    fn host_cap_requires_the_client_bit() {
        // Without CLIENT_CAP_PAD_AUDIO the answer is no on EVERY platform (on Windows the
        // env + provisioning legs are environment-dependent — not unit-tested here).
        assert!(!host_cap(0));
        assert!(!host_cap(punktfunk_core::quic::CLIENT_CAP_CURSOR));
    }
}
