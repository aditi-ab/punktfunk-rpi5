//! Pad audio (the 0xD1 plane): render the host's per-gamepad DualSense streams — voice-coil
//! haptics (kind 0, the BACK channel pair) and the built-in speaker (kind 1, the FRONT pair) —
//! into a USB-connected physical DualSense's own 4-channel audio device.
//!
//! Tier A only (v1): a WIRED DualSense / DualSense Edge — Bluetooth exposes no audio device, so
//! wired is what makes the 4-ch sibling exist at all. The gamepad worker detects tier A at slot
//! open ([`crate::gamepad`]) and declares the pad's render capabilities to the core
//! ([`punktfunk_core::client::NativeClient::set_pad_audio_caps`]), which rides them on the
//! arrival (flags bits 8/9) toward a `HOST_CAP_PAD_AUDIO` host; the host then emits 0xD1 for
//! exactly those pads. This module owns everything after that:
//!
//! - **Correlation** pad ↔ audio device. Windows: the SDL HID interface path → the devnode's
//!   `ContainerID` (registry) → the active eRender endpoint whose stamped
//!   `PKEY_Device_ContainerId` matches AND whose device format has 4 channels. Linux: the
//!   PipeWire node belonging to a DualSense CARD that carries four channels — see
//!   `pick_pad_sink`, and the section below for why "a sink that looks like a DualSense" is
//!   not enough.
//! - **The renderer worker** ([`spawn`]): drains [`NativeClient::next_pad_audio`] (the plane's
//!   single consumer), Opus-decodes per (pad, kind) with seq-gap PLC (the session audio path's
//!   [`AudioGapTracker`] discipline), interleaves both pairs into one 4-ch stream
//!   (speaker → channels 0/1, haptics → 2/3 — the DS5 device's own layout), and plays it on the
//!   correlated device: WASAPI shared/event-driven on Windows, a PipeWire playback stream with
//!   `target.object` on Linux — both with the session players' 3-quantum prime/cap/re-prime
//!   ring policy at a SMALLER floor (haptics are felt latency). The output device is opened
//!   lazily on the first arriving frame and re-correlated with backoff when it goes away.
//!
//! # Linux: four channels, in order, or nothing is felt
//!
//! The voice coils ARE channels 3 and 4 of the pad's USB audio function. Everything on the
//! Linux side follows from that one fact, and none of it is optional:
//!
//! - **The node must have four channels.** A DualSense card almost never presents as one by
//!   default. PipeWire's ACP picks a stereo profile, and modern `alsa-ucm-conf` (which gained
//!   `USB-Audio/Sony/DualSense-PS5.conf` in 2026-08) splits the card into a MONO `Speaker` sink
//!   and a stereo `Headphones` sink instead. Streaming a quad into any of those renders the
//!   haptics into the headphone jack and folds the coil pair away — audibly plausible, felt as
//!   nothing. This is the client-side twin of the "set the controller to Pro Audio" advice the
//!   host-side sink exists to make unnecessary (`audio/linux/pad_sink.rs` in the host tree),
//!   and here we automate it: `ensure_pro_audio` moves the card's profile and puts it back
//!   when the session ends.
//! - **The channels must map by index, not by position.** Games — and this plane — treat the
//!   quad as four raw channels; a positioned stream into a positioned sink gets helpfully
//!   re-mixed. So the stream is `AUX0..AUX3` with `stream.dont-remix`, which is both what
//!   GE-Proton's pulse leg forces and what our own host sink advertises.
//! - **It must be a real card, never a look-alike.** A Punktfunk HOST minting its pad sink on
//!   this same machine publishes the full DualSense identity ON PURPOSE — that is how Proton
//!   finds it. Rendering into it would loop the plane back at the host instead of driving a
//!   pad. `device.id` tells them apart: a card's node has one, a stream-sink does not.
//!
//! The `Settings` side: `pad_haptics` (bool) and `pad_speaker` (`"pad"`/`"mix"`/`"off"`) gate
//! the `CLIENT_CAP_PAD_AUDIO` advertisement and the per-pad capability bits; `"mix"` (fold the
//! speaker into the main stream audio) is a declared TODO and renders as `"off"`.

use punktfunk_core::audio::AudioGapTracker;
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The render layout: 4 interleaved f32 channels — speaker FL/FR on 0/1, haptics (rear pair
/// on the wire layout, the voice coils on the device) on 2/3.
const PAD_CHANNELS: usize = 4;

/// Mixer depth bound (frames @48 kHz — 100 ms). Only guards a wedged/absent output; the live
/// latency bound is the platform ring policy's cap.
const MAX_BUFFER_FRAMES: usize = 4800;

/// Device (re)correlation backoff bounds: a missing DualSense audio device is polled at
/// [`RETRY_MIN`] doubling to [`RETRY_MAX`] — correlation enumerates the audio graph, so it must
/// not run per frame.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);

// ---- settings vocabulary --------------------------------------------------------------------

/// Whether the `pad_speaker` setting asks for a renderer: `"pad"` = the physical pad's speaker
/// (the only implemented target). `"mix"` — fold the speaker stream into the main session
/// audio — is a declared TODO: it logs once and renders as `"off"` so the setting name can ship
/// before the mixer leg does. Anything else (including `"off"`) = no renderer.
pub fn speaker_active(mode: &str) -> bool {
    match mode {
        "pad" => true,
        "mix" => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::info!(
                    "pad_speaker=\"mix\" is not implemented yet (TODO: fold the DS5 speaker \
                     stream into the main session audio) — treating it as \"off\""
                );
            });
            false
        }
        _ => false,
    }
}

// ---- tier-A detection -----------------------------------------------------------------------

/// Tier A = a physical DualSense (`054C:0CE6`) or DualSense Edge (`054C:0DF2`) on a WIRED
/// connection — only USB exposes the pad's 4-ch audio device (Bluetooth pads are tier B/C,
/// out of scope here). Pure so the policy is testable; the wired signal comes from
/// `SDL_GetGamepadConnectionState`, falling back to the audio-sibling probe
/// ([`wired_audio_sibling`]) when SDL answers Unknown.
pub(crate) fn is_tier_a_ds5(vid: u16, pid: u16, wired: bool) -> bool {
    vid == 0x054C && matches!(pid, 0x0CE6 | 0x0DF2) && wired
}

/// The wired fallback when SDL cannot say (`ConnectionState::Unknown`): does the pad's audio
/// sibling exist? A Bluetooth DS5 exposes no audio device at all, so a DualSense sound CARD in
/// the graph IS the wired signal. Linux ignores the HID path (the card match is
/// identity-based); Windows resolves the path's container against the render endpoints.
///
/// Deliberately weaker than what the renderer needs: any profile proves the pad is plugged in,
/// even the stereo one that cannot carry the coils — moving the card to a four-channel profile
/// is `ensure_pro_audio`'s job, and it must not be gated on the answer to this question.
#[cfg(target_os = "linux")]
pub(crate) fn wired_audio_sibling(_hid_path: Option<&str>) -> bool {
    match walk_graph() {
        Ok((sinks, cards)) => {
            cards.iter().any(|c| c.ds5) || sinks.iter().any(|s| s.ds5 && s.device_id.is_some())
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "pad-audio wired probe: no PipeWire graph");
            false
        }
    }
}

#[cfg(windows)]
pub(crate) fn wired_audio_sibling(hid_path: Option<&str>) -> bool {
    hid_path.is_some_and(|p| correlate_pad_endpoint(p).is_ok())
}

// ---- tier-A pad registry (gamepad worker → renderer worker) ---------------------------------

/// One tier-A pad the gamepad worker holds open: its wire index and (Windows correlation) the
/// SDL HID device path. Registered at slot open, dropped at slot close.
struct TierAPad {
    index: u8,
    /// Read by the Windows correlation only — Linux matches the sink by signature.
    #[cfg_attr(not(windows), allow(dead_code))]
    hid_path: Option<String>,
}

/// The tier-A pads currently open, shared between the gamepad worker (writer, at slot
/// open/close) and the session renderer worker (reader, at device correlation). A process-wide
/// static because the two workers meet nowhere else: the gamepad service is app-lifetime, the
/// renderer is per-session.
static TIER_A_PADS: Mutex<Vec<TierAPad>> = Mutex::new(Vec::new());

/// Gamepad worker: a tier-A slot opened on wire index `index` (idempotent per index).
pub(crate) fn register_tier_a(index: u8, hid_path: Option<String>) {
    let mut pads = TIER_A_PADS.lock().unwrap();
    pads.retain(|p| p.index != index);
    pads.push(TierAPad { index, hid_path });
}

/// Gamepad worker: the tier-A slot on `index` closed (no-op for non-tier-A indices).
pub(crate) fn unregister_tier_a(index: u8) {
    TIER_A_PADS.lock().unwrap().retain(|p| p.index != index);
}

/// The first registered tier-A pad's HID path (v1 renders one physical DS5).
#[cfg(windows)]
fn first_tier_a_hid_path() -> Option<String> {
    TIER_A_PADS
        .lock()
        .unwrap()
        .first()
        .and_then(|p| p.hid_path.clone())
}

// ---- correlation: Linux (the pad's own four-channel card node) -------------------------------

/// Does this PipeWire object's name/description look like a DualSense? The ALSA node name
/// carries the USB vendor string (`Sony_Interactive_Entertainment`), descriptions carry the
/// product (`DualSense`), and the kernel's fallback device name is `Wireless Controller` —
/// any of the three identifies the pad. The weaker half of the identity: the USB ids in the
/// proplist are the strong one, and this covers the cards that publish neither.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_ds5_sink(name: &str, description: &str) -> bool {
    let hit = |s: &str| {
        s.contains("Sony_Interactive_Entertainment")
            || s.contains("DualSense")
            || s.starts_with("Wireless Controller")
    };
    hit(name) || hit(description)
}

/// The DualSense audio function's USB ids — the strong half of the identity, and the same pair
/// GE-Proton matches on (`vendor.id == 0x054c && product.id ∈ {0ce6, 0df2}`).
#[cfg(any(target_os = "linux", test))]
const DS5_VENDOR: u32 = 0x054C;
#[cfg(any(target_os = "linux", test))]
const DS5_PRODUCTS: [u32; 2] = [0x0CE6, 0x0DF2];

/// Read a PipeWire `*.vendor.id` / `*.product.id` proplist value. These are written BASE 16 —
/// sometimes `0x`-prefixed (the ALSA monitor's own stamp), sometimes bare (udev's
/// `ID_VENDOR_ID`) — and both spellings mean the same number. Reading `"054c"` as decimal
/// would simply fail here rather than mis-match, but `"0994"` would not, which is why this is
/// a function and not an inline parse.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_usb_id(v: &str) -> Option<u32> {
    let v = v.trim();
    let hex = v
        .strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    u32::from_str_radix(hex, 16).ok()
}

