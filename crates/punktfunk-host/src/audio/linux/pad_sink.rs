//! Per-pad DualSense audio sink (Linux): one PipeWire `Audio/Sink` stream node per
//! DualSense-family pad, wearing the identity DS5-native titles and GE-Proton's
//! controller-audio routing match on — so a game that renders voice-coil haptics or pad-speaker
//! audio finds "the controller's audio device" and plays into us. We own the sink, so the
//! `process()` callback IS the capture: 4-ch F32 48 kHz (FL FR RL RR — front pair = speaker,
//! back pair = voice coils, the same quad layout the Windows endpoint is stamped with) lands
//! directly in the chunk channel that feeds the 0xD1 lanes (`native/pad_audio.rs`).
//!
//! Modeled on the stream-sink mode of [`super::PwAudioCapturer`] (same MainLoop-on-a-thread,
//! Terminate channel, ready handshake, bounded lossy chunk hand-off) with two deliberate
//! differences: **no default-sink claim** (nothing may auto-route here — games target it BY
//! IDENTITY) and a low `priority.session` so WirePlumber never elects it against real hardware.
//!
//! **Identity** (design `dualsense-audio-haptics-and-speaker.md` §3/§5): GE-Proton 11-2+
//! matches layered — pulse proplist (`device.bus == "usb"`, `device.vendor.id == 0x054c`,
//! `device.product.id ∈ {0x0ce6, 0x0df2}`), then name substrings
//! (`Sony_Interactive_Entertainment…Wireless_Controller`, `DualSense`); the community
//! WirePlumber rule keys on the node-name substring and sets `node.description =
//! "Wireless Controller"` (we mint it that way from the start). A pure PipeWire node cannot
//! satisfy wine's ContainerId derivation (udev walk to a `usb_device` parent → `GUID_NULL`)
//! nor GE's raw-ALSA fast path — both fall back to the Pulse-routed leg, which winepulse
//! serves from exactly this node (it enumerates sinks). Every identity string has an env
//! override for field debugging (`PUNKTFUNK_PAD_SINK_NAME` / `PUNKTFUNK_PAD_SINK_DESC`, with
//! `{pad}` / `{mac}` placeholders).

use anyhow::{anyhow, Context, Result};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Message asking the PipeWire loop thread to quit (sent from `Drop`).
struct Terminate;

/// The pad sink's fixed channel count — quad, mirroring the Windows endpoint stamp
/// (`native/pad_audio.rs::CAP_CHANNELS` splits on the same layout).
const PAD_CHANNELS: u32 = 4;

/// How many pad slots may carry a sink (`PUNKTFUNK_PAD_AUDIO_SLOTS`, default all 4 — a PipeWire
/// stream node is cheap, unlike the Windows devnode mint whose default is 1).
pub(crate) fn pad_audio_slots() -> u8 {
    std::env::var("PUNKTFUNK_PAD_AUDIO_SLOTS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(4)
        .clamp(1, 4)
}

/// Whether a PipeWire daemon is plausibly reachable from this process — the Linux analogue of
/// "startup provisioning published at least one endpoint" for [`host_cap`]'s existence leg
/// (`native/pad_audio.rs`). A stat, not a connect: the handshake path runs per-Hello and must
/// not block. `PIPEWIRE_REMOTE` names a non-default socket — trust it (the session capturer
/// honors it via libpipewire, and a wrong value degrades to spawn-time failure, pad kept).
pub(crate) fn pipewire_reachable() -> bool {
    if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| std::path::Path::new(&dir).join("pipewire-0").exists())
        .unwrap_or(false)
}

/// The pad's virtual MAC as colon-separated display hex — [`ds_pairing_reply`]'s bytes 1..7
/// are LSB-first (the report layout `hid-playstation` adopts as the HID `uniq` via `%pMR`,
/// i.e. printed reversed), so the display form reverses them. Unique per pad (the low octet
/// carries the pad index), which keeps multi-pad sinks distinct for the same reason the MAC
/// itself must be: SDL/Steam and the matchers dedup by serial.
///
/// [`ds_pairing_reply`]: pf_inject::dualsense_proto::ds_pairing_reply
fn pad_mac(pad: u8) -> String {
    let reply = crate::inject::dualsense_proto::ds_pairing_reply(pad);
    let m = &reply[1..7];
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        m[5], m[4], m[3], m[2], m[1], m[0]
    )
}

/// Expand the `{pad}` / `{mac}` placeholders of an identity template. Callers pass the MAC in
/// the form the surrounding string wants: colon display form for proplist values, bare hex for
/// the ALSA-style node name (udev serials carry no colons).
fn expand(template: &str, pad: u8, mac: &str) -> String {
    template
        .replace("{pad}", &pad.to_string())
        .replace("{mac}", mac)
}

/// The full identity a pad sink wears, resolved once at open.
struct PadSinkIdentity {
    node_name: String,
    description: String,
    serial: String,
    product_id: &'static str,
    product_name: &'static str,
}

