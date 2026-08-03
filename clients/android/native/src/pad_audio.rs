//! Pad audio on Android (the 0xD1 plane) — tier A, WP9.
//!
//! The Android twin of [`pf_client_core::pad_audio`]: drain the host's per-pad DualSense streams,
//! Opus-decode haptics (kind 0) and speaker (kind 1), interleave them into the pad's own
//! 4-channel layout, and render them on the physical pad.
//!
//! # Why this needs a USB driver instead of an audio API
//!
//! Every other client hands the 4-channel stream to the platform's audio graph — WASAPI on
//! Windows, PipeWire on Linux, CoreAudio on Apple. **Android has no such option for this device.**
//! AOSP's `UsbAlsaManager` carries a hardcoded VID/PID denylist that includes the DualSense
//! (`054c:0ce6`), so the kernel enumerates the pad's playback node and the framework then discards
//! it: `hasOutput: false`. There is no `AudioDeviceInfo` for `setPreferredDevice` to target, and
//! `/dev/snd` is closed to apps by SELinux. Android's own `UsbRequest` API cannot help either — it
//! rejects any endpoint that is not bulk or interrupt.
//!
//! So this path drives the pad's isochronous endpoint directly, through `uac-host` on the file
//! descriptor Java already owns. That is measured, not hoped: on a Nothing Phone (3) the claim
//! succeeds unprivileged, the gamepad and the pad's microphone both keep working, and the
//! underrun-free floor is **4 ms in flight** — including under eight-core load with the SoC in
//! severe thermal throttling.
//!
//! # The exclusivity that shapes everything here, and what is actually known about it
//!
//! `valid_flag0` bit 1 (`HAPTICS_SELECT`) disables the audio-haptics path, and every rumble write
//! that exists — `hid-playstation`, SDL, and our own [`crate::feedback`] path via `DsDevice` —
//! asserts it. So haptics and classic rumble cannot both drive the coils **as coded**, and the
//! arbitration selects rather than blends.
//!
//! What is NOT established is the stronger claim this module used to make: that the coils and the
//! rumble motors are the same physical actuators, exclusive *in the firmware*. No teardown, vendor
//! document or measurement supports it here; it traces to one reverse-engineered comment in SDL,
//! and SDL's own modern path sets `HAPTICS_SELECT` **alone** (amplitude rides `ucEnableBits3`),
//! which reads more like an independent mute for the audio path than a shared-actuator interlock.
//! The combination that would settle it — rumble asserted with `HAPTICS_SELECT` CLEARED — is
//! emitted by no code anywhere, so nothing here writes it either.
//!
//! The arbitration is therefore built on **evidence, not prediction**: haptics owns the coils only
//! while haptics frames are actually arriving (see [`haptics_owns_coils`]). That is correct under
//! either hypothesis, and it is what keeps a rumble-only title rumbling — it renders no haptics
//! audio, the host's silence gate emits nothing, and the pad simply keeps its motors.

use std::collections::VecDeque;

use punktfunk_core::audio::AudioGapTracker;
use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};

#[cfg(target_os = "android")]
use punktfunk_core::client::NativeClient;
#[cfg(target_os = "android")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "android")]
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::thread::JoinHandle;
#[cfg(target_os = "android")]
use std::time::Duration;

/// The pad's render layout: 4 interleaved channels — speaker FL/FR on 0/1, the voice coils on
/// 2/3. Feeding a 2-channel stream would leave the coils silent rather than fail, which is the
/// failure mode most worth not having.
const PAD_CHANNELS: usize = 4;

/// Both plane kinds decode as 48 kHz stereo.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const SAMPLE_RATE: u32 = 48_000;

/// Ring ceiling, in sample frames. 60 ms — far above the in-flight depth, because this bounds
/// *decoder* backlog when the USB side stalls, not stream latency. Overflow drops the oldest.
const MAX_BUFFER_FRAMES: usize = (SAMPLE_RATE as usize / 1000) * 60;

/// Largest Opus frame this decodes in one call: 120 ms at 48 kHz, the codec's maximum.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const MAX_FRAME_SAMPLES: usize = 5760;

/// How much audio to keep in flight on the USB endpoint.
///
/// WP7 measured the underrun-free floor on real hardware at **4 ms** (clean across three sweeps,
/// including one under eight-core load with the CPU thermally throttled); 3 ms was marginal and
/// 2 ms never survived. 6 ms takes one step of headroom above that floor, because the same
/// measurement found isolated transient events roughly once per three seconds that are *not*
/// depth-dependent — so the floor is a floor, not a target.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const IN_FLIGHT_MS: u32 = 6;

// ---- tier-A registry ---------------------------------------------------------------------------

