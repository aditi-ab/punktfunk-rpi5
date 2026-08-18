//! Android audio playback (android-only): pull audio packets from the connector, decode to
//! interleaved f32 (stereo or 5.1/7.1 surround), and feed AAudio via its realtime data callback
//! through a jitter ring. Mirrors [`crate::decode`]: one thread we own (the decode producer)
//! plus a shutdown flag; the realtime callback thread is owned by AAudio.
//!
//! **Two planes, one pipeline.** A session runs Opus on `0xC9` (48 kHz, 5 ms frames — what every
//! host has always spoken) **or** lossless PCM on `0xD3` at the negotiated rate and depth
//! (`design/hi-res-audio.md`), never both, and which one is a session-wide fact settled in the
//! handshake — [`punktfunk_core::client::NativeClient::audio_codec`] — not a per-packet one.
//! Everything below reads it once through [`SessionAudio`]. The two planes share the jitter ring,
//! the A/V sync loop and the gap tracker *unchanged*, because they share a datagram header; only
//! the payload decode differs, and the concealment — a lossless format has no PLC to borrow
//! (§4.5), so [`punktfunk_core::audio::pcm::PcmConceal`] stands in for libopus's.
//!
//! **The device is not assumed to work.** Opening AAudio is a negotiation with a vendor HAL, and
//! this plane used to treat it as a formality: one Exclusive attempt, one Shared retry, and from
//! there everything was taken on trust. Three separate failures all came out as "the app has no
//! sound" with a perfectly healthy log — a configuration that opens but never routes, a
//! `request_start` that fails, and a disconnect (which by AAudio's contract kills the stream for
//! good and is not rare on a TV, where an HDMI mode switch is a routine event this very client
//! provokes). So the open now walks a LADDER ([`open_ladder`]), every rung has to prove the device
//! is really pulling before it is accepted ([`arm`]), and a supervisor owns the whole plane for the
//! session and reopens it when the device goes away ([`supervise`]). `debug.punktfunk.audio_sharing`
//! / `audio_perf` / `audio_reopen` pin any of it from `adb shell setprop`, for the device that
//! reports silence and cannot be handed a custom build.
//!
//! **The ladder's rate dimension.** Every rung used to ask for 48 kHz, so rejecting a stream whose
//! GRANTED rate differed was free. With a negotiated rate it is not: a device that will not grant
//! the session's rate would fail every rung and the supervisor would disable audio for the whole
//! session, which is the one outcome the design calls unacceptable. So the rung carries the rate it
//! asked for, [`arm`] compares against THAT rather than a constant, and the ladder ends with a rung
//! that asks for nothing at all (AAudio's own choice) for the HAL that refuses an explicit request
//! but is natively at the rate we wanted. What the ladder deliberately does NOT contain is a rung
//! at any OTHER rate: opening one would mean either playing the wire at the wrong speed or
//! resampling it behind the user's back, and §9's rule is "say so and fall back, not resample
//! quietly". The fallback that keeps such a device in audio therefore happens BEFORE the `Hello`
//! — see [`output_rate_is_openable`], which is why that function exists. It matters more now than
//! it did, not less: the plane carries both rate families
//! ([`punktfunk_core::audio::pcm::rate_is_supported`]), 44 100 Hz is granted almost everywhere and
//! 176 400 Hz almost nowhere, so which rungs a given device will open is genuinely unknown until
//! one is opened.
//!
//! The layout is the host-RESOLVED channel count (`NativeClient::audio_channels`, negotiated at
//! connect), so an older/clamping host that can only capture stereo is decoded + played as stereo.
//! 2 = stereo / 6 = 5.1 / 8 = 7.1, in the canonical wire order FL FR FC LFE RL RR SL SR. **That
//! now applies to the lossless plane too**: `0xD3` was stereo-only while a surround frame did not
//! fit a datagram, but the frame ladder is channel-aware and the restriction was one host-side
//! condition — so every per-frame size here comes from the resolved count, and a 5.1 lossless
//! session simply negotiates a shorter frame (and a higher packet rate) than a stereo one.
//!
//! The ring started as a port of `punktfunk-client-linux/src/audio.rs`, but AAudio — unlike
//! PipeWire, which adaptively rate-matches the stream and absorbs a shallow buffer — hands us a raw
//! realtime callback and makes us own the buffer. So this client diverges deliberately to stop the
//! Android-only crackle: (1) the callback is allocation/free-free — decoded buffers are recycled to
//! the producer via a free-list instead of being freed on the audio thread (Android's Scudo `free`
//! has unbounded tail latency); (2) the jitter ring is deeper than the other clients' and decoupled
//! from the tiny LowLatency burst size, with de-prime hysteresis so a transient drain doesn't
//! manufacture a silence; (3) the AAudio HW buffer is primed above its 2-burst default and grown on
//! XRuns (Google's anti-glitch technique).
//!
//! (2) is now the SHARED `punktfunk_core::audio::JitterPolicy` at `JitterTuning::AAUDIO`, which also
//! fixed what this ring was missing: it had a hard cap but nothing that walked the depth back down,
//! so drift and arrival bursts raised latency permanently and Android settled on its ceiling.
//!
//! It is also **A/V synchronised** (`design/audio-latency-overhaul.md`): the decode thread reads the
//! host capture `pts_ns` every `AudioPacket` has always carried, compares where this frame will
//! actually play against where the picture it belongs with reached glass
//! (`decode::DisplayTracker` publishes that), and asks the ring for a depth that closes the gap.
//! Only ASKS — `JitterPolicy` clamps the request between its own underrun-driven floor and the hard
//! cap, so continuity outranks sync and a link whose jitter genuinely needs more buffer than the
//! picture is away keeps its buffer, with the residual reported on the HUD instead of taken out of
//! the listener's stream. With no video reference (below API 33 there are no render callbacks, so
//! nothing confirms a present) the target stays `None` and the ring behaves exactly as it did.

use crate::audio_format::SessionAudio;
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

/// What one playback open attempt yields: the stream, plus both halves of the PCM hand-off — the
/// sender the decode thread fills and the receiver that returns drained buffers for refill.
///
/// A named struct rather than the tuple this used to be, because the tuple's type tripped
/// `clippy::type_complexity` (the Android target is linted since `:kit:cargoNdkClippy`) and because
/// the open path now carries the rung it succeeded on into the logs.
struct LiveStream {
    stream: AudioStream, // dropping it closes the AAudio stream
    tx: SyncSender<Vec<f32>>,
    free_rx: Receiver<Vec<f32>>,
    rung: OpenRung,
}

/// One rung of the AAudio open ladder — a sharing mode, a performance mode and the sample rate
/// they are tried at.
///
/// `rate` is `Some(hz)` for an explicit request (AAudio's contract: an explicitly-set rate is
/// honoured or the open FAILS — it never silently substitutes) and `None` for AAUDIO_UNSPECIFIED,
/// which lets the HAL name its own. The unspecified rung is not a licence to play at whatever came
/// back: [`arm`] accepts it only when the granted rate equals the session's, so it rescues the
/// device that refuses an explicit 48 000/96 000 while already running at it, and rejects the one
/// that would have handed us a different rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenRung {
    sharing: AudioSharingMode,
    perf: AudioPerformanceMode,
    rate: Option<i32>,
}

/// Why [`decode_loop`] returned — only one of them is worth reopening the device for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeExit {
    /// `AudioPlayback` was dropped (session teardown, or Kotlin's `nativeStopAudio`).
    Shutdown,
    /// AAudio reported the stream disconnected. The stream is now dead by AAudio's contract and
    /// the only recovery is close + open a new one — see [`supervise`].
    Disconnected,
    /// The connector closed: no more audio is coming, so there is nothing to reopen FOR.
    SessionClosed,
    /// The plane cannot run at all (the decoder would not build — libopus refusing the negotiated
    /// rate is the only way this happens today, since the PCM arm cannot fail). Reopening the
    /// DEVICE would not change that, so it is not a reason to walk the ladder again.
    Fatal,
}

/// Decoded-chunk hand-off depth: 64 frames of slack (matches the core's AUDIO_QUEUE).
const RING_CHUNKS: usize = 64;
/// How long [`arm`] waits for a freshly started stream's FIRST data callback before writing the
/// rung off. Generous: a LowLatency stream calls back every few ms, and even a legacy path with a
/// large HDMI period is well inside this. The ring is un-primed for all of it, so what the device
/// pulls here is the priming silence — the point is only to prove that it pulls at all.
const START_WATCHDOG_MS: u64 = 400;
const START_WATCHDOG_POLL_MS: u64 = 10;
/// Settling time between a disconnect and the reopen. An HDMI route change (a TV switching mode,
/// an AVR re-handshaking) is not instantaneous, and reopening into the middle of one just spends
/// the ladder on rungs that were always going to fail.
const REOPEN_SETTLE_MS: u64 = 250;
/// How many times a reopen may find no usable device before the plane gives up — ~2 s of trying,
/// which comfortably outlasts an HDMI mode switch. Bounded rather than infinite so a device that
/// disconnects permanently (unplugged, claimed by another app for good) settles into silence
/// instead of a forever loop of opens on the session's audio thread.
const REOPEN_ATTEMPTS: u32 = 8;
/// Packets decoded with AAudio never having taken a single sample before we call it — expressed
/// as a DURATION, because the two planes do not agree on what a packet is worth: 5 ms on Opus, and
/// down to the ladder's shortest 1 ms rung on a `0xD3` session that is 24-bit surround or at the
/// top of the rate ladder. A packet count would have meant ~0.2 s there and a warning that fires
/// before a slow HAL has finished waking.
const DEAD_STREAM_WARN_MS: u64 = 1_000;

