//! The **monitored node's own rate**, read from the PipeWire registry — the lookup
//! `design/hi-res-audio.md` §4.4 names as the prerequisite for hi-res in
//! `PUNKTFUNK_STREAM_SINK=0` monitor mode, and §8.3 left as a placeholder.
//!
//! **Why the obvious answer is worthless here.** In monitor mode the host records somebody
//! else's sink *through PipeWire's resampler*, which hands us whatever rate we asked for and
//! reports it back cleanly however narrow the thing upstream really is — the same blindness
//! WASAPI's `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` has on the Windows capture path (§4.3). So the
//! rate our own capture stream negotiates (what `AudioCapturer::sample_rate` reports) cannot
//! answer §8.4's condition 4, and neither can any amount of listening to it. The question is
//! about the node upstream of that resampler, and that is a registry question, not a stream one.
//!
//! **Two facts, two round trips.** WirePlumber's `default` metadata says WHICH node an untargeted
//! `stream.capture.sink=true` stream gets linked to; only the node itself says what rate it runs
//! at, and the rate is not in the registry's announce props — the same trap
//! `pf_client_core::pad_audio::walk_graph` documents for `audio.channels`, where reading the
//! announce subset looked like it worked and returned zeroes on every real machine. So each
//! candidate sink is bound and the elected one is asked for its negotiated `Format`.
//!
//! **Every failure is a decline, and the asymmetry is the point.** Over-claiming costs a session
//! that says 96 kHz, spends 4.6 Mbps saying it, and carries interpolated 48 kHz — "both ends
//! audit clean, the content is wrong", the class of bug this whole feature exists to prevent.
//! Under-claiming costs Opus 48 kHz, which is the excellent thing every session ships today. So
//! a node that is gone, a key that is unset, a format that was never negotiated and a graph that
//! does not answer in time all resolve the same way: [`super::super::CaptureRate::Unknown`], and
//! the gate declines. **Nothing here ever guesses a rate.**

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