/// Which wire pad indices are currently rendering tier-A audio, as a bitmask over the 16 wire
/// slots.
///
/// Read on the rumble poll thread and written on the JNI thread, so it is an atomic rather than a
/// lock: the reader is on a latency path and must never block behind a start/stop.
static TIER_A_PADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Mark (or clear) a pad as rendering tier-A audio.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn set_tier_a(pad: u8, on: bool) {
    use std::sync::atomic::Ordering;
    let bit = 1u32 << (pad & 0x0f);
    if on {
        TIER_A_PADS.fetch_or(bit, Ordering::Relaxed);
    } else {
        TIER_A_PADS.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// Is this pad's HAPTICS lane armed — i.e. did a renderer open a stream that wants the coils?
///
/// Armed is necessary but NOT sufficient to take the pad off wire rumble; see
/// [`haptics_owns_coils`]. Speaker-only rendering never arms this: the speaker pair is a
/// different pair of channels and cannot be disturbed by a rumble write.
pub(crate) fn haptics_armed(pad: u8) -> bool {
    TIER_A_PADS.load(std::sync::atomic::Ordering::Relaxed) & (1u32 << (pad & 0x0f)) != 0
}

/// Last instant a real (non-concealed) haptics frame was decoded for each pad, as ms on the
/// process clock; `0` = never. Written by the render thread, read by the rumble poll thread.
static HAPTICS_SEEN_MS: [std::sync::atomic::AtomicU64; 16] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 16];

/// Process epoch for [`HAPTICS_SEEN_MS`] — `Instant` is not `const`-constructible.
fn epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// Milliseconds since [`epoch`], **1-based**. The `+ 1` reserves `0` as an unambiguous "never
/// stamped" sentinel: without it, a haptics frame arriving in the first millisecond of the process
/// would stamp `0` and be read as never-arrived, handing the coils to wire rumble mid-effect.
/// Stamp and comparison share this clock, so the offset cancels and adds no skew.
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64 + 1
}

/// A pad is only silent for haptics once the host has stopped sending for longer than its own
/// silence gate can explain. The host gates at −60 dBFS with a 250 ms hangover, so a title that
/// renders no haptics audio emits NOTHING on the 0xD1 plane; doubling the hangover covers wire
/// jitter and concealment without letting a real gap read as "live".
const HAPTICS_IDLE_MS: u64 = 500;

/// Stamp a decoded haptics frame. Concealment (PLC) deliberately does not count — filling a gap
/// is not evidence that the game is still driving the coils.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn note_haptics_frame(pad: u8) {
    HAPTICS_SEEN_MS[(pad & 0x0f) as usize].store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Clear a pad's liveness (slot teardown). Wire indices are recycled, so a stale stamp would let
/// a fresh pad inherit the previous one's ownership.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn clear_haptics_liveness(pad: u8) {
    HAPTICS_SEEN_MS[(pad & 0x0f) as usize].store(0, std::sync::atomic::Ordering::Relaxed);
}

/// The arbitration, pure and testable: haptics owns the coils only while frames are ACTUALLY
/// arriving. `seen_ms == 0` (never) is never live.
pub(crate) fn haptics_owns_at(armed: bool, seen_ms: u64, now: u64) -> bool {
    armed && seen_ms != 0 && now.saturating_sub(seen_ms) < HAPTICS_IDLE_MS
}

/// Does this pad's haptics stream currently own the coils, so wire rumble must stand down?
///
/// Arbitrating on **evidence** rather than on a prediction about the hardware is deliberate. Every
/// rumble write this tree emits asserts `valid_flag0` bit 1 (`HAPTICS_SELECT`), which disables the
/// audio-haptics path, so the two cannot both drive the coils *as coded* — whatever the firmware
/// would allow. But a title that never renders haptics audio produces no 0xD1 frames at all, and
/// suppressing its rumble on the assumption that "the stream carries the feedback" silences it
/// outright. Frame arrival is the signal that tells the two cases apart, and it costs nothing.
pub(crate) fn haptics_owns_coils(pad: u8) -> bool {
    let i = (pad & 0x0f) as usize;
    haptics_owns_at(
        haptics_armed(pad),
        HAPTICS_SEEN_MS[i].load(std::sync::atomic::Ordering::Relaxed),
        now_ms(),
    )
}

// ---- the 4-channel mixer ---------------------------------------------------------------------