// --- Jitter-ring depths now come from the SHARED policy (`punktfunk_core::audio::JitterTuning`). --
// They used to be four Android-only constants here. The rationale for Android being DEEPER than the
// other clients still holds and is preserved in `JitterTuning::AAUDIO`: unlike PipeWire, which
// adaptively rate-matches the stream to the graph clock and masks host↔DAC drift, AAudio hands us a
// raw callback and we own the buffer, so drift and Wi-Fi power-save bunching land as
// underruns/overflows = crackle.
//
// Two things changed with the move. The prime floor drops 40 ms → 25 ms, because the policy GROWS
// the target on the devices that actually underrun instead of every device pre-paying for the worst
// one. And the ring finally sheds: it had a hard cap but nothing that walked the depth back down, so
// any drift or burst raised latency permanently and Android converged on its 120 ms ceiling and
// stayed there — the "audio latency is too high" report.
/// Throttle the AAudio XRun-driven HW-buffer grow check (cheap, but no need to poll every quantum).
const XRUN_CHECK_EVERY: u32 = 128;

/// Why one arriving frame could not be turned into samples. Small on purpose — the decode loop
/// only logs it and moves on to the next packet.
#[derive(Debug)]
enum DecodeErr {
    Opus(opus::Error),
    /// A `0xD3` payload that is not a whole number of samples at the negotiated depth — a
    /// truncated or hostile datagram. There is no partial-frame reading of it that is not a
    /// permanent desync of every sample after it, so it is dropped whole.
    Ragged,
}

impl std::fmt::Display for DecodeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeErr::Opus(e) => write!(f, "opus: {e}"),
            DecodeErr::Ragged => write!(f, "PCM payload is not a whole number of samples"),
        }
    }
}

/// Decoder for the audio plane: a plain Opus stereo decoder (the validated path), an Opus
/// multistream decoder for 5.1/7.1, or — on the lossless `0xD3` plane — no decoder at all, since
/// interleaved little-endian samples are a stride unpack. All three sit behind one `decode_float`
/// / `conceal` pair so the loop below never branches on the plane. Built from the host-RESOLVED
/// format; mirrors the Linux client's `AudioDec` and core's own `AudioPcmState`.
enum AudioDec {
    Stereo(opus::Decoder),
    Surround(opus::MSDecoder),
    /// The `0xD3` plane. `conceal` rides here rather than next to the Opus arms because it is the
    /// thing a lossless format cannot borrow: `AudioGapTracker` feeds libopus PLC on `0xC9`, and
    /// there is nothing in a raw frame from which to synthesize its successor (§4.5). `scratch`
    /// exists because `pcm::to_f32` clears and reserves its output — it cannot write into the
    /// fixed slice the loop hands out — so the samples are staged and then COPIED in, clamped.
    Pcm {
        bits: u8,
        scratch: Vec<f32>,
        conceal: punktfunk_core::audio::pcm::PcmConceal,
    },
}

impl AudioDec {
    fn new(fmt: SessionAudio) -> Result<AudioDec, opus::Error> {
        let channels = fmt.channels as u8;
        if fmt.is_pcm() {
            return Ok(AudioDec::Pcm {
                bits: fmt.bits,
                scratch: Vec::with_capacity(fmt.frame_samples()),
                conceal: punktfunk_core::audio::pcm::PcmConceal::new(),
            });
        }
        // The negotiated rate, not a constant — even though on this plane it is always 48 000.
        // libopus accepts only 8/12/16/24/48 kHz and rejects 96 000 outright, which is the entire
        // reason `0xD3` exists, so passing it through costs nothing and makes libopus itself the
        // validator: a host that claimed Opus at a rate it cannot open fails loudly HERE (one
        // "audio disabled" line naming the codec) instead of decoding at the wrong rate.
        if channels == 2 {
            Ok(AudioDec::Stereo(opus::Decoder::new(
                fmt.rate_hz,
                opus::Channels::Stereo,
            )?))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            Ok(AudioDec::Surround(opus::MSDecoder::new(
                fmt.rate_hz,
                l.streams,
                l.coupled,
                l.mapping,
            )?))
        }
    }

    /// Turn one arriving frame into interleaved f32 in `out`, returning **per-channel** samples
    /// (the unit both planes' callers count in, and the unit concealment is sized from).
    ///
    /// `out` is a fixed slice and is never grown: on the PCM arm the staged samples are copied in
    /// clamped to what fits, which is what makes an oversized or malformed datagram a truncated
    /// frame rather than an overrun on the decode thread.
    fn decode_float(
        &mut self,
        input: &[u8],
        out: &mut [f32],
        channels: usize,
    ) -> Result<usize, DecodeErr> {
        match self {
            AudioDec::Stereo(d) => d.decode_float(input, out, false).map_err(DecodeErr::Opus),
            AudioDec::Surround(d) => d.decode_float(input, out, false).map_err(DecodeErr::Opus),
            AudioDec::Pcm {
                bits,
                scratch,
                conceal,
            } => {
                // No host emits an empty `0xD3` payload — PCM has no DTX — but a torn datagram
                // can present as one, and it must NOT reach `PcmConceal::accept`: accepting an
                // empty frame would clear the last good frame and leave the next loss with
                // nothing to conceal from.
                if input.is_empty() {
                    return Ok(0);
                }
                let n = punktfunk_core::audio::pcm::to_f32(input, *bits, scratch)
                    .ok_or(DecodeErr::Ragged)?;
                let n = n.min(out.len());
                out[..n].copy_from_slice(&scratch[..n]);
                // The next loss is concealed from what the ring actually RECEIVED — the staged
                // prefix, not the decoded frame. They differ only for an oversized datagram no
                // conforming host sends, and taking the staged length keeps the concealment
                // source bounded by the same fixed buffer as everything else.
                conceal.accept(&out[..n]);
                Ok(n / channels.max(1))
            }
        }
    }

    /// Synthesize ONE concealed frame into `out` for a packet that never arrived, returning
    /// per-channel samples (0 = nothing to build from yet, so the caller should let the ring
    /// carry the gap rather than emit an uninitialized buffer).
    ///
    /// Opus interpolates from the decoder's own state (empty input = libopus PLC). `0xD3` has no
    /// such state, so [`punktfunk_core::audio::pcm::PcmConceal`] repeats-and-fades the last good
    /// frame and decays a sustained gap to silence — a clean dropout beats a warble.
    fn conceal(
        &mut self,
        out: &mut [f32],
        frame_samples: usize,
        channels: usize,
    ) -> Result<usize, DecodeErr> {
        // libopus synthesizes into a slice sized by the LAST decoded frame; asking for more than
        // that is asking it to invent audio the stream never had, and asking with 0 is asking
        // before anything has decoded.
        let plc = (frame_samples * channels).min(out.len());
        match self {
            AudioDec::Stereo(d) if plc > 0 => d
                .decode_float(&[], &mut out[..plc], false)
                .map_err(DecodeErr::Opus),
            AudioDec::Surround(d) if plc > 0 => d
                .decode_float(&[], &mut out[..plc], false)
                .map_err(DecodeErr::Opus),
            AudioDec::Stereo(_) | AudioDec::Surround(_) => Ok(0),
            AudioDec::Pcm {
                scratch, conceal, ..
            } => {
                if !conceal.conceal(scratch) {
                    return Ok(0); // nothing has arrived yet — nothing to repeat
                }
                let n = scratch.len().min(out.len());
                out[..n].copy_from_slice(&scratch[..n]);
                Ok(n / channels.max(1))
            }
        }
    }
}

