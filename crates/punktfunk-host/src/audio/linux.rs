//! PipeWire audio capture of the default sink's monitor (system output).
//!
//! Connects to the user's PipeWire daemon (via `XDG_RUNTIME_DIR`, inherited from the Sway
//! session) and opens an input stream with `stream.capture.sink=true`, which routes the
//! default sink's monitor into us — no portal needed (unlike screen capture). The (`!Send`)
//! MainLoop/Stream live on a dedicated thread; interleaved `f32` chunks leave over a bounded
//! channel (dropped if the encoder falls behind, never blocking the PipeWire loop).
//!
//! The stream is opened at the *session's* channel count (2/6/8). If the sink has fewer
//! channels than requested, PipeWire's channel-mixer fills the extra positions with silence
//! (zero upmix), so a stereo desktop still produces a valid 5.1/7.1 capture. Dropping the
//! capturer quits the loop thread (via a `pipewire::channel` Terminate message), tearing the
//! stream down promptly — required so a surround session can replace a stereo capturer
//! without leaking a PipeWire consumer (see CLAUDE.md: a wedged link head-blocks the daemon).

use super::{AudioCapturer, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Message asking the PipeWire loop thread to quit (sent from `Drop`).
struct Terminate;

pub struct PwAudioCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
}

impl PwAudioCapturer {
    pub fn open(channels: u32) -> Result<PwAudioCapturer> {
        anyhow::ensure!(
            matches!(channels, 1 | 2 | 6 | 8),
            "unsupported audio channel count {channels} (want 2, 6 or 8)"
        );
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        thread::Builder::new()
            .name("punktfunk-pw-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(tx, quit_rx, channels) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire audio thread failed");
                }
            })
            .context("spawn pipewire audio thread")?;
        Ok(PwAudioCapturer {
            chunks: rx,
            channels,
            quit: quit_tx,
        })
    }
}

impl Drop for PwAudioCapturer {
    fn drop(&mut self) {
        // Ask the loop thread to quit; the stream/core/loop unwind there (RAII). A failed
        // send means the thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
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

    fn channels(&self) -> u32 {
        self.channels
    }

    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
    }
}

/// SPA channel position array for the GameStream surround order FL FR FC LFE RL RR [SL SR]
/// (= the PipeWire/PulseAudio default map for 6/8 channels, and the order Moonlight's
/// renderers expect — moonlight-common-c: "we use FL FR C LFE RL RR SL SR"). Values are
/// `enum spa_audio_channel` (spa/param/audio/raw.h): FL=3 FR=4 FC=5 LFE=6 SL=7 SR=8 RL=12
/// RR=13.
fn spa_positions(channels: u32) -> [u32; 64] {
    const FL: u32 = 3;
    const FR: u32 = 4;
    const FC: u32 = 5;
    const LFE: u32 = 6;
    const SL: u32 = 7;
    const SR: u32 = 8;
    const RL: u32 = 12;
    const RR: u32 = 13;
    const MONO: u32 = 2;
    let mut pos = [0u32; 64];
    let order: &[u32] = match channels {
        1 => &[MONO],
        2 => &[FL, FR],
        6 => &[FL, FR, FC, LFE, RL, RR],
        8 => &[FL, FR, FC, LFE, RL, RR, SL, SR],
        _ => unreachable!("validated in open()"),
    };
    pos[..order.len()].copy_from_slice(order);
    pos
}

/// Virtual microphone: a PipeWire `Audio/Source` node host apps can record from. The host pushes
/// decoded client-mic PCM in; the loop thread's producer callback drains it (silence on
/// underrun) into PipeWire buffers. Mirrors [`PwAudioCapturer`] but inverted (Direction::Output).
pub struct PwMicSource {
    pcm: std::sync::mpsc::SyncSender<Vec<f32>>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
}

impl PwMicSource {
    pub fn open(channels: u32) -> Result<PwMicSource> {
        anyhow::ensure!(
            matches!(channels, 1 | 2),
            "virtual mic supports 1 or 2 channels, got {channels}"
        );
        let (pcm_tx, pcm_rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        thread::Builder::new()
            .name("punktfunk-pw-mic".into())
            .spawn(move || {
                if let Err(e) = mic_pw_thread(pcm_rx, quit_rx, channels) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire virtual-mic thread failed");
                }
            })
            .context("spawn pipewire virtual-mic thread")?;
        Ok(PwMicSource {
            pcm: pcm_tx,
            channels,
            quit: quit_tx,
        })
    }
}