/// Interleave the two independent stereo streams into one 4-channel frame stream.
///
/// The kinds arrive on different cadences (haptics 5 ms, speaker 10 ms), so each has its own
/// write cursor and [`pop`](Self::pop) emits everything the further-ahead kind has filled, with
/// the lagging or absent kind's pair reading silence. A haptics-only session therefore renders
/// the coils with a silent speaker pair, and vice versa, instead of stalling on the missing kind.
///
/// Samples are `i16` — the DualSense's own wire format — so nothing converts on the hot path.
/// Pure logic, unit-tested below; pacing lives in the USB ring downstream.
pub(crate) struct QuadMixer {
    /// Interleaved 4-channel samples; the front is the next frame out. Always
    /// `ready_frames() * PAD_CHANNELS` long.
    ring: VecDeque<i16>,
    /// Per-kind write cursor in FRAMES relative to the ring front, indexed by the wire `kind`.
    written: [usize; 2],
    /// Frames dropped to the ceiling — a stalled USB side, visible in the logs.
    dropped: u64,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl QuadMixer {
    pub(crate) fn new() -> QuadMixer {
        QuadMixer {
            ring: VecDeque::new(),
            written: [0; 2],
            dropped: 0,
        }
    }

    /// Write one decoded stereo chunk (interleaved L/R) for `kind` at that kind's cursor,
    /// zero-extending as needed. Both cursors shift together on overflow, so the two kinds can
    /// never skew relative to one another.
    pub(crate) fn push(&mut self, kind: u8, stereo: &[i16]) {
        // Name both kinds rather than defaulting: a kind this build does not know belongs
        // nowhere in a 4-channel frame, and quietly folding it into the coil pair would render
        // an unknown stream straight into the actuators.
        let (k, off) = match kind {
            PAD_AUDIO_KIND_HAPTICS => (0usize, 2usize),
            PAD_AUDIO_KIND_SPEAKER => (1usize, 0usize),
            _ => return,
        };
        let frames = stereo.len() / 2;
        let base = self.written[k];
        let need = (base + frames) * PAD_CHANNELS;
        if self.ring.len() < need {
            self.ring.resize(need, 0);
        }
        for (i, fr) in stereo.chunks_exact(2).enumerate() {
            let at = (base + i) * PAD_CHANNELS + off;
            self.ring[at] = fr[0];
            self.ring[at + 1] = fr[1];
        }
        self.written[k] = base + frames;
        let over = self.ready_frames().saturating_sub(MAX_BUFFER_FRAMES);
        if over > 0 {
            self.dropped += over as u64;
            self.drop_front(over);
        }
    }

    /// Frames ready to output: the further-ahead kind's cursor.
    pub(crate) fn ready_frames(&self) -> usize {
        self.written[0].max(self.written[1])
    }

    /// Frames discarded to the ceiling since construction.
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Append every ready frame (interleaved 4-channel) to `out`; returns the frame count.
    pub(crate) fn pop(&mut self, out: &mut Vec<i16>) -> usize {
        let frames = self.ready_frames();
        let n = frames * PAD_CHANNELS;
        out.extend(self.ring.drain(..n.min(self.ring.len())));
        for w in &mut self.written {
            *w = w.saturating_sub(frames);
        }
        frames
    }

    /// Throw the ready frames away — no sink to render them on right now.
    pub(crate) fn discard(&mut self) {
        let f = self.ready_frames();
        self.drop_front(f);
    }

    fn drop_front(&mut self, frames: usize) {
        let n = (frames * PAD_CHANNELS).min(self.ring.len());
        self.ring.drain(..n);
        let f = n / PAD_CHANNELS;
        for w in &mut self.written {
            *w = w.saturating_sub(f);
        }
    }
}

// ---- decode + packet loss concealment ---------------------------------------------------------

#[cfg(target_os = "android")]
/// Per-kind decode state: a stereo 48 kHz Opus decoder, the seq-gap tracker, and the last decoded
/// frame size, which is the unit PLC synthesises in.
struct KindStream {
    dec: opus::Decoder,
    gaps: AudioGapTracker,
    frame_samples: usize,
}

/// Concealment frames to synthesise before decoding `seq`.
///
/// Zero until something has decoded, because there is nothing to size the PLC from yet. The
/// tracker is fed regardless, so a gap seen before the first real frame cannot resurface later as
/// a phantom. Pure, and unit-tested.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn plc_frames(gaps: &mut AudioGapTracker, seq: u32, frame_samples: usize) -> u32 {
    let missing = gaps.missing_before(seq);
    if frame_samples == 0 {
        0
    } else {
        missing
    }
}

// ---- the USB sink ------------------------------------------------------------------------------

/// Everything that talks to the pad. Linux and Android only: `usbfs` is a Linux kernel ABI, and
/// this crate also builds as a host cdylib on macOS dev boxes, where the mixer and PLC above still
/// compile and still run their tests.
#[cfg(target_os = "android")]
mod sink {
    use super::{IN_FLIGHT_MS, PAD_CHANNELS, SAMPLE_RATE};

    /// Open the pad's 4-channel playback stream on a descriptor Java owns.
    ///
    /// # Safety
    ///
    /// `fd` must be a live usbfs descriptor from an open `UsbDeviceConnection` that outlives the
    /// returned device — this **borrows** it and never closes it, because closing is
    /// `UsbDeviceConnection.close()`'s job and a double close would strand an unrelated
    /// descriptor much later.
    pub(super) unsafe fn device(fd: i32) -> usbfs_iso::UsbFsDevice {
        // SAFETY: forwarded from this function's own contract, which the JNI entry point upholds
        // by keeping the Java connection open for the lifetime of the renderer thread.
        unsafe { usbfs_iso::UsbFsDevice::from_borrowed_fd(fd) }
    }