/// Diagnostics — written by the decode thread + the realtime callback, logged periodically. The
/// audio analogue of the video `fed`/`rendered` counters (we can't "screenshot" sound).
///
/// The ring's DEPTH is not here: the A/V sync loop needs the same number in the same units, so it
/// is published once through [`punktfunk_core::audio::AudioSyncCell`] and read from there by the
/// log line below. One publisher, one reading — a second copy is a second thing to go stale.
#[derive(Default)]
struct Counters {
    // Wire frames decoded OK — Opus packets off `0xC9` (~200/s at 5 ms) or PCM frames off `0xD3`
    // (up to 1 000/s at the ladder's 1 ms rung, which 24-bit surround and the top of the rate
    // ladder land on). One counter for both planes because only one of them ever runs.
    frames_decoded: AtomicU64,
    pcm_written: AtomicU64, // PCM frames copied out to AAudio (device clock is pulling)
    underruns: AtomicU64,   // callbacks that emitted silence (ring not primed / drained)
    target_ms: AtomicU64,   // the policy's LIVE target depth (it grows on this device's underruns)
    /// Sync-driven inserts: one duplicated, crossfaded frame each (`JitterStep::insert_front`).
    /// Concealment must be visible next to the underruns it prevents — a ring that is quietly
    /// being deepened is a link whose picture keeps moving away from its audio.
    inserts: AtomicU64,
    /// Data callbacks since the process started, primed or not. Distinct from `pcm_written`
    /// (which only counts SERVED reads) because that is exactly the distinction the start
    /// watchdog needs: a device that is pulling but un-primed still ticks this, a stream that
    /// opened into the void ticks nothing at all. See [`wait_for_first_callback`].
    callbacks: AtomicU64,
}

/// Whether the A/V sync loop runs this session. `false` leaves `JitterPolicy`'s sync target at
/// `None`, which reproduces the pre-overhaul ring behaviour exactly — the point of the hatch.
///
/// Two levers because Android has neither of the other clients' launch surfaces. `PUNKTFUNK_NO_AV_SYNC`
/// keeps the contract the desktop clients document (and works when the client is driven from a
/// shell), but an app started from the launcher inherits no such environment, so the one a field
/// tester can actually reach is the sysprop — `adb shell setprop debug.punktfunk.no_av_sync 1`,
/// no rebuild, exactly like `debug.punktfunk.presenter`. A loop that steers PLAYBACK has to be
/// bisectable on the device that reports the regression, not only on the bench.
fn av_sync_enabled() -> bool {
    if matches!(
        std::env::var("PUNKTFUNK_NO_AV_SYNC").as_deref(),
        Ok("1") | Ok("true")
    ) {
        return false;
    }
    !matches!(
        sysprop(c"debug.punktfunk.no_av_sync").as_deref(),
        Some("1") | Some("true")
    )
}

/// Read an Android system property; `None` when unset, empty or not UTF-8.
///
/// One reader for all of them: the audio plane now has four field-reachable knobs
/// (`no_av_sync`, `audio_sharing`, `audio_perf`, `audio_reopen`) and the open-ladder ones exist
/// precisely so a device that reports silence can be bisected WITHOUT a rebuild — an app launched
/// from a TV's home screen inherits no environment, so a sysprop is the only lever a field tester
/// can actually reach.
fn sysprop(name: &std::ffi::CStr) -> Option<String> {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: a valid NUL-terminated name + a PROP_VALUE_MAX-sized buffer is always safe.
    let n = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
    if n <= 0 {
        return None;
    }
    std::str::from_utf8(&buf[..n as usize])
        .ok()
        .map(str::to_owned)
}

/// Is this an Android TV / set-top box (as opposed to a phone, tablet or handheld)?
///
/// `ro.build.characteristics` carries a comma-separated trait list and contains `tv` on every
/// Android TV build. Read natively rather than plumbed down from Kotlin's `FEATURE_LEANBACK`
/// (which `StreamScreen` already computes for the video plane) to keep this decision inside the
/// audio module — the JNI entry point's signature is a compatibility surface for the kit, and the
/// only thing that wants this fact is [`open_ladder`]. An unset property reads as "not a TV",
/// which keeps the pre-existing behaviour on anything that does not answer.
fn is_tv_device() -> bool {
    sysprop(c"ro.build.characteristics")
        .is_some_and(|s| s.split(',').any(|trait_| trait_.trim() == "tv"))
}

/// Owned by [`crate::session::SessionHandle`]: the supervisor thread that owns the AAudio stream
/// and the decode loop for as long as the session lives.
pub struct AudioPlayback {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayback {
    /// Spawn the audio supervisor: it opens AAudio at the host-RESOLVED format by walking
    /// [`open_ladder`], runs the decode loop against it, and reopens it if the device disconnects.
    /// `None` only if the thread itself could not be spawned — an open failure is reported by the
    /// supervisor (the caller leaves video streaming either way).
    ///
    /// `game_audio` (the experimental low-latency mode) tags the stream usage=Game for the HAL's
    /// game-audio routing; off, the stream is untagged as it was before the overhaul. `is_tv` is
    /// Kotlin's `FEATURE_LEANBACK` and steers the ladder — see [`open_ladder`].
    pub fn start(
        client: Arc<NativeClient>,
        game_audio: bool,
        is_tv: bool,
    ) -> Option<AudioPlayback> {
        // Everything about the format comes from what the host RESOLVED, never from what this
        // device asked for: the channel count (2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire
        // order FL FR FC LFE RL RR SL SR), the plane, the rate, the depth and the frame duration.
        let fmt = SessionAudio::of(&client);
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-audio".into())
            .spawn(move || supervise(client, game_audio, is_tv, fmt, &sd))
            .ok()?;
        Some(AudioPlayback {
            shutdown,
            join: Some(join),
        })
    }
}

/// Would this device open a playback stream at `rate_hz`? Asked **before** the `Hello`, from
/// [`crate::session::connect`], and the reason it is asked there at all.
///
/// AAudio's contract is that an explicitly-requested rate is honoured or the open FAILS — it never
/// silently substitutes. Which means a device that will not grant the requested rate cannot be
/// rescued *after* negotiation: the wire would already be carrying that rate's frames, the plane is
/// never renegotiated mid-session (§6), and the only ways to play them on a stream of another rate
/// are the wrong speed or a resampler nobody asked for. §7 states the rule directly — *"a client
/// that cannot open a 96 kHz output must not set `CLIENT_CAP_AUDIO_HIRES`"* — and this is how this
/// client knows.
///
/// **Android is the platform where this can genuinely fail, and the 44.1 kHz family widened the
/// gap rather than narrowing it.** 44 100 Hz is granted by very nearly every output — it is what
/// half the world's material is — while 176 400 Hz is granted by very nearly none, and neither
/// answer is guessable from the rate alone. So the caller probes every rung it is willing to ask
/// for (see `session::connect`'s fallback ladder) instead of assuming any of them opens.
///
/// The probe is the most permissive rung the ladder would ever reach (Shared + no performance
/// hint), so a `true` here means SOME rung can open it; it is not a promise that the Exclusive one
/// will. It is also a measurement at one instant: a route change between here and playback can
/// still invalidate it, which is why [`open_ladder`] carries the rate too.
///
/// `channels` is the layout the session will REQUEST, not the one the host will resolve — the
/// resolved count does not exist until the `Welcome`. It is the closest truth available here, and
/// it errs the safe way: a device that cannot open 5.1 at all declines hi-res and gets Opus, which
/// is the plane it would have been left on anyway.
///
/// Opened and immediately dropped — never started, no data callback, so nothing is routed and no
/// audio focus is taken. Never called for the 48 kHz rung (universally granted, and the ladder's
/// floor), so an ordinary session opens nothing here and pays nothing for it.
///
/// ⚠ Never `request_start` this stream. The ndk wrapper's `Drop` **unwraps** `AAudioStream_close`'s
/// status, so closing a stream the HAL is unhappy about panics rather than logging — which is why
/// [`open_any`] stops a rung before dropping it. A stream that was opened and never started closes
/// cleanly from OPEN, so this probe has nothing to tear down.
pub fn output_rate_is_openable(rate_hz: u32, channels: u8) -> bool {
    let built = AudioStreamBuilder::new().map(|b| {
        b.direction(AudioDirection::Output)
            .sample_rate(rate_hz as i32)
            .channel_count(punktfunk_core::audio::normalize_channels(channels) as i32)
            // The same f32 device format playback uses — see `try_open`. The wire depth is a wire
            // fact and never reaches AAudio, so probing at 24-bit would be probing the wrong thing.
            .format(AudioFormat::PCM_Float)
            .sharing_mode(AudioSharingMode::Shared)
            .performance_mode(AudioPerformanceMode::None)
            .open_stream()
    });
    match built {
        Ok(Ok(stream)) => {
            // Belt and braces: the contract says an explicit rate is granted or the open fails,
            // but this is the one place cheap enough to check rather than trust, and a HAL that
            // lied here would otherwise have talked us into negotiating a wire we cannot play.
            let granted = stream.sample_rate();
            if granted != rate_hz as i32 {
                log::warn!(
                    "audio: probe asked AAudio for {rate_hz} Hz and was granted {granted} Hz — treating the rate as unavailable"
                );
                return false;
            }
            true
        }
        Ok(Err(e)) => {
            log::info!("audio: this device will not open a {rate_hz} Hz output ({e})");
            false
        }
        Err(e) => {
            // No builder at all is a broken AAudio, not a verdict about the rate. Say no: the
            // caller's fallback is the legacy 48 kHz plane, which is the safe answer either way.
            log::warn!("audio: AAudio stream builder unavailable for the {rate_hz} Hz probe ({e})");
            false
        }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // The supervisor stopped + closed the AAudio stream on its way out.
    }
}

/// The AAudio configurations to try, best first.
///
/// **Why a ladder at all.** This used to be two hard-coded attempts — Exclusive, then Shared — and
/// only an *open* failure demoted. Everything after the open (a start failure, a stream that never
/// calls back) simply gave up or, worse, looked healthy while playing nothing. A device that can
/// open a configuration it cannot route therefore ended as permanent silence with no signal in the
/// log beyond a perfectly ordinary "AAudio started" line.
///
/// **Why a TV starts at Shared.** Exclusive is MMAP, the lowest-latency path AAudio has, and it is
/// the one rung whose behaviour we cannot verify from inside the process — a HAL may accept it and
/// route it nowhere. The latency it buys was never actually banked: the jitter-ring depths above
/// are unchanged from the Shared-only era (`JitterTuning::AAUDIO` still primes at 25 ms), so on a
/// mains-powered HDMI box the few ms an MMAP path saves are worth strictly less than not betting
/// the entire audio plane on it. Phones and handhelds — where the depths might one day come down,
/// and where MMAP is exercised by every other app on the device — keep Exclusive first.
///
/// **Why rate is the OUTERMOST dimension.** Every sharing/performance mode is tried at the
/// session's negotiated rate before anything is tried at another: a Shared, resampled stream at
/// the RIGHT rate is worth more than an MMAP stream at the wrong one, because the wrong one is not
/// mis-tuned — it is the wrong audio. The second (and last) rate rung asks for nothing at all
/// (AAUDIO_UNSPECIFIED), for the HAL that refuses an explicit request but is natively at the rate
/// we wanted; [`arm`] still holds it to the session's rate, so it can only ever rescue, never
/// mislabel.
///
/// **What is deliberately NOT here: a rung at any rate but the session's.** A 48 kHz rung would
/// open on almost any device — and then the wire carries the resolved rate's frames, which that
/// stream would play at the wrong speed, or we would resample them behind the user's back, which is
/// exactly what §9 forbids ("say so and fall back, not resample quietly"). Mid-session the plane
/// cannot be renegotiated either — the host never switches tags under a client whose device is
/// already open (§6). So the fallback that keeps such a device in audio has to happen BEFORE the
/// `Hello`, and it does: [`output_rate_is_openable`] downgrades the REQUEST so this session is
/// never at an unplayable rate in the first place.
///
/// **Overrides.** `debug.punktfunk.audio_sharing` (`exclusive`|`shared`) and
/// `debug.punktfunk.audio_perf` (`lowlatency`|`none`) pin the ladder to one sharing/performance
/// mode, so a device that reports no audio can be bisected with `adb shell setprop` instead of a
/// rebuild — the same reasoning as `debug.punktfunk.no_av_sync`. There is deliberately no rate
/// override: a pinned rate that disagreed with the wire would produce the mislabelled playback the
/// whole design is written to prevent, and it is not a knob a field tester could use safely.
fn open_ladder(is_tv: bool, fmt: SessionAudio) -> Vec<OpenRung> {
    use AudioPerformanceMode::{LowLatency, None as PerfNone};
    use AudioSharingMode::{Exclusive, Shared};
    let mut modes: Vec<(AudioSharingMode, AudioPerformanceMode)> =
        match sysprop(c"debug.punktfunk.audio_sharing").as_deref() {
            Some("exclusive") => vec![(Exclusive, LowLatency), (Exclusive, PerfNone)],
            Some("shared") => vec![(Shared, LowLatency), (Shared, PerfNone)],
            _ if is_tv => vec![(Shared, LowLatency), (Shared, PerfNone)],
            _ => vec![
                (Exclusive, LowLatency),
                (Shared, LowLatency),
                (Shared, PerfNone),
            ],
        };
    // Not every device honours LowLatency (it is a request, like everything else on the builder),
    // and a HAL that mishandles it is exactly the sort we are laddering around — so `none` has to
    // be reachable as a forced choice, not only as the last rung.
    match sysprop(c"debug.punktfunk.audio_perf").as_deref() {
        Some("none") => modes.iter_mut().for_each(|m| m.1 = PerfNone),
        Some("lowlatency") | Some("low") => modes.iter_mut().for_each(|m| m.1 = LowLatency),
        _ => {}
    }
    modes.dedup();
    let mut rungs = Vec::with_capacity(modes.len() * 2);
    for rate in [Some(fmt.rate_hz as i32), None] {
        for &(sharing, perf) in &modes {
            rungs.push(OpenRung {
                sharing,
                perf,
                rate,
            });
        }
    }
    rungs
}

/// Everything an open attempt needs that does not vary between rungs.
struct OpenCtx<'a> {
    /// The session's resolved format — what the ring is sized in and what [`arm`] holds an
    /// unspecified-rate rung to.
    fmt: SessionAudio,
    tuning: punktfunk_core::audio::JitterTuning,
    hard_cap_max: usize,
    game_audio: bool,
    counters: &'a Arc<Counters>,
    sync: &'a Arc<punktfunk_core::audio::AudioSyncCell>,
    /// Set by the AAudio error callback when the device disconnects — see [`supervise`].
    disconnected: &'a Arc<AtomicBool>,
}

