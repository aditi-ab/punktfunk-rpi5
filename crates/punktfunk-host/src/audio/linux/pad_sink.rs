//! Per-pad DualSense audio sink (Linux): one PipeWire `Audio/Sink` stream node per
//! DualSense-family pad, wearing the identity DS5-native titles and GE-Proton's
//! controller-audio routing match on — so a game that renders voice-coil haptics or pad-speaker
//! audio finds "the controller's audio device" and plays into us. We own the sink, so the
//! `process()` callback IS the capture: 4-ch F32 48 kHz (AUX0..AUX3 — front pair = speaker,
//! back pair = voice coils, the same quad *order* the Windows endpoint is stamped with) lands
//! directly in the chunk channel that feeds the 0xD1 lanes (`native/pad_audio.rs`).
//!
//! Modeled on the stream-sink mode of [`super::PwAudioCapturer`] (same MainLoop-on-a-thread,
//! Terminate channel, ready handshake, bounded lossy chunk hand-off) with two deliberate
//! differences: **no default-sink claim** (nothing may auto-route here — games target it BY
//! IDENTITY) and a low `priority.session` so WirePlumber never elects it against real hardware.
//!
//! **Identity** (design `dualsense-audio-haptics-and-speaker.md` §3/§5): GE-Proton matches
//! layered — pulse proplist (`device.bus == "usb"`, `device.vendor.id == 0x054c`,
//! `device.product.id ∈ {0x0ce6, 0x0df2}`), then name substrings
//! (`Sony_Interactive_Entertainment…Wireless_Controller`, `DualSense`); the community
//! WirePlumber rule keys on the node-name substring and sets `node.description =
//! "Wireless Controller"` (we mint it that way from the start).
//!
//! We wear a real pad's **name** and Pro Audio's **channel layout** — deliberately not the same
//! profile for both, because no single real-pad profile satisfies GE on its own.
//!
//! Since alsa-ucm-conf gained `USB-Audio/Sony/DualSense-PS5` (2026-08-03) a real pad's profiles
//! are UCM SplitPCM views of one 4-channel PCM: a mono `Speaker__sink`, a stereo `Headphones`
//! sink, and a 4-channel `Direct__Direct__sink` (added "for wine compatibility"), plus ACP's
//! always-present Pro Audio. GE renders haptics as an `AUX0..AUX3` stream, so on every
//! *positioned* profile the graph re-mixes and the voice-coil pair is folded away — that is the
//! whole content of the field advice "you only need the controller audio set to Pro Audio", and
//! it is why this sink is one flat AUX quad rather than an emulation of the split topology. But
//! the pad-SPEAKER half of GE only binds to a sink whose name says `Speaker__sink`, and its
//! Windows 4-channel format forcing hangs off the same test. So the name says `Speaker__sink`
//! and the channels are Pro Audio's. GE explicitly supports that combination on real hardware
//! (see [`split_target`] and the node-name comment).
//!
//! What a pure PipeWire node still cannot satisfy is wine's ContainerId derivation (udev walk to
//! a `usb_device` parent → `GUID_NULL`; our pad is uhid and has no USB parent at all) and GE's
//! raw-ALSA leg (`snd_pcm_open` on an `api.alsa.path` that must be a real card). Its
//! `pipewire:NODE=` leg we *can* satisfy — see [`split_target`]. Every identity string has an
//! env override for field debugging (`PUNKTFUNK_PAD_SINK_NAME` / `PUNKTFUNK_PAD_SINK_DESC` with
//! `{pad}` / `{mac}` placeholders, `PUNKTFUNK_PAD_SINK_SPLIT_NAME`).

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
    card_name: &'static str,
    long_card_name: String,
    /// GE-Proton's `api.alsa.split.name` — the node name it opens as `pipewire:NODE=…` for the
    /// haptic stream. Empty disables the key. See [`split_target`].
    split_name: String,
}