    /// Find the pad's 4-channel playback stream and open it.
    ///
    /// Four channels is a hard requirement, not a preference: the voice coils *are* channels 3
    /// and 4, so a 2-channel alternate setting would open successfully and then render haptics
    /// into nothing.
    pub(super) fn open<'d>(
        dev: &'d usbfs_iso::UsbFsDevice,
    ) -> Result<uac_host::Playback<'d>, uac_host::Error> {
        let blob = dev.raw_descriptors()?;
        let function = uac_host::parse(&blob)?;
        let stream = function
            .output_streams()
            .find(|s| usize::from(s.channels()) == PAD_CHANNELS)
            .ok_or(uac_host::Error::NoAudioFunction)?;

        let opts = uac_host::OpenOptions {
            depth: usbfs_iso::Depth::Millis(IN_FLIGHT_MS),
            // One packet per URB: the finest granularity the bus offers, and what WP7 measured
            // the 4 ms floor with. Packing more multiplies one completion's latency.
            packets_per_urb: Some(1),
            // Keep the endpoint fed rather than gapping when the decoder is momentarily late.
            // A hole in an isochronous stream is silence forever; silence we chose is better.
            underrun: usbfs_iso::Underrun::FillSilence,
            ..Default::default()
        };
        stream.open_with(dev, uac_host::Format::S16Le, SAMPLE_RATE, opts)
    }
}

// ---- the self test ------------------------------------------------------------------------------

/// Drive the pad directly with a synthetic tone, through **the real client path**.
///
/// This exists because the two things most likely to be wrong here cannot be unit-tested and are
/// invisible without a host: whether the descriptor Kotlin handed over is one this renderer may
/// drive exclusively, and whether the interface claim succeeds on this kernel. A standalone
/// harness proves neither — it owns its descriptor by construction, which is exactly the condition
/// that was violated when this renderer was handed the HID link's fd and the two engines began
/// stealing each other's URB completions.
///
/// Opens the sink the same way [`render`] does and writes a sine into the voice-coil pair, which
/// is felt rather than heard. Returns sample frames written, or a negative [`SelfTest`] code.
///
/// # Safety
///
/// `fd` must be a live usbfs descriptor whose connection outlives the call, and which **nothing
/// else is driving transfers on**.
#[cfg(target_os = "android")]
pub(crate) unsafe fn self_test(fd: i32, seconds: i32, hz: i32) -> i32 {
    // SAFETY: the caller's contract.
    let dev = unsafe { sink::device(fd) };
    let mut playback = match sink::open(&dev) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("pad audio self-test: could not open the stream: {e}");
            return SelfTest::OPEN_FAILED;
        }
    };
    log::info!(
        "pad audio self-test: {} ch {} at {} Hz, {} us in flight",
        playback.channels(),
        playback.format(),
        playback.rate(),
        playback.schedule().in_flight_us()
    );

    let rate = playback.rate();
    let channels = playback.channels() as usize;
    let frames_per_chunk = (rate as usize / 1000).max(1);
    let mut chunk = vec![0i16; frames_per_chunk * channels];
    let mut phase = 0.0f32;
    let step = std::f32::consts::TAU * hz.clamp(20, 500) as f32 / rate as f32;
    let total = u64::from(rate) * seconds.clamp(1, 30) as u64;
    let mut written = 0u64;

    while written < total {
        for frame in chunk.chunks_mut(channels) {
            let sample = (phase.sin() * 16_384.0) as i16;
            phase += step;
            if phase >= std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
            frame.fill(0);
            // Channels 2 and 3 are the voice coils; the speaker pair stays silent so a pass is
            // unambiguously FELT rather than merely audible.
            for c in 2..channels {
                frame[c] = sample;
            }
        }
        if let Err(e) = playback.write_interleaved(&chunk) {
            log::warn!("pad audio self-test: write failed after {written} frames: {e}");
            return SelfTest::WRITE_FAILED;
        }
        written += frames_per_chunk as u64;
    }
    let _ = playback.drain(Duration::from_millis(500));

    let stats = playback.stats();
    log::info!(
        "pad audio self-test: {} frames, {} urbs, {} underruns, {} short bytes, {} urb errors",
        playback.frames_written(),
        stats.urbs_completed,
        stats.underruns,
        stats.short_bytes,
        stats.urb_errors
    );
    // Underruns are a producer-pacing property and deliberately NOT a failure here: the question
    // this answers is whether the client can drive the pad at all. Data reaching the bus is the
    // pass condition.
    if stats.urb_errors > 0 || playback.frames_written() == 0 {
        return SelfTest::NO_DATA;
    }
    playback.frames_written().min(i32::MAX as u64) as i32
}

/// Negative results from [`self_test`]. Positive values are sample frames written.
#[cfg(target_os = "android")]
pub(crate) struct SelfTest;

