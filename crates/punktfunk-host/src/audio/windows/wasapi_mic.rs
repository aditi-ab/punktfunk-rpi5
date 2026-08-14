//! WASAPI virtual microphone (Windows) — the inverse of [`super::wasapi_cap`]. Windows has no
//! user-mode way to *create* a capture (microphone) endpoint, so we target an EXISTING virtual audio
//! device and write the client's decoded mic PCM into that device's **render** endpoint; the device's
//! **capture** endpoint then surfaces as a microphone that host apps can record from.
//!
//! The target comes from the [`audio_control::wire_now`] plan (recomputed on every open): VB-Audio
//! the minted "Punktfunk Microphone" (tier-0, see `super::minted`), then by name: VB-Audio
//! "CABLE Input" (bundled by installers until the audio-substrate change), the Steam Streaming
//! Microphone, VoiceMeeter, or anything with "virtual" in the name; `PUNKTFUNK_MIC_DEVICE` overrides.
//! The plan reserves the mic target and points the desktop-audio loopback at a DIFFERENT endpoint, so
//! injecting here can never echo into the host→client audio stream (see
//! [`wiring_plan`](super::wiring_plan) for the precedence rules and the headless cable-only case).
//! If no candidate is present we auto-install the Steam Streaming audio pair (see
//! [`install_steam_audio_pair`]); failing that we return an error with install guidance and the
//! caller (the mic pump) retries with backoff — a cable that appears later (driver install finishing
//! after boot) is picked up without a host restart.
//!
//! **Liveness.** Any WASAPI error in the render loop (endpoint invalidated/removed, audio engine
//! restart) exits the worker thread, which flips the `alive` flag — [`VirtualMic::push`] then
//! returns `false` and the pump reopens (re-planning, so endpoint churn re-resolves). Before this
//! existed, the first device change silently killed mic passthrough for the rest of the host's life.
//!
//! `push` enqueues decoded interleaved-f32 PCM into a bounded ring (drop-oldest so mic latency
//! stays bounded — the bound follows the adaptive prime threshold, or the legacy ~120 ms until
//! the pump drives it); a dedicated COM-apartment thread renders it event-driven through a
//! jitter buffer (prime → hold → re-prime, see the render loop — clients arrive in bursts, the
//! device pulls per-period) whose prime depth the mic pump sets from measured uplink jitter
//! ([`VirtualMic::set_target_depth`]), filling silence when the client isn't talking. WASAPI
//! objects are `!Send`, so they live entirely on that thread (mirrors `WasapiLoopbackCapturer`).
//!
//! **Idle stop (host must be able to SLEEP).** A RUNNING WASAPI stream makes the audio stack
//! hold a kernel power request ("An audio stream is currently in use", attributed to the
//! target device in `powercfg /requests`) that vetoes system sleep — and this pump is
//! host-lifetime, so rendering silence 24/7 kept every idle Punktfunk box awake forever
//! (field report 2026-08-12: "doesn't go to sleep anymore; powercfg shows the Steam Streaming
//! Microphone"). So after [`IDLE_STOP_AFTER`] of silence-only output the render loop stops the
//! stream (`IAudioClient::Stop` — the client stays initialized, the mic ENDPOINT keeps
//! existing, only the power request is released) and parks on the queue's condvar; the next
//! pushed frame resumes it within one device period. During a session the box is kept awake by
//! the session's own `DisplayWakeRequest` (pf-frame), never by this silence.
//! `PUNKTFUNK_MIC_ALWAYS_ON=1` restores the old always-running stream in case a virtual audio
//! driver misbehaves when its render side pauses.

