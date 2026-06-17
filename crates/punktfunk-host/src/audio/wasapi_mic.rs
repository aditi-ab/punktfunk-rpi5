//! WASAPI virtual microphone (Windows) — the inverse of [`super::wasapi_cap`]. Windows has no
//! user-mode way to *create* a capture (microphone) endpoint, so we target an EXISTING virtual audio
//! device and write the client's decoded mic PCM into that device's **render** endpoint; the device's
//! **capture** endpoint then surfaces as a microphone that host apps can record from.
//!
//! Target device, by friendly-name substring (first match wins; override with `PUNKTFUNK_MIC_DEVICE`):
//! "Steam Streaming Microphone" (ships with Steam Remote Play — exactly this purpose), VB-Audio
//! "CABLE Input", VoiceMeeter, or anything with "virtual" in the name. If none is present we return an
//! error with install guidance and the host runs without mic passthrough.
//!
//! `push` enqueues decoded interleaved-f32 PCM into a bounded ring (drop-oldest beyond ~80 ms so mic
//! latency stays bounded); a dedicated COM-apartment thread renders it event-driven, filling silence
//! when the client isn't talking. WASAPI objects are `!Send`, so they live entirely on that thread
//! (mirrors `WasapiLoopbackCapturer`).

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
    "steam streaming microphone",
    "cable input",
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

/// Resolve the virtual-mic target among render endpoints by friendly-name. Logs all candidates so a
/// missing device is diagnosable.
fn find_device() -> Result<wasapi::Device> {
    let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
    let collection = enumerator
        .get_device_collection(&Direction::Render)
        .context("render device collection")?;
    let n = collection.get_nbr_devices().context("device count")?;
    let want = std::env::var("PUNKTFUNK_MIC_DEVICE")
        .ok()
        .map(|s| s.to_lowercase());
    let mut names = Vec::new();
    let mut found = None;
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
            found = Some(dev);
        }
        names.push(name);
    }
    found.ok_or_else(|| {
        anyhow!(
            "no virtual-mic device among render endpoints {names:?}. Install VB-Audio Virtual Cable \
             or enable Steam Remote Play's microphone (Steam Streaming Microphone), or set \
             PUNKTFUNK_MIC_DEVICE=<friendly-name substring>."
        )
    })
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
        let device = find_device()?;
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
