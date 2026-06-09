//! PipeWire audio capture of the default sink's monitor (system output).
//!
//! Connects to the user's PipeWire daemon (via `XDG_RUNTIME_DIR`, inherited from the Sway
//! session) and opens an input stream with `stream.capture.sink=true`, which routes the
//! default sink's monitor into us — no portal needed (unlike screen capture). The (`!Send`)
//! MainLoop/Stream live on a dedicated thread; interleaved `f32` chunks leave over a bounded
//! channel (dropped if the encoder falls behind, never blocking the PipeWire loop).

use super::{AudioCapturer, CHANNELS, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

pub struct PwAudioCapturer {
    chunks: Receiver<Vec<f32>>,
}

impl PwAudioCapturer {
    pub fn open() -> Result<PwAudioCapturer> {
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        thread::Builder::new()
            .name("lumen-pw-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(tx) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire audio thread failed");
                }
            })
            .context("spawn pipewire audio thread")?;
        Ok(PwAudioCapturer { chunks: rx })
    }
}

impl AudioCapturer for PwAudioCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            Err(RecvTimeoutError::Timeout) => Err(anyhow!("no PipeWire audio within 5s")),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire audio thread ended")),
        }
    }
}

fn pw_thread(tx: std::sync::mpsc::SyncSender<Vec<f32>>) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    crate::pwinit::ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw audio MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw audio Context")?;
    let core = context
        .connect_rc(None)
        .context("pw audio connect (is PipeWire running in this session?)")?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "lumen-audio",
        properties! {
            *pw::keys::MEDIA_TYPE          => "Audio",
            *pw::keys::MEDIA_CATEGORY      => "Capture",
            *pw::keys::MEDIA_ROLE          => "Music",
            // Capture the default sink's monitor (system output), not a microphone.
            *pw::keys::STREAM_CAPTURE_SINK => "true",
            // Ask for a ~5ms quantum (= one Opus frame) so buffers arrive smoothly rather than
            // in large bursts the client's low-latency jitter buffer would hear as glitching.
            *pw::keys::NODE_LATENCY        => "240/48000",
        },
    )
    .context("pw audio Stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(tx)
        .state_changed(|_s, _ud, old, new| {
            tracing::info!(?old, ?new, "pipewire audio stream state");
        })
        .param_changed(|_stream, _tx, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = AudioInfoRaw::default();
            if info.parse(param).is_ok() {
                tracing::info!(
                    format = ?info.format(),
                    rate = info.rate(),
                    channels = info.channels(),
                    "audio format negotiated"
                );
            }
        })
        .process(|stream, tx| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let d = &mut datas[0];
                let (offset, size) = {
                    let c = d.chunk();
                    (c.offset() as usize, c.size() as usize)
                };
                let Some(buf) = d.data() else { return };
                if offset > buf.len() {
                    return;
                }
                let region = &buf[offset..(offset + size).min(buf.len())];
                // Negotiated as F32LE; reinterpret the byte region as interleaved f32.
                let n = region.len() / 4;
                static FIRST: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(samples = n, frames = n / 2, "audio first capture buffer");
                }
                let mut samples = Vec::with_capacity(n);
                for i in 0..n {
                    let b = [
                        region[i * 4],
                        region[i * 4 + 1],
                        region[i * 4 + 2],
                        region[i * 4 + 3],
                    ];
                    samples.push(f32::from_le_bytes(b));
                }
                let _ = tx.try_send(samples); // drop if the encoder is behind
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire audio callback — chunk dropped");
            }
        })
        .register()
        .context("register audio stream listener")?;

    // Request F32LE, 48 kHz, stereo.
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize audio format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("audio pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None, // PW_ID_ANY — autoconnect to the default sink monitor
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw audio stream connect")?;

    mainloop.run();
    Ok(())
}
