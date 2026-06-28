//! WASAPI loopback capture of the default render endpoint (system output) — the Windows analogue
//! of the PipeWire sink-monitor backend. Delivers interleaved f32 PCM at 48 kHz in the requested
//! channel count (stereo / 5.1 / 7.1, canonical wire order FL FR FC LFE RL RR SL SR via the
//! explicit `dwChannelMask`), ready for the Opus path with NO resampling (WASAPI shared-mode
//! autoconvert does any SRC + up/downmix to the requested layout). WASAPI objects are
//! COM-apartment-bound and not `Send`, so they live on a dedicated thread (mirrors
//! `linux::PwAudioCapturer`); only the channel + stop flag + join handle are in the struct.

use super::{AudioCapturer, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub struct WasapiLoopbackCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WasapiLoopbackCapturer {
    pub fn open(channels: u32) -> Result<WasapiLoopbackCapturer> {
        anyhow::ensure!(
            matches!(channels, 2 | 6 | 8),
            "WASAPI loopback backend supports 2/6/8 channels (got {channels})"
        );
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        // Bring-up handshake: report open success/failure before returning, so a missing render
        // endpoint surfaces as Err (caller continues without audio) rather than a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let stop_t = stop.clone();
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-audio".into())
            .spawn(move || {
                if let Err(e) = capture_thread(tx, stop_t, ready_tx, channels) {
                    tracing::error!(error = format!("{e:#}"), "wasapi loopback thread failed");
                }
            })
            .context("spawn wasapi audio thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                tracing::info!(
                    channels,
                    "WASAPI loopback capture: 48 kHz f32 (default render endpoint)"
                );
                Ok(WasapiLoopbackCapturer {
                    chunks: rx,
                    channels,
                    stop,
                    join: Some(join),
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!(
                "wasapi loopback init timed out (no default render endpoint?)"
            )),
        }
    }
}

impl Drop for WasapiLoopbackCapturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl AudioCapturer for WasapiLoopbackCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet sink is NOT a failure — return an empty chunk so the caller keeps the capturer
            // alive. Only a dead capture thread is an Err (→ caller reopens). Matches the Linux path.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("wasapi audio thread ended")),
        }
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
    }
}

fn capture_thread(
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    channels: u32,
) -> Result<()> {
    // Interleaved f32: channels * 4 bytes per frame.
    let block_align = channels as usize * 4;
    // COM must be initialized on THIS thread (MTA), before any device call.
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    let res = (|| -> Result<()> {
        // Loopback = capture the RENDER endpoint: get the default render device, but open a CAPTURE
        // client with loopback=true over it. NOTE: the virtual mic (`super::wasapi_mic`) is guarded
        // to NEVER target this same endpoint — otherwise the client's injected mic would be captured
        // here and streamed back to the client (infinite echo). Keep that guard in sync if this
        // device selection ever changes.
        let device = DeviceEnumerator::new()
            .context("DeviceEnumerator")?
            .get_default_device(&Direction::Render)
            .context("default render endpoint (loopback needs a render device)")?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // 48 kHz f32 interleaved in the requested channel layout; autoconvert lets WASAPI's
        // shared-mode SRC match the engine mix format to ours (incl. up/downmix to the requested
        // channel count), so we never resample/remix in Rust. The explicit dwChannelMask pins the
        // wire order (FL FR FC LFE RL RR SL SR; 7.1 = 0x63F, not 0xFF). Loopback is implied by
        // capturing a RENDER device with Direction::Capture in shared mode (STREAMFLAGS_LOOPBACK).
        let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            channels as usize,
            Some(mask),
        );
        let (default_period, _min_period) =
            audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Capture, &mode)
            .context("initialize loopback client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .context("IAudioCaptureClient")?;
        audio_client
            .start_stream()
            .context("start loopback stream")?;
        let _ = ready.send(Ok(()));

        let mut bytes: VecDeque<u8> = VecDeque::new();
        while !stop.load(Ordering::Relaxed) {
            // Loopback fires events only while audio renders; the finite timeout keeps `stop` responsive.
            if h_event.wait_for_event(100).is_err() {
                continue;
            }
            loop {
                match capture_client.get_next_packet_size() {
                    Ok(Some(0)) | Ok(None) => break,
                    Ok(Some(_n)) => {
                        capture_client
                            .read_from_device_to_deque(&mut bytes)
                            .context("read loopback")?;
                    }
                    Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
                }
            }
            let whole = (bytes.len() / block_align) * block_align;
            if whole == 0 {
                continue;
            }
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            let mut samples = Vec::with_capacity(whole / 4);
            for c in raw.chunks_exact(4) {
                samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            let _ = tx.try_send(samples); // non-blocking, lossy — same discipline as PipeWire
        }
        audio_client.stop_stream().ok();
        Ok(())
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live loopback round trip — skipped unless `PUNKTFUNK_WASAPI_LIVE=1` and a render endpoint
    /// exists. Opens the capturer and pulls one chunk of interleaved f32.
    #[test]
    fn live_open_and_read() {
        if std::env::var("PUNKTFUNK_WASAPI_LIVE").is_err() {
            return;
        }
        let mut cap = match WasapiLoopbackCapturer::open(2) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no render endpoint on this box ({e:#}) — skipping");
                return;
            }
        };
        assert_eq!(cap.channels(), 2);
        match cap.next_chunk() {
            Ok(samples) => assert!(
                samples.len() % 2 == 0,
                "interleaved stereo => even sample count"
            ),
            Err(e) => eprintln!("no audio within timeout (silent system?): {e:#}"),
        }
    }
}