#[cfg(target_os = "android")]
impl SelfTest {
    /// The claim or stream open failed — the OEM-kernel case, or a descriptor another engine owns.
    pub(crate) const OPEN_FAILED: i32 = -1;
    /// The stream opened but a write failed part-way.
    pub(crate) const WRITE_FAILED: i32 = -2;
    /// It ran, but nothing reached the bus.
    pub(crate) const NO_DATA: i32 = -3;
}

// ---- the renderer worker -----------------------------------------------------------------------

/// A running renderer: the stop flag and the thread, joined on drop.
///
/// Mirrors [`crate::mic::MicCapture`]'s discipline — dropping the handle is what stops the stream,
/// so a session teardown that forgets a step cannot leave a thread writing to a descriptor Java is
/// about to close.
#[cfg(target_os = "android")]
pub(crate) struct PadAudio {
    pad: u8,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[cfg(target_os = "android")]
impl Drop for PadAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        // Belt and braces: the thread clears these itself on the way out, but if it died in a way
        // that skipped that, leaving the pad off wire rumble would cost the user all feedback.
        set_tier_a(self.pad, false);
        clear_haptics_liveness(self.pad);
    }
}

/// Start the renderer for a pad whose descriptor Java has handed over.
///
/// Returns `None` when neither kind is enabled (nothing to render) or the thread will not start.
/// **The caller must keep the `UsbDeviceConnection` open until the returned handle is dropped** —
/// the renderer borrows the descriptor and never closes it.
#[cfg(target_os = "android")]
pub(crate) fn start(
    client: Arc<NativeClient>,
    pad: u8,
    fd: i32,
    haptics: bool,
    speaker: bool,
) -> Option<PadAudio> {
    if !haptics && !speaker {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let join = spawn(client, Arc::clone(&stop), pad, fd, haptics, speaker)?;
    Some(PadAudio {
        pad,
        stop,
        join: Some(join),
    })
}

/// Spawn the pad-audio renderer — the 0xD1 plane's single consumer on Android.
///
/// `fd` is the pad's usbfs descriptor from `UsbDeviceConnection.getFileDescriptor()`; the caller
/// **must** keep that connection open until [`stop`](AtomicBool) has been observed and the handle
/// joined. Returns `None` if the thread could not be started.
#[cfg(target_os = "android")]
pub(crate) fn spawn(
    client: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    pad: u8,
    fd: i32,
    haptics: bool,
    speaker: bool,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-pad-audio".into())
        .spawn(move || run(&client, &stop, pad, fd, haptics, speaker))
        .map_err(|e| log::warn!("pad-audio thread failed to start: {e}"))
        .ok()
}

#[cfg(target_os = "android")]
fn run(client: &NativeClient, stop: &AtomicBool, pad: u8, fd: i32, haptics: bool, speaker: bool) {
    // Ask the scheduler for audio priority. Android does not hand SCHED_FIFO to ordinary app
    // threads, so -16 (ANDROID_PRIORITY_AUDIO) is the realistic knob — and WP7 measured that it
    // both applies and is enough to hold the 4 ms floor against eight busy cores.
    // SAFETY: `setpriority` on the calling thread; no pointers, no shared state.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -16);
    }

    // SAFETY: the caller's contract — the Java connection outlives this thread.
    let dev = unsafe { sink::device(fd) };
    // Through a reference, deliberately: `UsbFsDevice` has a `Drop`, and opening the stream in
    // this same scope would make the borrow outlive the value it borrows.
    render(&dev, client, stop, pad, haptics, speaker);
}

/// Open the pad's stream and render on it until the session stops or the device goes away.
#[cfg(target_os = "android")]
fn render(
    dev: &usbfs_iso::UsbFsDevice,
    client: &NativeClient,
    stop: &AtomicBool,
    pad: u8,
    haptics: bool,
    speaker: bool,
) {
    // A host that cannot send 0xD1 will never render anything here, so opening the stream would
    // claim the interface and (before the arbitration below) take the pad off wire rumble in
    // exchange for nothing. Against every released host this is the DEFAULT path — `pad_haptics`
    // is on with no UI to turn it off — so without this gate a wired DualSense simply stops
    // rumbling. Checked before `sink::open` so the iso interface is never claimed pointlessly.
    if client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO == 0 {
        log::warn!("pad audio: host cannot send it (no HOST_CAP_PAD_AUDIO) — pad {pad} stays on wire rumble");
        drain_until_stop(client, stop);
        return;
    }
    match sink::open(dev) {
        Ok(mut playback) => {
            log::info!(
                "pad audio: pad={pad} {} ch {} at {} Hz, {} us in flight",
                playback.channels(),
                playback.format(),
                playback.rate(),
                playback.schedule().in_flight_us()
            );
            // ONLY NOW commit the trade. Declaring the pad's render capability makes the host
            // emit 0xD1, and taking the pad off wire rumble is what makes tier A and tier C
            // mutually exclusive — doing either before the stream is known to open would, on a
            // kernel that refuses the claim, leave the user with no haptics of any kind.
            let caps = (if haptics { 0x01 } else { 0 }) | (if speaker { 0x02 } else { 0 });
            client.set_pad_audio_caps(pad, caps);
            // Arm the HAPTICS lane only — a speaker-only setup drives channels 0/1, which no
            // rumble write can disturb, so taking the motors away would kill rumble with nothing
            // rendering haptics in exchange.
            set_tier_a(pad, haptics);

            pump(client, stop, pad, haptics, speaker, &mut playback);

            // Give the pad back to wire rumble before this thread goes away.
            client.set_pad_audio_caps(pad, 0);
            set_tier_a(pad, false);
            clear_haptics_liveness(pad);
        }
        Err(e) => {
            // A kernel that refuses the claim: some OEM kernels do, and there is no app-side fix.
            // Nothing was declared and nothing was suppressed, so the session simply carries on
            // at tier C with ordinary rumble — a clean degrade rather than silent total loss.
            log::warn!("pad audio unavailable on pad {pad}, staying on rumble: {e}");
            drain_until_stop(client, stop);
        }
    }
}

