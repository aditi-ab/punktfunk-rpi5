//! The monitored node's own rate, from the PipeWire registry — not the capture stream's.
//!
//! In `PUNKTFUNK_STREAM_SINK=0` the host records someone else's sink through PipeWire's
//! resampler, which reports whatever rate we asked for. `AudioCapturer::sample_rate` is
//! therefore the request, not the device. WirePlumber's `default` metadata names the
//! elected sink; only a bind's negotiated `Format` names its rate (the announce props
//! omit it — same trap as `audio.channels` in `pf_client_core::pad_audio::walk_graph`).
//!
//! Every failure is [`super::super::CaptureRate::Unknown`]: a missing node, unset key,
//! unnegotiated format, or timeout all decline. Over-claiming advertises 96 kHz of
//! interpolated 48 kHz; under-claiming is Opus. Nothing here guesses a rate.
//! See `design/hi-res-audio.md`.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

/// WirePlumber's elected default sink — the node an untargeted `stream.capture.sink=true`
/// stream is linked to. Not [`super::stream_sink`]'s `default.configured.audio.sink`: that
/// is the user's preference, unset when they never chose, and can name a gone node.
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// Handshake budget. Shorter than [`super::stream_sink`]'s 5 s claim timeout: a stall
/// here delays `Welcome`; giving up only declines to Opus.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `{"name":"alsa_output.…"}` from WirePlumber / [`super::stream_sink`]. Hand-parsed: a
/// miss is `None` → decline, never a wrong rate. Node names have no quotes or backslashes.
fn sink_name_from_json(value: &str) -> Option<String> {
    // Every `"name"` followed by `:`, not the first match: another member's value can
    // contain the word.
    for (at, key) in value.match_indices("\"name\"") {
        let Some(rest) = value[at + key.len()..].trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(quoted) = rest.trim_start().strip_prefix('"') else {
            return None; // `null`, a number, an object — anything but a name.
        };
        let name = quoted.split_once('"')?.0;
        return (!name.is_empty()).then(|| name.to_string());
    }
    None
}

