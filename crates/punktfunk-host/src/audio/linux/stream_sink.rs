//! Session-scoped default-sink claim for the host-owned stream sink.
//!
//! In stream-sink mode (see [`super`]) the capture stream is an `Audio/Sink`
//! node and must be the session default so host apps play into it. A claim
//! saves `default.configured.audio.sink` and points it at our sink; release
//! restores. WirePlumber's `linking.follow-default-target` then moves running
//! streams. A claimed sink does not depend on display hardware, so a modeset
//! that drops HDMI cannot flip the default under capture.
//!
//! Refcounted, latest-wins: concurrent sessions each hold a claim; the newest
//! routes to *its* sink, and only the last release restores. The ledger lock
//! is held across the metadata round-trip so a stale restore cannot overwrite
//! a fresh claim.
//!
//! Crash self-healing: a leftover `punktfunk-speaker-*` name is never saved as
//! a restore target (that would wedge routing on a ghost). Restore then deletes
//! the key and WirePlumber elects from availability.

use anyhow::{anyhow, Context, Result};
use std::sync::Mutex;

/// `node.name` prefix for every host-owned stream sink. Full names uniqued
/// per capturer (`punktfunk-speaker-<pid>-<seq>`) so overlapping capturers
/// never alias. The staleness rule matches this prefix.
pub(super) const SINK_NAME_PREFIX: &str = "punktfunk-speaker";

/// WirePlumber preferred-sink key (subject 0 on `default` metadata;
/// value is `{"name":"<node.name>"}` typed `Spa:String:JSON`).
const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";

#[derive(Debug, PartialEq)]
enum Restore {
    /// Re-set the saved pre-claim JSON (`{"name":"..."}`).
    Value(String),
    /// No pre-claim preference, or the saved one was a stale punktfunk claim.
    Delete,
}

/// Claim bookkeeping, split from PipeWire I/O so the restore rules unit-test
/// on every platform.
struct Ledger {
    holders: u32,
    restore: Option<Restore>,
}

impl Ledger {
    const fn new() -> Ledger {
        Ledger {
            holders: 0,
            restore: None,
        }
    }

    /// Count a new claim. `true` = first holder; caller must [`note_previous`].
    fn on_claim(&mut self) -> bool {
        self.holders += 1;
        self.holders == 1
    }

    /// Apply the staleness rule to what the first claim found.
    fn note_previous(&mut self, prev: Option<String>) {
        self.restore = Some(match prev {
            Some(v) if !v.contains(SINK_NAME_PREFIX) => Restore::Value(v),
            _ => Restore::Delete,
        });
    }

    /// Count a release. Last holder returns the restore action.
    fn on_release(&mut self) -> Option<Restore> {
        self.holders = self.holders.saturating_sub(1);
        if self.holders == 0 {
            self.restore.take()
        } else {
            None
        }
    }
}

static LEDGER: Mutex<Ledger> = Mutex::new(Ledger::new());