/// The full identity test over a proplist's four relevant keys: the USB ids when the object
/// publishes them, the name/description signature otherwise.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn props_say_ds5(
    vendor: Option<&str>,
    product: Option<&str>,
    name: &str,
    description: &str,
) -> bool {
    let ids = vendor.and_then(parse_usb_id) == Some(DS5_VENDOR)
        && product
            .and_then(parse_usb_id)
            .is_some_and(|p| DS5_PRODUCTS.contains(&p));
    ids || is_ds5_sink(name, description)
}

/// One PipeWire sink node reduced to what the pad matcher needs. Pure data, so the entire
/// selection is unit-testable off-box — the graph walk is the only part that needs a daemon.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SinkNode {
    /// `node.name` — what a stream targets via `target.object`.
    pub(crate) name: String,
    pub(crate) description: String,
    /// `device.id`: the CARD this node belongs to, and the object whose profile has to move
    /// when no four-channel node exists. `None` means the node is not a card's at all — which
    /// is how a Punktfunk host's own minted pad sink (full DualSense identity, deliberately)
    /// is told apart from a pad in the user's hands.
    pub(crate) device_id: Option<u32>,
    /// `audio.channels`, falling back to the length of `audio.position`.
    pub(crate) channels: u32,
    /// `audio.position`, already split on commas. Empty when the node publishes none.
    pub(crate) positions: Vec<String>,
    /// `api.alsa.split.name` — WirePlumber's hidden four-channel parent behind a split card
    /// (the `HiFi` verb's mono `Speaker` / stereo `Headphones` sinks both name it). GE-Proton's
    /// preferred haptic leg opens exactly this node.
    pub(crate) split_parent: Option<String>,
    /// This node's OWN proplist said DualSense (cards state it more reliably — see
    /// `pick_pad_sink`, which accepts either).
    pub(crate) ds5: bool,
    /// `media.class` was `Audio/Sink/Internal` (or the node called itself
    /// `api.alsa.split.parent`): the hidden RAW node behind a split card, carrying the
    /// hardware's own channels. Usable, but the LAST four-channel choice — see
    /// `pick_pad_sink` for the measurement that says so.
    pub(crate) internal: bool,
}

/// A PipeWire `Device` — an ALSA card — reduced to the same shape.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardDevice {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) ds5: bool,
}

/// What the graph walk found for the pad.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PadSinkPick {
    /// Render here: a four-channel node belonging to a DualSense card (or the hidden parent
    /// one of its split sinks names).
    Node(String),
    /// The pad is here but exposes no four-channel node — its card profile has to move first.
    /// Carries the `device.id` to move.
    NeedsProfile(u32),
}

/// Is this channel map unpositioned — the `AUX0..AUX3` / unknown shape that no part of the
/// graph will position-remix? The Pro Audio profile and WirePlumber's split parents both
/// produce it; a `FL,FR,RL,RR` "surround 4.0" profile does not (that one still works, but only
/// because the stream sets `stream.dont-remix`).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_unpositioned(positions: &[String]) -> bool {
    positions.is_empty()
        || positions.iter().all(|p| {
            let p = p.trim();
            p.is_empty() || p.starts_with("AUX") || p == "UNK" || p == "NA"
        })
}

