//! Audio playback: decoded PCM → a PipeWire playback stream.
//!
//! Mirrors the host's virtual-mic producer (`punktfunk-host::audio::linux`) with the same
//! adaptive jitter buffer: the session pump pushes 5 ms Opus-decoded chunks on the
//! network clock; PipeWire pulls whole quanta on the device clock. Prime to ~3 quanta
//! before producing, cap the ring so latency stays bounded, re-prime after a real drain.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;

struct Terminate;

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<Terminate>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayer {
    /// Spawn the PipeWire playback thread. Failure (no PipeWire in the session) is
    /// survivable — the caller streams video-only.
    pub fn spawn() -> Result<AudioPlayer> {
        // 64 × 5 ms = 320 ms of slack between the pump and the PipeWire loop.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(pcm_rx, quit_rx) {
                    tracing::warn!(error = %e, "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        Ok(AudioPlayer {
            pcm_tx,
            quit_tx,
            thread: Some(thread),
        })
    }

    /// Queue one interleaved-stereo f32 chunk. Drops the chunk if the PipeWire side is
    /// wedged (the renderer conceals the gap; never block the session pump).
    pub fn push(&self, pcm: Vec<f32>) {
        if let Err(TrySendError::Disconnected(_)) = self.pcm_tx.try_send(pcm) {
            // Thread already dead — Drop will reap it; nothing to do per-chunk.
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Producer-side state: incoming decoded PCM and the ring the process callback drains.
struct PlayerData {
    rx: Receiver<Vec<f32>>,
    ring: VecDeque<f32>,
    primed: bool,
}

fn pw_thread(
    pcm_rx: Receiver<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let stream = pw::stream::StreamBox::new(
        &core,
        "punktfunk-client",
        properties! {
            *pw::keys::MEDIA_TYPE       => "Audio",
            *pw::keys::MEDIA_CATEGORY   => "Playback",
            *pw::keys::MEDIA_ROLE       => "Game",
            *pw::keys::NODE_NAME        => "punktfunk-client",
            *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream",
            // ~5 ms quantum (one Opus frame) keeps the ring — and so the latency — small.
            *pw::keys::NODE_LATENCY     => "240/48000",
        },
    )
    .context("pw Stream")?;

    let ud = PlayerData {
        rx: pcm_rx,
        ring: VecDeque::new(),
        primed: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(?old, ?new, "pipewire playback stream state");
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk);
                }
                let stride = 4 * CHANNELS; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let want_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want = want_frames * CHANNELS;

                // Adaptive jitter buffer (same shape as the host's virtual mic): prime to
                // ~3 quanta, cap at ~1 quantum of slack beyond that, re-prime after a
                // genuine drain.
                let target = (3 * want).clamp(720 * CHANNELS, 9600 * CHANNELS);
                while ud.ring.len() > target.max(want) + want {
                    ud.ring.pop_front();
                }
                if !ud.primed && ud.ring.len() >= target {
                    ud.primed = true;
                }

                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if ud.primed {
                            ud.ring.pop_front().unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                if ud.ring.is_empty() {
                    ud.primed = false;
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire playback callback");
            }
        })
        .register()
        .context("register playback listener")?;

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
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire playback loop exited");
    Ok(())
}