impl PadSinkIdentity {
    fn new(pad: u8, edge: bool) -> PadSinkIdentity {
        let mac = pad_mac(pad);
        let mac_bare: String = mac.chars().filter(|c| *c != ':').collect();
        let (model, product_id, product_name) = if edge {
            (
                "DualSense_Edge",
                "0df2",
                "DualSense Edge Wireless Controller",
            )
        } else {
            ("DualSense", "0ce6", "DualSense Wireless Controller")
        };
        // The ALSA-style name a REAL pad's card gets from udev (vendor_product_serial), which
        // is what every known name-substring matcher was written against. `-00.analog-surround-40`
        // = card profile suffix for the quad layout.
        let node_name = match std::env::var("PUNKTFUNK_PAD_SINK_NAME") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac_bare),
            _ => format!(
                "alsa_output.usb-Sony_Interactive_Entertainment_{model}_Wireless_Controller_{mac_bare}-00.analog-surround-40"
            ),
        };
        // What the community WirePlumber rule renames real pads TO — minted that way directly.
        let description = match std::env::var("PUNKTFUNK_PAD_SINK_DESC") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac),
            _ => "Wireless Controller".to_string(),
        };
        PadSinkIdentity {
            node_name,
            description,
            serial: format!(
                "Sony_Interactive_Entertainment_{model}_Wireless_Controller_{mac_bare}"
            ),
            product_id,
            product_name,
        }
    }
}

/// A live per-pad sink + its capture. Same next-chunk contract as every
/// [`AudioCapturer`](crate::audio::AudioCapturer): empty chunk = quiet sink (keep me), `Err` =
/// dead loop thread (reopen me). Dropping tears the sink node down promptly via the Terminate
/// channel (a wedged PipeWire link head-blocks the daemon — see the session capturer's docs).
pub struct PadSinkCapturer {
    chunks: Receiver<Vec<f32>>,
    quit: pipewire::channel::Sender<Terminate>,
    /// The minted node name, for logs and the devtest.
    pub node_name: String,
}

impl PadSinkCapturer {
    /// Mint the sink for wire pad `pad` (`edge` = DualSense Edge identity) and start capturing.
    /// Fails if PipeWire is unreachable — the caller's reopen-with-backoff owns the retry.
    pub fn open(pad: u8, edge: bool) -> Result<PadSinkCapturer> {
        let identity = PadSinkIdentity::new(pad, edge);
        let node_name = identity.node_name.clone();
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // Bring-up handshake (the session capturer's discipline): a PipeWire that isn't running
        // must surface as an open ERROR, engaging the caller's backoff — not a zombie thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        thread::Builder::new()
            .name(format!("punktfunk-pw-pad{pad}"))
            .spawn(move || {
                if let Err(e) = pad_sink_thread(tx, quit_rx, identity, ready_tx) {
                    tracing::warn!(pad, error = %format!("{e:#}"), "pipewire pad-sink thread failed");
                }
            })
            .context("spawn pipewire pad-sink thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("pipewire pad-sink init timed out")),
        }
        Ok(PadSinkCapturer {
            chunks: rx,
            quit: quit_tx,
            node_name,
        })
    }
}

impl Drop for PadSinkCapturer {
    fn drop(&mut self) {
        // A failed send means the loop thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
    }
}

impl crate::audio::AudioCapturer for PadSinkCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet pad sink (no game rendering pad audio — the common case) is NOT a
            // failure; the per-pad streamer keeps us and its silence gate stays closed.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire pad-sink thread ended")),
        }
    }

    fn channels(&self) -> u32 {
        PAD_CHANNELS
    }
}

/// SPA channel positions for the pad quad: FL FR RL RR (`enum spa_audio_channel`: FL=3 FR=4
/// RL=12 RR=13). NOT the session capturer's 4-ch order — the pad layout has no center/LFE; the
/// rear pair is the voice coils.
fn pad_positions() -> [u32; 64] {
    let mut pos = [0u32; 64];
    pos[..4].copy_from_slice(&[3, 4, 12, 13]);
    pos
}

