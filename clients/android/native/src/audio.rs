//! Android audio playback (android-only): pull Opus packets from the connector, decode to
//! interleaved f32 (stereo or 5.1/7.1 surround), and feed AAudio via its realtime data callback
//! through a jitter ring. Mirrors [`crate::decode`]: one thread we own (the Opus decode producer)
//! plus a shutdown flag; the realtime callback thread is owned by AAudio.
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
//! The layout is the host-RESOLVED channel count (`NativeClient::audio_channels`, negotiated at
//! connect), so an older/clamping host that can only capture stereo is decoded + played as stereo.
//! 2 = stereo / 6 = 5.1 / 8 = 7.1, in the canonical wire order FL FR FC LFE RL RR SL SR.
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

/// One rung of the AAudio open ladder — a sharing mode and a performance mode tried together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenRung {
    sharing: AudioSharingMode,
    perf: AudioPerformanceMode,
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
    /// The plane cannot run at all (the Opus decoder would not build). Reopening the DEVICE would
    /// not change that, so it is not a reason to walk the ladder again.
    Fatal,
}

const SAMPLE_RATE: i32 = 48_000;
/// Decoded-chunk hand-off depth: 64 × 5 ms = 320 ms slack (matches the core's AUDIO_QUEUE).
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
/// Opus packets decoded with AAudio never having taken a single sample before we call it: ~1 s at
/// the protocol's 5 ms frames.
const DEAD_STREAM_WARN_PACKETS: u64 = 200;

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

/// Opus decoder for the audio plane: a plain stereo decoder (the validated path) or a multistream
/// decoder for 5.1/7.1, both behind one `decode_float`. Built from the host-RESOLVED channel count
/// via the shared layout table. Mirrors the Linux client's `AudioDec`.
enum AudioDec {
    Stereo(opus::Decoder),
    Surround(opus::MSDecoder),
}

impl AudioDec {
    fn new(channels: u8) -> Result<AudioDec, opus::Error> {
        if channels == 2 {
            Ok(AudioDec::Stereo(opus::Decoder::new(
                SAMPLE_RATE as u32,
                opus::Channels::Stereo,
            )?))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            Ok(AudioDec::Surround(opus::MSDecoder::new(
                SAMPLE_RATE as u32,
                l.streams,
                l.coupled,
                l.mapping,
            )?))
        }
    }