#[cfg(target_os = "android")]
/// Keep the plane drained without rendering, so a host that is sending 0xD1 does not back up
/// against a consumer that never reads.
fn drain_until_stop(client: &NativeClient, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        if client.next_pad_audio(Duration::from_millis(20)).is_none()
            && stop.load(Ordering::Relaxed)
        {
            return;
        }
    }
}

/// The steady state: decode arriving frames, interleave, and hand whole frames to the pad.
#[cfg(target_os = "android")]
fn pump(
    client: &NativeClient,
    stop: &AtomicBool,
    pad: u8,
    haptics: bool,
    speaker: bool,
    playback: &mut uac_host::Playback<'_>,
) {
    let mut mixer = QuadMixer::new();
    let mut streams: [Option<KindStream>; 2] = [None, None];
    // Periodic accounting. Without it the only way to tell "the host is sending nothing" from
    // "frames arrive but render silently" is to guess, and those two have completely different
    // causes — one is host-side routing, the other is here.
    let mut frames_in = 0u64;
    let mut samples_in = 0u64;
    let mut peak = 0i32;
    let mut last_report = std::time::Instant::now();
    let mut pcm: Vec<i16> = Vec::with_capacity(MAX_FRAME_SAMPLES * 2);
    let mut out: Vec<i16> = Vec::with_capacity(MAX_BUFFER_FRAMES * PAD_CHANNELS);

    while !stop.load(Ordering::Relaxed) {
        // Report BEFORE the frame gate. Silence on the plane is a legitimate — and highly
        // diagnostic — state: it means the host's capture hears nothing, which is a routing
        // problem upstream rather than anything here. Reporting only when a frame arrives makes
        // that state indistinguishable from the renderer being dead.
        if last_report.elapsed() >= Duration::from_secs(1) {
            let st = playback.stats();
            log::info!(
                "pad audio: {frames_in} frames in, {samples_in} samples, peak={peak}, \
                 {} written, {} underruns, {} short",
                playback.frames_written(),
                st.underruns,
                st.short_bytes
            );
            last_report = std::time::Instant::now();
            peak = 0;
        }

        let Some(frame) = client.next_pad_audio(Duration::from_millis(10)) else {
            continue;
        };

        // The settings gate each kind independently: haptics off but speaker on is a legitimate
        // configuration, and the host may still be sending both.
        let wanted = match frame.kind {
            PAD_AUDIO_KIND_HAPTICS => haptics,
            PAD_AUDIO_KIND_SPEAKER => speaker,
            _ => false,
        };
        if !wanted {
            continue;
        }

        // A real haptics frame is the evidence that the game is driving the coils, and therefore
        // that wire rumble must stand down for this pad (see `haptics_owns_coils`). Stamped on
        // arrival rather than after decode so a decoder hiccup cannot hand the coils back
        // mid-effect; concealment never reaches here, so PLC still does not count.
        if frame.kind == PAD_AUDIO_KIND_HAPTICS {
            note_haptics_frame(pad);
        }

        frames_in += 1;
        let k = usize::from(frame.kind).min(1);
        let st = match &mut streams[k] {
            Some(s) => s,
            slot @ None => match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo) {
                Ok(dec) => slot.insert(KindStream {
                    dec,
                    gaps: AudioGapTracker::default(),
                    frame_samples: 0,
                }),
                Err(e) => {
                    log::warn!("pad audio: no Opus decoder for kind {}: {e}", frame.kind);
                    continue;
                }
            },
        };

        // Conceal whatever the sequence numbers say is missing, before decoding what arrived.
        let missing = plc_frames(&mut st.gaps, frame.seq, st.frame_samples);
        for _ in 0..missing {
            pcm.resize(st.frame_samples * 2, 0);
            match st.dec.decode(&[], &mut pcm, false) {
                Ok(n) => mixer.push(frame.kind, &pcm[..n * 2]),
                Err(_) => break,
            }
        }

        // An empty payload is DTX silence: the tracker has already accounted for the sequence,
        // and there is nothing to decode.
        if !frame.opus.is_empty() {
            pcm.resize(MAX_FRAME_SAMPLES * 2, 0);
            match st.dec.decode(&frame.opus, &mut pcm, false) {
                Ok(n) => {
                    st.frame_samples = n;
                    samples_in += n as u64;
                    // Peak of what actually decoded: distinguishes "frames arriving but silent"
                    // (a host-side routing problem) from "frames arriving with signal that is not
                    // reaching the actuators" (a problem here).
                    peak = peak.max(
                        pcm[..n * 2]
                            .iter()
                            .map(|s| i32::from(s.abs()))
                            .max()
                            .unwrap_or(0),
                    );
                    mixer.push(frame.kind, &pcm[..n * 2]);
                }
                Err(e) => log::debug!("pad audio: opus decode failed: {e}"),
            }
        }

        // Hand over whole frames only. `write` stages any remainder internally, so a partial
        // chunk is never padded with silence mid-stream.
        out.clear();
        if mixer.pop(&mut out) > 0 {
            if let Err(e) = playback.write_interleaved(&out) {
                if is_fatal(&e) {
                    log::warn!("pad audio: stream lost: {e}");
                    return;
                }
                log::debug!("pad audio: write hiccup: {e}");
                mixer.discard();
            }
        }
    }

    let _ = playback.drain(Duration::from_millis(100));
    let stats = playback.stats();
    log::info!(
        "pad audio stopped: {} frames, {} underruns, {} short bytes, {} dropped by backlog",
        playback.frames_written(),
        stats.underruns,
        stats.short_bytes,
        mixer.dropped_frames(),
    );
}