/// GE-Proton reads `api.alsa.split.name` off the sink it is about to render haptics into and,
/// on its preferred leg, opens *that* node through its bundled pipewire-alsa plugin as
/// `pipewire:NODE=<name>` with `aux_channels=1` (patches 0114/0115/0116 of `proton-ds5-haptic`).
/// On a real pad the key names the **hidden 4-channel parent** WirePlumber mints for the UCM
/// SplitPCM profile — the public mono `Speaker__sink` is only a 1-channel split of it, so
/// rendering four channels at the public sink would lose the voice-coil pair.
///
/// We have no split: the sink IS the four-channel AUX node, so the honest value of the key is
/// our own `node.name` — GE then targets us directly instead of falling back to a leg that was
/// written to work around a topology we do not have. Without the key that leg cannot engage at
/// all (`get_dualsense_haptic_target` returns NULL), which is why titles GE auto-switches into
/// "Windows Sony audio mode" (the 8-format-probe games: Assassin's Creed, Death Stranding DC,
/// MH Wilds) never reached our sink.
///
/// `PUNKTFUNK_PAD_SINK_SPLIT_NAME` is the field lever: `0`/`false`/`off` drops the key (GE then
/// takes its Pulse leg, which also works for us because our channel positions already match its
/// forced `AUX0..AUX3` map), any other value overrides the target verbatim.
fn split_target(node_name: &str) -> String {
    resolve_split_target(
        node_name,
        std::env::var("PUNKTFUNK_PAD_SINK_SPLIT_NAME").ok(),
    )
}

/// [`split_target`]'s decision, with the environment lifted out so it is testable.
fn resolve_split_target(node_name: &str, override_var: Option<String>) -> String {
    match override_var.as_deref().map(str::trim) {
        Some("0" | "false" | "off" | "no") => String::new(),
        Some(v) if !v.is_empty() => v.to_string(),
        _ => node_name.to_string(),
    }
}

impl PadSinkIdentity {
    fn new(pad: u8, edge: bool) -> PadSinkIdentity {
        let mac = pad_mac(pad);
        let mac_bare: String = mac.chars().filter(|c| *c != ':').collect();
        // The pad's USB `iProduct` string verbatim — a plain DualSense reports "Wireless
        // Controller" with NO model word (only the Edge carries one). Getting this wrong is not
        // cosmetic: udev builds the ALSA name out of manufacturer+product, so an invented
        // `DualSense_` infix broke the contiguous `Sony_Interactive_Entertainment_Wireless_
        // Controller` substring that the community WirePlumber rule and GE-Proton's
        // `alsa_output.usb-Sony_Interactive_Entertainment_…` matchers key on.
        let (usb_product, product_id, product_name, card_name) = if edge {
            (
                "DualSense_Edge_Wireless_Controller",
                "0df2",
                "DualSense Edge Wireless Controller",
                "DualSense Edge Wireless Controller",
            )
        } else {
            (
                "Wireless_Controller",
                "0ce6",
                "DualSense Wireless Controller",
                "Wireless Controller",
            )
        };
        // udev's `ID_SERIAL`: manufacturer_product_serial. A real pad has no USB serial, so ALSA
        // falls back to the card index; we carry the pad's virtual MAC there instead, which keeps
        // multi-pad sinks distinct without disturbing the matched prefix.
        let serial = format!("Sony_Interactive_Entertainment_{usb_product}_{mac_bare}");
        // The `…-00.<verb>__Speaker__sink` suffix is LOAD-BEARING, not decoration. GE-Proton's
        // `is_dualsense_speaker_sink()` is a pure substring test for `Speaker__sink` (plus the
        // USB ids, or the `alsa_output.usb-Sony_Interactive_Entertainment_` + `Wireless_Controller`
        // pair we also carry), and three things hang off it: `apply_windows_sony_audio_format()`
        // forces the wine endpoint to the Windows 4×48 kHz `KSAUDIO_SPEAKER_QUAD` layout DS5
        // titles probe for, the pad-SPEAKER (mono controller-effect) streams will only bind and
        // retarget to a sink it accepts, and the whole controller-audio endpoint lands on the
        // identity Spider-Man's working path used. A suffix naming any other profile — the
        // `analog-surround-40` we used to mint, or a truthful `pro-output-0` — matches none of
        // it, which left the speaker half of this feature with nothing to attach to.
        //
        // Carrying `Speaker__sink` AND [`split_target`] at once is a real pad's shape, not a
        // contrivance: GE's own `is_dualsense_endpoint_speaker_sink` notes that "Edge speaker
        // sinks may also carry raw haptic metadata", and handles the pair by keeping them as
        // routing targets while withholding the *shared* mono endpoint id (a Spider-Man
        // enumeration crash). The exclusion GE once had in `is_dualsense_speaker_sink` itself is
        // gone. What we do NOT copy is a real pad's mono channel count: the sink stays four raw
        // AUX channels — which is exactly what that endpoint is forced to advertise anyway.
        let node_name = match std::env::var("PUNKTFUNK_PAD_SINK_NAME") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac_bare),
            _ => format!("alsa_output.usb-{serial}-00.HiFi__Speaker__sink"),
        };
        // What the community WirePlumber rule renames real pads TO — minted that way directly.
        // Deliberately NOT the udev/hwdb description a real card gets ("DualSense wireless
        // controller (PS5)"): wine hands `node.description` straight to the endpoint's
        // `PKEY_Device_FriendlyName`, and the title matchers do a case-sensitive
        // `wcsstr(name, L"Wireless Controller")` (FF14, FF7R) that a lowercase "wireless" fails.
        let description = match std::env::var("PUNKTFUNK_PAD_SINK_DESC") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac),
            _ => "Wireless Controller".to_string(),
        };
        let split_name = split_target(&node_name);
        PadSinkIdentity {
            long_card_name: format!(
                "Sony Interactive Entertainment {card_name} at usb-punktfunk-pad{pad}, full speed"
            ),
            node_name,
            description,
            serial,
            product_id,
            product_name,
            card_name,
            split_name,
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
    /// What GE-Proton will read as `api.alsa.split.name`; empty when the key is suppressed.
    pub split_name: String,
}