/// Choose the node the renderer should open, or the card whose profile is in the way.
///
/// The whole requirement is four channels on a DualSense's own card: the voice coils ARE
/// channels 3 and 4, so a stereo or mono node opens perfectly and renders the haptics into the
/// headphone jack. v1 renders ONE physical DS5 — with two plugged in the first match wins.
///
/// ⚠⚠ **A card's PUBLIC four-channel sink beats its hidden parent, even though the parent is
/// the "rawer" node.** Measured on a Steam Deck (SteamOS 3.7, `alsa-ucm-conf` with
/// `DualSense-PS5.conf`), where the card offers both:
///
/// | node | `audio.position` | what our channels reach |
/// |---|---|---|
/// | `HiFi__SpeakerHaptic__sink` (public, 4 ch) | `FL,FR,RL,RR` | split `[AUX1,AUX1,AUX2,AUX3]` |
/// | `alsa_output.hw_Controller_0` (`Audio/Sink/Internal`, 4 ch) | `AUX0..AUX3` | the hardware |
///
/// The hardware map is **AUX1 = the mono speaker, AUX2/AUX3 = the two voice coils, AUX0 =
/// nothing**. Our stream is speaker on 0/1 and haptics on 2/3, so index-exact into the PARENT
/// puts speaker-left into the dead AUX0 and only speaker-right into the speaker — half the
/// speaker signal thrown away. The public split sink maps BOTH of our speaker channels onto
/// AUX1 (the fold the UCM author intended) and passes the coil pair straight through. Haptics
/// are identical either way; the speaker is not, so the public sink wins.
///
/// The parent stays as the next choice for cards that publish nothing else — and Pro Audio's
/// `pro-output-0`, which is a PUBLIC `AUX` quad, is caught by the first rule.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn pick_pad_sink(sinks: &[SinkNode], cards: &[CardDevice]) -> Option<PadSinkPick> {
    let ds5_card = |id: u32| cards.iter().any(|c| c.id == id && c.ds5);
    // A card node only — see the module docs on `device.id`. The identity may come from either
    // end: split sinks routinely publish neither vendor ids nor a recognisable name, and it is
    // their CARD that says DualSense.
    let mine: Vec<&SinkNode> = sinks
        .iter()
        .filter(|s| s.device_id.is_some_and(|id| s.ds5 || ds5_card(id)))
        .collect();
    if mine.is_empty() {
        return None;
    }
    let quad = |s: &&&SinkNode| s.channels == 4;
    // Public quads first — unpositioned (Pro Audio) ahead of positioned only for determinism,
    // since `stream.dont-remix` makes the two equivalent to us.
    if let Some(s) = mine
        .iter()
        .filter(|s| !s.internal)
        .filter(quad)
        .find(|s| is_unpositioned(&s.positions))
    {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    if let Some(s) = mine.iter().filter(|s| !s.internal).find(quad) {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    // Only now the hidden parent (see the table above for what this costs the speaker).
    if let Some(s) = mine.iter().find(quad) {
        return Some(PadSinkPick::Node(s.name.clone()));
    }
    // A split card whose four-channel parent we cannot see in the registry (it is an
    // `Audio/Sink/Internal` node, and a restricted client may not be shown it) still names it
    // on every public split sink. Target it by name — that is GE-Proton's leg 1.
    if let Some(parent) = mine
        .iter()
        .find_map(|s| s.split_parent.clone().filter(|p| !p.is_empty()))
    {
        return Some(PadSinkPick::Node(parent));
    }
    // A pad, but only positioned stereo/mono profiles: the card has to move to Pro Audio.
    mine.first()
        .and_then(|s| s.device_id)
        .map(PadSinkPick::NeedsProfile)
}

/// Read a sink node's facts out of a proplist. Split out because it has to run against the
/// node's INFO props, not the registry's — see [`walk_graph`].
///
/// Linux-only, unlike its pure-logic neighbours: `DictRef` comes from `pipewire`, which is a
/// `cfg(target_os = "linux")` dependency. Widening this to `any(…, test)` the way the testable
/// helpers around it do puts the item into the Windows `lib test` target, where the crate does
/// not exist — E0433, visible only under `--all-targets`, and so only on the Windows CI leg.
#[cfg(target_os = "linux")]
pub(crate) fn sink_from_props(props: &pipewire::spa::utils::dict::DictRef) -> Option<SinkNode> {
    // Both spellings: PipeWire's own objects use the `device.`-prefixed keys, the pulse-facing
    // proplist GE reads uses the bare ones. Cheap to accept both.
    let vendor = props
        .get("device.vendor.id")
        .or_else(|| props.get("vendor.id"));
    let product = props
        .get("device.product.id")
        .or_else(|| props.get("product.id"));
    // `Audio/Sink` and `Audio/Sink/Internal` alike: the hidden four-channel parent behind a
    // split card wears the latter, and it is a usable (if second-choice) target.
    let class = props.get("media.class")?;
    if !class.starts_with("Audio/Sink") {
        return None;
    }
    let name = props.get("node.name")?;
    let description = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .unwrap_or(name);
    let positions: Vec<String> = props
        .get("audio.position")
        .map(|p| {
            p.trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(SinkNode {
        device_id: props.get("device.id").and_then(|v| v.parse().ok()),
        channels: props
            .get("audio.channels")
            .and_then(|v| v.parse().ok())
            .unwrap_or(positions.len() as u32),
        split_parent: props.get("api.alsa.split.name").map(str::to_string),
        ds5: props_say_ds5(vendor, product, name, description),
        internal: class.ends_with("/Internal")
            || props
                .get("api.alsa.split.parent")
                .is_some_and(|v| !matches!(v, "false" | "0")),
        positions,
        name: name.to_string(),
        description: description.to_string(),
    })
}

/// Walk the graph on a private mainloop: every `Audio/Sink…` node and every `Device`, reduced
/// to the matcher's shapes. [`crate::audio::devices`]'s discipline (a few ms against a live
/// daemon, a clean error when there is none) — a separate walk because that one is the settings
/// picker's and deliberately publishes only name + description.
///
/// ⚠⚠ **TWO rounds, because a registry `global` event does NOT carry the node's whole
/// proplist.** It carries a small announce subset — enough for `media.class`, `node.name` and
/// `device.id`, which is exactly why this looked like it worked — but `audio.channels` and
/// `audio.position` are NOT in it. Reading them from there yields 0 channels for every node on
/// a real machine, which then reads as "this card has no four-channel node" and sends the
/// renderer off to change the card's profile for no reason. They live in the node's INFO
/// props, so each candidate is bound and its `info` event awaited. (`pw-dump` and `pactl` show
/// these fields because they bind every object too; that is what made the registry-only version
/// look plausible against their output.)
#[cfg(target_os = "linux")]
fn walk_graph() -> anyhow::Result<(Vec<SinkNode>, Vec<CardDevice>)> {
    use anyhow::Context;
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let sinks: Rc<RefCell<Vec<SinkNode>>> = Rc::default();
    let cards: Rc<RefCell<Vec<CardDevice>>> = Rc::default();
    // The bound node proxies and their listeners have to outlive the callback that made them.
    let bound: Rc<RefCell<Vec<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, sinks, cards, bound) = (
                registry.clone(),
                sinks.clone(),
                cards.clone(),
                bound.clone(),
            );
            move |g| {
                let Some(props) = g.props else { return };
                match g.type_ {
                    pw::types::ObjectType::Node => {
                        // The announce subset is enough to know this is a sink; everything the
                        // matcher weighs comes from the info props below.
                        if !props
                            .get("media.class")
                            .is_some_and(|c| c.starts_with("Audio/Sink"))
                        {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                            return;
                        };
                        let listener = node
                            .add_listener_local()
                            .info({
                                let sinks = sinks.clone();
                                move |info| {
                                    let Some(p) = info.props() else { return };
                                    if let Some(s) = sink_from_props(p) {
                                        let mut v = sinks.borrow_mut();
                                        // `info` can fire more than once per node; keep one.
                                        if let Some(old) = v.iter_mut().find(|o| o.name == s.name) {
                                            *old = s;
                                        } else {
                                            v.push(s);
                                        }
                                    }
                                }
                            })
                            .register();
                        bound.borrow_mut().push((node, listener));
                    }
                    pw::types::ObjectType::Device => {
                        // Cards DO announce their identity keys, and nothing else about them
                        // is weighed, so these need no second round.
                        let vendor = props
                            .get("device.vendor.id")
                            .or_else(|| props.get("vendor.id"));
                        let product = props
                            .get("device.product.id")
                            .or_else(|| props.get("product.id"));
                        let name = props.get("device.name").unwrap_or_default();
                        let description = props
                            .get("device.description")
                            .or_else(|| props.get("device.nick"))
                            .unwrap_or(name);
                        cards.borrow_mut().push(CardDevice {
                            id: g.id,
                            ds5: props_say_ds5(vendor, product, name, description),
                            name: name.to_string(),
                            description: description.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        })
        .register();

    // Round 1 delivers the globals (and binds the sinks); round 2 collects the `info` events
    // those binds provoked. Each round parks its sync seq for the one `done` listener.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _round_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    for _ in 0..2 {
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
    }
    let out = (sinks.borrow().clone(), cards.borrow().clone());
    // Drop the bound proxies before the core that owns them.
    bound.borrow_mut().clear();
    Ok(out)
}

// ---- the Pro Audio profile swap (Linux) ------------------------------------------------------

/// One profile a card offers, reduced to what the chooser needs.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardProfile {
    pub(crate) index: u32,
    pub(crate) name: String,
    pub(crate) description: String,
    /// `SPA_PARAM_PROFILE_available` said anything other than `no`. An unavailable profile is
    /// one the card cannot currently enter (an unplugged jack, a busy PCM) — selecting it
    /// would silently leave the card where it was.
    pub(crate) available: bool,
}

/// Which profile carries the pad's four channels. **Pro Audio** first: PipeWire adds it to
/// every ALSA card, it exposes each PCM raw as `AUX` channels, and it is the one the community
/// fix names. Failing that, a positioned four-channel output ("surround 4.0") at least HAS the
/// coil channels — `stream.dont-remix` keeps them in place once we are on it.
///
/// Everything else — stereo, mono, the `HiFi` splits — is a profile the coils cannot be
/// reached through at all, so no fallback below these two is worth taking.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn pick_profile(profiles: &[CardProfile]) -> Option<&CardProfile> {
    let usable = |p: &&CardProfile| p.available;
    profiles
        .iter()
        .filter(usable)
        .find(|p| p.name == "pro-audio")
        .or_else(|| {
            profiles.iter().filter(usable).find(|p| {
                p.name.contains("surround-40") || p.name.contains("quad") || p.name == "direct"
            })
        })
}

/// A profile we moved and owe the user back. One card at a time — v1 renders one physical DS5.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct ProfileSwap {
    device_id: u32,
    previous: u32,
}

#[cfg(target_os = "linux")]
static PROFILE_SWAP: Mutex<Option<ProfileSwap>> = Mutex::new(None);

/// Cards whose profile we already moved WITHOUT getting a four-channel node out of it. One
/// failed swap is information; repeating it on every backoff retry would flip a device in the
/// user's sound settings back and forth for as long as the session lasts.
#[cfg(target_os = "linux")]
static PROFILE_TRIED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Which profile a [`set_card_profile`] call is after.
#[cfg(target_os = "linux")]
enum ProfileTarget {
    /// Pick by [`pick_profile`] — the four-channel one.
    FourChannel,
    /// Select this index verbatim (the restore leg).
    Index(u32),
}

/// Move the pad's card onto a four-channel profile, remembering what it was so the session can
/// put it back. Idempotent: a card already on a four-channel profile is left alone.
///
/// This changes a device the user can see in their sound settings, so it is loud in the log,
/// always reverted at session end ([`restore_profile`]), never persisted (`save = false`, so
/// WirePlumber does not adopt it as the card's remembered choice), and switchable off entirely
/// with `PUNKTFUNK_PAD_AUDIO_PROFILE=0` for anyone who would rather drive their own card.
#[cfg(target_os = "linux")]
fn ensure_pro_audio(device_id: u32) -> anyhow::Result<()> {
    if matches!(
        std::env::var("PUNKTFUNK_PAD_AUDIO_PROFILE").as_deref(),
        Ok("0" | "false" | "off" | "no")
    ) {
        anyhow::bail!(
            "the DualSense card has no four-channel profile active and \
             PUNKTFUNK_PAD_AUDIO_PROFILE=0 forbids moving it — switch the controller to \
             \"Pro Audio\" in your sound settings to feel haptics"
        );
    }
    let previous = set_card_profile(device_id, ProfileTarget::FourChannel)?;
    let mut swap = PROFILE_SWAP.lock().unwrap();
    // Only the FIRST swap is the user's own setting; a later re-correlation must not record
    // our own Pro Audio pick as the thing to restore.
    if swap.is_none() {
        *swap = Some(ProfileSwap {
            device_id,
            previous,
        });
    }
    Ok(())
}

/// Put a swapped card back the way we found it (session end, or the renderer giving up).
#[cfg(target_os = "linux")]
fn restore_profile() {
    let Some(swap) = PROFILE_SWAP.lock().unwrap().take() else {
        return;
    };
    match set_card_profile(swap.device_id, ProfileTarget::Index(swap.previous)) {
        Ok(_) => tracing::info!(
            device = swap.device_id,
            profile = swap.previous,
            "DualSense card profile restored"
        ),
        // An unplugged pad is the ordinary way this fails — its card is gone, and so is the
        // profile we owed back.
        Err(e) => tracing::debug!(
            error = %format!("{e:#}"),
            "DualSense card profile not restored (pad unplugged?)"
        ),
    }
}

/// Select a profile on a card, returning the index it had before. The one live half of the
/// swap: bind the `Device`, enumerate `EnumProfile` + the active `Profile`, choose, `set_param`.
///
/// Three mainloop rounds rather than one, because each depends on the previous round's replies:
/// the registry has to deliver the card before we can bind it, and the bound proxy has to
/// answer `enum_params` before we know which index to ask for.
#[cfg(target_os = "linux")]
fn set_card_profile(device_id: u32, want: ProfileTarget) -> anyhow::Result<u32> {
    use anyhow::{anyhow, Context};
    use pipewire as pw;
    use pw::spa::param::ParamType;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let profiles: Rc<RefCell<Vec<CardProfile>>> = Rc::default();
    let active: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    let device: Rc<RefCell<Option<pw::device::Device>>> = Rc::default();
    let dev_listener: Rc<RefCell<Option<pw::device::DeviceListener>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, device, dev_listener) =
                (registry.clone(), device.clone(), dev_listener.clone());
            let (profiles, active) = (profiles.clone(), active.clone());
            move |g| {
                if g.id != device_id || g.type_ != pw::types::ObjectType::Device {
                    return;
                }
                let Ok(d) = registry.bind::<pw::device::Device, _>(g) else {
                    return;
                };
                let l = d
                    .add_listener_local()
                    .param({
                        let (profiles, active) = (profiles.clone(), active.clone());
                        move |_seq, id, _index, _next, param| {
                            let Some(p) = param.and_then(parse_profile) else {
                                return;
                            };
                            match id {
                                ParamType::EnumProfile => profiles.borrow_mut().push(p),
                                ParamType::Profile => active.set(Some(p.index)),
                                _ => {}
                            }
                        }
                    })
                    .register();
                *dev_listener.borrow_mut() = Some(l);
                *device.borrow_mut() = Some(d);
            }
        })
        .register();

    // One `done` listener drives every round; each round parks its sync seq here first.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_listener = core
        .add_listener_local()
        .done({
            let (mainloop, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    mainloop.quit();
                }
            }
        })
        .register();
    let round = |issue: &dyn Fn() -> anyhow::Result<()>| -> anyhow::Result<()> {
        issue()?;
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
        Ok(())
    };

    round(&|| Ok(()))?; // 1: the registry replays its globals; our card gets bound
    round(&|| {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} is not in the PipeWire graph"))?;
        d.enum_params(0, Some(ParamType::EnumProfile), 0, u32::MAX);
        d.enum_params(1, Some(ParamType::Profile), 0, 1);
        Ok(())
    })?; // 2: profile list + the active one

    let previous = active
        .get()
        .ok_or_else(|| anyhow!("card {device_id} did not report an active profile"))?;
    // Decide with the borrow SCOPED: round 3 runs the mainloop again, and the param listener
    // that fills this list runs from inside it — holding a shared borrow across that call
    // would turn a re-emitted `EnumProfile` into a `RefCell` panic.
    let pick = {
        let list = profiles.borrow();
        match want {
            ProfileTarget::Index(i) => i,
            ProfileTarget::FourChannel => {
                let p = pick_profile(&list).ok_or_else(|| {
                    anyhow!(
                        "the DualSense card offers no four-channel profile ({} enumerated: {})",
                        list.len(),
                        list.iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                tracing::info!(
                    profile = %p.name,
                    description = %p.description,
                    was = previous,
                    "moving the DualSense card to a four-channel profile so the voice coils are \
                     reachable (restored when the session ends)"
                );
                p.index
            }
        }
    };
    if pick == previous {
        return Ok(previous);
    }
    let pod = profile_pod(pick).context("serialize Profile pod")?;
    round(&|| {
        let d = device.borrow();
        let d = d
            .as_ref()
            .ok_or_else(|| anyhow!("card {device_id} vanished mid-swap"))?;
        d.set_param(
            ParamType::Profile,
            0,
            pw::spa::pod::Pod::from_bytes(&pod).ok_or_else(|| anyhow!("bad Profile pod"))?,
        );
        Ok(())
    })?; // 3: flush the set_param before the loop and its proxies drop
    Ok(previous)
}

/// Parse one `EnumProfile` / `Profile` object pod.
#[cfg(target_os = "linux")]
fn parse_profile(pod: &pipewire::spa::pod::Pod) -> Option<CardProfile> {
    use pipewire::spa::pod::{deserialize::PodDeserializer, Value};
    // `SPA_PARAM_AVAILABILITY_no` — the one availability that means "cannot be selected".
    const AVAILABILITY_NO: u32 = 1;
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    let mut p = CardProfile {
        available: true,
        ..CardProfile::default()
    };
    for prop in obj.properties {
        match (prop.key, prop.value) {
            (pipewire::spa::sys::SPA_PARAM_PROFILE_index, Value::Int(i)) => p.index = i as u32,
            (pipewire::spa::sys::SPA_PARAM_PROFILE_name, Value::String(s)) => p.name = s,
            (pipewire::spa::sys::SPA_PARAM_PROFILE_description, Value::String(s)) => {
                p.description = s
            }
            (pipewire::spa::sys::SPA_PARAM_PROFILE_available, Value::Id(id)) => {
                p.available = id.0 != AVAILABILITY_NO
            }
            _ => {}
        }
    }
    Some(p)
}

/// The `Profile` object pod that selects `index`. `save = false` on purpose: this is a
/// borrowed profile for the length of a session, not a preference to write into the user's
/// WirePlumber state.
#[cfg(target_os = "linux")]
fn profile_pod(index: u32) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    use pipewire::spa;
    use spa::pod::{Object, Property, PropertyFlags, Value};
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamProfile.as_raw(),
        id: spa::param::ParamType::Profile.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_PROFILE_index,
                flags: PropertyFlags::empty(),
                value: Value::Int(index as i32),
            },
            Property {
                key: spa::sys::SPA_PARAM_PROFILE_save,
                flags: PropertyFlags::empty(),
                value: Value::Bool(false),
            },
        ],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .context("serialize")?
    .0
    .into_inner())
}