/// The `!Send` MainLoop/Stream thread: mint the sink, hand capture chunks over, run until
/// Terminate / daemon death. Mirrors the session capturer's `pw_thread` stream-sink arm minus
/// the default-sink claim and the desktop-plane stats (the pad plane's observability lives in
/// the streamer's gate/encode logs).
fn pad_sink_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    identity: PadSinkIdentity,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw pad-sink MainLoop")?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).context("pw pad-sink Context")?;
        let core = context
            .connect_rc(None)
            .context("pw pad-sink connect (is PipeWire running in this session?)")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Daemon death ends this thread → the chunk channel disconnects → `next_chunk` errors →
        // the per-pad streamer reopens with backoff (the session capturer's zombie-thread fix).
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(id, res, message, "pipewire core error — pad sink ends");
                    mainloop.quit();
                }
            })
            .register();

        let mut props = properties! {
            *pw::keys::MEDIA_TYPE       => "Audio",
            *pw::keys::MEDIA_CLASS      => "Audio/Sink",
            // One Opus-haptics frame (~5 ms) per quantum, like the session sink — haptics are
            // felt latency; bursty delivery would ride through to the client's jitter buffer.
            *pw::keys::NODE_LATENCY     => "240/48000",
            // Must NEVER win WirePlumber's default election against real hardware — games reach
            // this sink BY IDENTITY, nothing auto-routes here (no stream_sink claim either).
            "priority.session"          => "50",
            // The pulse-proplist leg of GE-Proton's match (§3): bus + vendor/product ids, plus
            // the human-readable pair pavucontrol and the game view show.
            "device.bus"                => "usb",
            "device.vendor.id"          => "054c",
            "device.vendor.name"        => "Sony Interactive Entertainment",
            "device.form_factor"        => "gamepad",
        };
        props.insert(*pw::keys::NODE_NAME, identity.node_name.as_str());
        props.insert(*pw::keys::NODE_DESCRIPTION, identity.description.as_str());
        props.insert(*pw::keys::NODE_NICK, identity.description.as_str());
        props.insert("device.serial", identity.serial.as_str());
        props.insert("device.product.id", identity.product_id);
        props.insert("device.product.name", identity.product_name);
        let stream = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", props)
            .context("pw pad-sink Stream")?;

        // Lossy-drop counter: a full channel means the 0xD1 encode thread stalled. Invisible
        // drops cost a field investigation on the desktop plane once — count and warn here too,
        // power-of-two throttled (this callback runs at the graph quantum).
        struct PadUd {
            tx: std::sync::mpsc::SyncSender<Vec<f32>>,
            dropped: u64,
        }
        let ud = PadUd { tx, dropped: 0 };
        let _listener = stream
            .add_local_listener_with_user_data(ud)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire pad-sink stream state");
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(move |_stream, _ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    // We own the sink, so this IS the format games render into (nothing can
                    // have narrowed it upstream — the same guarantee as stream-sink mode).
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "pad-sink format negotiated"
                    );
                }
            })
            .process(|stream, ud| {
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
                    if ud.tx.try_send(samples).is_err() {
                        ud.dropped += 1;
                        if ud.dropped.is_power_of_two() {
                            tracing::warn!(
                                dropped = ud.dropped,
                                "pad-audio encode thread not keeping up — captured pad audio \
                                 dropped (haptics will click)"
                            );
                        }
                    }
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire pad-sink callback — chunk dropped");
                }
            })
            .register()
            .context("register pad-sink stream listener")?;

        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        info.set_rate(crate::audio::SAMPLE_RATE);
        info.set_channels(PAD_CHANNELS);
        info.set_position(pad_positions());
        let obj = pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("serialize pad-sink format pod")?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).context("pad-sink pod from bytes")?];

        // RT_PROCESS for the same reason as every host-owned stream node here: the sink must be
        // a synchronous graph member that joins its producers' driver group, or `process()`
        // never fires on a busy graph (see the mic's connect comment in mod.rs).
        stream
            .connect(
                spa::utils::Direction::Input, // we CONSUME what games render into the sink
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("pw pad-sink stream connect")?;

        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire pad-sink loop exited (capturer dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_mac_is_reversed_display_form_and_per_pad_unique() {
        // DS_FEATURE_PAIRING bytes 1..7 are 74 E7 D6 3A 53 35 LSB-first → display reverses.
        assert_eq!(pad_mac(0), "35:53:3A:D6:E7:74");
        // The pad index offsets the LOW octet — the LAST display octet.
        assert_eq!(pad_mac(1), "35:53:3A:D6:E7:75");
        assert_ne!(pad_mac(2), pad_mac(3));
    }

    #[test]
    fn identity_carries_every_match_surface() {
        let id = PadSinkIdentity::new(0, false);
        // The name-substring matchers (GE-Proton + the community WirePlumber rule).
        assert!(id.node_name.contains("Sony_Interactive_Entertainment"));
        assert!(id.node_name.contains("Wireless_Controller"));
        assert!(id.node_name.contains("DualSense"));
        assert!(id.node_name.ends_with("-00.analog-surround-40"));
        // No colons in a udev-style serial/name.
        assert!(!id.node_name.contains(':'));
        assert_eq!(id.description, "Wireless Controller");
        assert_eq!(id.product_id, "0ce6");
        let edge = PadSinkIdentity::new(1, true);
        assert!(edge.node_name.contains("DualSense_Edge"));
        assert_eq!(edge.product_id, "0df2");
        // Distinct pads mint distinct names (the serial octet).
        assert_ne!(id.node_name, PadSinkIdentity::new(1, false).node_name);
    }

    #[test]
    fn template_expansion() {
        assert_eq!(expand("pad{pad}-{mac}", 2, "AABB"), "pad2-AABB");
        assert_eq!(expand("static", 0, "x"), "static");
    }
}