use super::{audio_control, MicBackendStats, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

const CHANNELS: u32 = 2;
/// 48 kHz stereo f32: 2 channels * 4 bytes.
const BLOCK_ALIGN: usize = 2 * 4;
/// LEGACY jitter-buffer priming depth (~48 ms): the render loop emits pure silence until this
/// much PCM is queued, then plays from the cushion. Old clients deliver mic audio in BURSTS
/// (the Mac client's input tap yields ~two 20 ms Opus packets every ~42 ms) while WASAPI pulls
/// a small block every device period (~10 ms) — with no cushion the queue sits near-empty and
/// most periods insert mid-stream silence: the "crackling mic" (heard live, Mac → Windows host
/// 2026-07-03; the Linux backend's process callback primes the same way and the identical
/// stream was clean there). The depth had to cover the worst inter-burst gap (~42 ms), so
/// ~48 ms with re-prime on a full drain. Today the pump MEASURES that gap and drives the prime
/// threshold per client ([`VirtualMic::set_target_depth`] — bursty clients still get ~48 ms,
/// modern 10 ms-cadence ones ~25–35 ms); this constant remains the fallback until the pump's
/// first estimate, and forever under `PUNKTFUNK_MIC_LEGACY_BUFFER=1`.
const PRIME_BYTES: usize = (SAMPLE_RATE as usize * 48 / 1000) * BLOCK_ALIGN;
/// LEGACY bound for the inject queue at ~120 ms (drop oldest beyond): the fixed priming
/// cushion plus arrival-burst headroom. Applies only while the pump isn't driving the target;
/// adaptive mode bounds the queue at prime + [`CAP_HEADROOM_BYTES`] instead (≈ 50–105 ms).
const MAX_QUEUE_BYTES: usize = (SAMPLE_RATE as usize * 120 / 1000) * BLOCK_ALIGN;
/// Producer-side overflow headroom (~32 ms) over the render loop's prime threshold when the
/// adaptive target drives the ring.
const CAP_HEADROOM_BYTES: usize = (SAMPLE_RATE as usize * 32 / 1000) * BLOCK_ALIGN;
/// Stop the render stream after this long of silence-only output (unprimed, nothing queued), so
/// an idle host releases the audio stack's sleep-blocking power request (see the module docs).
/// Long enough that mid-conversation pauses never cycle the stream; resume is one condvar
/// notify + `IAudioClient::Start`, well under the jitter buffer's prime depth.
const IDLE_STOP_AFTER: Duration = Duration::from_secs(10);
/// While the stream is idle-stopped the render thread parks on the queue condvar; this timeout
/// only bounds how long a host-shutdown `stop` can go unnoticed (same discipline as the pump's
/// `drain_sleep`).
const IDLE_WAKE_CHECK: Duration = Duration::from_millis(250);

/// The mic inject ring plus the wake signal for an idle-stopped render thread: `push` notifies
/// on the empty→non-empty transition, which is exactly the moment a stopped stream must resume.
type MicQueue = (Mutex<VecDeque<u8>>, Condvar);

/// `PUNKTFUNK_MIC_ALWAYS_ON=1`: never idle-stop the render stream (pre-2026-08 behaviour — the
/// host then blocks system sleep for its whole life). Escape hatch in case a virtual audio
/// driver's capture side misbehaves while its render side is paused.
fn mic_always_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PUNKTFUNK_MIC_ALWAYS_ON").is_some_and(|v| v != "0"))
}

pub struct WasapiVirtualMic {
    queue: Arc<MicQueue>,
    stop: Arc<AtomicBool>,
    /// False once the render thread has exited (device error or stop) — the pump's reopen signal.
    alive: Arc<AtomicBool>,
    /// Ring policy/telemetry shared with the render thread (see [`RingShared`]).
    ring: Arc<RingShared>,
    join: Option<JoinHandle<()>>,
}