/// Why a rung that opened could not be used.
enum ArmError {
    /// The grant did not match the request, or the start itself failed. This rung is out.
    Unusable(String),
    /// It opened and started but produced no data callback in time. Probably a device that cannot
    /// route this configuration — but possibly just a slow one, which is why this is the one
    /// failure [`open_any`] is willing to come back to.
    NotPulling,
}

/// Log the configuration a stream actually came up with.
///
/// The GRANTED modes, which need not be the ones asked for: AAudio may resolve an Exclusive
/// request to Shared and LowLatency to None, and `perf != LowLatency` means it fell to a legacy
/// path with different burst behaviour. Printing both sides is what lets a field log distinguish
/// that from plain jitter.
///
/// The RATE used to be diagnostic here too — a granted rate other than 48 000 was the tell for
/// that legacy path. It is now load-bearing instead: [`arm`] refuses any rung whose granted rate
/// is not the one the session negotiated, because playing a 96 kHz wire through a 48 kHz stream is
/// not a tuning problem, it is the wrong audio (§9). So this line can only ever print the rate the
/// session resolved — which is exactly why it still prints it: it is the field-log proof that the
/// device really opened at the rate the `Welcome` claimed.
fn log_started(live: &LiveStream, proven: bool) {
    let s = &live.stream;
    log::info!(
        "audio: AAudio started rate={} ch={} fmt={:?} perf={:?} share={:?} burst={} buf={}/{} (asked {:?}{})",
        s.sample_rate(),
        s.channel_count(),
        s.format(),
        s.performance_mode(),
        s.sharing_mode(),
        s.frames_per_burst(),
        s.buffer_size_in_frames(),
        s.buffer_capacity_in_frames(),
        live.rung,
        if proven { "" } else { ", UNPROVEN" },
    );
}

/// Walk the ladder until a rung opens, starts, and proves the device is really pulling from it.
///
/// If no rung proves itself, the first one that at least opened and started is reopened and
/// accepted anyway. The watchdog is a heuristic about a device that never calls back, and a
/// heuristic must not be able to turn working audio into no audio at all: a stream that is merely
/// slow to start would otherwise walk the whole ladder and end with the plane disabled, which is
/// strictly worse than the behaviour this function replaced. Reopened rather than held open across
/// the remaining attempts, because a started stream can itself be what makes the next rung fail.
fn open_any(ladder: &[OpenRung], ctx: &OpenCtx) -> Option<LiveStream> {
    let mut unproven: Option<OpenRung> = None;
    for rung in ladder {
        let live = match try_open(*rung, ctx) {
            Ok(live) => live,
            Err(e) => {
                log::info!("audio: open {rung:?} failed ({e}) — next rung");
                continue;
            }
        };
        match arm(&live, ctx.fmt, ctx.counters, true) {
            Ok(()) => {
                log_started(&live, true);
                return Some(live);
            }
            Err(err) => {
                match &err {
                    ArmError::Unusable(why) => {
                        log::warn!("audio: {rung:?} opened but is unusable ({why}) — next rung")
                    }
                    ArmError::NotPulling => {
                        log::warn!(
                            "audio: {rung:?} started but took no samples in {START_WATCHDOG_MS} ms — next rung"
                        );
                        unproven.get_or_insert(*rung);
                    }
                }
                // Ordered teardown before the close in `drop`: the ndk wrapper unwraps
                // AAudioStream_close's status, so hand the HAL a stopped stream.
                let _ = live.stream.request_stop();
            }
        }
    }
    let rung = unproven?;
    log::warn!(
        "audio: no rung proved it was pulling — falling back to {rung:?} unproven; if this device is silent, this line is where to look"
    );
    let live = try_open(rung, ctx).ok()?;
    match arm(&live, ctx.fmt, ctx.counters, false) {
        Ok(()) => {
            log_started(&live, false);
            Some(live)
        }
        Err(_) => {
            let _ = live.stream.request_stop();
            None
        }
    }
}