/// `Err` is a decline, not a fault.
pub(super) fn monitored_sink_rate() -> Result<u32> {
    use pipewire as pw;
    use pw::spa::param::audio::AudioInfoRaw;
    use pw::spa::param::ParamType;
    use std::cell::RefCell;
    use std::rc::Rc;

    pf_capture::pwinit::ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("monitor-rate MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("monitor-rate Context")?;
    let core = context
        .connect_rc(None)
        .context("monitor-rate connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("monitor-rate registry")?;

    struct Op {
        metadata: Option<pw::metadata::Metadata>,
        md_listener: Option<pw::metadata::MetadataListener>,
        elected: Option<String>,
        /// Bound eagerly: a registry global cannot be bound later, and which sink is
        /// elected is unknown until the next round's metadata replay.
        sinks: Vec<(String, pw::node::Node, pw::node::NodeListener)>,
        rate: Option<u32>,
        /// 0 = globals replaying (binds issued in the callback), 1 = bound objects
        /// replaying, 2 = elected sink `Format`.
        phase: u8,
        expected: Option<pw::spa::utils::result::AsyncSeq>,
        outcome: Option<Result<()>>,
    }
    let op = Rc::new(RefCell::new(Op {
        metadata: None,
        md_listener: None,
        elected: None,
        sinks: Vec::new(),
        rate: None,
        phase: 0,
        expected: None,
        outcome: None,
    }));

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let op = op.clone();
            let registry = registry.clone();
            move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    pw::types::ObjectType::Metadata => {
                        if op.borrow().metadata.is_some()
                            || props.get("metadata.name") != Some("default")
                        {
                            return;
                        }
                        match registry.bind::<pw::metadata::Metadata, _>(global) {
                            Ok(md) => {
                                // Property replay on bind is the only getter for the elected sink.
                                let listener = md
                                    .add_listener_local()
                                    .property({
                                        let op = op.clone();
                                        move |subject, key, _type, value| {
                                            if subject == 0 && key == Some(DEFAULT_SINK_KEY) {
                                                op.borrow_mut().elected =
                                                    value.and_then(sink_name_from_json);
                                            }
                                            0
                                        }
                                    })
                                    .register();
                                let mut o = op.borrow_mut();
                                o.metadata = Some(md);
                                o.md_listener = Some(listener);
                            }
                            Err(e) => {
                                op.borrow_mut().outcome =
                                    Some(Err(anyhow!("bind default metadata: {e}")));
                            }
                        }
                    }
                    pw::types::ObjectType::Node => {
                        // Rate is not in the announce props — binding is what fetches it.
                        // `starts_with("Audio/Sink")` includes `Audio/Sink/Internal`.
                        if !props
                            .get("media.class")
                            .is_some_and(|c| c.starts_with("Audio/Sink"))
                        {
                            return;
                        }
                        let Some(name) = props.get("node.name") else {
                            return;
                        };
                        let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                            return;
                        };
                        let listener = node
                            .add_listener_local()
                            .param({
                                let op = op.clone();
                                move |_seq, id, _index, _next, param| {
                                    if id != ParamType::Format {
                                        return;
                                    }
                                    let Some(param) = param else { return };
                                    let mut info = AudioInfoRaw::default();
                                    // 0 or a non-audio/raw pod (IEC958/DSD) is not a rate.
                                    // `parse` can leave a partially-filled struct; do not trust it.
                                    if info.parse(param).is_ok() && info.rate() != 0 {
                                        op.borrow_mut().rate.get_or_insert(info.rate());
                                    }
                                }
                            })
                            .register();
                        op.borrow_mut()
                            .sinks
                            .push((name.to_string(), node, listener));
                    }
                    _ => {}
                }
            }
        })
        .register();

    let _core_listener = core
        .add_listener_local()
        .done({
            let op = op.clone();
            let core = core.clone();
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE {
                    return;
                }
                let mut o = op.borrow_mut();
                if o.expected != Some(seq) || o.outcome.is_some() {
                    return;
                }
                match o.phase {
                    0 => {
                        // Binds were issued during this replay, after this sync was queued;
                        // their own state arrives on the next round.
                        if o.metadata.is_none() {
                            o.outcome = Some(Err(anyhow!(
                                "no 'default' metadata object (is WirePlumber running?)"
                            )));
                            mainloop.quit();
                            return;
                        }
                        o.phase = 1;
                        o.expected = core.sync(0).ok();
                    }
                    1 => {
                        let Some(elected) = o.elected.clone() else {
                            o.outcome = Some(Err(anyhow!(
                                "'{DEFAULT_SINK_KEY}' is unset — no sink has been elected, so \
                                 there is nothing for a monitor capture to follow"
                            )));
                            mainloop.quit();
                            return;
                        };
                        // By index, so no borrow of `o.sinks` is alive across the writes below.
                        let Some(i) = o.sinks.iter().position(|(n, _, _)| *n == elected) else {
                            o.outcome = Some(Err(anyhow!(
                                "the elected default sink '{elected}' is not in the graph"
                            )));
                            mainloop.quit();
                            return;
                        };
                        // `Format` (configured), never `EnumFormat` (capability). On an ALSA
                        // adapter this is the device side; the monitor tap is the graph side.
                        // Low is a safe decline; high would over-claim. The monitor port's
                        // own format is the extra hop this does not take.
                        o.sinks[i].1.enum_params(0, Some(ParamType::Format), 0, 1);
                        o.phase = 2;
                        o.expected = core.sync(0).ok();
                    }
                    _ => {
                        o.outcome = Some(Ok(()));
                        mainloop.quit();
                    }
                }
            }
        })
        .error({
            let op = op.clone();
            let mainloop = mainloop.clone();
            move |id, _seq, res, message| {
                op.borrow_mut().outcome.get_or_insert(Err(anyhow!(
                    "pipewire core error id={id} res={res}: {message}"
                )));
                mainloop.quit();
            }
        })
        .register();

    let timer = mainloop.loop_().add_timer({
        let op = op.clone();
        let mainloop = mainloop.clone();
        move |_| {
            op.borrow_mut()
                .outcome
                .get_or_insert(Err(anyhow!("registry round-trip timed out")));
            mainloop.quit();
        }
    });
    let _ = timer.update_timer(Some(PROBE_TIMEOUT), None);

    op.borrow_mut().expected = core.sync(0).ok();
    mainloop.run();

    let mut o = op.borrow_mut();
    match o.outcome.take() {
        // No negotiated format means the sink is idle/suspended — the rate it will pick
        // is not a fact yet. Decline rather than predict.
        Some(Ok(())) => o.rate.take().ok_or_else(|| {
            anyhow!("the elected default sink has no negotiated format (it is idle/suspended)")
        }),
        Some(Err(e)) => Err(e),
        None => Err(anyhow!("registry loop exited unexpectedly")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact JSON from WirePlumber and [`super::super::stream_sink`], plus a spaced form.
    /// Formatting is not in the protocol.
    #[test]
    fn reads_the_node_name_wireplumber_writes() {
        assert_eq!(
            sink_name_from_json(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into())
        );
        assert_eq!(
            sink_name_from_json(r#"{ "name": "punktfunk-speaker-4242-0" }"#),
            Some("punktfunk-speaker-4242-0".into())
        );
    }

    /// `None` is a decline. A plausible parse would look up a node and believe its rate.
    #[test]
    fn nothing_parseable_is_never_guessed() {
        for v in [
            "",
            "{}",
            r#"{"name":}"#,
            r#"{"name":""}"#,
            r#"{"name":null}"#,
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            r#"{"nickname":"alsa_output.x"}"#,
        ] {
            assert_eq!(sink_name_from_json(v), None, "{v:?} must not parse");
        }
    }

    #[test]
    fn only_the_name_key_is_read() {
        assert_eq!(
            sink_name_from_json(r#"{"other":"name","name":"alsa_output.x"}"#),
            Some("alsa_output.x".into())
        );
    }
}