/// Atomics shared between the pump-facing handle and the render thread: the pump's adaptive
/// de-jitter target in, the effective prime threshold + reset-on-read counters out. All
/// `Relaxed` — a slowly-moving target and telemetry, not synchronization.
#[derive(Default)]
struct RingShared {
    /// Pump-set jitter target in bytes. `0` = the pump never spoke (legacy mode, or its first
    /// estimate hasn't landed) → the render loop keeps the fixed [`PRIME_BYTES`] and `push`
    /// keeps the fixed [`MAX_QUEUE_BYTES`].
    target_bytes: AtomicUsize,
    /// Effective prime threshold (bytes) of the last render iteration.
    prime_bytes: AtomicUsize,
    /// Full-drain re-prime arms (see [`MicBackendStats`]).
    reprimes: AtomicU64,
    /// Per-channel samples dropped by the overflow cap.
    overflow: AtomicU64,
}

impl WasapiVirtualMic {
    pub fn open(channels: u32) -> Result<Self> {
        anyhow::ensure!(
            channels == CHANNELS,
            "virtual mic is stereo-only (got {channels})"
        );
        let queue = Arc::new((Mutex::new(VecDeque::<u8>::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let ring = Arc::new(RingShared::default());
        // Bring-up handshake: report the resolved device (or the error) before returning, so a missing
        // virtual-mic device surfaces as Err (the caller retries with backoff) not a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<String>>(1);
        let (q, st, rg, al) = (queue.clone(), stop.clone(), ring.clone(), alive.clone());
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-mic".into())
            .spawn(move || {
                if let Err(e) = render_thread(q, st, rg, ready_tx) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi virtual-mic thread failed");
                }
                // Normal stop or device error alike: this instance is done — the pump reopens.
                al.store(false, Ordering::Release);
            })
            .context("spawn wasapi mic thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(name)) => {
                tracing::info!(device = %name,
                    "WASAPI virtual mic ready (client mic → this device's render endpoint)");
                Ok(WasapiVirtualMic {
                    queue,
                    stop,
                    alive,
                    ring,
                    join: Some(join),
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("wasapi virtual-mic init timed out")),
        }
    }
}

impl Drop for WasapiVirtualMic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl VirtualMic for WasapiVirtualMic {
    fn push(&self, pcm: &[f32]) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let (lock, wake) = &*self.queue;
        let Ok(mut q) = lock.lock() else {
            return false;
        };
        let was_empty = q.is_empty();
        q.reserve(pcm.len() * 4);
        for &s in pcm {
            q.extend(s.to_le_bytes());
        }
        // Drop-oldest to keep latency bounded (mic is real-time; stale audio is worse than
        // dropped). With the pump driving the target, the bound follows the render loop's prime
        // threshold + headroom; otherwise (legacy / no estimate yet) the fixed 120 ms applies.
        let cap = if self.ring.target_bytes.load(Ordering::Relaxed) == 0 {
            MAX_QUEUE_BYTES
        } else {
            // `max(PRIME_BYTES)` covers the one render period before the loop first publishes.
            self.ring
                .prime_bytes
                .load(Ordering::Relaxed)
                .max(PRIME_BYTES)
                + CAP_HEADROOM_BYTES
        };
        if q.len() > cap {
            let excess = q.len() - cap;
            q.drain(..excess);
            self.ring
                .overflow
                .fetch_add((excess / BLOCK_ALIGN) as u64, Ordering::Relaxed);
        }
        drop(q);
        if was_empty {
            // Empty→non-empty is the resume moment for an idle-stopped stream (the waiter
            // re-checks the queue under the lock, so this cannot be a lost wakeup).
            wake.notify_one();
        }
        true
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn discard(&self) {
        if let Ok(mut q) = self.queue.0.lock() {
            q.clear();
        }
    }

    fn channels(&self) -> u32 {
        CHANNELS
    }

    fn set_target_depth(&self, samples_per_ch: usize) {
        self.ring
            .target_bytes
            .store(samples_per_ch * BLOCK_ALIGN, Ordering::Relaxed);
    }

    fn depth(&self) -> Option<(usize, usize)> {
        let prime = self.ring.prime_bytes.load(Ordering::Relaxed);
        if prime == 0 {
            return None; // render loop hasn't run yet
        }
        let q = self.queue.0.lock().ok()?;
        Some((q.len() / BLOCK_ALIGN, prime / BLOCK_ALIGN))
    }

    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats {
            reprimes: self.ring.reprimes.swap(0, Ordering::Relaxed),
            overflow_dropped: self.ring.overflow.swap(0, Ordering::Relaxed),
        }
    }
}

/// Resolve the mic inject target from the wiring plan, auto-installing the Steam Streaming pair
/// when nothing usable exists (then re-planning). Runs on the COM-initialized render thread.
fn resolve_target() -> Result<(wasapi::Device, String)> {
    // The minted endpoints must exist BEFORE this open resolves its write target: the pump
    // holds one device for its lifetime, so racing the provisioning worker here left the pump
    // on the cable while later plans paired the default recording with the minted microphone
    // nothing wrote into (see `minted::ensure_blocking`). Instant once latched.
    super::minted::ensure_blocking();
    // park_defaults=false: the mic pump runs while the host is idle — only the desktop-audio
    // capture may park the box's defaults (playback on the silent sink, recording on the virtual
    // mic) for a stream's lifetime. An idle box must keep the operator's own devices default —
    // an idle-parked recording default is the 2026-08 Helldivers 2 tank (`audio_control` docs).
    let mut wiring = audio_control::wire_now(false);
    if wiring.mic_render.is_none() && !wiring.mic_withheld {
        // A WITHHELD mic skips the install attempt: the Streaming Microphone exists — the plan
        // gave it to the loopback — so reinstalling the pair changes nothing and costs a 5 s
        // endpoint-settle sleep per reopen.
        tracing::info!("no usable virtual mic device present — attempting auto-install");
        if install_steam_audio_pair() {
            wiring = audio_control::wire_now(false);
        }
    }
    let Some(ep) = wiring.mic_render else {
        if wiring.mic_withheld {
            anyhow::bail!(
                "the Steam Streaming Microphone is carrying desktop audio (game audio outranks \
                 the mic; taking it would have silenced the stream) — install VB-Audio Virtual \
                 Cable to give the mic its own device, or set PUNKTFUNK_MIC_DEVICE=<friendly-name \
                 substring> to force a target."
            );
        }
        anyhow::bail!(
            "no virtual-mic render endpoint on this box. Install Steam (the host mints its own \
             microphone endpoint from Steam's streaming drivers — Steam never needs to run), or \
             install VB-Audio Virtual Cable, or set PUNKTFUNK_MIC_DEVICE=<friendly-name \
             substring>."
        );
    };
    let name = ep.0.clone();
    Ok((audio_control::open_endpoint(&ep)?, name))
}

/// Best-effort: install BOTH Steam Streaming audio devices (the "Steam pair") so mic passthrough
/// works out of the box and the host has a desktop-audio sink distinct from the mic. Steam Remote
/// Play ships `SteamStreamingMicrophone.inf` + `SteamStreamingSpeakers.inf`: the microphone gives the
/// virtual mic a target whose **capture** endpoint apps record from, and the speakers give a
/// **render** endpoint a headless box can loopback-capture that is NOT the mic — so the loopback and
/// the mic land on different devices and never echo (see [`super::wiring_plan`]). The Streaming
/// Microphone's render side doubles as the client-only-audio silent sink, so the desktop-audio
/// capture ([`super::wasapi_cap`]) also installs the pair when no silent sink exists. Returns true
/// if either installed. No-op when Steam isn't installed (INFs absent), the install is denied
/// (needs admin — the host runs as SYSTEM), or `PUNKTFUNK_NO_MIC_INSTALL` is set.
pub(crate) fn install_steam_audio_pair() -> bool {
    // Microphone first (the mic's actual target); speakers second (the distinct desktop-audio sink).
    let mic = try_install_steam_audio("SteamStreamingMicrophone.inf");
    let spk = try_install_steam_audio("SteamStreamingSpeakers.inf");
    mic || spk
}

/// Full path of a Steam Remote Play driver INF under Steam's per-arch driver directory
/// (`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\{arch}\<inf_name>`), as a NUL-terminated
/// UTF-16 buffer. Shared by [`try_install_steam_audio`] and the pad-endpoint provisioning
/// ([`super::pad_endpoint`]), which feeds the same INF to `UpdateDriverForPlugAndPlayDevicesW`
/// when no installed Steam Streaming Speakers devnode exposes its `oemNN.inf`. `None` when the
/// environment expansion fails (existence is the caller's check).
pub(crate) fn steam_driver_inf_path(inf_name: &str) -> Option<Vec<u16>> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;

    // Steam ships per-arch driver INFs under `Steam\drivers\Windows10\{arch}\`.
    #[cfg(target_arch = "x86_64")]
    let subdir = "x64";
    #[cfg(target_arch = "aarch64")]
    let subdir = "arm64";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let subdir = "x86";
    let template: Vec<u16> =
        format!("%CommonProgramFiles(x86)%\\Steam\\drivers\\Windows10\\{subdir}\\{inf_name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
    let mut path = vec![0u16; 1024];
    // SAFETY: `template` is a locally built NUL-terminated UTF-16 buffer that outlives the call, and
    // the output slice is a live local whose length the callee is told via the slice itself.
    let n =
        unsafe { ExpandEnvironmentStringsW(PCWSTR(template.as_ptr()), Some(path.as_mut_slice())) };
    if n == 0 || n as usize > path.len() {
        return None;
    }
    path.truncate(n as usize); // keeps the NUL
    Some(path)
}

/// Do Steam's streaming-audio driver INFs exist on this box? The auto-install RE-ARM trigger:
/// INF files appearing later (Steam installed mid-run) are invisible to the endpoint-set
/// fingerprint — files are not endpoints — so the desktop-audio capture's install latch keys on
/// this instead of staying once-per-process ([`super::wasapi_cap`]).
pub(crate) fn steam_infs_present() -> bool {
    use std::os::windows::ffi::OsStringExt;
    ["SteamStreamingMicrophone.inf", "SteamStreamingSpeakers.inf"]
        .iter()
        .any(|inf| {
            steam_driver_inf_path(inf).is_some_and(|wide| {
                // Drop the trailing NUL the FFI callers need; `exists` wants the bare path.
                let len = wide.len().saturating_sub(1);
                std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide[..len])).exists()
            })
        })
}

