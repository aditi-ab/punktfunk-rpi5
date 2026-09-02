//! Per-pad DualSense audio (wire `0xD1`).
//!
//! Capture of the pad's own device — WASAPI loopback ([`crate::audio::pad_endpoint`]) on
//! Windows, the per-pad PipeWire sink (`crate::audio::pad_sink`) on Linux — is de-interleaved
//! into speaker (front) and voice-coil (back) pairs, silence-gated, Opus-encoded at 48 kHz
//! CBR, and sent as [`PAD_AUDIO_MAGIC`](punktfunk_core::quic::PAD_AUDIO_MAGIC) datagrams.
//!
//! One thread per pad, spawned and reaped by [`super::input`]. Capture death reopens with
//! backoff; seq stays monotonic across reopens so the client sees a gap, not a restart.

use super::*;

/// Bit N of `kinds` is wire kind N — same packing as the arrival's audio-caps.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(super) const KIND_BIT_HAPTICS: u8 = 1 << punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(super) const KIND_BIT_SPEAKER: u8 = 1 << punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER;

/// 5 ms: haptics are felt latency. Speaker is 10 ms (coding efficiency). Wire cadences.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const HAPTICS_FRAME_MS: u32 = 5;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const SPEAKER_FRAME_MS: u32 = 10;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const HAPTICS_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * HAPTICS_FRAME_MS as usize / 1000;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const SPEAKER_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * SPEAKER_FRAME_MS as usize / 1000;
/// Quad: FL FR = speaker, BL BR = voice coils. Own copy: `pad_endpoint::PAD_CHANNELS` is Windows-gated.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const CAP_CHANNELS: usize = 4;

/// ≈ −60 dBFS. Opens on the first frame at this peak — haptics are felt latency.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const GATE_OPEN_PEAK: f32 = 1e-3;
/// 250 ms: long enough for a decaying haptic tail (and the decoder's), short enough idle is free.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const GATE_HANGOVER_MS: u32 = 250;

/// Band-limited rumble; 64 kbps CBR stays under one MTU.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const HAPTICS_BITRATE: i32 = 64_000;
/// Programme audio: 64 kbps CELT-only at 10 ms is artifacty; 96 kbps CBR is still ~120 B/frame.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const SPEAKER_BITRATE: i32 = 96_000;

/// Opens on the first signal frame; closes after [`GATE_HANGOVER_MS`] of quiet. Idle pad → no datagrams.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
struct SilenceGate {
    hangover_frames: u32,
    quiet: u32,
    /// Starts closed so a pad that never renders never sends.
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

    /// Signal opens on this frame. The hangover-completing quiet frame is suppressed.
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

/// Seq is frozen while gated (client tells silence from loss by continuity) and kept across
/// capture reopens (gap, not restart).
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

    /// `None` = gated: do not send, do not advance. Encode failure after this leaves a one-frame seq gap.
    fn admit(&mut self, frame: &[f32]) -> Option<u32> {
        if !self.gate.feed(frame) {
            return None;
        }
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(seq)
    }
}

/// FL FR → speaker, BL BR → haptics. A ragged tail (not a multiple of 4) is dropped, never smeared.
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

/// Disabled kinds are not split out, so they never reach an encoder.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
struct PadFramer {
    kinds: u8,
    acc: Vec<f32>,
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

    /// Haptics emit first — felt latency.
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

    /// Drop partials across a capture gap. Seq/gate live on [`LaneCtl`], which survives reopens.
    fn clear(&mut self) {
        self.acc.clear();
        self.front.clear();
    }
}

