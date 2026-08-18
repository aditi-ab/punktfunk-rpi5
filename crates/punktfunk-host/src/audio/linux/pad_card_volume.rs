//! Put the usbip pad's REAL sound card at unity gain — the host half of the attenuation the
//! client fixes in `pf_client_core::pad_audio::pin_sink_volume`.
//!
//! ## The defect
//!
//! WirePlumber starts every new card's sink at `device.routes.default-sink-volume`. That is 0.4,
//! and 0.4 is a *cubed* number: what a mixer shows as 40 % is 0.4³ = 0.064 of linear amplitude,
//! −23.88 dB. The setting is global — it cannot be scoped to one device from configuration — so
//! there is no config file we could ship to exempt the pad, and it fires again on every fresh
//! card, which for a usbip pad means every single attach.
//!
//! It is a reasonable default for a laptop speaker somebody is about to turn up. It is wrong
//! here twice:
//!
//! - nobody chose it, and nobody would think to look for it: the pad's sink is not a listening
//!   volume anyone reaches for, so it reads as weak hardware rather than as a slider; and
//! - **both ends of a session mint one.** The game's samples cross this sink on the host and the
//!   pad's own sink on the client, so the two multiply: 0.064² = −47.8 dB by the time a game's
//!   haptics reach a voice coil. That is the difference between "the haptics are subtle" and
//!   "I'm not sure the haptics are connected".
//!
//! ## Why it lands on the host at all
//!
//! [`super::pad_usb`] captures at the pad's isochronous OUT endpoint, which is DOWNSTREAM of
//! this sink: PipeWire applies the sink's volume when it mixes into the ALSA device, and what
//! reaches the wire — and therefore what we encode and send — is already attenuated. Fixing it
//! on the client cannot recover what the host threw away before the encoder saw it.
//!
//! Nothing here is restored on the way out, deliberately. The client restores a *profile* it
//! borrowed, because that overrides a choice the user made; this overrides a default nobody
//! made, and putting −24 dB back would be restoring the bug.
//!
//! Best effort throughout: this is a volume, and every failure costs loudness rather than audio.
//! `PUNKTFUNK_PAD_SINK_VOLUME=0` skips it entirely, for bisecting a box where something else is
//! doing the attenuating.

use anyhow::{Context, Result};
use std::time::Duration;

/// The pad's USB identity, as its ALSA card publishes it.
const DS5_VENDOR: u32 = 0x054c;
const DS5_PRODUCTS: [u32; 2] = [0x0ce6, 0x0df2];

/// How long to keep looking for the card after the pad attaches, and how often.
///
/// The USB device is live well before its sink is: `snd-usb-audio` has to probe it, PipeWire has
/// to build the device, and WirePlumber has to apply the very default we are here to undo — and
/// pinning BEFORE that lands would simply be overwritten. So this retries rather than firing
/// once, and gives up quietly: a pad whose card never appears is a pad with no sink to pin.
const ATTEMPTS: u32 = 15;
const INTERVAL: Duration = Duration::from_secs(1);

/// Pin every DualSense card sink in the graph to unity, in the background.
///
/// Detached on purpose. The caller is the pad-audio capture thread's open path, and a second of
/// waiting for a card to appear there is a second of missing pad audio.
pub(crate) fn spawn_pin(pad: u8) {
    if matches!(
        std::env::var("PUNKTFUNK_PAD_SINK_VOLUME").as_deref(),
        Ok("0" | "false" | "off" | "no")
    ) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name(format!("punktfunk1-padvol{pad}"))
        .spawn(move || {
            // An error retries like an absent card does: the pad attaching is exactly the moment
            // the graph is busy, and giving up on one transient connect failure would leave the
            // attenuation in place for the whole session. Only the last one is reported.
            let mut last_err = None;
            for _ in 0..ATTEMPTS {
                match pin_pad_sinks() {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!(
                            pad,
                            sinks = n,
                            "pad card sink pinned to 0 dB (WirePlumber starts every new card at \
                             40% = -23.88 dB, and host+client stack)"
                        );
                        return;
                    }
                    Err(e) => last_err = Some(format!("{e:#}")),
                }
                std::thread::sleep(INTERVAL);
            }
            tracing::debug!(
                pad,
                error = last_err.unwrap_or_else(|| "no DualSense card sink in the graph".into()),
                "pad sink volume not pinned — pad audio may be quiet if this box attenuates it"
            );
        })
    {
        tracing::debug!(pad, error = %e, "pad sink volume thread not spawned");
    }
}