/// Correlate: walk the graph, pick the pad's four-channel node, and move the card's profile if
/// that is what stands between us and one. Returns the `node.name` to target.
#[cfg(target_os = "linux")]
pub fn correlate_pad_sink() -> anyhow::Result<String> {
    use anyhow::anyhow;
    let (sinks, cards) = walk_graph()?;
    match pick_pad_sink(&sinks, &cards) {
        Some(PadSinkPick::Node(name)) => Ok(name),
        Some(PadSinkPick::NeedsProfile(device_id)) => {
            if PROFILE_TRIED.lock().unwrap().contains(&device_id) {
                return Err(anyhow!(
                    "the DualSense card has no four-channel node and moving its profile did \
                     not help earlier this session — not moving it again"
                ));
            }
            ensure_pro_audio(device_id)?;
            // The card re-mints its nodes on a profile change; give the graph a moment to
            // publish them rather than failing into the caller's multi-second backoff.
            let mut last = Vec::new();
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                let (sinks, cards) = walk_graph()?;
                if let Some(PadSinkPick::Node(name)) = pick_pad_sink(&sinks, &cards) {
                    return Ok(name);
                }
                last = sinks;
            }
            // The swap did not help, so it is pure cost to the user: put the card back before
            // reporting, and remember not to move this one again.
            PROFILE_TRIED.lock().unwrap().push(device_id);
            restore_profile();
            // Name what we saw: a card whose nodes publish no `audio.channels` at all looks
            // exactly like a card stuck on stereo from here, and the two want different fixes.
            //
            // ⚠ The other way to land here is a profile change the session manager REFUSED —
            // `set_param` on a device is a write, and a sandboxed (flatpak) client is commonly
            // granted read-only permission on objects it does not own. There is no reply to
            // read, so this is where that shows up. Hence the manual instruction: switching the
            // card by hand is the same fix, and it always works.
            Err(anyhow!(
                "the DualSense card has no four-channel node, and moving its profile did not \
                 produce one — set the controller's Profile to \"Pro Audio\" in your sound \
                 settings (a sandboxed client may not be allowed to do it for you). Its sinks \
                 are [{}] (run `punktfunk-session --pad-audio-test` for the full graph)",
                last.iter()
                    .filter(|s| s.ds5)
                    .map(|s| format!("{}={}ch", s.name, s.channels))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        None => Err(anyhow!("no DualSense sound card in the PipeWire graph")),
    }
}

// ---- the on-glass devtest (Linux) ------------------------------------------------------------

/// `punktfunk-session --pad-audio-test`: report what the correlation sees, then drive a tone
/// into the pad so "nothing happens" can be told apart from "nothing arrives".
///
/// The two failures this separates are the whole reason it exists. A silent pad with a host
/// streaming could be the plane (nothing arriving), the graph (arriving and folded away), or
/// the pad (arriving, routed, and the firmware muted). This walks the same correlation the
/// renderer does, prints every DualSense object it found and the node it chose, and then puts a
/// 200 Hz sine on the voice-coil pair — channels 3 and 4 — with the speaker pair silent. If the
/// pad buzzes, everything below the plane is good.
#[cfg(target_os = "linux")]
pub fn pad_audio_test(seconds: u64, coils: bool, speaker: bool) -> anyhow::Result<()> {
    // Whatever happens in here, the card goes back the way we found it. An early `?` used to
    // skip the restore and leave a real Deck sitting on Pro Audio.
    let out = pad_audio_test_inner(seconds, coils, speaker);
    restore_profile();
    out
}

#[cfg(target_os = "linux")]
fn pad_audio_test_inner(seconds: u64, coils: bool, speaker: bool) -> anyhow::Result<()> {
    let (sinks, cards) = walk_graph()?;
    // The totals first: "no DualSense here" and "this walk saw nothing at all" print the same
    // empty list otherwise, and they are completely different faults.
    println!(
        "== DualSense objects in the PipeWire graph (of {} sinks, {} cards) ==",
        sinks.len(),
        cards.len()
    );
    for c in cards.iter().filter(|c| c.ds5) {
        println!("card   id={:<5} {}  ({})", c.id, c.name, c.description);
    }
    for s in sinks.iter().filter(|s| {
        s.ds5
            || s.device_id
                .is_some_and(|id| cards.iter().any(|c| c.id == id && c.ds5))
    }) {
        println!(
            "{:<7} device.id={:<7} channels={} position={:<24} {}{}",
            if s.internal { "parent" } else { "sink" },
            s.device_id
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-(virtual)".into()),
            s.channels,
            if s.positions.is_empty() {
                "-".into()
            } else {
                s.positions.join(",")
            },
            s.name,
            s.split_parent
                .as_deref()
                .map(|p| format!("  split.parent={p}"))
                .unwrap_or_default(),
        );
    }
    match pick_pad_sink(&sinks, &cards) {
        None => {
            println!("\nno DualSense sound card found — is the pad plugged in over USB?");
            anyhow::bail!("no DualSense sound card in the PipeWire graph");
        }
        Some(PadSinkPick::Node(n)) => println!("\npick: render on {n}"),
        Some(PadSinkPick::NeedsProfile(d)) => println!(
            "\npick: card {d} has no four-channel node — moving it to a four-channel profile"
        ),
    }

    let out = PadOut::open()?;
    println!(
        "playing {seconds}s: {} — the coils are channels 3/4, the speaker 1/2",
        match (coils, speaker) {
            (true, true) => "a tone on BOTH pairs",
            (true, false) => "a tone on the voice coils only",
            (false, true) => "a tone on the speaker only",
            (false, false) => "silence (both pairs off)",
        }
    );
    // 200 Hz at half scale: low enough that the coils move air rather than click, loud enough
    // to feel through a grip. 480-frame (10 ms) chunks, paced by the wall clock — this is a
    // devtest, so the ring policy downstream is what absorbs the jitter.
    let mut phase = 0f32;
    let step = std::f32::consts::TAU * 200.0 / 48_000.0;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let mut chunk = out.take_buffer();
        chunk.clear();
        for _ in 0..480 {
            let s = phase.sin() * 0.5;
            phase = (phase + step) % std::f32::consts::TAU;
            let sp = if speaker { s } else { 0.0 };
            let co = if coils { s } else { 0.0 };
            chunk.extend_from_slice(&[sp, sp, co, co]);
        }
        out.push(chunk);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(out);
    Ok(())
}

// ---- correlation: Windows (HID container → render endpoint) ---------------------------------

/// One enumerated render endpoint reduced to what the matcher needs. Pure data so the container
/// match is unit-testable off-box.
#[cfg(any(windows, test))]
pub(crate) struct EndpointCandidate {
    /// The `IMMDevice` endpoint id (`{0.0.0.00000000}.{…}`) — what WASAPI's device targeting
    /// takes.
    pub(crate) id: String,
    /// The endpoint's `PKEY_Device_ContainerId` as a braced lowercase GUID string; `None` when
    /// unreadable.
    pub(crate) container: Option<String>,
    /// Channel count of the endpoint's device format.
    pub(crate) channels: u16,
}

/// The endpoint belonging to the pad: container match AND a 4-channel device format (the DS5
/// audio function is the only 4-ch endpoint in its container — the mix-format gate keeps a
/// hypothetical sibling stereo endpoint from winning).
#[cfg(any(windows, test))]
pub(crate) fn pick_pad_endpoint<'a>(
    endpoints: &'a [EndpointCandidate],
    container: &str,
) -> Option<&'a EndpointCandidate> {
    endpoints.iter().find(|e| {
        e.channels == 4
            && e.container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(container))
    })
}

/// A device INSTANCE id from a device INTERFACE path: strip the `\\?\` (or `\\.\`) prefix,
/// the `#` separators become `\`, and the trailing `{interface-class-guid}` segment drops —
/// `\\?\HID#VID_054C&PID_0CE6#8&2de&0&0000#{4d1e55b2-…}` → `HID\VID_054C&PID_0CE6\8&2de&0&0000`.
/// That instance id is the devnode's key under `HKLM\SYSTEM\CurrentControlSet\Enum`, where its
/// `ContainerID` lives.
#[cfg(any(windows, test))]
pub(crate) fn hid_instance_from_interface_path(path: &str) -> Option<String> {
    let p = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
        .unwrap_or(path);
    let mut segs: Vec<&str> = p.split('#').collect();
    if let Some(last) = segs.last() {
        if last.starts_with('{') && last.ends_with('}') {
            segs.pop();
        }
    }
    if segs.len() != 3 || segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(segs.join("\\"))
}

