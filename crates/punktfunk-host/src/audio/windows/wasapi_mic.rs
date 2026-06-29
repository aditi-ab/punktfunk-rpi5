//! WASAPI virtual microphone (Windows) — the inverse of [`super::wasapi_cap`]. Windows has no
//! user-mode way to *create* a capture (microphone) endpoint, so we target an EXISTING virtual audio
//! device and write the client's decoded mic PCM into that device's **render** endpoint; the device's
//! **capture** endpoint then surfaces as a microphone that host apps can record from.
//!
//! Target device, by friendly-name substring (first match wins; override with `PUNKTFUNK_MIC_DEVICE`):
//! VB-Audio "CABLE Input" (bundled by the installer — the preferred, dedicated mic target), the
//! "Steam Streaming Microphone", VoiceMeeter, or anything with "virtual" in the name.
//! [`super::audio_control`] sets the default playback to a DIFFERENT loopback-capable device so the
//! chosen mic is never the endpoint the loopback captures. If no candidate is present we auto-install
//! the Steam Streaming audio pair (see [`install_steam_audio_pair`]); failing that we return an error
//! with install guidance and the host runs without mic passthrough.
//!
//! **Anti-echo guard (the whole point of this being non-trivial).** The desktop-audio plane
//! ([`super::wasapi_cap`]) loopback-captures the **default render endpoint**. WASAPI loopback
//! captures the *mixed* output of an endpoint — i.e. everything any app renders to it, including
//! what THIS module writes. So if the virtual-mic target is the same device the loopback captures,
//! the client's uplinked mic is captured straight back into the host→client audio stream: an
//! infinite echo. [`find_device`] therefore **excludes the default render endpoint** from the
//! candidates — the mic is guaranteed to land on a different device. (Linux gets this for free: its
//! mic is a dedicated `Audio/Source` node, structurally separate from the monitored sink.)
//!
//! `push` enqueues decoded interleaved-f32 PCM into a bounded ring (drop-oldest beyond ~80 ms so mic
//! latency stays bounded); a dedicated COM-apartment thread renders it event-driven, filling silence
//! when the client isn't talking. WASAPI objects are `!Send`, so they live entirely on that thread
//! (mirrors `WasapiLoopbackCapturer`).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{VirtualMic, SAMPLE_RATE};
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
/// Bound the inject queue at ~80 ms so the passed-through mic stays low-latency (drop oldest beyond).
const MAX_QUEUE_BYTES: usize = (SAMPLE_RATE as usize * 80 / 1000) * BLOCK_ALIGN;

/// Render-endpoint friendly-name substrings (lowercased) we can write into so the device's capture
/// endpoint becomes a host mic. Ordered by preference.
const CANDIDATES: &[&str] = &[
    "cable input", // VB-Audio Virtual Cable — bundled by the installer; the preferred dedicated mic target
    "steam streaming microphone",
    "voicemeeter input",
    "voicemeeter aux input",
    "virtual",
];