/// One pass: walk the graph, and set every DualSense CARD sink to unity. Returns how many were
/// pinned, so the caller can tell "the card is not here yet" from "done".
fn pin_pad_sinks() -> Result<usize> {
    use pipewire as pw;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context.connect_rc(None).context("pw connect")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    /// A bound sink and what the set_param needs to know about it.
    struct Sink {
        node: pw::node::Node,
        _listener: pw::node::NodeListener,
        /// `device.id` from the announce props — `None` for a node that belongs to no card.
        card: Option<u32>,
        /// `audio.channels`, which arrives only with the bound node's `info`. Zero until then.
        channels: Rc<Cell<u32>>,
    }

    let sinks: Rc<RefCell<Vec<Sink>>> = Rc::default();
    let ds5_cards: Rc<RefCell<Vec<u32>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (registry, sinks, ds5_cards) = (registry.clone(), sinks.clone(), ds5_cards.clone());
            move |g| {
                let Some(props) = g.props else { return };
                let usb_id = |k: &str| {
                    props
                        .get(k)
                        .and_then(|v| {
                            let v = v.trim();
                            // The specimen publishes `0x054c`; a bare `054c` read with base 0
                            // is octal and yields nonsense, so the radix is chosen explicitly.
                            v.strip_prefix("0x")
                                .or_else(|| v.strip_prefix("0X"))
                                .map(|h| u32::from_str_radix(h, 16))
                                .unwrap_or_else(|| u32::from_str_radix(v, 16))
                                .ok()
                        })
                };
                match g.type_ {
                    // Cards announce their identity keys, so no second round is needed for them.
                    pw::types::ObjectType::Device => {
                        let vendor = usb_id("device.vendor.id");
                        let product = usb_id("device.product.id");
                        if vendor == Some(DS5_VENDOR)
                            && product.is_some_and(|p| DS5_PRODUCTS.contains(&p))
                        {
                            ds5_cards.borrow_mut().push(g.id);
                        }
                    }
                    pw::types::ObjectType::Node => {
                        if !props
                            .get("media.class")
                            .is_some_and(|c| c.starts_with("Audio/Sink"))
                        {
                            return;
                        }
                        let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                            return;
                        };
                        // `audio.channels` is NOT in the announce subset — reading it there looks
                        // like it works and returns zero on every real machine (the same trap
                        // `pf_client_core::pad_audio::walk_graph` documents). Bind for it.
                        let channels = Rc::new(Cell::new(0u32));
                        let listener = node
                            .add_listener_local()
                            .info({
                                let channels = channels.clone();
                                move |info| {
                                    let Some(p) = info.props() else { return };
                                    if let Some(c) =
                                        p.get("audio.channels").and_then(|v| v.parse().ok())
                                    {
                                        channels.set(c);
                                    }
                                }
                            })
                            .register();
                        sinks.borrow_mut().push(Sink {
                            node,
                            _listener: listener,
                            card: props.get("device.id").and_then(|v| v.parse().ok()),
                            channels,
                        });
                    }
                    _ => {}
                }
            }
        })
        .register();

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
    let round = |issue: &dyn Fn() -> Result<()>| -> Result<()> {
        issue()?;
        awaited.set(Some(core.sync(0).context("pw sync")?));
        mainloop.run();
        Ok(())
    };

    round(&|| Ok(()))?; // 1: globals replay; sinks get bound
    round(&|| Ok(()))?; // 2: the binds' `info` events land, carrying audio.channels

    // A `Cell` because `round` takes an `Fn` — the set_params have to be issued from inside it,
    // and a closure that incremented a plain counter would be `FnMut`.
    let pinned = Cell::new(0usize);
    round(&|| {
        let cards = ds5_cards.borrow();
        for s in sinks.borrow().iter() {
            // A CARD's sink only. A Punktfunk host minting its own pad sink on this same box
            // publishes the full DualSense identity on purpose (that is how Proton finds it) and
            // is not a thing to set a hardware volume on; `device.id` is what tells them apart.
            let Some(card) = s.card else { continue };
            if !cards.contains(&card) {
                continue;
            }
            let channels = s.channels.get();
            if channels == 0 {
                continue;
            }
            let pod = unity_volume_pod(channels)?;
            let Some(pod) = pw::spa::pod::Pod::from_bytes(&pod) else {
                continue;
            };
            s.node.set_param(pw::spa::param::ParamType::Props, 0, pod);
            pinned.set(pinned.get() + 1);
        }
        Ok(())
    })?; // 3: flush the set_params before the loop and its proxies drop
    Ok(pinned.get())
}

/// The `Props` object pod that puts every channel of a sink at unity gain (1.0 linear = 0 dB;
/// see the module docs for why that is not the same number a mixer would call 100 %).
fn unity_volume_pod(channels: u32) -> Result<Vec<u8>> {
    use pipewire::spa;
    use spa::pod::{Object, Property, PropertyFlags, Value, ValueArray};
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: spa::param::ParamType::Props.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PROP_volume,
                flags: PropertyFlags::empty(),
                value: Value::Float(1.0),
            },
            Property {
                key: spa::sys::SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Float(vec![1.0; channels.max(1) as usize])),
            },
        ],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .context("serialize Props pod")?
    .0
    .into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pod is what the fix IS, so it has to be the shape PipeWire reads: a `Props` object
    /// carrying one unity float per channel. A pod whose array is the wrong length is the
    /// failure this guards — PipeWire ignores a `channelVolumes` that does not match the port
    /// count, which would look exactly like the pin silently not working.
    #[test]
    fn unity_pod_is_one_float_per_channel() {
        use pipewire::spa::pod::{deserialize::PodDeserializer, Value, ValueArray};
        for channels in [1u32, 2, 4] {
            let bytes = unity_volume_pod(channels).expect("serialize");
            let (_, value) = PodDeserializer::deserialize_any_from(&bytes).expect("parse");
            let Value::Object(obj) = value else {
                panic!("not an object pod");
            };
            let vols = obj
                .properties
                .iter()
                .find(|p| p.key == pipewire::spa::sys::SPA_PROP_channelVolumes)
                .map(|p| p.value.clone())
                .expect("channelVolumes");
            let Value::ValueArray(ValueArray::Float(v)) = vols else {
                panic!("channelVolumes is not a float array");
            };
            assert_eq!(v.len(), channels as usize);
            assert!(v.iter().all(|&x| x == 1.0), "every channel must be unity");
        }
    }

    /// Zero channels must not serialize an empty array — an empty `channelVolumes` is not
    /// "leave it alone", it is a pod PipeWire may take literally.
    #[test]
    fn unity_pod_never_empty() {
        let bytes = unity_volume_pod(0).expect("serialize");
        assert!(!bytes.is_empty());
    }
}