/// Parse a serialized `VT_CLSID` PROPVARIANT registry blob (the on-disk shape of the MMDevices
/// property store: 8-byte header `[vt, 0, 0, 0, 1, 0, 0, 0]`, then the GUID in registry byte
/// order) into a braced lowercase GUID string. `None` for anything else.
#[cfg(any(windows, test))]
pub(crate) fn container_guid_from_blob(bytes: &[u8]) -> Option<String> {
    const VT_CLSID: u8 = 0x48;
    if bytes.len() < 24 || bytes[0] != VT_CLSID {
        return None;
    }
    let g = &bytes[8..24];
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    Some(format!(
        "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    ))
}

/// Resolve the SDL HID interface path to the matching 4-ch render endpoint id — the Windows
/// correlation chain: interface path → instance id → devnode `ContainerID` (registry) → the
/// active eRender endpoint whose stamped `PKEY_Device_ContainerId` matches with a 4-channel
/// device format. Registry-only for the property reads (the MMDevices ACL denies writes, never
/// reads); the endpoint enumeration runs on its own MTA thread like [`crate::audio::devices`].
#[cfg(windows)]
pub(crate) fn correlate_pad_endpoint(hid_path: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let instance = hid_instance_from_interface_path(hid_path)
        .with_context(|| format!("unrecognised HID interface path shape: {hid_path}"))?;
    let container = hid_container_id(&instance)
        .with_context(|| format!("no ContainerID on devnode {instance}"))?;
    let endpoints = render_endpoints()?;
    pick_pad_endpoint(&endpoints, &container)
        .map(|e| e.id.clone())
        .with_context(|| {
            format!(
                "no active 4-ch render endpoint in container {container} \
                 ({} endpoints inspected)",
                endpoints.len()
            )
        })
}

/// The devnode's `ContainerID` value (a braced GUID string) under
/// `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>`.
#[cfg(windows)]
fn hid_container_id(instance: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"))
        .with_context(|| format!(r"open Enum\{instance}"))?;
    key.get_value::<String, _>("ContainerID")
        .context("read ContainerID")
}

/// Enumerate the active eRender endpoints with the container + channel facts the matcher needs.
/// Its own short-lived MTA thread (the caller may sit in an STA — the [`crate::audio::devices`]
/// discipline); one broken endpoint must not hide the rest.
#[cfg(windows)]
fn render_endpoints() -> anyhow::Result<Vec<EndpointCandidate>> {
    use anyhow::{anyhow, Context};
    std::thread::Builder::new()
        .name("pf-pad-audio-enum".into())
        .spawn(|| -> anyhow::Result<Vec<EndpointCandidate>> {
            wasapi::initialize_mta()
                .ok()
                .context("CoInitializeEx (MTA)")?;
            let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
            let coll = enumerator
                .get_device_collection(&wasapi::Direction::Render)
                .context("render endpoint collection")?;
            let mut out = Vec::new();
            for i in 0..coll.get_nbr_devices().context("endpoint count")? {
                let Ok(dev) = coll.get_device_at_index(i) else {
                    continue;
                };
                let Ok(id) = dev.get_id() else {
                    continue;
                };
                let channels = dev
                    .get_device_format()
                    .map(|f| f.get_nchannels())
                    .unwrap_or(0);
                out.push(EndpointCandidate {
                    container: endpoint_container_id(&id),
                    id,
                    channels,
                });
            }
            Ok(out)
        })
        .context("spawn pad-audio enumeration thread")?
        .join()
        .map_err(|_| anyhow!("pad-audio enumeration thread panicked"))?
}

/// The endpoint's stamped `PKEY_Device_ContainerId`, read from its MMDevices property store in
/// the registry (`…\MMDevices\Audio\Render\{ep-guid}\Properties`, value
/// `"{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2"`, a serialized VT_CLSID blob).
#[cfg(windows)]
fn endpoint_container_id(endpoint_id: &str) -> Option<String> {
    let guid = endpoint_id.rfind('{').map(|i| &endpoint_id[i..])?;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\Properties"
        ))
        .ok()?;
    let v = key
        .get_raw_value("{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2")
        .ok()?;
    container_guid_from_blob(&v.bytes)
}

// ---- the 4-channel mixer --------------------------------------------------------------------

/// Interleave the two independent stereo streams into one 4-ch frame stream: speaker
/// ([`PAD_AUDIO_KIND_SPEAKER`]) on channels 0/1, haptics ([`PAD_AUDIO_KIND_HAPTICS`]) on 2/3.
/// Each kind has its own write cursor (they arrive on different cadences — 10 ms vs 5 ms);
/// [`pop`](Self::pop) emits every frame the FURTHER-ahead kind has filled, with the lagging
/// (or absent) kind's pair reading zeros — so a haptics-only session plays voice-coil audio
/// with a silent speaker pair and vice versa. Pure logic (unit-tested); pacing and latency
/// bounds live in the platform ring downstream.
pub(crate) struct QuadMixer {
    /// Interleaved 4-ch samples; the front is the next frame to output. Length is always
    /// `ready_frames() * 4` (pushes zero-extend for their own cursor only).
    ring: std::collections::VecDeque<f32>,
    /// Per-kind write cursor in FRAMES relative to the ring front (`[haptics, speaker]` —
    /// indexed by the wire `kind`).
    written: [usize; 2],
}

impl QuadMixer {
    pub(crate) fn new() -> QuadMixer {
        QuadMixer {
            ring: std::collections::VecDeque::new(),
            written: [0; 2],
        }
    }

    /// Write one decoded stereo chunk (interleaved L/R) for `kind` at that kind's cursor,
    /// zero-extending the ring as needed. Depth is bounded by [`MAX_BUFFER_FRAMES`] — overflow
    /// drops the oldest frames (both kinds shift together, so the interleave never skews).
    pub(crate) fn push(&mut self, kind: u8, stereo: &[f32]) {
        let k = (kind as usize).min(1);
        let off = if kind == PAD_AUDIO_KIND_SPEAKER { 0 } else { 2 };
        let frames = stereo.len() / 2;
        let base = self.written[k];
        let need = (base + frames) * PAD_CHANNELS;
        if self.ring.len() < need {
            self.ring.resize(need, 0.0);
        }
        for (i, fr) in stereo.chunks_exact(2).enumerate() {
            let at = (base + i) * PAD_CHANNELS + off;
            self.ring[at] = fr[0];
            self.ring[at + 1] = fr[1];
        }
        self.written[k] = base + frames;
        let over = self.ready_frames().saturating_sub(MAX_BUFFER_FRAMES);
        if over > 0 {
            self.drop_front(over);
        }
    }

    /// Frames ready to output: the further-ahead kind's cursor (the other pair reads zeros).
    pub(crate) fn ready_frames(&self) -> usize {
        self.written[0].max(self.written[1])
    }

    /// Append every ready frame (interleaved 4-ch) to `out`; returns the frame count. Both
    /// cursors move back together, so a kind that lagged simply resumes at the new front.
    pub(crate) fn pop(&mut self, out: &mut Vec<f32>) -> usize {
        let frames = self.ready_frames();
        let n = frames * PAD_CHANNELS;
        debug_assert_eq!(self.ring.len(), n);
        out.extend(self.ring.drain(..n.min(self.ring.len())));
        for w in &mut self.written {
            *w = w.saturating_sub(frames);
        }
        frames
    }

    /// Throw the ready frames away (no output device right now).
    pub(crate) fn discard(&mut self) {
        let f = self.ready_frames();
        self.drop_front(f);
    }

    fn drop_front(&mut self, frames: usize) {
        let n = (frames * PAD_CHANNELS).min(self.ring.len());
        self.ring.drain(..n);
        let f = n / PAD_CHANNELS;
        for w in &mut self.written {
            *w = w.saturating_sub(f);
        }
    }
}

// ---- decode + PLC ---------------------------------------------------------------------------

/// Per-(pad, kind) decode state: a stereo 48 kHz Opus decoder, the seq-gap tracker, and the
/// last decoded frame size (the PLC synthesis unit — session.rs's audio-thread discipline).
struct KindStream {
    dec: opus::Decoder,
    gaps: AudioGapTracker,
    frame_samples: usize,
}

/// How many concealment frames to synthesize before decoding `seq`: the tracker's capped gap
/// count — but 0 until a first frame decoded (`frame_samples == 0`; there is nothing to size
/// the PLC from). The tracker is ALWAYS fed, so a pre-first-frame gap can't replay later as a
/// phantom gap. Pure (unit-tested).
fn plc_frames(gaps: &mut AudioGapTracker, seq: u32, frame_samples: usize) -> u32 {
    let missing = gaps.missing_before(seq);
    if frame_samples == 0 {
        0
    } else {
        missing
    }
}

// ---- the renderer worker --------------------------------------------------------------------

/// Spawn the pad-audio renderer thread — the 0xD1 plane's single consumer, started by the
/// session pump whenever the settings could render (`pad_haptics` / `pad_speaker == "pad"`).
/// The output device is opened LAZILY on the first arriving frame: frames only flow once a
/// tier-A pad declared render caps on its arrival, so a session without a wired DualSense
/// costs one idle 10 ms poll loop and never touches the audio graph. Exits on the session
/// stop flag (join it like the audio thread) or the plane closing.
pub(crate) fn spawn(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    haptics: bool,
    speaker: bool,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-pad-audio".into())
        .spawn(move || run(&connector, &stop, haptics, speaker))
        .map_err(|e| tracing::warn!(error = %e, "pad-audio thread failed to start"))
        .ok()
}

