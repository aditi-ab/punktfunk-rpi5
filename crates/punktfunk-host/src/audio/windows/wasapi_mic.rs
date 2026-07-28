//! WASAPI virtual microphone (Windows) — the inverse of [`super::wasapi_cap`]. Windows has no
//! user-mode way to *create* a capture (microphone) endpoint, so we target an EXISTING virtual audio
//! device and write the client's decoded mic PCM into that device's **render** endpoint; the device's
//! **capture** endpoint then surfaces as a microphone that host apps can record from.
//!
//! The target comes from the [`audio_control::wire_now`] plan (recomputed on every open): VB-Audio
//! "CABLE Input" (bundled by the installer — the dedicated mic target), the Steam Streaming
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
//! `push` enqueues decoded interleaved-f32 PCM into a bounded ring (drop-oldest beyond ~120 ms so
//! mic latency stays bounded); a dedicated COM-apartment thread renders it event-driven through an
//! adaptive jitter buffer (prime → hold → re-prime, see the render loop — clients arrive in bursts,
//! the device pulls per-period), filling silence when the client isn't talking. WASAPI objects are
//! `!Send`, so they live entirely on that thread (mirrors `WasapiLoopbackCapturer`).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{audio_control, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

const CHANNELS: u32 = 2;
/// 48 kHz stereo f32: 2 channels * 4 bytes.
const BLOCK_ALIGN: usize = 2 * 4;
/// Jitter-buffer priming depth (~48 ms): the render loop emits pure silence until this much PCM
/// is queued, then plays from the cushion. Clients deliver mic audio in BURSTS (the Mac client's
/// input tap yields ~two 20 ms Opus packets every ~42 ms) while WASAPI pulls a small block every
/// device period (~10 ms) — with no cushion the queue sits near-empty and most periods insert
/// mid-stream silence: the "crackling mic" (heard live, Mac → Windows host 2026-07-03; the Linux
/// backend's process callback primes the same way and the identical stream was clean there). The
/// depth must cover the worst inter-burst gap (~42 ms), so ~48 ms with re-prime on a full drain.
const PRIME_BYTES: usize = (SAMPLE_RATE as usize * 48 / 1000) * BLOCK_ALIGN;
/// Bound the inject queue at ~120 ms so the passed-through mic stays low-latency (drop oldest
/// beyond): the priming cushion (~48 ms) plus arrival-burst headroom.
const MAX_QUEUE_BYTES: usize = (SAMPLE_RATE as usize * 120 / 1000) * BLOCK_ALIGN;

pub struct WasapiVirtualMic {
    queue: Arc<Mutex<VecDeque<u8>>>,
    stop: Arc<AtomicBool>,
    /// False once the render thread has exited (device error or stop) — the pump's reopen signal.
    alive: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WasapiVirtualMic {
    pub fn open(channels: u32) -> Result<Self> {
        anyhow::ensure!(
            channels == CHANNELS,
            "virtual mic is stereo-only (got {channels})"
        );
        let queue = Arc::new(Mutex::new(VecDeque::<u8>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        // Bring-up handshake: report the resolved device (or the error) before returning, so a missing
        // virtual-mic device surfaces as Err (the caller retries with backoff) not a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<String>>(1);
        let (q, st, al) = (queue.clone(), stop.clone(), alive.clone());
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-mic".into())
            .spawn(move || {
                if let Err(e) = render_thread(q, st, ready_tx) {
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
        let Ok(mut q) = self.queue.lock() else {
            return false;
        };
        q.reserve(pcm.len() * 4);
        for &s in pcm {
            q.extend(s.to_le_bytes());
        }
        // Drop-oldest to keep latency bounded (mic is real-time; stale audio is worse than dropped).
        if q.len() > MAX_QUEUE_BYTES {
            let excess = q.len() - MAX_QUEUE_BYTES;
            q.drain(..excess);
        }
        true
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn discard(&self) {
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
    }

    fn channels(&self) -> u32 {
        CHANNELS
    }
}

/// Resolve the mic inject target from the wiring plan, auto-installing the Steam Streaming pair
/// when nothing usable exists (then re-planning). Runs on the COM-initialized render thread.
fn resolve_target() -> Result<(wasapi::Device, String)> {
    // set_playback=false: the mic pump runs while the host is idle — only the desktop-audio
    // capture may park the playback default (on the silent sink) for a stream's lifetime.
    let mut wiring = audio_control::wire_now(false);
    if wiring.mic_render.is_none() {
        tracing::info!("no usable virtual mic device present — attempting auto-install");
        if install_steam_audio_pair() {
            wiring = audio_control::wire_now(false);
        }
    }
    let Some(ep) = wiring.mic_render else {
        anyhow::bail!(
            "no virtual-mic render endpoint on this box. Install VB-Audio Virtual Cable (the host \
             installer bundles it) or enable Steam Remote Play's microphone (Steam Streaming \
             Microphone), or set PUNKTFUNK_MIC_DEVICE=<friendly-name substring>."
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
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    if std::env::var_os("PUNKTFUNK_NO_MIC_INSTALL").is_some() {
        return false;
    }
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
        return false;
    }

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
    queue: Arc<Mutex<VecDeque<u8>>>,
    stop: Arc<AtomicBool>,
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
    let setup = (|| -> Result<(wasapi::AudioClient, wasapi::AudioRenderClient, wasapi::Handle, String)> {
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
        Ok((audio_client, render_client, h_event, name))
    })();
    let (audio_client, render_client, h_event, name) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(name));

    // Any error below (endpoint invalidated/removed, engine restart) propagates out of the loop,
    // ending the thread — the `alive` flag flips in the spawn wrapper and the pump reopens.
    //
    // Adaptive jitter buffer (mirrors the Linux backend's process callback): clients push mic
    // audio in bursts on their own clock while the device pulls a block every period from an
    // independent clock, so a greedy per-period drain leaves the queue near-empty and pads most
    // periods with mid-stream silence — audible as constant crackling. Instead: emit silence
    // until [`PRIME_BYTES`] is buffered, then play from the cushion (zero-filling only a
    // momentary shortfall), and re-prime only after a genuine FULL drain (the client went quiet —
    // between talk spurts the cushion rebuilds, and [`VirtualMic::discard`] resets it across
    // session gaps).
    let mut buf: Vec<u8> = Vec::new();
    let mut primed = false;
    while !stop.load(Ordering::Relaxed) {
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
        // Silence base; overwrite with queued mic PCM once the cushion is primed.
        buf[..need].fill(0);
        {
            let mut q = queue.lock().unwrap();
            if !primed && q.len() >= PRIME_BYTES {
                primed = true;
            }
            if primed {
                let n = q.len().min(need);
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                if q.is_empty() {
                    primed = false; // fully drained — re-prime before producing again
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