/// [`stop`](PadAudioHandle::stop) flags and joins; [`signal`](PadAudioHandle::signal) only flags
/// so the input thread can overlap joins instead of serializing the ~5 s quiet-endpoint timeout.
pub(super) struct PadAudioHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PadAudioHandle {
    pub(super) fn signal(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Bounded by the capturer's ~5 s quiet-endpoint recv. Mid-session reaps go through a detached
    /// reaper (`input.rs::PadAudioSlots::stop`); session teardown joins inline (10 s grace).
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

/// Fallback if `stop()` never ran (reaper-spawn failure).
impl Drop for PadAudioHandle {
    fn drop(&mut self) {
        self.reap();
    }
}

/// Advertise [`HOST_CAP_PAD_AUDIO`](punktfunk_core::quic::HOST_CAP_PAD_AUDIO) when the client asked
/// ([`CLIENT_CAP_PAD_AUDIO`](punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO)), `PUNKTFUNK_PAD_AUDIO` ≠ "0",
/// and a source exists: Windows has a provisioned endpoint; Linux has a reachable PipeWire daemon
/// (sinks mint lazily at spawn).
pub(super) fn host_cap(client_caps: u8) -> bool {
    let asked = client_caps & punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO != 0;
    #[cfg(target_os = "windows")]
    {
        // Startup can fail transiently with nothing latched; retry here — first session ask.
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
        let _ = asked;
        false
    }
}

/// Stream `kinds` (bit 0 = haptics, bit 1 = speaker) toward `conn`. `stop` is this handle's own
/// flag. `None` if the slot has no endpoint (failed/still-running provision, or pad ≥
/// `PUNKTFUNK_PAD_AUDIO_SLOTS`) or spawn fails; the pad still works, without audio.
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
        // Devnode-without-endpoint (`find`): refuse rather than spin open/backoff on an empty id.
        return None;
    }
    if ep.needs_aeb_kick {
        // Stamps stored but not served — DualSense identity never adopted. Opening anyway is
        // worse: AUTOCONVERTPCM succeeds on a wrong-format endpoint, so the stream looks
        // healthy while haptics/speaker mis-route.
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
            // COM for the visibility flips; capturer opens run on their own thread.
            let _ = wasapi::initialize_mta();
            // Park HIDDEN with no pad attached: a visible idle "Wireless Controller" speaker
            // makes libScePad titles take the DualSense-haptics path against an unserviced
            // endpoint. Show only for this pad's lifetime; backoff absorbs audiosrv re-activate.
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

/// Mutually exclusive: a usbip pad already owns a real ALSA card; minting sinks beside it
/// duplicates the node graph.
#[cfg(target_os = "linux")]
enum LinuxPadCapture {
    Usb(crate::audio::pad_usb::PadUsbCapturer),
    /// UHID pad: mint the sinks a real card would have had.
    Sink(crate::audio::pad_sink::PadSinkCapturer),
}

#[cfg(target_os = "linux")]
impl crate::audio::AudioCapturer for LinuxPadCapture {
    fn next_chunk(&mut self) -> anyhow::Result<Vec<f32>> {
        match self {
            LinuxPadCapture::Usb(c) => c.next_chunk(),
            LinuxPadCapture::Sink(c) => c.next_chunk(),
        }
    }

    fn next_chunk_within(&mut self, budget: std::time::Duration) -> anyhow::Result<Vec<f32>> {
        match self {
            LinuxPadCapture::Usb(c) => c.next_chunk_within(budget),
            LinuxPadCapture::Sink(c) => c.next_chunk_within(budget),
        }
    }

    fn channels(&self) -> u32 {
        match self {
            LinuxPadCapture::Usb(c) => c.channels(),
            LinuxPadCapture::Sink(c) => c.channels(),
        }
    }
}

/// Open capture lazily on the streamer thread (open-with-backoff, same as Windows). `edge`
/// selects DualSense Edge for a minted sink. `None` only for empty kinds, a slot past
/// `PUNKTFUNK_PAD_AUDIO_SLOTS`, or spawn failure; the pad still works, without audio.
///
/// Capture follows the pad transport flag, not whether a stream is published yet — otherwise
/// the race between pad arrival and this thread would mint a duplicate node graph over a
/// real usbip card.
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
    let usb = pf_inject::dualsense_usbip::usbip_preferred();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            pad_audio_thread(
                conn,
                pad,
                kinds,
                move || {
                    if usb {
                        crate::audio::pad_usb::PadUsbCapturer::open(pad).map(LinuxPadCapture::Usb)
                    } else {
                        crate::audio::pad_sink::PadSinkCapturer::open(pad, edge)
                            .map(LinuxPadCapture::Sink)
                    }
                },
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

/// Other hosts have no virtual DualSense audio source; [`host_cap`] never advertises the cap.
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

/// `encode_errs` is a power-of-two throttle (~200 fails/s unthrottled).
#[cfg(any(target_os = "windows", target_os = "linux"))]
struct Lane {
    kind: u8,
    ctl: LaneCtl,
    enc: opus::Encoder,
    encode_errs: u64,
}

/// Stereo 48 kHz hard-CBR. Haptics: LowDelay (CELT-only, 2.5 ms lookahead) at 64 kbps.
/// Speaker: `Application::Audio` at 96 kbps — extra ~4 ms is inaudible; CELT-only artifacts were not.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn build_lanes(kinds: u8) -> Result<Vec<Lane>, opus::Error> {
    let mut lanes = Vec::new();
    for (bit, kind, frame_ms, app, bitrate) in [
        (
            KIND_BIT_HAPTICS,
            punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS,
            HAPTICS_FRAME_MS,
            opus::Application::LowDelay,
            HAPTICS_BITRATE,
        ),
        (
            KIND_BIT_SPEAKER,
            punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER,
            SPEAKER_FRAME_MS,
            opus::Application::Audio,
            SPEAKER_BITRATE,
        ),
    ] {
        if kinds & bit == 0 {
            continue;
        }
        let mut enc = opus::Encoder::new(crate::audio::SAMPLE_RATE, opus::Channels::Stereo, app)?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate)).ok();
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

/// Capture death reopens with [`INJECTOR_REOPEN_BACKOFF`] (encoders + seq kept). ConnectionLost
/// or a gone datagram path ends the thread; a single TooLarge costs that frame only.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn pad_audio_thread<C: crate::audio::AudioCapturer>(
    conn: quinn::Connection,
    pad: u8,
    kinds: u8,
    open: impl Fn() -> anyhow::Result<C>,
    stop: Arc<AtomicBool>,
) {
    // Same boost as session send: live pad audio is a ≤10 ms cadence.
    crate::native::boost_thread_priority(false);
    let mut lanes = match build_lanes(kinds) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio opus encoder init failed — pad continues without audio");
            return;
        }
    };
    if lanes.is_empty() {
        return; // spawn() refuses kinds == 0
    }
    let mut framer = PadFramer::new(kinds);
    // One Opus frame; 96 kbps CBR at ≤10 ms is ~120 bytes. 1500 is session-plane slack.
    let mut opus_buf = vec![0u8; 1500];
    // Capture death reopens instead of muting the pad for the session. First open rides this too.
    let mut capturer: Option<C> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    let mut oversized_drops: u64 = 0;
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
                    framer.clear();
                }
                Err(e) => {
                    tracing::debug!(pad, error = %format!("{e:#}"), "pad-audio open failed — will retry");
                    last_failed = Some(std::time::Instant::now());
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
        }
        // Empty chunk = quiet endpoint (idle timeout), not death. Only Err drops the capturer.
        let chunk = match capturer.as_mut().unwrap().next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(pad, error = %format!("{e:#}"), "pad-audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        let mut end_plane = false;
        framer.feed(&chunk, |kind, frame| {
            if end_plane {
                return;
            }
            let Some(lane) = lanes.iter_mut().find(|l| l.kind == kind) else {
                return; // framer emits only enabled kinds
            };
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
                    match conn.send_datagram(d.into()) {
                        Ok(()) => {}
                        // The only outcome that really is "the session is over".
                        Err(quinn::SendDatagramError::ConnectionLost(_)) => end_plane = true,
                        // One frame, not the plane. seq already advanced; client conceals the gap.
                        Err(quinn::SendDatagramError::TooLarge) => {
                            oversized_drops += 1;
                            if oversized_drops.is_power_of_two() {
                                tracing::warn!(
                                    pad,
                                    kind,
                                    count = oversized_drops,
                                    opus_bytes = n,
                                    "pad-audio datagram rejected as too large — dropping the \
                                     frame and continuing"
                                );
                            }
                        }
                        // Datagrams are gone for this connection. Next frame will not land.
                        Err(e) => {
                            tracing::warn!(
                                pad,
                                error = %e,
                                "the QUIC datagram path is unavailable — ending this pad's audio"
                            );
                            end_plane = true;
                        }
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
        if end_plane {
            break 'session;
        }
    }
    // Dropping the capturer stops its WASAPI thread. No cross-session park: pad capture is per-pad.
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};

    fn frame(level: f32, n: usize) -> Vec<f32> {
        vec![level; n * 2]
    }

    #[test]
    fn gate_opens_immediately_and_closes_after_hangover() {
        let mut g = SilenceGate::new(HAPTICS_FRAME_MS);
        // 250 ms / 5 ms.
        assert_eq!(g.hangover_frames, 50);
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // Threshold opens on this frame (haptics are felt latency).
        assert!(g.feed(&frame(GATE_OPEN_PEAK, HAPTICS_FRAME_SAMPLES)));
        // 49 quiet frames ride the hangover; the 50th completes 250 ms and is suppressed.
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // Sub-threshold does not reopen; negative peaks count as signal.
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
        assert_eq!(g.hangover_frames, 25); // 250 ms / 10 ms
        assert!(g.feed(&frame(0.1, SPEAKER_FRAME_SAMPLES)));
        for _ in 0..24 {
            assert!(g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
    }

    #[test]
    fn seq_freezes_while_gated_and_survives_reopen() {
        let mut lane = LaneCtl::new(HAPTICS_FRAME_MS);
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(0));
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(1));
        // Hangover is still sent (seq advances); then the gate closes and seq freezes.
        for i in 0..49u32 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), Some(2 + i));
        }
        for _ in 0..500 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), None);
        }
        // Reopen resets only the framer — LaneCtl is untouched, so the next audible frame continues.
        assert_eq!(lane.admit(&frame(0.9, HAPTICS_FRAME_SAMPLES)), Some(51));
    }

    #[test]
    fn splitter_exact_pairs() {
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
        // 10 ms of capture: two 5 ms haptics frames from the back pair, then one 10 ms speaker frame.
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
        // 20 ms of all-ones: 4 potential haptics frames, 2 potential speaker frames.
        let quad = vec![1.0f32; 4 * HAPTICS_FRAME_SAMPLES * CAP_CHANNELS];
        let mut kinds_seen = Vec::new();
        // Haptics-only: the front pair is never split out.
        let mut f = PadFramer::new(KIND_BIT_HAPTICS);
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_HAPTICS; 4]);
        let mut f = PadFramer::new(KIND_BIT_SPEAKER);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_SPEAKER; 2]);
        // kinds = 0 is never spawned, but the framer must still be total.
        let mut f = PadFramer::new(0);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert!(kinds_seen.is_empty());
    }

    #[test]
    fn framer_clear_drops_partials_only() {
        let mut f = PadFramer::new(KIND_BIT_HAPTICS | KIND_BIT_SPEAKER);
        let mut emitted = 0;
        // 100 samples: no frame boundary yet.
        f.feed(&vec![0.1; 100 * CAP_CHANNELS], |_, _| emitted += 1);
        assert_eq!(emitted, 0);
        f.clear();
        // After the gap: one haptics frame from 240 fresh samples — the 100 stale would skew later boundaries.
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
        // Without CLIENT_CAP_PAD_AUDIO the answer is no on every platform (Windows env +
        // provisioning legs are environment-dependent — not unit-tested here).
        assert!(!host_cap(0));
        assert!(!host_cap(punktfunk_core::quic::CLIENT_CAP_CURSOR));
    }
}