impl Drop for PwMicSource {
    fn drop(&mut self) {
        let _ = self.quit.send(Terminate);
    }
}

impl VirtualMic for PwMicSource {
    fn push(&self, pcm: &[f32]) {
        let _ = self.pcm.try_send(pcm.to_vec()); // drop if the PipeWire side is behind
    }
    fn channels(&self) -> u32 {
        self.channels
    }
}

/// Producer-side state for the virtual-mic loop: incoming decoded PCM and a small ring buffer
/// the process callback drains into PipeWire buffers (capped, so latency stays bounded).
/// `primed` is a jitter buffer gate — see the process callback.
struct MicUserData {
    rx: Receiver<Vec<f32>>,
    ring: VecDeque<f32>,
    channels: usize,
    primed: bool,
}

fn mic_pw_thread(
    pcm_rx: Receiver<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    crate::pwinit::ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw mic MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw mic Context")?;
    let core = context
        .connect_rc(None)
        .context("pw mic connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    // media.class=Audio/Source advertises us as a microphone (a recordable source), NOT a
    // playback stream — without it, Direction::Output + Playback would route to the speakers.
    let stream = pw::stream::StreamBox::new(
        &core,
        "punktfunk-mic",
        properties! {
            *pw::keys::MEDIA_TYPE        => "Audio",
            *pw::keys::MEDIA_CLASS       => "Audio/Source",
            *pw::keys::NODE_NAME         => "punktfunk-mic",
            *pw::keys::NODE_DESCRIPTION  => "Punktfunk Remote Microphone",
            // ~5 ms quantum (one Opus frame) so recording apps get smooth low-latency chunks.
            *pw::keys::NODE_LATENCY      => "240/48000",
        },
    )
    .context("pw mic Stream")?;

    let ud = MicUserData {
        rx: pcm_rx,
        ring: VecDeque::new(),
        channels: channels as usize,
        primed: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::info!(?old, ?new, "pipewire virtual-mic stream state");
        })
        .param_changed(|_s, _ud, id, param| {
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
                    "virtual-mic format negotiated"
                );
            }
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                // Pull all newly-decoded PCM into the ring.
                while let Ok(frame) = ud.rx.try_recv() {
                    ud.ring.extend(frame);
                }
                let stride = 4 * ud.channels; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let want_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want = want_frames * ud.channels; // interleaved samples this quantum needs
                static FIRST: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(
                        quantum_frames = want_frames,
                        quantum_ms = want_frames as f32 / 48.0,
                        "virtual-mic consumer connected"
                    );
                }

                // Adaptive jitter buffer. The client pushes 5 ms frames; the recorder pulls a
                // whole *quantum* (often 20–43 ms) from an independent clock. A drain of one
                // quantum must not outrun what's buffered, or every call underruns to silence
                // (the original ~58% gaps). So prime to ~3 quanta before producing, hold there,
                // and re-prime only after a genuine full drain (the client went quiet). The ring
                // is capped at a few quanta so latency stays bounded.
                let target = (3 * want).clamp(720 * ud.channels, 9600 * ud.channels);
                while ud.ring.len() > target.max(want) + want {
                    ud.ring.pop_front(); // bound latency: drop the oldest beyond ~1 quantum slack
                }
                if !ud.primed && ud.ring.len() >= target {
                    ud.primed = true;
                }

                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if ud.primed {
                            ud.ring.pop_front().unwrap_or(0.0) // silence on a momentary underrun
                        } else {
                            0.0 // not yet primed — emit silence while the buffer fills
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                if ud.ring.is_empty() {
                    ud.primed = false; // fully drained — re-prime before producing again
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire virtual-mic callback");
            }
        })
        .register()
        .context("register virtual-mic stream listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    info.set_position(spa_positions(channels));
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize mic format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("mic pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output, // we PRODUCE samples (a source)
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw mic stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire virtual-mic loop exited (source dropped)");
    Ok(())
}

fn pw_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
) -> Result<()> {
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

    // Cross-thread teardown: the capturer's Drop sends Terminate; quit the loop here.
    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "punktfunk-audio",
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
                    tracing::info!(samples = n, "audio first capture buffer");
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

    // Request F32LE, 48 kHz, at the session's channel count with explicit positions —
    // PipeWire's channel-mixer up/downmixes the sink monitor to this layout.
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    info.set_position(spa_positions(channels));
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
    tracing::debug!("pipewire audio loop exited (capturer dropped)");
    Ok(())
}