fn run(connector: &NativeClient, stop: &AtomicBool, haptics: bool, speaker: bool) {
    // The pad's audio is a haptic: a late decode is a rumble that lands after the hit. Same
    // best-effort priority as the main decode leg (`audio_rt`).
    crate::audio_rt::boost_and_log("pf-pad-audio");
    // Per-kind decode state for the ONE rendered pad (v1: the first pad that streams; the
    // spec's per-(pad, kind) fan-out degenerates to per-kind once the pad is latched).
    let mut streams: [Option<KindStream>; 2] = [None, None];
    let mut mixer = QuadMixer::new();
    let mut pcm = vec![0f32; 5760 * 2]; // scratch: max Opus frame (120 ms) × stereo
    let mut out: Option<PadOut> = None;
    let mut active_pad: Option<u8> = None;
    let mut other_pad_logged = false;
    let mut open_fail_logged = false;
    let mut retry_at = Instant::now();
    let mut backoff = RETRY_MIN;
    while !stop.load(Ordering::SeqCst) {
        let Some(f) = connector.next_pad_audio(Duration::from_millis(10)) else {
            if connector.is_session_ended() {
                break;
            }
            continue;
        };
        // The host only emits kinds the arrival declared, but the settings gate is re-checked
        // here so a stale host can never force an undeclared renderer.
        if f.kind > 1
            || (f.kind == PAD_AUDIO_KIND_HAPTICS && !haptics)
            || (f.kind == PAD_AUDIO_KIND_SPEAKER && !speaker)
        {
            continue;
        }
        // v1 renders ONE physical DualSense: latch the first streaming pad, drop the rest.
        match active_pad {
            None => active_pad = Some(f.pad),
            Some(p) if p != f.pad => {
                if !other_pad_logged {
                    other_pad_logged = true;
                    tracing::info!(
                        rendered = p,
                        ignored = f.pad,
                        "pad audio from a second pad — v1 renders one physical DualSense"
                    );
                }
                continue;
            }
            _ => {}
        }
        let k = f.kind as usize;
        if streams[k].is_none() {
            match opus::Decoder::new(48_000, opus::Channels::Stereo) {
                Ok(dec) => {
                    streams[k] = Some(KindStream {
                        dec,
                        gaps: AudioGapTracker::new(),
                        frame_samples: 0,
                    })
                }
                Err(e) => {
                    tracing::warn!(error = %e, kind = f.kind, "pad-audio opus decoder failed");
                    continue;
                }
            }
        }
        let st = streams[k].as_mut().expect("inserted above");
        // Conceal lost packets (a seq gap) with libopus PLC before decoding the arrival —
        // the session audio thread's exact discipline. A frozen seq (the host paused the
        // stream) produces no packets at all, which is silence by construction.
        for _ in 0..plc_frames(&mut st.gaps, f.seq, st.frame_samples) {
            let n = st.frame_samples * 2;
            if let Ok(samples) = st.dec.decode_float(&[], &mut pcm[..n], false) {
                mixer.push(f.kind, &pcm[..samples * 2]);
            }
        }
        if !f.opus.is_empty() {
            match st.dec.decode_float(&f.opus, &mut pcm, false) {
                Ok(samples) => {
                    st.frame_samples = samples;
                    mixer.push(f.kind, &pcm[..samples * 2]);
                }
                Err(e) => tracing::debug!(error = %e, kind = f.kind, "pad-audio opus decode"),
            }
        }
        // Output: open lazily (frames flowing prove a tier-A pad exists), drop + re-correlate
        // with backoff when the device goes away (USB unplug kills the sink/endpoint).
        if out.as_ref().is_some_and(PadOut::finished) {
            tracing::info!("pad-audio output ended (device gone?) — re-correlating");
            out = None;
            retry_at = Instant::now() + backoff;
            backoff = (backoff * 2).min(RETRY_MAX);
        }
        if out.is_none() && Instant::now() >= retry_at {
            match PadOut::open() {
                Ok(o) => {
                    tracing::info!("pad-audio output opened on the DualSense audio device");
                    out = Some(o);
                    backoff = RETRY_MIN;
                    open_fail_logged = false;
                }
                Err(e) => {
                    if !open_fail_logged {
                        open_fail_logged = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "no DualSense audio device — pad audio parked (retrying with backoff)"
                        );
                    }
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
        match &out {
            Some(o) => {
                let mut chunk = o.take_buffer();
                if mixer.pop(&mut chunk) > 0 {
                    o.push(chunk);
                }
            }
            None => mixer.discard(),
        }
    }
    // Drop the output BEFORE the profile goes back: the card cannot leave a profile whose PCM
    // we still hold open, and a failed restore would leave the user's pad on Pro Audio.
    drop(out);
    #[cfg(target_os = "linux")]
    restore_profile();
    tracing::debug!("pad-audio pull thread exited");
}

// ---- platform output: Linux (PipeWire) ------------------------------------------------------

/// The platform output half: a dedicated device thread fed interleaved 4-ch f32 chunks over a
/// bounded channel with a recycle pool (the `AudioPlayer` shape), targeting the correlated
/// DualSense device. `finished()` is the device-gone signal — the worker drops the handle and
/// re-correlates with backoff.
#[cfg(target_os = "linux")]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PadOut {
    /// Correlate (the pad's four-channel card node, moving its profile if need be) and open the
    /// PipeWire playback stream on it.
    fn open() -> anyhow::Result<PadOut> {
        use anyhow::Context;
        let target = correlate_pad_sink()?;
        tracing::info!(sink = %target, "pad-audio sink matched");
        // 64 × 5 ms of slack between the renderer worker and the PipeWire loop, with the
        // recycle pool keeping the steady state allocation-free (the AudioPlayer shape).
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                // The pad stream's `process` runs on THIS thread (no RT_PROCESS), so this is
                // the thread that has to make the pad's device cycles. Best-effort.
                crate::audio_rt::boost_and_log("pf-pad-audio-out");
                if let Err(e) = pad_pw_thread(pcm_rx, recycle_tx, quit_rx, target) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio playback thread ended");
                }
            })
            .context("spawn pad-audio playback thread")?;
        Ok(PadOut {
            pcm_tx,
            recycle_rx,
            quit_tx,
            thread: Some(thread),
        })
    }

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(target_os = "linux")]
impl Drop for PadOut {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The PipeWire playback thread on the DualSense node: 4 unpositioned `AUX0..AUX3` channels
/// (ch0/1 the pad's speaker, ch2/3 the voice coils), a 5 ms quantum, and the session player's
/// adaptive ring policy at a SMALLER floor — haptics are felt latency, so the prime target is
/// 3 quanta bounded to [240, 2400] frames instead of the main player's [720, 9600].
#[cfg(target_os = "linux")]
fn pad_pw_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<()>,
    target: String,
) -> anyhow::Result<()> {
    use anyhow::Context;
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

    let props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-pad-audio",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Pad Audio",
        // ~5 ms quantum (one haptics Opus frame) keeps the ring — and the felt latency — small.
        *pw::keys::NODE_LATENCY     => "240/48000",
        // The correlated DualSense sink (raw key — the `keys::TARGET_OBJECT` constant is
        // feature-gated on a newer libpipewire than we require; the wire name is stable).
        "target.object"             => target.as_str(),
        // The pad unplugging must END this stream (the worker re-correlates), not let the
        // session manager re-route 4-ch haptics onto the desktop speakers.
        "node.dont-reconnect"       => "true",
        // ⚠ LOAD-BEARING: without this the graph POSITION-remixes our quad into whatever the
        // node's own channel map says, and on any positioned map the voice-coil pair is folded
        // into the speaker pair and disappears — the exact failure the "set it to Pro Audio"
        // advice exists to route around. With it, channel k goes to channel k, which is the
        // only mapping the pad's firmware understands. GE-Proton's pulse leg forces the same
        // thing (`PA_STREAM_NO_REMIX_CHANNELS` + a forced AUX map).
        *pw::keys::STREAM_DONT_REMIX => "true",
    };
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", props).context("pw Stream")?;

    struct PadPlayData {
        rx: std::sync::mpsc::Receiver<Vec<f32>>,
        recycle: std::sync::mpsc::SyncSender<Vec<f32>>,
        ring: std::collections::VecDeque<f32>,
        primed: bool,
    }
    let ud = PadPlayData {
        rx: pcm_rx,
        recycle: recycle_tx,
        ring: std::collections::VecDeque::new(),
        primed: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed({
            let mainloop = mainloop.clone();
            move |_s, _ud, old, new| {
                tracing::debug!(?old, ?new, "pipewire pad-audio stream state");
                // Device gone (USB unplug) with dont-reconnect: the stream errors out — end
                // the thread so the worker's `finished()` check re-correlates with backoff.
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    mainloop.quit();
                }
            }
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(mut chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk.iter().copied());
                    chunk.clear();
                    let _ = ud.recycle.try_send(chunk);
                }
                let stride = 4 * PAD_CHANNELS; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let want_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want = want_frames * PAD_CHANNELS;

                // The adaptive jitter buffer at the pad floor: prime to ~3 quanta within
                // [240, 2400] frames, cap ~1 quantum of slack beyond, re-prime after a drain.
                let target = (3 * want).clamp(240 * PAD_CHANNELS, 2400 * PAD_CHANNELS);
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
                tracing::error!("panic in pipewire pad-audio callback");
            }
        })
        .register()
        .context("register pad-audio listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(PAD_CHANNELS as u32);
    // AUX0..AUX3 (`enum spa_audio_channel`: `SPA_AUDIO_CHANNEL_START_Aux` = 0x1000), NOT a
    // positioned FL FR RL RR layout. Aux positions carry no spatial meaning, so they are what
    // the pad's own Pro Audio profile and WirePlumber's split parents advertise, what GE-Proton
    // forces on its own haptic streams, and what our host-side sink mints — one vocabulary end
    // to end. `stream.dont-remix` above makes the routing index-exact regardless; this makes it
    // index-exact by AGREEMENT as well, so nothing downstream has a position to reason about.
    const AUX0: u32 = 0x1000;
    let mut positions = [0u32; 64];
    positions[..4].copy_from_slice(&[AUX0, AUX0 + 1, AUX0 + 2, AUX0 + 3]);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pad format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pad pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw pad stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire pad-audio loop exited");
    Ok(())
}

// ---- platform output: Windows (WASAPI) ------------------------------------------------------