/// Point the configured default at `sink_name` (refcounted; see module docs).
/// Never fails the caller: missing WirePlumber still captures; apps just
/// are not rerouted (legacy behaviour).
pub(super) fn claim(sink_name: &str) {
    let mut ledger = LEDGER.lock().unwrap();
    let first = ledger.on_claim();
    // Latest claim wins: even with an existing holder, route to the newest session's sink.
    match set_configured_sink(Some(&format!(r#"{{"name":"{sink_name}"}}"#))) {
        Ok(prev) => {
            if first {
                ledger.note_previous(prev);
            }
            tracing::info!(
                sink = sink_name,
                "claimed default sink for the stream session"
            );
        }
        Err(e) => {
            if first {
                // Nothing to restore — release hands election back to WirePlumber
                // (`Delete`), which is also correct if it starts working by then.
                ledger.note_previous(None);
            }
            tracing::warn!(error = %format!("{e:#}"),
                "could not claim the default sink — host apps may keep playing to the previous output");
        }
    }
}

/// Release one claim; the last restore writes the pre-claim default back.
pub(super) fn release() {
    let mut ledger = LEDGER.lock().unwrap();
    let Some(restore) = ledger.on_release() else {
        return;
    };
    let value = match &restore {
        Restore::Value(v) => Some(v.as_str()),
        Restore::Delete => None,
    };
    match set_configured_sink(value) {
        Ok(_) => tracing::info!(
            restored = value.unwrap_or("<automatic>"),
            "restored default sink after the stream session"
        ),
        Err(e) => tracing::warn!(error = %format!("{e:#}"),
            "could not restore the default sink — set it manually (wpctl set-default)"),
    }
}

/// Connect, find `default` metadata, read [`CONFIGURED_SINK_KEY`], set `value`
/// (`None` deletes). Returns the previous value. Own short-lived main loop on
/// the calling thread — claims come from session start/end, never a PW callback.
fn set_configured_sink(value: Option<&str>) -> Result<Option<String>> {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    pf_capture::pwinit::ensure_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("claim MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("claim Context")?;
    let core = context
        .connect_rc(None)
        .context("claim connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("claim registry")?;

    /// Round-trip phases: 0 = globals replaying, 1 = metadata properties
    /// replaying, 2 = mutation flushing.
    struct Op {
        metadata: Option<pw::metadata::Metadata>,
        md_listener: Option<pw::metadata::MetadataListener>,
        previous: Option<String>,
        phase: u8,
        expected: Option<pw::spa::utils::result::AsyncSeq>,
        outcome: Option<Result<()>>,
    }
    let op = Rc::new(RefCell::new(Op {
        metadata: None,
        md_listener: None,
        previous: None,
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
                if global.type_ != pw::types::ObjectType::Metadata
                    || op.borrow().metadata.is_some()
                    || global.props.as_ref().and_then(|p| p.get("metadata.name")) != Some("default")
                {
                    return;
                }
                match registry.bind::<pw::metadata::Metadata, _>(global) {
                    Ok(md) => {
                        // Fresh bind replays existing properties; capture the
                        // current configured sink before mutating it.
                        let listener = md
                            .add_listener_local()
                            .property({
                                let op = op.clone();
                                move |subject, key, _type, value| {
                                    if subject == 0 && key == Some(CONFIGURED_SINK_KEY) {
                                        op.borrow_mut().previous = value.map(str::to_owned);
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
                        op.borrow_mut().outcome = Some(Err(anyhow!("bind default metadata: {e}")));
                    }
                }
            }
        })
        .register();

    let value_owned = value.map(str::to_owned);
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
                        // Globals replayed. Missing `default` metadata means no
                        // session manager to negotiate with.
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
                        let md = o.metadata.as_ref().unwrap();
                        md.set_property(
                            0,
                            CONFIGURED_SINK_KEY,
                            value_owned.as_ref().map(|_| "Spa:String:JSON"),
                            value_owned.as_deref(),
                        );
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

    // Ledger lock is held across this call — a sick-but-connected daemon must
    // not wedge session start/end. Bail after 5 s.
    let timer = mainloop.loop_().add_timer({
        let op = op.clone();
        let mainloop = mainloop.clone();
        move |_| {
            op.borrow_mut()
                .outcome
                .get_or_insert(Err(anyhow!("metadata round-trip timed out")));
            mainloop.quit();
        }
    });
    let _ = timer.update_timer(Some(std::time::Duration::from_secs(5)), None);

    op.borrow_mut().expected = core.sync(0).ok();
    mainloop.run();

    let mut o = op.borrow_mut();
    match o.outcome.take() {
        Some(Ok(())) => Ok(o.previous.take()),
        Some(Err(e)) => Err(e),
        None => Err(anyhow!("metadata loop exited unexpectedly")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_release_roundtrip() {
        let mut l = Ledger::new();
        assert!(l.on_claim(), "first claim must save the previous value");
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()));
        assert_eq!(
            l.on_release(),
            Some(Restore::Value(r#"{"name":"alsa_output.hdmi"}"#.into()))
        );
    }

    #[test]
    fn nested_claims_restore_once() {
        let mut l = Ledger::new();
        assert!(l.on_claim());
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()));
        assert!(
            !l.on_claim(),
            "second claim must not overwrite the saved value"
        );
        assert_eq!(l.on_release(), None, "inner release must not restore");
        assert_eq!(
            l.on_release(),
            Some(Restore::Value(r#"{"name":"alsa_output.hdmi"}"#.into()))
        );
    }

    /// A leftover `punktfunk-speaker-*` name must not become the restore target.
    #[test]
    fn stale_own_claim_degrades_to_delete() {
        let mut l = Ledger::new();
        assert!(l.on_claim());
        l.note_previous(Some(r#"{"name":"punktfunk-speaker-4242-0"}"#.into()));
        assert_eq!(l.on_release(), Some(Restore::Delete));
    }

    #[test]
    fn unset_previous_deletes() {
        let mut l = Ledger::new();
        assert!(l.on_claim());
        l.note_previous(None);
        assert_eq!(l.on_release(), Some(Restore::Delete));
    }

    /// Unbalanced release must not underflow or restore.
    #[test]
    fn unbalanced_release_is_harmless() {
        let mut l = Ledger::new();
        assert_eq!(l.on_release(), None);
        assert!(
            l.on_claim(),
            "ledger must stay usable after an unbalanced release"
        );
    }
}