/// Install one Steam Streaming driver INF by filename via `DiInstallDriverW` (loaded from
/// `newdev.dll`, like Apollo, to avoid an extra windows-crate feature). See
/// [`install_steam_audio_pair`] for the contract; `inf_name` is a bare filename under Steam's
/// per-arch `drivers\Windows10\{arch}\` directory.
///
/// Safe: `inf_name` is a `&str` and every FFI argument is built locally from it, so there is no
/// precondition a caller could break — the `unsafe` is the `LoadLibraryExW`/`transmute`/call chain
/// inside, which is this function's own business.
fn try_install_steam_audio(inf_name: &str) -> bool {
    use windows::core::{s, w, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    if std::env::var_os("PUNKTFUNK_NO_MIC_INSTALL").is_some() {
        return false;
    }
    let Some(path) = steam_driver_inf_path(inf_name) else {
        return false;
    };

    // SAFETY: a static NUL-terminated literal, loaded from System32 only (the flag), so this cannot
    // pick up a planted `newdev.dll` from the working directory. The handle is checked before use.
    let Ok(newdev) =
        (unsafe { LoadLibraryExW(w!("newdev.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        tracing::warn!("could not load newdev.dll — Steam-audio auto-install unavailable");
        return false;
    };
    // SAFETY: `newdev` is the live module just loaded; the export name is a static literal.
    let Some(addr) = (unsafe { GetProcAddress(newdev, s!("DiInstallDriverW")) }) else {
        return false;
    };
    // BOOL DiInstallDriverW(HWND hwndParent, PCWSTR InfPath, DWORD Flags, PBOOL NeedReboot)
    type DiInstall = unsafe extern "system" fn(HWND, PCWSTR, u32, *mut i32) -> i32;
    // SAFETY: `addr` is the non-null export just resolved and `DiInstall` mirrors its documented
    // signature (commented above).
    let f: DiInstall = unsafe { std::mem::transmute(addr) };
    // SAFETY: `path` is the expanded, NUL-terminated buffer above and outlives the call; a null
    // parent HWND and a null `NeedReboot` are both documented as accepted.
    let ok = unsafe {
        f(
            HWND(std::ptr::null_mut()),
            PCWSTR(path.as_ptr()),
            0,
            std::ptr::null_mut(),
        )
    } != 0;
    if ok {
        tracing::info!(
            inf = inf_name,
            "installed a Steam Streaming virtual audio device"
        );
        std::thread::sleep(Duration::from_secs(5)); // let the audio subsystem register the endpoint
    } else {
        // SAFETY: reads this thread's last-error value; takes no arguments and touches no memory.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        tracing::info!(
            inf = inf_name,
            ?err,
            "Steam-audio device not auto-installed (Steam absent / not admin) — see install guidance"
        );
    }
    ok
}

fn render_thread(
    queue: Arc<MicQueue>,
    stop: Arc<AtomicBool>,
    shared: Arc<RingShared>,
    ready: SyncSender<Result<String>>,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Open + start the render stream. The WASAPI objects must outlive the loop, so build them here and
    // keep them (a closure that *returned* them would drop them); on any failure report Err and exit.
    let setup = (|| -> Result<(wasapi::AudioClient, wasapi::AudioRenderClient, wasapi::Handle, i64, String)> {
        let (device, name) = resolve_target()?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // 48 kHz stereo f32; autoconvert lets WASAPI shared-mode SRC match the device mix format.
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS as usize,
            None,
        );
        let (default_period, _min) = audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        // Pre-fill the whole buffer with silence so the stream starts cleanly (no startup glitch).
        let buf_frames = audio_client.get_buffer_size().context("buffer size")? as usize;
        let _ = render_client.write_to_device(buf_frames, &vec![0u8; buf_frames * BLOCK_ALIGN], None);
        audio_client.start_stream().context("start render stream")?;
        Ok((audio_client, render_client, h_event, default_period, name))
    })();
    let (audio_client, render_client, h_event, default_period, name) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(name));
    // One device period in bytes (period is in 100 ns units; floor 10 ms if it reads absurd) —
    // the device-side pull granularity the adaptive prime threshold builds on.
    let period_bytes = ((default_period.max(0) as usize * SAMPLE_RATE as usize / 10_000_000)
        .max(SAMPLE_RATE as usize / 100))
        * BLOCK_ALIGN;

    // Any error below (endpoint invalidated/removed, engine restart) propagates out of the loop,
    // ending the thread — the `alive` flag flips in the spawn wrapper and the pump reopens.
    //
    // Jitter buffer (mirrors the Linux backend's process callback): clients push mic audio in
    // bursts on their own clock while the device pulls a block every period from an independent
    // clock, so a greedy per-period drain leaves the queue near-empty and pads most periods
    // with mid-stream silence — audible as constant crackling. Instead: emit silence until the
    // prime threshold is buffered, then play from the cushion (zero-filling only a momentary
    // shortfall), and re-prime only after a genuine FULL drain (the client went quiet — between
    // talk spurts the cushion rebuilds, and [`VirtualMic::discard`] resets it across session
    // gaps). The threshold = one device period + the pump's measured-jitter target
    // ([`VirtualMic::set_target_depth`]); the fixed [`PRIME_BYTES`] until the pump's first
    // estimate, and forever under `PUNKTFUNK_MIC_LEGACY_BUFFER=1` (the pump then never drives
    // the target).
    let mut buf: Vec<u8> = Vec::new();
    let mut primed = false;
    // Idle stop (see the module docs): `running` mirrors the stream's Start/Stop state, and
    // `idle_mark` is the queue length last seen while unprimed plus when it was first seen at
    // that length — IDLE_STOP_AFTER of unprimed output at an UNCHANGED length is the idle
    // verdict. Keying on the length (not just emptiness) covers a sub-prime tail a vanished
    // client left behind, while any genuinely new audio moves the length (a push appends a
    // whole frame and the drop-oldest cap sits above the prime threshold, so an unprimed queue
    // can never coincidentally return to its marked length) and restarts the window.
    let always_on = mic_always_on();
    let mut running = true;
    let mut idle_mark: Option<(usize, Instant)> = None;
    'render: while !stop.load(Ordering::Relaxed) {
        if !running {
            // Idle-stopped: park on the condvar until mic audio arrives (push notifies on the
            // empty→non-empty edge); the timeout only keeps `stop` responsive.
            {
                let (lock, wake) = &*queue;
                let mut q = lock.lock().unwrap();
                while q.is_empty() {
                    if stop.load(Ordering::Relaxed) {
                        break 'render;
                    }
                    let (guard, _timed_out) = wake.wait_timeout(q, IDLE_WAKE_CHECK).unwrap();
                    q = guard;
                }
            }
            // A resume failure means the endpoint died while we slept — propagate, so the
            // thread exits, `alive` flips, and the pump reopens (fresh plan) as for any death.
            audio_client
                .start_stream()
                .context("resume render stream")?;
            running = true;
            idle_mark = None;
            tracing::debug!("virtual mic stream resumed (client mic audio arrived)");
        }
        // The device signals when it wants more data; finite timeout keeps `stop` responsive.
        if h_event.wait_for_event(100).is_err() {
            continue;
        }
        let space = audio_client
            .get_available_space_in_frames()
            .context("available space")? as usize;
        if space == 0 {
            continue;
        }
        let need = space * BLOCK_ALIGN;
        if buf.len() < need {
            buf.resize(need, 0);
        }
        let target = shared.target_bytes.load(Ordering::Relaxed);
        let prime = if target == 0 {
            PRIME_BYTES
        } else {
            period_bytes + target
        };
        shared.prime_bytes.store(prime, Ordering::Relaxed);
        // Silence base; overwrite with queued mic PCM once the cushion is primed.
        buf[..need].fill(0);
        {
            let mut q = queue.0.lock().unwrap();
            if !primed && q.len() >= prime {
                primed = true;
            }
            if primed {
                let n = q.len().min(need);
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                if q.is_empty() {
                    primed = false; // fully drained — re-prime before producing again
                    shared.reprimes.fetch_add(1, Ordering::Relaxed);
                }
            }
            if primed {
                idle_mark = None;
            } else if !always_on {
                // Unprimed = this period was pure silence. After IDLE_STOP_AFTER of that at an
                // unchanged queue length, stop the stream so the box can sleep. Decided under
                // the queue lock, and only when nothing has arrived for the whole window (see
                // `idle_mark`'s docs) — a burst landing at the boundary moves the length and
                // resets the window instead of being cleared. What IS cleared is ≥10 s stale
                // (the pump's stale-gap discard would drop it before the next real frame
                // anyway), which keeps "queue empty" ⇔ "nothing to play" for the wait above.
                match idle_mark {
                    Some((len, since)) if len == q.len() => {
                        if since.elapsed() >= IDLE_STOP_AFTER {
                            q.clear();
                            audio_client
                                .stop_stream()
                                .context("idle-stop render stream")?;
                            running = false;
                            idle_mark = None;
                            tracing::debug!(
                                "virtual mic stream idle-stopped (releases the sleep-blocking \
                                 audio power request; next mic frame resumes it)"
                            );
                            continue 'render;
                        }
                    }
                    _ => idle_mark = Some((q.len(), Instant::now())),
                }
            }
        }
        render_client
            .write_to_device(space, &buf[..need], None)
            .context("write_to_device")?;
    }
    audio_client.stop_stream().ok();
    Ok(())
}