#[cfg(windows)]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl PadOut {
    /// Correlate (HID container → endpoint id) and open a shared event-driven render stream ON
    /// that endpoint (`audio_wasapi::render_thread`'s shape — autoconvert, default period).
    fn open() -> anyhow::Result<PadOut> {
        use anyhow::{anyhow, Context};
        let hid_path =
            first_tier_a_hid_path().ok_or_else(|| anyhow!("no tier-A pad registered"))?;
        let endpoint = correlate_pad_endpoint(&hid_path)?;
        tracing::info!(endpoint = %endpoint, "pad-audio endpoint correlated");
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                if let Err(e) = pad_render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, &endpoint) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio render thread ended");
                }
            })
            .context("spawn pad-audio render thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(PadOut {
                pcm_tx,
                recycle_rx,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("pad-audio render init timed out")),
        }
    }

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(windows)]
impl Drop for PadOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The WASAPI render thread ON the correlated endpoint: shared event-driven, autoconvert, 4 ch
/// f32 masked FL|FR|BL|BR (0x33 — the DS5 endpoint's own layout, so the map is identity), and
/// the session player's ring policy at the pad floor ([240, 2400] frames instead of
/// [720, 9600] — haptics are felt latency). Any device error (unplug) ends the thread; the
/// worker's `finished()` check re-correlates with backoff.
#[cfg(windows)]
fn pad_render_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    let res = (|| -> anyhow::Result<()> {
        const BLOCK_ALIGN: usize = PAD_CHANNELS * 4; // f32 interleaved
        let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
        // Not `get_device`: that helper resolved through a freed string through wasapi 0.23, and
        // this path additionally wants the ACTIVE-only filter — see [`crate::audio::device_by_id`]
        // (audio_wasapi.rs, mounted as `crate::audio` on Windows by lib.rs's `#[path]` swap —
        // there is no `audio_wasapi` module name).
        let device = crate::audio::device_by_id(&enumerator, &Direction::Render, endpoint_id)
            .map_err(|e| anyhow!("correlated endpoint not found: {e:#}"))?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // FL|FR|BL|BR: front pair = the pad's speaker, back pair = the voice coils.
        let desired = WaveFormat::new(32, 32, &SampleType::Float, 48_000, PAD_CHANNELS, Some(0x33));
        let (default_period, _min_period) =
            audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize pad render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        audio_client
            .start_stream()
            .context("start pad render stream")?;
        let _ = ready.send(Ok(()));

        // The adaptive jitter buffer in f32-byte units, at the pad floor (see the module doc).
        let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut primed = false;
        let mut out = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            if h_event.wait_for_event(100).is_err() {
                continue;
            }
            while let Ok(mut chunk) = pcm_rx.try_recv() {
                for s in chunk.iter() {
                    ring.extend(s.to_le_bytes());
                }
                chunk.clear();
                let _ = recycle_tx.try_send(chunk);
            }
            let avail_frames = audio_client
                .get_available_space_in_frames()
                .context("available space")? as usize;
            if avail_frames == 0 {
                continue;
            }
            let want_bytes = avail_frames * BLOCK_ALIGN;

            // Prime to ~3 quanta within [240, 2400] frames; cap ~1 quantum of slack beyond;
            // instant re-prime on a genuine drain.
            let target = (3 * want_bytes).clamp(240 * BLOCK_ALIGN, 2400 * BLOCK_ALIGN);
            let cap = target.max(want_bytes) + want_bytes;
            if ring.len() > cap {
                ring.drain(..ring.len() - cap);
            }
            if !primed && ring.len() >= target {
                primed = true;
            }

            out.clear();
            out.resize(want_bytes, 0);
            if primed {
                let n = ring.len().min(want_bytes);
                for (dst, b) in out.iter_mut().zip(ring.drain(..n)) {
                    *dst = b;
                }
            }
            if ring.is_empty() {
                primed = false;
            }
            render_client
                .write_to_device(avail_frames, &out, None)
                .context("write_to_device")?;
        }
        audio_client.stop_stream().ok();
        Ok(())
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
    }
    res
}