/// The `default` metadata key naming the sink WirePlumber has **elected** — the node an
/// untargeted `stream.capture.sink=true` stream is actually linked to, which is exactly what
/// legacy monitor-mode capture is.
///
/// ⚠ Deliberately NOT [`super::stream_sink`]'s `default.configured.audio.sink`: the neighbouring
/// key, on the same object, and the tempting one to reuse. That one is the user's *preference* —
/// unset on a box whose owner never chose an output, and (as that module's crash self-healing
/// note records) perfectly able to name a node that no longer exists. A preference cannot say
/// what the thing we are about to record is running at; only the elected node can.
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// How long the whole round trip may take before it gives up and declines.
///
/// This runs inside the handshake, *before* the `Welcome`: a sick-but-connected graph must cost a
/// connecting client a fallback to Opus, never a stall. Shorter than [`super::stream_sink`]'s 5 s
/// claim timeout on purpose — that one has something to lose by giving up early (host apps keep
/// playing to the previous output for the rest of the session), this one has nothing at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The `node.name` inside a `default` metadata value (`{"name":"alsa_output.…"}`, typed
/// `Spa:String:JSON` — the same shape [`super::stream_sink`] writes).
///
/// Hand-parsed rather than pulled through a JSON dependency: the value is written by WirePlumber
/// from a fixed template, node names are PipeWire identifiers (`alsa_output.pci-0000_00_1f.3…`,
/// `bluez_output.AA_BB…`) which contain neither quotes nor backslashes, and the cost of this
/// being fooled is `None` → decline, never a wrong rate.
fn sink_name_from_json(value: &str) -> Option<String> {
    // Every occurrence, not the first: this is an object, and some other member's VALUE reading
    // `"name"` must not shift the parse onto it. Only an occurrence followed by `:` is the key.
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

/// The rate the sink we would monitor is genuinely running at, or an error saying why it is not
/// knowable. See the module docs — an error here is a decline, not a fault.
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

    /// Round-trip phases: 0 = globals replaying (binds issued from the callback), 1 = the bound
    /// objects replaying their own state, 2 = the elected sink's `Format` being enumerated.
    struct Op {
        metadata: Option<pw::metadata::Metadata>,
        md_listener: Option<pw::metadata::MetadataListener>,
        /// `node.name` of the elected default sink, from [`DEFAULT_SINK_KEY`].
        elected: Option<String>,
        /// Every `Audio/Sink…` node, bound as its global was announced and keyed by the
        /// announce `node.name`. Bound EAGERLY because a registry global cannot be bound later:
        /// the proxy has to be made while its `GlobalObject` is in hand, and which one we want
        /// is not known until the metadata replay of the *next* round. A handful of proxies on
        /// any real box, and this path only runs when hi-res is on the table.
        sinks: Vec<(String, pw::node::Node, pw::node::NodeListener)>,
        /// The rate the elected node's negotiated `Format` reported.
        rate: Option<u32>,
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
                                // The server replays existing properties to a fresh bind, which
                                // is how the elected sink arrives — there is no getter.
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
                        // `media.class` and `node.name` ARE in the announce subset; the rate is
                        // not, which is what binding buys. `Audio/Sink…` with the ellipsis on
                        // purpose — `Audio/Sink/Internal` nodes exist and can be elected.
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
                                    // A rate of `0`, or a pod that is not audio/raw at all (an
                                    // IEC958/DSD passthrough sink), is not an answer — leave it
                                    // unset and let the caller decline. `parse` reading a
                                    // partially-filled struct is why this is checked rather
                                    // than trusted.
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
                        // All pre-existing globals replayed, and every bind was issued from
                        // inside that replay — i.e. AFTER this sync was queued, so nothing they
                        // provoke has arrived yet. That is what the next round is for.
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
                        // The binds have replayed: the metadata's properties (so the elected
                        // sink is known) and the nodes' info. Now ask the ONE node that matters
                        // for the format it actually negotiated.
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
                        // `Format` — the CONFIGURED one — and never `EnumFormat`, which lists
                        // what the node *could* be asked for. Reading a capability as if it were
                        // a fact is precisely the guess this feature exists to refuse.
                        //
                        // ⚠ On an adapter node — every ALSA sink is one — `Format` is the
                        // FOLLOWER's, i.e. the device side, while the monitor ports a legacy
                        // capture taps sit on the graph side. PipeWire opens the device at the
                        // graph rate whenever the device can do it, so on any ordinary box the
                        // two are the same number; they diverge only for a device that cannot
                        // run the graph's rate at all. Reading LOW then is safe (we decline a
                        // rate the tap could have carried); reading HIGH is the direction that
                        // would over-claim, and it needs a device that does 96 kHz on a graph
                        // that will not — which is also the graph most likely to switch up,
                        // since the capture stream this gate is deciding for asks for 96 kHz.
                        // The exactly-right answer is the monitor PORT's own format, one more
                        // registry hop; named here rather than implied to be covered.
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
        // A sink with no negotiated format is a SUSPENDED one: PipeWire closed the device
        // because nothing was playing, and the rate it will pick when something does is not a
        // fact yet. Decline rather than predict it — the cost is Opus for this session.
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

    /// The compact shape WirePlumber and [`super::super::stream_sink`] both write, plus a spaced
    /// one — nothing in the metadata protocol promises the formatting, so neither does this.
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

    /// Anything that is not a name is `None` — which is a DECLINE, not a fallback. A parser that
    /// returned something plausible here would hand the gate a node to look up and, if it
    /// matched, a rate to believe.
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

    /// The key is matched with its quotes, so a *value* that merely contains the word cannot
    /// hijack the read.
    #[test]
    fn only_the_name_key_is_read() {
        assert_eq!(
            sink_name_from_json(r#"{"other":"name","name":"alsa_output.x"}"#),
            Some("alsa_output.x".into())
        );
    }
}
