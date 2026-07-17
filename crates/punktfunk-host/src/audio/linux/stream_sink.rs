//! Session-scoped **default-sink claim** for the host-owned stream sink.
//!
//! In stream-sink mode (see the module docs in [`super`]) the capture stream registers itself
//! as an `Audio/Sink` node; for host apps to actually play into it, it must be the *default*
//! sink for the duration of a stream session. WirePlumber elects the default from the
//! `default.configured.audio.sink` metadata key (what `wpctl set-default` writes), and with
//! `linking.follow-default-target` (default true) moves already-running app streams when it
//! changes. So a claim is: save the current configured value, point it at our sink; a release
//! restores it. Live-diagnosed motivation (bazzite host, 2026-07-14): the *hardware* default
//! (HDMI audio on a TV) vanishes on every gamescope modeset, making WirePlumber ping-pong the
//! default HDMI↔auto_null ~8×/s — a capture stream that follows the default relinks on every
//! flip and the client hears crackle. A claimed stream sink is immune: nothing about it
//! depends on display hardware.
//!
//! **Refcounted, latest-wins.** Concurrent sessions (GameStream + punktfunk/1) each hold a
//! claim on their own capturer's sink; the newest claim points routing at *its* sink, and only
//! the release of the last claim restores the pre-claim value — a session ending must never
//! yank the default from under one still running. The ledger lock is held **across** the
//! metadata round-trip so a racing claim/release pair can't interleave their writes (a stale
//! restore overwriting a fresh claim would silence the surviving session).
//!
//! **Crash self-healing.** If the host dies while claimed, the configured default is left
//! pointing at a `punktfunk-speaker-*` node that no longer exists; WirePlumber then falls back
//! to availability-based election (local audio keeps working) and the next claim overwrites
//! the stale value. A stale punktfunk name is never saved as a restore target — restoring it
//! would wedge routing on a ghost sink forever — the restore degrades to *deleting* the key,
//! i.e. handing the choice back to WirePlumber's automatic election.

use anyhow::{anyhow, Context, Result};
use std::sync::Mutex;

/// `node.name` prefix for every host-owned stream sink (full names are uniqued per capturer:
/// `punktfunk-speaker-<pid>-<seq>`, so overlapping capturers — mid-session reopen, concurrent
/// sessions — never alias in `target`/metadata lookups). The staleness rule below matches on
/// this prefix.
pub(super) const SINK_NAME_PREFIX: &str = "punktfunk-speaker";

/// The metadata key WirePlumber reads the user's preferred sink from (subject 0 on the
/// `default` metadata object; value is `{"name":"<node.name>"}` typed `Spa:String:JSON`).
const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";

/// What the last release writes back.
#[derive(Debug, PartialEq)]
enum Restore {
    /// Re-set the saved pre-claim value (the raw `{"name":"..."}` JSON).
    Value(String),
    /// Remove the key: no pre-claim preference existed, or the saved one was a stale
    /// punktfunk claim from a crashed host (see the module docs).
    Delete,
}

/// Pure claim bookkeeping — separated from the PipeWire I/O so the refcount/restore rules are
/// unit-testable on every platform.
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

    /// Count a new claim; `true` means this is the first holder and the caller must save the
    /// pre-claim value via [`note_previous`](Self::note_previous).
    fn on_claim(&mut self) -> bool {
        self.holders += 1;
        self.holders == 1
    }

    /// Record what the first claim found, applying the staleness rule.
    fn note_previous(&mut self, prev: Option<String>) {
        self.restore = Some(match prev {
            Some(v) if !v.contains(SINK_NAME_PREFIX) => Restore::Value(v),
            _ => Restore::Delete,
        });
    }

    /// Count a release; the last holder gets the restore action to apply.
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

/// Point the configured default sink at `sink_name` (refcounted; see the module docs). Never
/// fails the caller: a box where the metadata write doesn't work (WirePlumber absent) still
/// gets a working capture — apps just aren't rerouted, which is exactly the legacy behaviour.
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
                // Nothing knowable to restore — the release will hand election back to
                // WirePlumber (Delete), which is also correct if IT starts working by then.
                ledger.note_previous(None);
            }
            tracing::warn!(error = %format!("{e:#}"),
                "could not claim the default sink — host apps may keep playing to the previous output");
        }
    }
}

/// Release one claim; the last release restores the pre-claim configured default.
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

/// One-shot metadata round-trip: connect, find the `default` metadata object, read the current
/// [`CONFIGURED_SINK_KEY`] value, then set it to `value` (`None` deletes the key). Returns the
/// **previous** value. Runs its own short-lived main loop on the calling thread — claims come
/// from session threads at start/end, never from a PipeWire callback.
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

    /// Round-trip phases: 0 = globals replaying, 1 = metadata properties replaying,
    /// 2 = mutation flushing.
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
                        // The server replays existing properties to a fresh bind; capture the
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
                        // All pre-existing globals replayed. No `default` metadata → no
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
                        // Property replay complete — `previous` holds the pre-claim value.
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

    // A sick-but-connected daemon must not wedge a session start/end (the ledger lock is held
    // across this call) — bail out after a bounded wait.
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

    /// First claim saves the pre-claim value; the last release yields it for restore.
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

    /// Nested claims (concurrent sessions): only the FIRST saves, only the LAST restores.
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

    /// A stale punktfunk claim left by a crashed host must NEVER become the restore target —
    /// it degrades to deleting the key (automatic election).
    #[test]
    fn stale_own_claim_degrades_to_delete() {
        let mut l = Ledger::new();
        assert!(l.on_claim());
        l.note_previous(Some(r#"{"name":"punktfunk-speaker-4242-0"}"#.into()));
        assert_eq!(l.on_release(), Some(Restore::Delete));
    }

    /// No pre-claim preference → restore deletes the key.
    #[test]
    fn unset_previous_deletes() {
        let mut l = Ledger::new();
        assert!(l.on_claim());
        l.note_previous(None);
        assert_eq!(l.on_release(), Some(Restore::Delete));
    }

    /// Release without claim (defensive) must not underflow or restore.
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