/// Bring one opened stream up and prove the device is really pulling from it.
///
/// Three things can go wrong AFTER a successful `open_stream`, and all three used to end as
/// permanent silence behind a healthy-looking log:
///
/// 1. **The grant differs from the request.** The data callback casts AAudio's buffer to `f32` and
///    writes `num_frames * channels` of them, so a stream that came back with a different layout
///    or format is not merely mis-tuned — it is an out-of-bounds write on a realtime thread.
///    The NDK contract says an explicitly-requested value is honoured or the open fails, so this
///    should be unreachable; "should be unreachable" is not a licence to trust a HAL about the
///    length of a buffer we are about to write.
/// 2. **`request_start` fails.** The old code gave up on the spot rather than trying the next rung,
///    so one grumpy configuration disabled audio for the whole session.
/// 3. **The stream starts and never calls back.** Nothing detected this, and it is the failure that
///    matters most: the decode thread cheerfully decodes into a device that will never play it,
///    every counter looks plausible, and the only symptom is silence.
///
/// ⚠ **The rate check is compared against the RUNG, not a constant, and it is no longer only a
/// memory-safety check.** Every rung used to ask for 48 kHz, so `!= 48000` could only mean a HAL
/// misbehaving. Now the session negotiates its rate, so this comparison is also the §9 rule — *a
/// client that opens its device and gets a rate other than the resolved one must say so and fall
/// back, not resample quietly* — and rejecting the rung is how it says so. A rung that asked for
/// nothing (AAUDIO_UNSPECIFIED) is held to the SESSION's rate: it exists to rescue a HAL that
/// refuses explicit requests while already running at the rate we wanted, never to accept whatever
/// the HAL felt like.
///
/// `prove_pulling` runs (3); the last-resort reopen in [`open_any`] passes `false`, having already
/// decided that an unproven stream beats no stream.
fn arm(
    live: &LiveStream,
    fmt: SessionAudio,
    counters: &Counters,
    prove_pulling: bool,
) -> Result<(), ArmError> {
    let s = &live.stream;
    let channels = fmt.channels;
    let want_rate = live.rung.rate.unwrap_or(fmt.rate_hz as i32);
    if s.channel_count() != channels as i32
        || s.sample_rate() != want_rate
        || s.format() != AudioFormat::PCM_Float
    {
        return Err(ArmError::Unusable(format!(
            "granted rate={} ch={} fmt={:?}, needed {want_rate}/{channels}/PCM_Float",
            s.sample_rate(),
            s.channel_count(),
            s.format(),
        )));
    }
    s.request_start()
        .map_err(|e| ArmError::Unusable(format!("request_start: {e}")))?;
    // Lift the AAudio HW buffer off its brittle ~2-burst LowLatency default so a single late
    // callback doesn't immediately underrun; the in-callback XRun loop grows it further if the
    // device still glitches. set_buffer_size_in_frames clamps to capacity.
    let burst = s.frames_per_burst().max(1);
    let _ = s.set_buffer_size_in_frames((burst * 3).min(s.buffer_capacity_in_frames()));
    if !prove_pulling {
        return Ok(());
    }
    let before = counters.callbacks.load(Ordering::Relaxed);
    let mut waited = 0u64;
    while counters.callbacks.load(Ordering::Relaxed) == before && waited < START_WATCHDOG_MS {
        std::thread::sleep(Duration::from_millis(START_WATCHDOG_POLL_MS));
        waited += START_WATCHDOG_POLL_MS;
    }
    if counters.callbacks.load(Ordering::Relaxed) == before {
        return Err(ArmError::NotPulling);
    }
    Ok(())
}

/// Own the audio device for the life of the session: open it, run the decode loop against it, and
/// open it again if AAudio disconnects.
///
/// **Why a supervisor.** AAudio's contract on a disconnect (an HDMI mode switch, an AVR
/// re-handshake, a headset unplugged, a route change) is that the stream is DEAD and the only
/// recovery is close + open a fresh one. This client's error callback logged a warning and did
/// nothing else, so any route change meant silence for the rest of the session while video carried
/// on untouched — and on a TV that is not a rare event, because the client itself drives an HDMI
/// mode switch on the video plane and the platform's own match-content-frame-rate setting drives
/// more.
///
/// The whole plane is rebuilt per generation rather than hot-swapping the channels under the decode
/// loop: reopening costs a few dropped packets once, and a fresh `JitterPolicy` is what you want
/// against a device that may have come back with a different burst size anyway.
fn supervise(
    client: Arc<NativeClient>,
    game_audio: bool,
    is_tv: bool,
    fmt: SessionAudio,
    shutdown: &AtomicBool,
) {
    // Fold this decode→AAudio thread into the client's hot-thread set so the ADPF session the
    // decode thread opens also keeps audio decode on a fast core (registered before the video
    // pump's first frame arrives, so it's captured when that session is created). No-op below API
    // 33. Done once for the thread, not once per generation — it is the same thread throughout.
    client.register_hot_thread();
    let tuning = punktfunk_core::audio::JitterTuning::AAUDIO;
    let counters = Arc::new(Counters::default());
    // The A/V sync hand-off: the realtime callback owns the ring (so it publishes the depth and
    // consumes the target), the decode thread owns the timestamps (so it computes the target).
    // Two atomics, because the callback must not block on the thread that decodes.
    let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
    // Either signal counts. Kotlin's `FEATURE_LEANBACK` is the authoritative one; the sysprop
    // catches a device reached through some path that did not pass the flag, and neither answering
    // simply keeps the phone ladder.
    let ladder = open_ladder(is_tv || is_tv_device(), fmt);
    // The one line that says what this session actually resolved — the `Welcome`'s answer, not the
    // request. A report of "hi-res is on but it sounds the same" is triaged from here: `codec=0`
    // means the host declined and the session is ordinary Opus, and the host's own log says why.
    log::info!(
        "audio: plane codec={} rate={} bits={} ch={} frame_us={} — open ladder {ladder:?}",
        fmt.codec,
        fmt.rate_hz,
        fmt.bits,
        fmt.channels,
        fmt.frame_us,
    );
    // An escape hatch for the reopen itself: if reopening ever turns out to fight a device (a HAL
    // that disconnects in a loop), the field can pin the old give-up-on-disconnect behaviour
    // without a rebuild rather than living with a restart storm.
    let reopen_allowed = !matches!(
        sysprop(c"debug.punktfunk.audio_reopen").as_deref(),
        Some("0") | Some("false")
    );

    let mut generation: u32 = 0;
    let mut reopen_attempt: u32 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let ctx = OpenCtx {
            fmt,
            tuning,
            // Worst transient the ring can hold before the policy trims it. Through the format's
            // own conversion rather than a samples-per-millisecond constant: the cap is a hard
            // ceiling on latency, and one computed 2.3 % shallow on a 44.1-family session would
            // trim a ring that was inside its budget.
            hard_cap_max: fmt.ms_samples(tuning.hard_cap_ms),
            game_audio,
            counters: &counters,
            sync: &sync,
            disconnected: &disconnected,
        };
        let live = match open_any(&ladder, &ctx) {
            Some(live) => {
                reopen_attempt = 0;
                live
            }
            // A reopen that lands in the middle of the very route change that caused the
            // disconnect finds no usable device and would otherwise disable audio permanently —
            // the exact outcome this supervisor exists to prevent. An HDMI mode switch or an AVR
            // re-handshake takes a beat, so keep trying across it before giving up.
            None if generation > 0 && reopen_attempt < REOPEN_ATTEMPTS => {
                reopen_attempt += 1;
                log::warn!(
                    "audio: reopen attempt {reopen_attempt}/{REOPEN_ATTEMPTS} found no usable configuration — retrying"
                );
                nap(shutdown, REOPEN_SETTLE_MS);
                continue;
            }
            None => {
                // Name the format, because at 96 kHz it is the likeliest cause and the cure is a
                // setting rather than a rebuild. It should be near-unreachable — `connect` proves
                // the rate is openable BEFORE the `Hello` (see `output_rate_is_openable`), so
                // getting here on a hi-res session means the device changed underneath one that
                // did open, and the reopen attempts above have already ridden out the settle.
                log::error!(
                    "audio: no AAudio configuration on the ladder could be opened and started at {} Hz / {} ch — audio disabled for this session (video unaffected){}",
                    fmt.rate_hz,
                    fmt.channels,
                    if fmt.rate_hz == punktfunk_core::audio::SAMPLE_RATE_HZ {
                        ""
                    } else {
                        "; this device would not give us the rate the host resolved — turn hi-res audio off in Settings to run this session at 48 kHz"
                    },
                );
                return;
            }
        };
        if generation > 0 {
            log::info!("audio: reopened after disconnect (generation {generation})");
        }
        let exit = decode_loop(
            &client,
            &live,
            shutdown,
            &disconnected,
            &counters,
            fmt,
            &sync,
        );
        let _ = live.stream.request_stop();
        drop(live); // → AAudioStream_close
        match exit {
            DecodeExit::Disconnected if reopen_allowed && !shutdown.load(Ordering::Relaxed) => {
                generation += 1;
                log::warn!("audio: device disconnected — reopening in {REOPEN_SETTLE_MS} ms");
                nap(shutdown, REOPEN_SETTLE_MS);
            }
            DecodeExit::Disconnected => {
                log::warn!("audio: device disconnected — not reopening (shutting down, or pinned by debug.punktfunk.audio_reopen)");
                break;
            }
            DecodeExit::Shutdown | DecodeExit::SessionClosed | DecodeExit::Fatal => break,
        }
    }
    log::info!(
        "audio: stopped ({}={} pcm_frames={} underruns={} generations={})",
        plane_counter_key(fmt),
        counters.frames_decoded.load(Ordering::Relaxed),
        counters.pcm_written.load(Ordering::Relaxed),
        counters.underruns.load(Ordering::Relaxed),
        generation + 1,
    );
}