/// Is this the end of the stream, or just a bad moment?
///
/// A vanished device is unrecoverable here — the descriptor belongs to a `UsbDeviceConnection`
/// that Java must re-open — so the thread exits and the session continues without tier A. Anything
/// else is treated as transient.
#[cfg(target_os = "android")]
fn is_fatal(e: &uac_host::Error) -> bool {
    matches!(e, uac_host::Error::Transport(t) if t.is_disconnected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_lands_on_the_front_pair_and_haptics_on_the_coils() {
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[100, 200]);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[300, 400]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 1);
        // Channels 0/1 are the speaker, 2/3 are the voice coils — the pad's own layout.
        assert_eq!(out, vec![100, 200, 300, 400]);
    }

    #[test]
    fn a_haptics_only_session_still_renders_with_a_silent_speaker_pair() {
        // The case that matters most: `pad_speaker = "off"` must not stall the coils waiting for
        // a kind that will never arrive.
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[7, 8, 9, 10]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![0, 0, 7, 8, 0, 0, 9, 10]);
    }

    #[test]
    fn the_two_kinds_never_skew_when_the_ceiling_drops_frames() {
        let mut m = QuadMixer::new();
        // Push well past the ceiling on one kind, then a marker on the other. Both cursors must
        // have moved together, so the marker still lands on the same output frame boundary.
        let flood = vec![1i16; (MAX_BUFFER_FRAMES + 500) * 2];
        m.push(PAD_AUDIO_KIND_HAPTICS, &flood);
        assert!(m.dropped_frames() > 0);
        assert_eq!(m.ready_frames(), MAX_BUFFER_FRAMES);

        m.push(PAD_AUDIO_KIND_SPEAKER, &[42, 43]);
        let mut out = Vec::new();
        let frames = m.pop(&mut out);
        assert_eq!(frames, MAX_BUFFER_FRAMES);
        assert_eq!(out.len(), frames * PAD_CHANNELS);
        // The speaker sample went to the FRONT of the ring (its cursor was reset with the drop),
        // not to wherever the flooded kind happened to be.
        assert_eq!(&out[..4], &[42, 43, 1, 1]);
    }

    #[test]
    fn interleaving_survives_uneven_cadences() {
        // Haptics arrive at 5 ms and the speaker at 10 ms; popping mid-flight must not lose the
        // lagging kind's alignment.
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 1, 2, 2]);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[9, 9]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![9, 9, 1, 1, 0, 0, 2, 2]);

        // Next round: both cursors are back at zero, so a fresh speaker frame aligns with a fresh
        // haptics frame rather than inheriting the previous round's offset.
        out.clear();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[5, 5]);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[6, 6]);
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![5, 5, 6, 6]);
    }

    #[test]
    fn an_unknown_kind_is_dropped_rather_than_rendered_into_the_coils() {
        let mut m = QuadMixer::new();
        m.push(9, &[999, 999]);
        assert_eq!(
            m.ready_frames(),
            0,
            "an unknown kind must not occupy a channel pair"
        );
        let mut out = Vec::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 2]);
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![0, 0, 1, 2]);
    }

    #[test]
    fn tier_a_registry_tracks_pads_independently() {
        // A rumble command reaching a tier-A pad mutes its coils for the session, so this gate
        // has to be exact rather than approximately right.
        set_tier_a(3, true);
        assert!(haptics_armed(3));
        assert!(!haptics_armed(4));
        set_tier_a(4, true);
        assert!(haptics_armed(3) && haptics_armed(4));
        set_tier_a(3, false);
        assert!(!haptics_armed(3), "clearing one pad must not clear another");
        assert!(haptics_armed(4));
        set_tier_a(4, false);
        assert!(!haptics_armed(4));
    }

    #[test]
    fn tier_a_registry_wraps_the_pad_index_into_the_wire_slot_space() {
        // The wire pad space is 4 bits; an out-of-range index must not shift the mask into
        // undefined territory (a shift >= 32 is a panic in debug and garbage in release).
        set_tier_a(0x1f, true);
        assert!(haptics_armed(0x0f), "0x1f and 0x0f are the same wire slot");
        set_tier_a(0x0f, false);
        assert!(!haptics_armed(0x1f));
    }

    /// The arbitration that keeps a rumble-only game working. Armed alone is NOT ownership: a
    /// title that never renders haptics audio produces no frames, so the host's silence gate
    /// emits nothing on 0xD1 and the pad must keep its motors.
    #[test]
    fn haptics_owns_the_coils_only_while_frames_actually_arrive() {
        // Never seen a frame — armed, but the game is not driving the coils.
        assert!(
            !haptics_owns_at(true, 0, 10_000),
            "an armed pad that has never received a frame must keep its rumble"
        );
        // A frame just arrived: haptics owns, rumble stands down.
        assert!(haptics_owns_at(true, 10_000, 10_000));
        // Still inside the idle window (the host's own 250 ms hangover, doubled).
        assert!(haptics_owns_at(true, 10_000, 10_000 + HAPTICS_IDLE_MS - 1));
        // The stream went quiet: the coils go back to wire rumble.
        assert!(
            !haptics_owns_at(true, 10_000, 10_000 + HAPTICS_IDLE_MS),
            "the coils must return to rumble once haptics stops arriving"
        );
        // Not armed (speaker-only, or no renderer): frames or not, rumble always owns.
        assert!(!haptics_owns_at(false, 10_000, 10_000));
    }

    /// Clock skew must never strand a pad in the suppressed state.
    #[test]
    fn a_stamp_ahead_of_now_does_not_wrap_the_idle_window() {
        // The clock is monotonic so this should not arise, but an unsigned underflow would wrap
        // to ~2^64 ms and read as EXPIRED — handing the coils back mid-effect. `saturating_sub`
        // pins it to 0 (still live), and it self-corrects once the clock catches up.
        assert!(haptics_owns_at(true, 10_000, 9_000));
        // The 1-based clock is what makes this distinguishable: a frame stamped in the process's
        // first millisecond must read as LIVE, not as never-stamped.
        assert!(
            haptics_owns_at(true, 1, 1),
            "a frame stamped at t=0 must not be mistaken for never-stamped"
        );
        assert!(
            !haptics_owns_at(true, 0, 0),
            "never-seen stays never-seen at t=0"
        );
    }

    /// Teardown drops the liveness stamp: wire indices are recycled, and a fresh pad must not
    /// inherit the previous occupant's ownership of the coils.
    #[test]
    fn clearing_liveness_hands_the_coils_back() {
        set_tier_a(2, true);
        note_haptics_frame(2);
        assert!(haptics_owns_coils(2));
        clear_haptics_liveness(2);
        assert!(
            !haptics_owns_coils(2),
            "a cleared stamp must release the coils"
        );
        set_tier_a(2, false);
    }

    #[test]
    fn discard_empties_without_disturbing_alignment() {
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 2, 3, 4]);
        m.discard();
        assert_eq!(m.ready_frames(), 0);
        let mut out = Vec::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[8, 9]);
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![8, 9, 0, 0]);
    }

    #[test]
    fn plc_stays_silent_until_something_has_decoded() {
        let mut g = AudioGapTracker::default();
        // A gap before the first decode has nothing to size concealment from, and must not be
        // replayed later as a phantom.
        assert_eq!(plc_frames(&mut g, 5, 0), 0);
        assert_eq!(plc_frames(&mut g, 6, 480), 0);
    }

    #[test]
    fn plc_conceals_a_real_gap_once_a_frame_size_is_known() {
        let mut g = AudioGapTracker::default();
        assert_eq!(plc_frames(&mut g, 0, 0), 0);
        assert_eq!(plc_frames(&mut g, 1, 480), 0);
        // Sequence 2 and 3 never arrived.
        assert_eq!(plc_frames(&mut g, 4, 480), 2);
    }
}