// ---- tests ----------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The speaker mode gate: only `"pad"` renders today; `"mix"` is the declared TODO and
    /// reads as off; unknown values (a future store, a typo) fail safe to off.
    #[test]
    fn speaker_mode_gates() {
        assert!(speaker_active("pad"));
        assert!(!speaker_active("off"));
        assert!(!speaker_active("mix")); // TODO leg — off until the mixer exists
        assert!(!speaker_active(""));
        assert!(!speaker_active("Pad")); // stored names are lowercase; anything else is off
    }

    /// Tier A needs all three facts: the DualSense/Edge VID:PID and a wired connection.
    #[test]
    fn tier_a_is_wired_ds5_or_edge_only() {
        assert!(is_tier_a_ds5(0x054C, 0x0CE6, true)); // DualSense
        assert!(is_tier_a_ds5(0x054C, 0x0DF2, true)); // DualSense Edge
        assert!(!is_tier_a_ds5(0x054C, 0x0CE6, false)); // Bluetooth → no audio device
        assert!(!is_tier_a_ds5(0x054C, 0x05C4, true)); // DualShock 4
        assert!(!is_tier_a_ds5(0x045E, 0x0CE6, true)); // wrong vendor, right product id
        assert!(!is_tier_a_ds5(0x28DE, 0x1205, true)); // Steam Deck
    }

    /// The Linux sink signature: USB vendor string in the node name, product in the
    /// description, or the kernel's bare "Wireless Controller" fallback — and nothing else.
    #[test]
    fn ds5_sink_signature_matching() {
        assert!(is_ds5_sink(
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.analog-stereo",
            "Wireless Controller Analog Stereo"
        ));
        assert!(is_ds5_sink(
            "alsa_output.usb-054c_0ce6-00",
            "DualSense Wireless Controller"
        ));
        assert!(is_ds5_sink("Wireless Controller", ""));
        assert!(is_ds5_sink("", "Wireless Controller Audio"));
        assert!(!is_ds5_sink(
            "alsa_output.pci-0000_0a_00.4.analog-stereo",
            "Built-in Audio Analog Stereo"
        ));
        // "Wireless Controller" must LEAD the string — a headset description mentioning
        // "... for Wireless Controller" is not the pad.
        assert!(!is_ds5_sink("headset", "Adapter for Wireless Controller"));
    }

    /// USB ids in a proplist are BASE 16, with or without an `0x` prefix. A decimal reading of
    /// `"054c"` fails outright, but of `"0994"` would succeed and be wrong — hence the parse.
    #[test]
    fn usb_ids_are_hex_either_spelling() {
        assert_eq!(parse_usb_id("054c"), Some(0x054C));
        assert_eq!(parse_usb_id("0x054c"), Some(0x054C));
        assert_eq!(parse_usb_id("0X0CE6"), Some(0x0CE6));
        assert_eq!(parse_usb_id(" 0df2 "), Some(0x0DF2));
        assert_eq!(parse_usb_id("0994"), Some(0x0994)); // NOT 994
        assert_eq!(parse_usb_id(""), None);
        assert_eq!(parse_usb_id("Sony"), None);
    }

    /// Identity from either half: the USB ids when published, the name signature otherwise —
    /// and BOTH ids have to agree, so another Sony audio device is not the pad.
    #[test]
    fn ds5_identity_from_ids_or_name() {
        assert!(props_say_ds5(
            Some("054c"),
            Some("0ce6"),
            "alsa_card.usb-x",
            ""
        ));
        assert!(props_say_ds5(Some("0x054C"), Some("0x0DF2"), "", "")); // Edge
        assert!(!props_say_ds5(Some("054c"), Some("0104"), "", "")); // a Sony headset
        assert!(!props_say_ds5(Some("046d"), Some("0ce6"), "", "")); // wrong vendor
        assert!(!props_say_ds5(None, None, "alsa_card.pci-0000_0a_00.4", ""));
        // No ids at all — the split sinks of a UCM card publish none — so the name carries it.
        assert!(props_say_ds5(
            None,
            None,
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.HiFi__Speaker__sink",
            "Speaker"
        ));
    }

    /// The unpositioned test: AUX (and the unknown markers) are index-routed, anything spatial
    /// is not.
    #[test]
    fn aux_and_unknown_maps_are_unpositioned() {
        let v = |s: &str| -> Vec<String> { s.split(',').map(|p| p.trim().to_string()).collect() };
        assert!(is_unpositioned(&v("AUX0,AUX1,AUX2,AUX3")));
        assert!(is_unpositioned(&v("UNK,UNK,UNK,UNK")));
        assert!(is_unpositioned(&[]));
        assert!(!is_unpositioned(&v("FL,FR,RL,RR")));
        assert!(!is_unpositioned(&v("MONO")));
        assert!(!is_unpositioned(&v("AUX0,AUX1,FL,FR")));
    }

    fn sink(name: &str, channels: u32, positions: &str, device_id: Option<u32>) -> SinkNode {
        SinkNode {
            name: name.into(),
            description: String::new(),
            device_id,
            channels,
            positions: if positions.is_empty() {
                Vec::new()
            } else {
                positions.split(',').map(str::to_string).collect()
            },
            split_parent: None,
            ds5: true,
            internal: false,
        }
    }

    /// Four channels on a real card is the whole requirement, and the unpositioned node wins
    /// when both shapes are present.
    #[test]
    fn pad_sink_pick_needs_four_channels_on_a_card() {
        let cards = [CardDevice {
            id: 42,
            ds5: true,
            ..CardDevice::default()
        }];
        let sinks = [
            sink("ds5.analog-stereo", 2, "FL,FR", Some(42)),
            sink("ds5.analog-surround-40", 4, "FL,FR,RL,RR", Some(42)),
            sink("ds5.pro-output-0", 4, "AUX0,AUX1,AUX2,AUX3", Some(42)),
        ];
        assert_eq!(
            pick_pad_sink(&sinks, &cards),
            Some(PadSinkPick::Node("ds5.pro-output-0".into()))
        );
        // Without the AUX node the positioned quad is taken (dont-remix makes it equivalent).
        assert_eq!(
            pick_pad_sink(&sinks[..2], &cards),
            Some(PadSinkPick::Node("ds5.analog-surround-40".into()))
        );
        // Stereo only: the pad is here, but the profile is in the way.
        assert_eq!(
            pick_pad_sink(&sinks[..1], &cards),
            Some(PadSinkPick::NeedsProfile(42))
        );
        assert_eq!(pick_pad_sink(&[], &cards), None);
    }

    /// A HOST's minted pad sink carries the full DualSense identity on purpose — and no
    /// `device.id`, because it is a stream and not a card. Rendering into it would loop the
    /// plane back at the host instead of driving a pad in someone's hands.
    #[test]
    fn pad_sink_pick_skips_a_virtual_host_sink() {
        let virtual_sink = sink(
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.HiFi__Speaker__sink",
            4,
            "AUX0,AUX1,AUX2,AUX3",
            None,
        );
        assert_eq!(
            pick_pad_sink(std::slice::from_ref(&virtual_sink), &[]),
            None
        );
        // With a real pad present as well, the real one is what gets picked.
        let cards = [CardDevice {
            id: 7,
            ds5: true,
            ..CardDevice::default()
        }];
        let real = sink("ds5.pro-output-0", 4, "AUX0,AUX1,AUX2,AUX3", Some(7));
        assert_eq!(
            pick_pad_sink(&[virtual_sink, real], &cards),
            Some(PadSinkPick::Node("ds5.pro-output-0".into()))
        );
    }

    /// The real thing, transcribed from a Steam Deck (SteamOS 3.7, `alsa-ucm-conf` with
    /// `DualSense-PS5.conf`) with a wired DualSense: the card publishes a four-channel
    /// `SpeakerHaptic` sink, a one-channel `Speaker` sink, AND a hidden four-channel parent.
    ///
    /// Two things this pins. The old name-only matcher would take whichever of the two public
    /// sinks the registry happened to replay first — a coin flip against a MONO node that
    /// cannot carry the coils at all. And the parent, despite being the unpositioned/raw one,
    /// must NOT win: its AUX0 is a dead hardware channel, so index-exact into it drops half
    /// the speaker signal (see `pick_pad_sink`'s table).
    #[test]
    fn pad_sink_pick_on_a_real_steamos_dualsense() {
        let cards = [CardDevice {
            id: 140,
            name: "alsa_card.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00"
                .into(),
            description: "DualSense wireless controller (PS5)".into(),
            ds5: true,
        }];
        let base =
            "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00";
        let mut parent = sink(
            "alsa_output.hw_Controller_0",
            4,
            "AUX0,AUX1,AUX2,AUX3",
            Some(140),
        );
        parent.internal = true;
        parent.ds5 = false; // the parent's own proplist carries no vendor ids or product name
        let mut haptic = sink(
            &format!("{base}.HiFi__SpeakerHaptic__sink"),
            4,
            "FL,FR,RL,RR",
            Some(140),
        );
        haptic.split_parent = Some("alsa_output.hw_Controller_0".into());
        let mut mono = sink(&format!("{base}.HiFi__Speaker__sink"), 1, "MONO", Some(140));
        mono.split_parent = Some("alsa_output.hw_Controller_0".into());

        // Registry replay order is not ours to choose, so it must not matter.
        for order in [
            vec![parent.clone(), haptic.clone(), mono.clone()],
            vec![mono.clone(), parent.clone(), haptic.clone()],
            vec![haptic.clone(), mono.clone(), parent.clone()],
        ] {
            assert_eq!(
                pick_pad_sink(&order, &cards),
                Some(PadSinkPick::Node(format!(
                    "{base}.HiFi__SpeakerHaptic__sink"
                ))),
                "the four-channel public sink must win from any enumeration order"
            );
        }
        // With only the mono sink and the parent visible, the parent is the right fallback —
        // half a speaker beats no coils.
        assert_eq!(
            pick_pad_sink(&[mono, parent], &cards),
            Some(PadSinkPick::Node("alsa_output.hw_Controller_0".into()))
        );
    }

    /// A split card's public sinks are mono/stereo, and the four-channel parent they name is
    /// the node the coils live behind — GE-Proton's preferred leg. The CARD carries the
    /// identity there; the split sinks themselves need not.
    #[test]
    fn pad_sink_pick_follows_a_split_parent() {
        let cards = [CardDevice {
            id: 3,
            ds5: true,
            ..CardDevice::default()
        }];
        let mut speaker = sink("ds5.HiFi__Speaker__sink", 1, "MONO", Some(3));
        speaker.ds5 = false; // identity comes from the card
        speaker.split_parent = Some("alsa_output.hw_3_0".into());
        let mut phones = sink("ds5.HiFi__Headphones__sink", 2, "FL,FR", Some(3));
        phones.ds5 = false;
        assert_eq!(
            pick_pad_sink(&[speaker, phones], &cards),
            Some(PadSinkPick::Node("alsa_output.hw_3_0".into()))
        );
    }

    /// Profile choice: Pro Audio first, a four-channel positioned output as the fallback, and
    /// an unavailable profile is never selected (it would silently leave the card put).
    #[test]
    fn profile_choice_prefers_pro_audio() {
        let p = |name: &str, index: u32, available: bool| CardProfile {
            index,
            name: name.into(),
            description: name.into(),
            available,
        };
        let all = [
            p("off", 0, true),
            p("output:analog-stereo", 1, true),
            p("output:analog-surround-40", 2, true),
            p("pro-audio", 3, true),
        ];
        assert_eq!(pick_profile(&all).map(|p| p.index), Some(3));
        assert_eq!(pick_profile(&all[..3]).map(|p| p.index), Some(2));
        assert_eq!(pick_profile(&all[..2]).map(|p| p.index), None);
        // Unavailable Pro Audio falls through to the positioned quad rather than being picked.
        let unavailable = [
            p("output:analog-surround-40", 2, true),
            p("pro-audio", 3, false),
        ];
        assert_eq!(pick_profile(&unavailable).map(|p| p.index), Some(2));
    }

    /// The Windows container matcher: container equality (case-insensitive — registry GUIDs
    /// come in both cases) AND the 4-channel format gate.
    #[test]
    fn endpoint_pick_needs_container_and_four_channels() {
        let cands = [
            EndpointCandidate {
                id: "{0.0.0.00000000}.{aaaa}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 2, // right container, stereo — not the pad function
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{bbbb}".into(),
                container: Some("{99999999-2222-3333-4444-555555555555}".into()),
                channels: 4, // 4-ch but another container
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{cccc}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 4, // the pad
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{dddd}".into(),
                container: None,
                channels: 4,
            },
        ];
        let hit = pick_pad_endpoint(&cands, "{11111111-2222-3333-4444-555555555555}").unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        // Case-insensitive (Enum stores uppercase, MMDevices lowercase).
        let hit = pick_pad_endpoint(
            &cands,
            "{11111111-2222-3333-4444-555555555555}"
                .to_uppercase()
                .as_str(),
        )
        .unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        assert!(pick_pad_endpoint(&cands, "{00000000-0000-0000-0000-000000000000}").is_none());
    }

    /// Interface path → instance id: prefix stripped, `#` → `\`, interface-class GUID dropped;
    /// garbage shapes are rejected rather than mis-keyed into the registry.
    #[test]
    fn hid_interface_path_to_instance_id() {
        assert_eq!(
            hid_instance_from_interface_path(
                r"\\?\HID#VID_054C&PID_0CE6#8&2de99099&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"
            )
            .as_deref(),
            Some(r"HID\VID_054C&PID_0CE6\8&2de99099&0&0000")
        );
        // hidapi paths come lowercase and sometimes without the class GUID — both parse.
        assert_eq!(
            hid_instance_from_interface_path(r"\\?\hid#vid_054c&pid_0df2#7&1a2b3c4d&1&0000")
                .as_deref(),
            Some(r"hid\vid_054c&pid_0df2\7&1a2b3c4d&1&0000")
        );
        for bad in ["", "/dev/hidraw3", r"\\?\HID#VID_054C", "a#b#c#d#e"] {
            assert_eq!(
                hid_instance_from_interface_path(bad),
                None,
                "{bad:?} parsed"
            );
        }
    }

    /// The serialized VT_CLSID blob (8-byte PROPVARIANT header + registry-order GUID) parses to
    /// the braced string; short/foreign blobs don't.
    #[test]
    fn container_blob_parses_vt_clsid() {
        // {11223344-5566-7788-99aa-bbccddeeff00}: data1/2/3 little-endian on disk.
        let mut blob = vec![0x48, 0, 0, 0, 1, 0, 0, 0];
        blob.extend_from_slice(&[
            0x44, 0x33, 0x22, 0x11, 0x66, 0x55, 0x88, 0x77, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ]);
        assert_eq!(
            container_guid_from_blob(&blob).as_deref(),
            Some("{11223344-5566-7788-99aa-bbccddeeff00}")
        );
        assert_eq!(container_guid_from_blob(&blob[..20]), None); // truncated
        let mut wrong_vt = blob.clone();
        wrong_vt[0] = 0x41; // VT_BLOB — a format value, not a container
        assert_eq!(container_guid_from_blob(&wrong_vt), None);
    }

    /// The 4-ch interleave: speaker frames land on channels 0/1 at the speaker cursor, haptics
    /// on 2/3 at theirs, and the pop emits exactly the further-ahead kind's frame count with
    /// the lagging kind's tail zeroed.
    #[test]
    fn mixer_interleaves_kinds_into_quad_frames() {
        let mut m = QuadMixer::new();
        // 2 speaker frames, 1 haptics frame.
        m.push(PAD_AUDIO_KIND_SPEAKER, &[1.0, 2.0, 3.0, 4.0]);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[5.0, 6.0]);
        assert_eq!(m.ready_frames(), 2);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![1.0, 2.0, 5.0, 6.0, 3.0, 4.0, 0.0, 0.0]);
        assert_eq!(m.ready_frames(), 0);
        // After the pop both cursors are back at the front: the next haptics frame starts a
        // fresh quad frame with a silent speaker pair.
        m.push(PAD_AUDIO_KIND_HAPTICS, &[7.0, 8.0]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![0.0, 0.0, 7.0, 8.0]);
    }

    /// A single flowing kind never conjures data on the other pair (kind 1 → 0/1, kind 0 → 2/3).
    #[test]
    fn mixer_missing_kind_stays_zero() {
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[0.5, -0.5, 0.25, -0.25]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.25, -0.25]);
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.5, -0.5]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    /// The depth bound: a wedged output can't grow the mixer past [`MAX_BUFFER_FRAMES`]; the
    /// oldest frames drop and both cursors shift together (no interleave skew).
    #[test]
    fn mixer_caps_depth_dropping_oldest() {
        let mut m = QuadMixer::new();
        let chunk = vec![1.0f32; 480 * 2]; // 480 frames per push
        for _ in 0..12 {
            m.push(PAD_AUDIO_KIND_HAPTICS, &chunk); // 5760 frames pushed
        }
        assert_eq!(m.ready_frames(), MAX_BUFFER_FRAMES);
        // A late speaker push still lands at ITS cursor (0 after the drops) — front of ring.
        m.push(PAD_AUDIO_KIND_SPEAKER, &[9.0, 9.0]);
        let mut out = Vec::new();
        m.pop(&mut out);
        assert_eq!(&out[..4], &[9.0, 9.0, 1.0, 1.0]);
        // `discard` empties without output.
        m.push(PAD_AUDIO_KIND_HAPTICS, &chunk);
        m.discard();
        assert_eq!(m.ready_frames(), 0);
    }

    /// Seq-gap PLC counting mirrors the session audio thread: nothing for the first packet or
    /// in-order flow, the exact gap for a loss, the tracker's cap for a burst — and 0 before a
    /// first decode (`frame_samples == 0`) while STILL consuming the gap (no phantom replay).
    #[test]
    fn plc_counts_gaps_like_the_session_audio_path() {
        let mut gaps = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut gaps, 0, 480), 0); // first packet
        assert_eq!(plc_frames(&mut gaps, 1, 480), 0); // in-order
        assert_eq!(plc_frames(&mut gaps, 5, 480), 3); // 2,3,4 lost
        assert_eq!(plc_frames(&mut gaps, 5, 480), 0); // duplicate
        assert_eq!(plc_frames(&mut gaps, 4, 480), 0); // reorder — nothing to conceal
        assert_eq!(plc_frames(&mut gaps, 1000, 480), 10); // burst, capped (MAX_CONCEAL_PACKETS)
                                                          // Before the first decode there is no frame size to synthesize from — but the tracker
                                                          // must still advance, or this gap would replay against the next packet.
        let mut gaps = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut gaps, 7, 0), 0);
        assert_eq!(plc_frames(&mut gaps, 12, 0), 0); // gap consumed silently
        assert_eq!(plc_frames(&mut gaps, 13, 480), 0); // in-order once decoding starts
    }
}