pub struct WasapiVirtualMic {
    queue: Arc<Mutex<VecDeque<u8>>>,
    stop: Arc<AtomicBool>,
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
        // Bring-up handshake: report the resolved device (or the error) before returning, so a missing
        // virtual-mic device surfaces as Err (the caller retries with backoff) not a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<String>>(1);
        let (q, st) = (queue.clone(), stop.clone());
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-mic".into())
            .spawn(move || {
                if let Err(e) = render_thread(q, st, ready_tx) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi virtual-mic thread failed");
                }
            })
            .context("spawn wasapi mic thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(name)) => {
                tracing::info!(device = %name,
                    "WASAPI virtual mic ready (client mic → this device's render endpoint)");
                Ok(WasapiVirtualMic {
                    queue,
                    stop,
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
    fn push(&self, pcm: &[f32]) {
        let Ok(mut q) = self.queue.lock() else {
            return;
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
    }
    fn channels(&self) -> u32 {
        CHANNELS
    }
}

/// The endpoint ID of the device the desktop-audio loopback records (the **default render
/// endpoint**, see [`super::wasapi_cap`]). The virtual mic must never target this device — injecting
/// there echoes the client's mic back into the host→client audio stream. `None` if it can't be
/// resolved (then [`find_device`] can't prove a candidate is safe and falls back to name-only
/// matching — no worse than before the guard existed).
fn default_render_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Render)
        .ok()?
        .get_id()
        .ok()
}

/// Resolve the virtual-mic target among render endpoints by friendly-name, **excluding the endpoint
/// the loopback captures** (the [`default_render_id`] anti-echo guard). Logs all candidates so a
/// missing/skipped device is diagnosable.
fn find_device() -> Result<wasapi::Device> {
    let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
    let collection = enumerator
        .get_device_collection(&Direction::Render)
        .context("render device collection")?;
    let n = collection.get_nbr_devices().context("device count")?;
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    // The device the loopback captures — a name match on it is rejected below (would echo).
    let loopback_id = default_render_id();
    let mut names = Vec::new();
    let mut found = None;
    let mut skipped_loopback = false;
    for i in 0..n {
        let Ok(dev) = collection.get_device_at_index(i) else {
            continue;
        };
        let name = dev.get_friendlyname().unwrap_or_default();
        let lname = name.to_lowercase();
        let hit = match &want {
            Some(w) => lname.contains(w),
            None => CANDIDATES.iter().any(|c| lname.contains(c)),
        };
        if hit && found.is_none() {
            // Anti-echo guard: never inject into the endpoint the loopback captures.
            let is_loopback = match (dev.get_id().ok(), loopback_id.as_deref()) {
                (Some(id), Some(lb)) => id == lb,
                _ => false,
            };
            if is_loopback {
                skipped_loopback = true;
                tracing::warn!(device = %name,
                    "virtual-mic candidate is the loopback (default render) endpoint — skipping; \
                     injecting there would echo the client's mic into the desktop-audio stream");
            } else {
                found = Some(dev);
            }
        }
        names.push(name);
    }
    found.ok_or_else(|| {
        if skipped_loopback {
            anyhow!(
                "the only virtual-mic candidate among render endpoints {names:?} is the default \
                 playback device the host loopback-captures — injecting there would echo the mic \
                 back to the client. Add a SEPARATE virtual audio device for the mic (e.g. the Steam \
                 Streaming Microphone) or set a different default playback device, then reconnect."
            )
        } else {
            anyhow!(
                "no virtual-mic device among render endpoints {names:?}. Install VB-Audio Virtual \
                 Cable or enable Steam Remote Play's microphone (Steam Streaming Microphone), or set \
                 PUNKTFUNK_MIC_DEVICE=<friendly-name substring>."
            )
        }
    })
}

/// Find the virtual-mic device, and if none exists, try to AUTO-INSTALL one so mic passthrough works
/// out of the box (then re-find). Falls back to the guidance error if nothing can be installed.
fn find_or_install_device() -> Result<wasapi::Device> {
    match find_device() {
        Ok(d) => Ok(d),
        Err(e) => {
            tracing::info!("no usable virtual mic device present — attempting auto-install");
            // SAFETY: `install_steam_audio_pair` is `unsafe` only because it `LoadLibraryExW`s
            // `newdev.dll` and calls `DiInstallDriverW` through a `transmute`d function pointer;
            // calling it imposes no extra precondition here (it takes no args and aliases nothing).
            // Its internal contract holds: the `DiInstall` type matches the documented
            // `BOOL DiInstallDriverW(HWND, PCWSTR, DWORD, PBOOL)` ABI, and it passes a
            // NUL-terminated UTF-16 INF path with null/zero optional args. Invoked once on the
            // dedicated mic thread.
            if unsafe { install_steam_audio_pair() } {
                find_device()
            } else {
                Err(e)
            }
        }
    }
}

/// Best-effort: install BOTH Steam Streaming audio devices (the "Steam pair") so mic passthrough
/// works out of the box and the host has a desktop-audio sink distinct from the mic. Steam Remote
/// Play ships `SteamStreamingMicrophone.inf` + `SteamStreamingSpeakers.inf`: the microphone gives the
/// virtual mic a target whose **capture** endpoint apps record from, and the speakers give a
/// **render** endpoint a headless box can loopback-capture that is NOT the mic — so the loopback and
/// the mic land on different devices and never echo (see [`find_device`]). Returns true if either
/// installed. No-op when Steam isn't installed (INFs absent), the install is denied (needs admin —
/// the host runs as SYSTEM), or `PUNKTFUNK_NO_MIC_INSTALL` is set.
unsafe fn install_steam_audio_pair() -> bool {
    // Microphone first (the mic's actual target); speakers second (the distinct desktop-audio sink).
    let mic = try_install_steam_audio("SteamStreamingMicrophone.inf");
    let spk = try_install_steam_audio("SteamStreamingSpeakers.inf");
    mic || spk
}

/// Install one Steam Streaming driver INF by filename via `DiInstallDriverW` (loaded from
/// `newdev.dll`, like Apollo, to avoid an extra windows-crate feature). See
/// [`install_steam_audio_pair`] for the contract; `inf_name` is a bare filename under Steam's
/// per-arch `drivers\Windows10\{arch}\` directory.
unsafe fn try_install_steam_audio(inf_name: &str) -> bool {
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
    let n = ExpandEnvironmentStringsW(PCWSTR(template.as_ptr()), Some(path.as_mut_slice()));
    if n == 0 || n as usize > path.len() {
        return false;
    }

    let Ok(newdev) = LoadLibraryExW(w!("newdev.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else {
        tracing::warn!("could not load newdev.dll — Steam-audio auto-install unavailable");
        return false;
    };
    let Some(addr) = GetProcAddress(newdev, s!("DiInstallDriverW")) else {
        return false;
    };
    // BOOL DiInstallDriverW(HWND hwndParent, PCWSTR InfPath, DWORD Flags, PBOOL NeedReboot)
    type DiInstall = unsafe extern "system" fn(HWND, PCWSTR, u32, *mut i32) -> i32;
    let f: DiInstall = std::mem::transmute(addr);
    let ok = f(
        HWND(std::ptr::null_mut()),
        PCWSTR(path.as_ptr()),
        0,
        std::ptr::null_mut(),
    ) != 0;
    if ok {
        tracing::info!(
            inf = inf_name,
            "installed a Steam Streaming virtual audio device"
        );
        std::thread::sleep(Duration::from_secs(5)); // let the audio subsystem register the endpoint
    } else {
        let err = windows::Win32::Foundation::GetLastError();
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
        let device = find_or_install_device()?;
        let name = device.get_friendlyname().unwrap_or_else(|_| "virtual mic".into());
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

    let mut buf: Vec<u8> = Vec::new();
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
        // Silence base; overwrite with queued mic PCM (zero-pad the tail when the client is quiet).
        buf[..need].fill(0);
        {
            let mut q = queue.lock().unwrap();
            let n = q.len().min(need);
            for (i, b) in q.drain(..n).enumerate() {
                buf[i] = b;
            }
        }
        render_client
            .write_to_device(space, &buf[..need], None)
            .context("write_to_device")?;
    }
    audio_client.stop_stream().ok();
    Ok(())
}