impl PadSinkCapturer {
    /// Mint the sink for wire pad `pad` (`edge` = DualSense Edge identity) and start capturing.
    /// Fails if PipeWire is unreachable — the caller's reopen-with-backoff owns the retry.
    pub fn open(pad: u8, edge: bool) -> Result<PadSinkCapturer> {
        let identity = PadSinkIdentity::new(pad, edge);
        let node_name = identity.node_name.clone();
        let split_name = identity.split_name.clone();
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
        // The identity a title has to match, in the log a field report will carry. Cheap once
        // per pad, and it is the only place the negotiated strings are visible without a live
        // `pactl` on the box.
        let split_log = if split_name.is_empty() {
            "(suppressed)"
        } else {
            split_name.as_str()
        };
        tracing::info!(
            pad,
            edge,
            node_name = %node_name,
            split_name = %split_log,
            "pad-audio sink minted (Pro Audio shape: 4ch AUX0..AUX3, ch0/1 speaker, ch2/3 coils)"
        );
        Ok(PadSinkCapturer {
            chunks: rx,
            quit: quit_tx,
            node_name,
            split_name,
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

/// SPA channel positions for the pad quad: AUX0..AUX3 (`enum spa_audio_channel`:
/// `SPA_AUDIO_CHANNEL_START_Aux` = 0x1000), NOT a positioned FL FR RL RR layout. This is the
/// shape a REAL DualSense exposes on the PipeWire path GE-Proton's haptics were built and
/// field-validated against: its `open_dualsense_haptic_pcm` targets the node through the
/// bundled pipewire-alsa plugin with `aux_channels=1` — "the hidden PipeWire parent for a
/// DualSense output exposes AUX0 through AUX3" (proton-ds5-haptic patch 0115) — and its pulse
/// fallback forces a `PA_CHANNEL_POSITION_AUX0..3` map. On a real pad that shape is the card's
/// Pro Audio profile (the community-reported requirement for GE ≥11-4). Aux positions carry no
/// spatial meaning, so nothing in the graph position-remixes into (or out of) the sink —
/// writers land by INDEX, exactly the raw quad the pad speaks: ch0/1 = speaker, ch2/3 = voice
/// coils (the same order the Windows endpoint is stamped with and `split_quad` assumes).
fn pad_positions() -> [u32; 64] {
    const AUX0: u32 = 0x1000;
    let mut pos = [0u32; 64];
    pos[..4].copy_from_slice(&[AUX0, AUX0 + 1, AUX0 + 2, AUX0 + 3]);
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
            // the human-readable pair pavucontrol and the game view show. Every one of these
            // reaches a wine/Proton client verbatim — pipewire-pulse fills a sink's proplist
            // from the node's own props (`fill_sink_info_proplist`), it does not curate them.
            "device.bus"                => "usb",
            "device.vendor.id"          => "054c",
            "device.vendor.name"        => "Sony Interactive Entertainment",
            "device.form_factor"        => "gamepad",
            "device.icon_name"          => "audio-card-analog-usb",
            // The shape, stated as props and not only as a negotiated format: four raw AUX
            // channels — ch0/1 speaker, ch2/3 voice coils — which is what "Pro Audio" means on a
            // real pad's card and the only layout that survives GE-Proton's AUX0..AUX3 stream
            // map unfolded.
            "audio.channels"            => "4",
            "audio.position"            => "AUX0,AUX1,AUX2,AUX3",
            "api.alsa.pcm.stream"       => "playback",
            "alsa.driver_name"          => "snd_usb_audio",
        };
        props.insert(*pw::keys::NODE_NAME, identity.node_name.as_str());
        props.insert(*pw::keys::NODE_DESCRIPTION, identity.description.as_str());
        props.insert(*pw::keys::NODE_NICK, identity.description.as_str());
        props.insert("device.serial", identity.serial.as_str());
        props.insert("device.product.id", identity.product_id);
        props.insert("device.product.name", identity.product_name);
        props.insert("alsa.card_name", identity.card_name);
        props.insert("alsa.long_card_name", identity.long_card_name.as_str());
        // GE-Proton's preferred haptic leg; see `split_target`. Omitted (not empty) when the
        // field lever turns it off, so `pa_proplist_gets` misses rather than returning "".
        if !identity.split_name.is_empty() {
            props.insert("api.alsa.split.name", identity.split_name.as_str());
        }
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
        // GE-Proton's `string_contains_dualsense_name` legs, each checked separately.
        assert!(id.node_name.contains("Sony_Interactive_Entertainment"));
        assert!(id.node_name.contains("Wireless_Controller"));
        // …and the CONTIGUOUS form the community WirePlumber rule and GE's
        // `alsa_output.usb-Sony_Interactive_Entertainment_` prefix test want. An invented
        // `DualSense_` infix used to split this in two and miss both.
        assert!(id
            .node_name
            .starts_with("alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller_"));
        // The suffix GE's `is_dualsense_speaker_sink` substring-tests for — the pad-speaker
        // binding and the Windows 4ch format forcing both hang off it (never `analog-*`, which
        // matches nothing of GE's and names a positioned profile we do not wear).
        assert!(id.node_name.ends_with("-00.HiFi__Speaker__sink"));
        assert!(id.node_name.contains("Speaker__sink"));
        // No colons in a udev-style serial/name.
        assert!(!id.node_name.contains(':'));
        // Case-sensitive `wcsstr(FriendlyName, L"Wireless Controller")` (FF14, FF7R).
        assert_eq!(id.description, "Wireless Controller");
        assert_eq!(id.product_id, "0ce6");
        assert_eq!(id.card_name, "Wireless Controller");
        assert!(id.long_card_name.contains("Sony Interactive Entertainment"));
        let edge = PadSinkIdentity::new(1, true);
        // GE tests the Edge with the full `DualSense_Edge_Wireless_Controller` substring.
        assert!(edge
            .node_name
            .contains("DualSense_Edge_Wireless_Controller"));
        assert!(edge.node_name.contains("Speaker__sink"));
        assert_eq!(edge.product_id, "0df2");
        // Distinct pads mint distinct names (the serial octet).
        assert_ne!(id.node_name, PadSinkIdentity::new(1, false).node_name);
    }

    #[test]
    fn split_target_points_at_the_node_itself_unless_overridden() {
        // No split on our side: the sink IS the four-channel parent GE wants to open as
        // `pipewire:NODE=…`, so the honest target is our own name.
        let id = PadSinkIdentity::new(0, false);
        assert_eq!(id.split_name, id.node_name);
        // The field lever, both ways — through the pure form, so no test mutates the process
        // environment out from under a parallel test runner.
        assert_eq!(resolve_split_target("n", None), "n");
        assert_eq!(resolve_split_target("n", Some(" ".into())), "n");
        assert!(resolve_split_target("n", Some("0".into())).is_empty());
        assert!(resolve_split_target("n", Some("off".into())).is_empty());
        assert_eq!(resolve_split_target("n", Some(" other ".into())), "other");
    }

    #[test]
    fn template_expansion() {
        assert_eq!(expand("pad{pad}-{mac}", 2, "AABB"), "pad2-AABB");
        assert_eq!(expand("static", 0, "x"), "static");
    }
}