/// What the decoded-frame counter is called in the log lines. Kept plane-specific rather than
/// renamed to something neutral so that the `opus=` an existing field report or triage note greps
/// for still means exactly what it always meant — and a lossless session is visibly a different
/// line rather than the same one with a surprising rate.
fn plane_counter_key(fmt: SessionAudio) -> &'static str {
    if fmt.is_pcm() {
        "pcm"
    } else {
        "opus"
    }
}

/// Sleep up to `total_ms`, in slices, giving up early once `shutdown` is set.
///
/// The supervisor's backoffs run on the same thread `AudioPlayback::drop` joins, so a plain
/// `sleep` would make closing a session wait out a reopen backoff — up to two seconds of it with
/// [`REOPEN_ATTEMPTS`] in play. Teardown latency is a user-visible thing; a settling delay is not
/// worth spending it.
fn nap(shutdown: &AtomicBool, total_ms: u64) {
    const SLICE_MS: u64 = 25;
    let mut left = total_ms;
    while left > 0 && !shutdown.load(Ordering::Relaxed) {
        let slice = left.min(SLICE_MS);
        std::thread::sleep(Duration::from_millis(slice));
        left -= slice;
    }
}

/// One open attempt at a given rung. Everything the realtime callback captures (channels, ring,
/// prime state) is rebuilt per attempt — `open_stream` consumes the builder AND the callback, so
/// nothing survives a failed try to reuse.
fn try_open(rung: OpenRung, ctx: &OpenCtx) -> ndk::audio::Result<LiveStream> {
    let OpenCtx {
        fmt,
        tuning,
        hard_cap_max,
        game_audio,
        ..
    } = *ctx;
    let channels = fmt.channels;
    let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
    // Recycle free-list: drained PCM buffers go BACK to the decode thread to be refilled, so
    // the realtime callback never frees heap (Android's Scudo allocator has unbounded free()
    // tail latency — a free on the audio thread is an XRun = a click) and the decode thread
    // rarely allocates. Same depth as the data channel.
    let (free_tx, free_rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);

    // Realtime consumer state, owned by the callback (FnMut) — no lock: AAudio calls it from
    // a single high-priority thread, and the decode thread only touches `tx`/`free_rx`.
    let cb_counters = ctx.counters.clone();
    let cb_sync = ctx.sync.clone();
    // Pre-reserve the ring so `extend` never reallocates on the realtime thread. Worst
    // transient before the trim below = the hard cap plus one full channel of the plane's OWN
    // frame — 5 ms on Opus (the protocol's fixed size, host `audio_thread`), the negotiated
    // `audio_frame_us` on `0xD3`, which at 96 kHz/24-bit stereo is 2 ms and on 24-bit surround
    // 1 ms. Sized from the resolved format — rate, depth AND channel count — rather than from a
    // 5 ms constant: at 96 kHz a 5 ms reserve is DOUBLE what it should be, which is merely
    // wasteful, but the same constant used the other way round (a plane whose frames were longer
    // than the reserve, which a surround session's are per frame even while being shorter in time)
    // would force a one-time realloc on the RT thread — asserted, not silently corrupted, in
    // `decode_loop`.
    let mut ring: VecDeque<f32> =
        VecDeque::with_capacity(hard_cap_max + RING_CHUNKS * fmt.frame_samples());
    // Shared de-jitter policy — prime depth, drift correction, de-prime hysteresis — told the
    // RESOLVED format on both axes, because it is denominated in both:
    //
    // - `new_at_rate`: every depth, target, shed threshold and the `buffer_ms`/`target_ms` this
    //   client reports are milliseconds converted to interleaved samples at the session's own rate
    //   and layout, so a 96 kHz session's figures must be in 96-sample milliseconds or every one of
    //   them is half what the tuning asked for. Core converts by multiplying first and dividing
    //   last (as `SessionAudio` does), which is why the 44.1 kHz family can be passed here at all —
    //   pre-dividing turned 44 100 Hz into 44 samples/ms and put all of them 2.3 % out.
    // - `set_frame_us`: two of its decisions are denominated in FRAMES, not milliseconds — the
    //   floor under the effective target (a device quantum plus one frame) and the smooth shed
    //   (drop exactly one frame) — and both were written when 5 ms was the only frame this
    //   protocol had. Left at the default a 96 kHz/24-bit session would shed 2.5 frames at a time
    //   and crossfade across a whole one, which is not a crossfade.
    //
    // Microseconds throughout: the ladder has sub-millisecond rungs and `audio_frame_us` is the
    // negotiated figure, so routing it through integer ms would truncate 2 500 µs to 2. An Opus
    // session passes 5 000, which is the constructor's own default — so the ordinary session is
    // bit-identical by construction rather than by a branch.
    let mut policy =
        punktfunk_core::audio::JitterPolicy::new_at_rate(tuning, channels as u8, fmt.rate_hz);
    policy.set_frame_us(fmt.frame_us);
    let mut cb_count: u32 = 0; // callbacks since open (throttles the XRun grow check)
    let mut last_xrun: i32 = 0; // last AAudio XRun count we grew the buffer for
    let callback = move |s: &AudioStream, data: *mut c_void, num_frames: i32| {
        // Proof of life for `arm`'s start watchdog, and the one counter that separates
        // "the device never pulled" from "the device pulled silence": bumped before any
        // early-out, primed or not.
        cb_counters.callbacks.fetch_add(1, Ordering::Relaxed);
        let want = num_frames as usize * channels;
        // SAFETY: AAudio provides `num_frames * channel_count` F32 slots at `data`, and
        // `arm` refused this stream unless the GRANTED channel count and format match the
        // `channels`/`PCM_Float` this cast assumes. Unchanged by the lossless plane: the
        // DEVICE format stays f32 whatever the wire depth is (see `try_open`'s builder), so
        // this cast is as sound at 24-bit as it was at 16.
        let out = unsafe { std::slice::from_raw_parts_mut(data as *mut f32, want) };
        // Drain decoded chunks into the ring WITHOUT freeing on the RT thread: `drain(..)`
        // empties each Vec but keeps its capacity, then the empty buffer is handed back for
        // reuse. The only RT-thread free is the rare case where the recycle channel is
        // momentarily full.
        while let Ok(mut chunk) = rx.try_recv() {
            ring.extend(chunk.drain(..));
            let _ = free_tx.try_send(chunk);
        }
        // A/V sync: take whatever depth the decode thread's sync loop last asked for, and
        // publish where the ring actually is so it can measure the result. The policy
        // clamps the request between its own underrun floor and the hard cap — continuity
        // outranks sync, always (see `JitterPolicy::set_sync_target`). Read AFTER the
        // drain, so the depth is everything a frame queued right now must wait behind.
        policy.set_sync_target(cb_sync.target());
        cb_sync.publish_depth(ring.len());
        // Jitter buffer: the shared policy decides prime/silence, trims a burst, and —
        // new here — sheds ONE crossfaded frame when the depth average has sat above target
        // long enough to be drift rather than jitter. Without that shed this ring had no
        // way back down: it clamped at 120 ms and stayed pinned there. "One frame" is a
        // REAL frame of this session, not a fixed 5 ms — `set_frame_us` above told the
        // policy the negotiated length, and it also caps the seam crossfade at half of it,
        // so a 2 ms lossless frame is not faded across its whole length.
        let step = policy.step(ring.len(), want);
        if step.drop_front > 0 {
            punktfunk_core::audio::crossfade_drop(&mut ring, step.drop_front, step.crossfade);
        }
        // The mirror: the sync loop asked for a DEEPER ring, answered with one duplicated,
        // crossfaded frame instead of a de-prime (see `JitterStep::insert_front`). Stays inside
        // the ring's reserve on this RT thread — `with_capacity` above leaves `RING_CHUNKS`
        // frames past the hard cap, and the policy only inserts BELOW its target.
        if step.insert_front > 0 {
            punktfunk_core::audio::crossfade_insert(&mut ring, step.insert_front, step.crossfade);
            cb_counters.inserts.fetch_add(1, Ordering::Relaxed);
        }
        let mut ran_short = false;
        if !step.silence {
            for slot in out.iter_mut() {
                *slot = ring.pop_front().unwrap_or_else(|| {
                    ran_short = true;
                    0.0
                });
            }
            cb_counters
                .pcm_written
                .fetch_add(num_frames as u64, Ordering::Relaxed);
        } else {
            out.fill(0.0);
            cb_counters.underruns.fetch_add(1, Ordering::Relaxed);
        }
        // No-op while un-primed, so a deliberate priming silence is never counted as an
        // underrun (which would otherwise drive the adaptive floor up for no reason).
        policy.note_read(ran_short);
        cb_counters
            .target_ms
            .store(policy.target_ms() as u64, Ordering::Relaxed);
        // Google's AAudio anti-glitch technique: when the device reports new XRuns, grow the
        // HW buffer by one burst (up to capacity). getXRunCount + setBufferSizeInFrames are
        // both callback-safe / non-blocking, and set clamps to capacity so it self-limits.
        // Throttled.
        cb_count = cb_count.wrapping_add(1);
        if cb_count % XRUN_CHECK_EVERY == 0 {
            let xr = s.x_run_count();
            if xr > last_xrun {
                last_xrun = xr;
                let burst = s.frames_per_burst().max(1);
                let grown = (s.buffer_size_in_frames() + burst).min(s.buffer_capacity_in_frames());
                let _ = s.set_buffer_size_in_frames(grown);
            }
        }
        AudioCallbackResult::Continue
    };

    let builder = AudioStreamBuilder::new()?
        .direction(AudioDirection::Output)
        // The wire order (FL FR FC LFE RL RR SL SR) is the standard AAudio/Android channel
        // order, so this is an IDENTITY mapping — no permute. AAudio infers the 5.1/7.1 mask
        // from `channel_count` (the ndk crate's builder exposes no setChannelMask); the host
        // captures + encodes in exactly this order.
        .channel_count(channels as i32)
        // ⚠ The DEVICE format is f32 on BOTH planes, deliberately — this is not an oversight
        // left over from the Opus-only era. Core decodes each plane to interleaved f32 (libopus
        // `decode_float`; `pcm::to_f32` normalises 16/24-bit codes by their full scale), so a
        // 24-bit session already arrives as floats and asking AAudio for PCM_I24_PACKED would
        // mean quantising them BACK — a second rounding, of the very samples the plane exists to
        // deliver unrounded. It would also be unreachable here: that format is API 31, above this
        // client's minSdk-28 floor. The wire depth is a WIRE fact; it never reaches the HAL.
        .format(AudioFormat::PCM_Float);
    // The rate is per-rung: an explicit request (honoured or the open fails — AAudio never
    // substitutes silently) or, on the last rate rung, nothing at all, letting the HAL name its
    // own. `arm` holds an unspecified rung to the session's rate afterwards, so "let AAudio
    // choose" can rescue a stubborn HAL but can never quietly change what we are playing.
    let builder = match rung.rate {
        Some(hz) => builder.sample_rate(hz),
        None => builder,
    };
    // Tag the stream as game audio (usage=Game / content=Movie): the audio HAL applies
    // its low-latency game-audio routing/policy and it's grouped correctly with the
    // game-mode profile. Advisory — ignored where the device has no such policy. Part of
    // the experimental low-latency stack; off, the stream stays untagged.
    let builder = if game_audio {
        builder
            .usage(AudioUsage::Game)
            .content_type(AudioContentType::Movie)
    } else {
        builder
    };
    // AAudio calls the error callback on its own thread (never the realtime one), and its
    // contract is that the stream is finished: the ONLY recovery is close + open a new
    // one, which is what setting this flag asks `supervise` to do. Doing it here would
    // deadlock — you may not close a stream from inside its own callbacks.
    let cb_disconnected = ctx.disconnected.clone();
    let stream = builder
        .performance_mode(rung.perf)
        .sharing_mode(rung.sharing)
        .data_callback(Box::new(callback))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("audio: AAudio error (device reroute/disconnect?): {e:?}");
            cb_disconnected.store(true, Ordering::SeqCst);
        }))
        .open_stream()?;
    Ok(LiveStream {
        stream,
        tx,
        free_rx,
        rung,
    })
}