    fn decode_float(
        &mut self,
        input: &[u8],
        out: &mut [f32],
        fec: bool,
    ) -> Result<usize, opus::Error> {
        match self {
            AudioDec::Stereo(d) => d.decode_float(input, out, fec),
            AudioDec::Surround(d) => d.decode_float(input, out, fec),
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
    opus_decoded: AtomicU64, // Opus packets decoded OK (~200/s at 5 ms frames)
    pcm_written: AtomicU64,  // PCM frames copied out to AAudio (device clock is pulling)
    underruns: AtomicU64,    // callbacks that emitted silence (ring not primed / drained)
    target_ms: AtomicU64,    // the policy's LIVE target depth (it grows on this device's underruns)
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
/// and the Opus decode loop for as long as the session lives.
pub struct AudioPlayback {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayback {
    /// Spawn the audio supervisor: it opens AAudio (48 kHz/f32, the host-resolved channel layout)
    /// by walking [`open_ladder`], runs the Opus decode loop against it, and reopens it if the
    /// device disconnects. `None` only if the thread itself could not be spawned — an open failure
    /// is reported by the supervisor (the caller leaves video streaming either way).
    ///
    /// `game_audio` (the experimental low-latency mode) tags the stream usage=Game for the HAL's
    /// game-audio routing; off, the stream is untagged as it was before the overhaul. `is_tv` is
    /// Kotlin's `FEATURE_LEANBACK` and steers the ladder — see [`open_ladder`].
    pub fn start(
        client: Arc<NativeClient>,
        game_audio: bool,
        is_tv: bool,
    ) -> Option<AudioPlayback> {
        // Build playback from the host-RESOLVED channel count (never the request): 2 = stereo /
        // 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR.
        let channels = punktfunk_core::audio::normalize_channels(client.audio_channels) as usize;
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("pf-audio".into())
            .spawn(move || supervise(client, game_audio, is_tv, channels, &sd))
            .ok()?;
        Some(AudioPlayback {
            shutdown,
            join: Some(join),
        })
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
/// **Overrides.** `debug.punktfunk.audio_sharing` (`exclusive`|`shared`) and
/// `debug.punktfunk.audio_perf` (`lowlatency`|`none`) pin the ladder to one sharing/performance
/// mode, so a device that reports no audio can be bisected with `adb shell setprop` instead of a
/// rebuild — the same reasoning as `debug.punktfunk.no_av_sync`.
fn open_ladder(is_tv: bool) -> Vec<OpenRung> {
    use AudioPerformanceMode::{LowLatency, None as PerfNone};
    use AudioSharingMode::{Exclusive, Shared};
    let mut rungs = match sysprop(c"debug.punktfunk.audio_sharing").as_deref() {
        Some("exclusive") => vec![
            OpenRung {
                sharing: Exclusive,
                perf: LowLatency,
            },
            OpenRung {
                sharing: Exclusive,
                perf: PerfNone,
            },
        ],
        Some("shared") => vec![
            OpenRung {
                sharing: Shared,
                perf: LowLatency,
            },
            OpenRung {
                sharing: Shared,
                perf: PerfNone,
            },
        ],
        _ if is_tv => vec![
            OpenRung {
                sharing: Shared,
                perf: LowLatency,
            },
            OpenRung {
                sharing: Shared,
                perf: PerfNone,
            },
        ],
        _ => vec![
            OpenRung {
                sharing: Exclusive,
                perf: LowLatency,
            },
            OpenRung {
                sharing: Shared,
                perf: LowLatency,
            },
            OpenRung {
                sharing: Shared,
                perf: PerfNone,
            },
        ],
    };
    // Not every device honours LowLatency (it is a request, like everything else on the builder),
    // and a HAL that mishandles it is exactly the sort we are laddering around — so `none` has to
    // be reachable as a forced choice, not only as the last rung.
    match sysprop(c"debug.punktfunk.audio_perf").as_deref() {
        Some("none") => rungs.iter_mut().for_each(|r| r.perf = PerfNone),
        Some("lowlatency") | Some("low") => rungs.iter_mut().for_each(|r| r.perf = LowLatency),
        _ => {}
    }
    rungs.dedup();
    rungs
}

/// Everything an open attempt needs that does not vary between rungs.
struct OpenCtx<'a> {
    channels: usize,
    ms: usize,
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
/// request to Shared and LowLatency to None, and `rate != 48000` or `perf != LowLatency` means it
/// quietly fell to a resampled legacy path with different burst behaviour. Printing both sides is
/// what lets a field log distinguish that from plain jitter.
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
        match arm(&live, ctx.channels, ctx.counters, true) {
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
    match arm(&live, ctx.channels, ctx.counters, false) {
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
///    writes `num_frames * channels` of them, so a stream that came back with a different layout,
///    rate or format is not merely mis-tuned — it is an out-of-bounds write on a realtime thread.
///    The NDK contract says an explicitly-requested value is honoured or the open fails, so this
///    should be unreachable; "should be unreachable" is not a licence to trust a HAL about the
///    length of a buffer we are about to write.
/// 2. **`request_start` fails.** The old code gave up on the spot rather than trying the next rung,
///    so one grumpy configuration disabled audio for the whole session.
/// 3. **The stream starts and never calls back.** Nothing detected this, and it is the failure that
///    matters most: the decode thread cheerfully decodes Opus into a device that will never play
///    it, every counter looks plausible, and the only symptom is silence.
///
/// `prove_pulling` runs (3); the last-resort reopen in [`open_any`] passes `false`, having already
/// decided that an unproven stream beats no stream.
fn arm(
    live: &LiveStream,
    channels: usize,
    counters: &Counters,
    prove_pulling: bool,
) -> Result<(), ArmError> {
    let s = &live.stream;
    if s.channel_count() != channels as i32
        || s.sample_rate() != SAMPLE_RATE
        || s.format() != AudioFormat::PCM_Float
    {
        return Err(ArmError::Unusable(format!(
            "granted rate={} ch={} fmt={:?}, asked {SAMPLE_RATE}/{channels}/PCM_Float",
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
    channels: usize,
    shutdown: &AtomicBool,
) {
    // Fold this Opus→AAudio thread into the client's hot-thread set so the ADPF session the decode
    // thread opens also keeps audio decode on a fast core (registered before the video pump's first
    // frame arrives, so it's captured when that session is created). No-op below API 33. Done once
    // for the thread, not once per generation — it is the same thread throughout.
    client.register_hot_thread();
    // Interleaved f32 samples per millisecond at this layout (48 kHz × channels); the ms-
    // denominated jitter-ring depths scale by it.
    let ms = (SAMPLE_RATE as usize / 1000) * channels;
    let tuning = punktfunk_core::audio::JitterTuning::AAUDIO;
    let counters = Arc::new(Counters::default());
    // The A/V sync hand-off: the realtime callback owns the ring (so it publishes the depth and
    // consumes the target), the decode thread owns the timestamps (so it computes the target).
    // Two atomics, because the callback must not block on the thread that decodes Opus.
    let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
    // Either signal counts. Kotlin's `FEATURE_LEANBACK` is the authoritative one; the sysprop
    // catches a device reached through some path that did not pass the flag, and neither answering
    // simply keeps the phone ladder.
    let ladder = open_ladder(is_tv || is_tv_device());
    log::info!("audio: open ladder {ladder:?}");
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
            channels,
            ms,
            tuning,
            // Worst transient the ring can hold before the policy trims it.
            hard_cap_max: tuning.hard_cap_ms as usize * ms,
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
                log::error!(
                    "audio: no AAudio configuration on the ladder could be opened and started — audio disabled for this session (video unaffected)"
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
            channels,
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
        "audio: stopped (opus={} pcm_frames={} underruns={} generations={})",
        counters.opus_decoded.load(Ordering::Relaxed),
        counters.pcm_written.load(Ordering::Relaxed),
        counters.underruns.load(Ordering::Relaxed),
        generation + 1,
    );
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
        channels,
        ms,
        tuning,
        hard_cap_max,
        game_audio,
        ..
    } = *ctx;
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
    // transient before the trim below = the hard cap plus one full channel of 5 ms (480-f32)
    // frames — the punktfunk protocol always sends 5 ms Opus frames (host `audio_thread`); a
    // larger frame would force a one-time realloc, asserted (not silently corrupted) in
    // `decode_loop`.
    let mut ring: VecDeque<f32> = VecDeque::with_capacity(hard_cap_max + RING_CHUNKS * 5 * ms);
    // Shared de-jitter policy — prime depth, drift correction, de-prime hysteresis. The
    // hysteresis this replaces was Android-only; Linux and Windows carried the instant
    // `if ring.is_empty()` re-prime until now.
    let mut policy = punktfunk_core::audio::JitterPolicy::new(tuning, channels as u8);
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
        // `channels`/`PCM_Float` this cast assumes.
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
        // new here — sheds ONE crossfaded 5 ms frame when the depth average has sat above
        // target long enough to be drift rather than jitter. Without that shed this ring
        // had no way back down: it clamped at 120 ms and stayed pinned there.
        let step = policy.step(ring.len(), want);
        if step.drop_front > 0 {
            punktfunk_core::audio::crossfade_drop(&mut ring, step.drop_front, step.crossfade);
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
        .sample_rate(SAMPLE_RATE)
        // The wire order (FL FR FC LFE RL RR SL SR) is the standard AAudio/Android channel
        // order, so this is an IDENTITY mapping — no permute. AAudio infers the 5.1/7.1 mask
        // from `channel_count` (the ndk crate's builder exposes no setChannelMask); the host
        // captures + Opus-encodes in exactly this order.
        .channel_count(channels as i32)
        .format(AudioFormat::PCM_Float);
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

/// Producer: `next_audio` → Opus `decode_float` → push interleaved f32 into the ring channel.
/// Buffers come from (and return to) the realtime callback's recycle free-list so the steady state
/// is allocation-free on both threads.
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
    channels: usize,
    sync: &punktfunk_core::audio::AudioSyncCell,
) -> DecodeExit {
    let tx = &live.tx;
    let free_rx = &live.free_rx;
    // Interleaved f32 samples per millisecond at this layout — the ring's 5 ms reserve check below.
    let ms = (SAMPLE_RATE as usize / 1000) * channels;
    // Opus decode scratch: worst-case 120 ms frame (5760 samples/ch) × channels.
    let pcm_scratch = 5760 * channels;
    let mut dec = match AudioDec::new(channels as u8) {
        Ok(d) => d,
        Err(e) => {
            log::error!("audio: opus decoder init: {e} — audio disabled");
            return DecodeExit::Fatal;
        }
    };
    let mut pcm = vec![0f32; pcm_scratch];
    let mut window_peak = 0f32; // loudest |sample| since the last log — tells a tone from silence
    let mut gaps = punktfunk_core::audio::AudioGapTracker::new();
    let mut frame_samples = 0usize; // per-channel samples of the last decoded frame — the PLC unit

    // A/V sync (audio latency overhaul). This thread is the only place holding all three
    // ingredients at once: the packet's host capture `pts_ns`, the ring depth (via the sync cell)
    // and the video plane's end-to-end figure. `pts_ns` arrived in every `AudioPacket` and was
    // dropped on the floor here for the plane's whole existence, which is why audio ran at whatever
    // depth its jitter ring settled at with nothing ever placing it against the picture.
    let av_sync_enabled = av_sync_enabled();
    let mut av = punktfunk_core::audio::AvSync::new(channels as u8);
    let video_e2e = client.video_e2e_shared();
    let av_offset_out = client.audio_av_offset_shared();
    let buffer_ms_out = client.audio_buffer_ms_shared();
    if !av_sync_enabled {
        log::info!("audio: A/V sync disabled (PUNKTFUNK_NO_AV_SYNC / debug.punktfunk.no_av_sync)");
    }
    // Both flags are polled at the 5 ms `next_audio` timeout, so a disconnect is noticed within one
    // packet time even on a silent link.
    while !shutdown.load(Ordering::Relaxed) {
        if disconnected.load(Ordering::Relaxed) {
            return DecodeExit::Disconnected;
        }
        match client.next_audio(Duration::from_millis(5)) {
            Ok(pkt) => {
                // Place this frame against the picture it belongs with, BEFORE it is queued:
                // `buffered_ahead` is everything that must still play first, so the depth read here
                // is exactly what delays it.
                let depth = sync.depth();
                // Published unconditionally — the ring's depth is worth seeing even with sync off,
                // and it is what makes a "the audio delay is way too high" report triageable at all.
                buffer_ms_out.store((depth / ms.max(1)) as u32, Ordering::Relaxed);
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
                // Conceal lost packets (a seq gap) with libopus PLC before decoding the one that
                // arrived: empty input synthesizes `frame_samples` of interpolation per missing
                // packet — an inaudible fade instead of the click a hard gap makes in the ring.
                for _ in 0..gaps.missing_before(pkt.seq) {
                    let plc = frame_samples * channels;
                    if plc == 0 {
                        break; // no decoded frame yet to size the concealment from
                    }
                    if let Ok(samples) = dec.decode_float(&[], &mut pcm[..plc], false) {
                        let mut buf = free_rx
                            .try_recv()
                            .unwrap_or_else(|_| Vec::with_capacity(pcm_scratch));
                        buf.clear();
                        buf.extend_from_slice(&pcm[..samples * channels]);
                        match tx.try_send(buf) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => return DecodeExit::Shutdown,
                        }
                    }
                }
                match dec.decode_float(&pkt.data, &mut pcm, false) {
                    Ok(samples) => {
                        frame_samples = samples;
                        let n = samples * channels;
                        for &s in &pcm[..n] {
                            window_peak = window_peak.max(s.abs());
                        }
                        // The ring's pre-reservation in `start` assumes the protocol's 5 ms (≤480-f32/ch)
                        // frames; a larger frame would force a one-time realloc on the RT thread. Catch a
                        // future host frame-size change here in debug, not as a silent audio glitch.
                        debug_assert!(
                            n <= 5 * ms,
                            "audio frame {n} f32 exceeds the 5 ms ring reserve"
                        );
                        let count = counters.opus_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                        // Reuse a recycled buffer if the callback handed one back; only allocate when the
                        // free-list is momentarily empty (startup / after a backpressure drop).
                        let mut buf = free_rx
                            .try_recv()
                            .unwrap_or_else(|_| Vec::with_capacity(pcm_scratch));
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
                        if count == DEAD_STREAM_WARN_PACKETS
                            && counters.pcm_written.load(Ordering::Relaxed) == 0
                        {
                            log::error!(
                                "audio: {count} Opus packets decoded but AAudio has not taken one sample — {:?} opened into a device that is not playing",
                                live.rung,
                            );
                        }
                        if count % 600 == 0 {
                            // `av_ms` is the sync loop's smoothed placement error (+ = audio behind
                            // the picture); 0 with sync off, or before it has a video reference.
                            // Logged next to the depth because a deep ring on a jittery link is
                            // correct and only the offset separates that from audio held late.
                            log::info!(
                                "audio: opus={count} pcm_frames={} underruns={} buffer_ms={} target_ms={} av_ms={} peak={window_peak:.3}",
                                counters.pcm_written.load(Ordering::Relaxed),
                                counters.underruns.load(Ordering::Relaxed),
                                (depth / ms.max(1)) as u64,
                                counters.target_ms.load(Ordering::Relaxed),
                                av.offset_ms(),
                            );
                            window_peak = 0.0;
                        }
                    }
                    Err(e) => log::debug!("audio: opus decode: {e}"),
                }
            }
            Err(PunktfunkError::NoFrame) => {} // timeout
            Err(_) => return DecodeExit::SessionClosed,
        }
    }
    DecodeExit::Shutdown
}