/// Producer: `next_audio` → decode (libopus, or a PCM stride unpack) → push interleaved f32 into
/// the ring channel. Buffers come from (and return to) the realtime callback's recycle free-list so
/// the steady state is allocation-free on both threads.
///
/// Runs on the supervisor's thread and returns when the session ends, the playback is dropped, or
/// the device disconnects — [`DecodeExit`] says which, because only one of them is worth reopening
/// the device for.
fn decode_loop(
    client: &Arc<NativeClient>,
    live: &LiveStream,
    shutdown: &AtomicBool,
    disconnected: &AtomicBool,
    counters: &Counters,
    fmt: SessionAudio,
    sync: &punktfunk_core::audio::AudioSyncCell,
) -> DecodeExit {
    let tx = &live.tx;
    let free_rx = &live.free_rx;
    let channels = fmt.channels;
    // Decode scratch, sized for the LARGEST frame the running plane can hand us — and only for
    // that plane, because they differ by more than an order of magnitude:
    //
    // - Opus: libopus's largest legal frame is 120 ms (5760 samples/ch), and it is always 48 kHz.
    // - `0xD3`: the longest rung of `pcm::FRAME_US_LADDER` at the negotiated rate. The frame
    //   duration is chosen from the path MTU at session start and this plane is never fragmented,
    //   so nothing longer can arrive from a conforming host.
    //
    // Sizing BOTH from the Opus worst case (the `5760 * channels` this replaces) would have been
    // 24× too big at 96 kHz, and — the reason the task called it out — sizing both from the PCM
    // one would be far too SMALL for Opus. Either way the copies into it are clamped, so a
    // non-conforming host truncates a frame rather than overrunning the decode thread's buffer.
    let scratch_samples = if fmt.is_pcm() {
        punktfunk_core::audio::pcm::samples_per_frame(
            fmt.rate_hz,
            punktfunk_core::audio::pcm::FRAME_US_LADDER[0],
            channels as u8,
        )
    } else {
        5760 * channels
    };
    let mut dec = match AudioDec::new(fmt) {
        Ok(d) => d,
        Err(e) => {
            log::error!(
                "audio: decoder init for codec={} rate={} ch={}: {e} — audio disabled",
                fmt.codec,
                fmt.rate_hz,
                channels,
            );
            return DecodeExit::Fatal;
        }
    };
    let mut pcm = vec![0f32; scratch_samples];
    let mut window_peak = 0f32; // loudest |sample| since the last log — tells a tone from silence
    let mut gaps = punktfunk_core::audio::AudioGapTracker::new();
    // The third thing in this loop denominated in FRAMES that had to be told what a frame is — the
    // same shape as `JitterPolicy::set_frame_us` and `DroughtConceal::new_at_frame_us` below.
    // `MAX_CONCEAL_MS` caps a single loss event at 50 ms of synthesized audio, and it derives the
    // packet count from this: left at the protocol's 5 ms it would be ten packets, which on a 2 ms
    // lossless frame is 20 ms — the cap tightening by two and a half times on precisely the
    // sessions whose packet rate went UP. Identical on every Opus session, which passes 5 000.
    gaps.set_frame_us(fmt.frame_us);
    let mut frame_samples = 0usize; // per-channel samples of the last decoded frame — the PLC unit
                                    // WP-C1 — the drought half of concealment. The loop below already conceals a SEQ GAP, but only
                                    // when a later packet arrives to reveal it; when the wire simply goes quiet — Wi-Fi power-save
                                    // bunching, the shape this preset already runs deeper for — nothing arrives to reveal anything
                                    // and the ring drains into an underrun and a de-prime whose re-prime is a longer artifact than
                                    // the audio that was missing.
                                    // Told the plane's real frame, so its wall-clock fuse and its `plc_ms` are spent at the rate
                                    // this session actually paces. It used to assume 5 ms, which on a 2 ms lossless frame blew the
                                    // fuse after two fifths of the time the tuning intends.
    let mut drought = punktfunk_core::audio::DroughtConceal::new_at_frame_us(
        punktfunk_core::audio::JitterTuning::AAUDIO.plc_max_ms(),
        fmt.frame_us,
    );
    let mut last_packet = std::time::Instant::now();

    // A/V sync (audio latency overhaul). This thread is the only place holding all three
    // ingredients at once: the packet's host capture `pts_ns`, the ring depth (via the sync cell)
    // and the video plane's end-to-end figure. `pts_ns` arrived in every `AudioPacket` and was
    // dropped on the floor here for the plane's whole existence, which is why audio ran at whatever
    // depth its jitter ring settled at with nothing ever placing it against the picture.
    let av_sync_enabled = av_sync_enabled();
    // At the RESOLVED rate, for the same reason `JitterPolicy` is: this type's proposal is
    // denominated in the ring's own samples-per-millisecond, so the two have to agree about what a
    // millisecond is or every correction is out by the rate ratio.
    let mut av = punktfunk_core::audio::AvSync::new_at_rate(channels as u8, fmt.rate_hz);
    let video_e2e = client.video_e2e_shared();
    let av_offset_out = client.audio_av_offset_shared();
    let buffer_ms_out = client.audio_buffer_ms_shared();
    if !av_sync_enabled {
        log::info!("audio: A/V sync disabled (PUNKTFUNK_NO_AV_SYNC / debug.punktfunk.no_av_sync)");
    }
    // One tick = one frame of THIS plane, which is what makes the drought arm below conceal at the
    // rate the callback drains at rather than racing it or falling behind. 5 ms on Opus (unchanged);
    // as little as 1 ms on a `0xD3` session at the short end of the ladder, where a 5 ms tick would
    // have synthesized one frame of cover per five owed and lost ground for the whole drought. Also
    // the poll period
    // for both exit flags, so a disconnect is still noticed within one packet time on a silent link.
    let tick = Duration::from_micros(fmt.frame_us.max(1) as u64);
    // The dead-stream warning is a DURATION, not a packet count — see `DEAD_STREAM_WARN_MS`.
    let dead_stream_warn_packets = (DEAD_STREAM_WARN_MS * 1000 / fmt.frame_us.max(1) as u64).max(1);
    while !shutdown.load(Ordering::Relaxed) {
        if disconnected.load(Ordering::Relaxed) {
            return DecodeExit::Disconnected;
        }
        match client.next_audio(tick) {
            Ok(pkt) => {
                // Place this frame against the picture it belongs with, BEFORE it is queued:
                // `buffered_ahead` is everything that must still play first, so the depth read here
                // is exactly what delays it.
                let depth = sync.depth();
                // Published unconditionally — the ring's depth is worth seeing even with sync off,
                // and it is what makes a "the audio delay is way too high" report triageable at all.
                // Converted through the format, never by a samples-per-millisecond divisor: at
                // 44 100 Hz that divisor is 44.1 and an integer one reported this 2.3 % DEEP, which
                // is the one direction that makes a healthy ring look like the reported fault.
                buffer_ms_out.store(fmt.samples_ms(depth), Ordering::Relaxed);
                if av_sync_enabled {
                    let ve2e = video_e2e.load(Ordering::Relaxed);
                    av.observe(punktfunk_core::audio::AvSyncObservation {
                        pts_ns: pkt.pts_ns,
                        now_local_ns: punktfunk_core::client::now_realtime_ns(),
                        clock_offset_ns: client.clock_offset_now_ns(),
                        buffered_ahead: depth,
                        // 0 = nothing confirmed on the glass yet (no render callback below API 33,
                        // or the stream has not presented a frame); no reference, no correction.
                        video_e2e_ns: (ve2e > 0).then_some(ve2e),
                    });
                    sync.set_target(av.desired_depth(depth));
                    av_offset_out.store(av.offset_ms() as i64, Ordering::Relaxed);
                }
                last_packet = std::time::Instant::now();
                // Anything the drought path already covered is audio the stream now has;
                // concealing it a second time here would insert samples it never carried and push
                // everything after them later.
                let already = drought.packet();
                // Conceal lost packets (a seq gap) before decoding the one that arrived: one
                // synthesized frame per missing packet — an inaudible fade instead of the click a
                // hard gap makes in the ring. libopus interpolates from its own state on `0xC9`;
                // `0xD3` has none to interpolate from, so `PcmConceal` repeats-and-fades and
                // decays a run to silence (§4.5). `AudioDec::conceal` hides which.
                for _ in 0..gaps.missing_before(pkt.seq).saturating_sub(already) {
                    if frame_samples == 0 {
                        break; // no decoded frame yet to size the concealment from
                    }
                    match dec.conceal(&mut pcm, frame_samples, channels) {
                        Ok(0) => break, // nothing to build from — let the ring carry the gap
                        Ok(samples) => {
                            let mut buf = free_rx
                                .try_recv()
                                .unwrap_or_else(|_| Vec::with_capacity(scratch_samples));
                            buf.clear();
                            buf.extend_from_slice(&pcm[..samples * channels]);
                            match tx.try_send(buf) {
                                Ok(()) | Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Disconnected(_)) => return DecodeExit::Shutdown,
                            }
                        }
                        Err(_) => break,
                    }
                }
                match dec.decode_float(&pkt.data, &mut pcm, channels) {
                    Ok(samples) => {
                        frame_samples = samples;
                        let n = samples * channels;
                        for &s in &pcm[..n] {
                            window_peak = window_peak.max(s.abs());
                        }
                        // The ring's pre-reservation in `try_open` is one frame of THIS plane per
                        // queued chunk; a larger frame would force a one-time realloc on the RT
                        // thread. Catch a host that changed its frame size — or a `Welcome` whose
                        // `audio_frame_us` disagrees with what it then sends — here in debug,
                        // rather than as a silent audio glitch.
                        debug_assert!(
                            n <= fmt.frame_samples(),
                            "audio frame {n} f32 exceeds the {} f32 ring reserve ({} µs at {} Hz)",
                            fmt.frame_samples(),
                            fmt.frame_us,
                            fmt.rate_hz,
                        );
                        let count = counters.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                        // Reuse a recycled buffer if the callback handed one back; only allocate when the
                        // free-list is momentarily empty (startup / after a backpressure drop).
                        let mut buf = free_rx
                            .try_recv()
                            .unwrap_or_else(|_| Vec::with_capacity(scratch_samples));
                        buf.clear();
                        buf.extend_from_slice(&pcm[..n]);
                        match tx.try_send(buf) {
                            Ok(()) | Err(TrySendError::Full(_)) => {} // drop-newest under backpressure
                            Err(TrySendError::Disconnected(_)) => return DecodeExit::Shutdown,
                        }
                        // The fingerprint of a stream that opened into a device which is not
                        // playing: we are decoding steadily and AAudio has never taken a single
                        // sample. `arm`'s watchdog catches it at open, so reaching this means the
                        // device stopped pulling AFTER it started — say so loudly, because from
                        // the outside it is indistinguishable from "the app has no sound".
                        if count == dead_stream_warn_packets
                            && counters.pcm_written.load(Ordering::Relaxed) == 0
                        {
                            log::error!(
                                "audio: {count} frames decoded ({DEAD_STREAM_WARN_MS} ms) but AAudio has not taken one sample — {:?} opened into a device that is not playing",
                                live.rung,
                            );
                        }
                        if count % 600 == 0 {
                            // `av_ms` is the sync loop's smoothed placement error (+ = audio behind
                            // the picture); 0 with sync off, or before it has a video reference.
                            // Logged next to the depth because a deep ring on a jittery link is
                            // correct and only the offset separates that from audio held late.
                            // `plc_ms` is concealment synthesized for packet droughts: a healthy
                            // `underruns` bought with a climbing `plc_ms` is a link in trouble,
                            // not a link that is fine.
                            log::info!(
                                "audio: {}={count} pcm_frames={} underruns={} buffer_ms={} target_ms={} av_ms={} plc_ms={} drift_inserts={} peak={window_peak:.3}",
                                plane_counter_key(fmt),
                                counters.pcm_written.load(Ordering::Relaxed),
                                counters.underruns.load(Ordering::Relaxed),
                                fmt.samples_ms(depth),
                                counters.target_ms.load(Ordering::Relaxed),
                                av.offset_ms(),
                                drought.total_ms(),
                                counters.inserts.load(Ordering::Relaxed),
                            );
                            window_peak = 0.0;
                        }
                    }
                    Err(e) => log::debug!("audio: decode: {e}"),
                }
            }
            Err(PunktfunkError::NoFrame) => {
                // Nothing on the wire. If the ring is draining with it, conceal — the same
                // synthesis the loss path uses, bounded by this preset's de-prime fuse so a
                // genuinely dead stream is not papered over. ONE frame per tick, not a burst:
                // this arm fires once per `tick`, which is one frame of this plane and therefore
                // the rate the callback drains at, so concealment keeps pace with playout instead
                // of racing ahead of a depth reading it has already invalidated. `frame_samples`
                // is 0 until something has decoded — there is no state to extrapolate from
                // before then.
                let depth_ms = fmt.samples_ms(sync.depth());
                if frame_samples > 0 && drought.conceal(last_packet.elapsed(), depth_ms) {
                    if let Ok(samples @ 1..) = dec.conceal(&mut pcm, frame_samples, channels) {
                        let mut buf = free_rx
                            .try_recv()
                            .unwrap_or_else(|_| Vec::with_capacity(scratch_samples));
                        buf.clear();
                        buf.extend_from_slice(&pcm[..samples * channels]);
                        match tx.try_send(buf) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => return DecodeExit::Shutdown,
                        }
                    }
                    sync.publish_plc_ms(drought.total_ms());
                }
            }
            Err(_) => return DecodeExit::SessionClosed,
        }
    }
    DecodeExit::Shutdown
}
