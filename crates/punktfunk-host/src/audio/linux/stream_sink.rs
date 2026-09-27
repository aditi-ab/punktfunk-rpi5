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
//! routes to *its* sink, the newest leaving hands the default to the next
//! newest, and only the last release restores. A `join` session
//! claims the sink it taps, so the owner leaving first restores nothing under
//! it. The ledger lock is held across the metadata round-trip so a stale
//! restore cannot overwrite a fresh claim.
//!
//! Crash self-healing: a leftover `punktfunk-speaker-*` name is never saved as
//! a restore target (that would wedge routing on a ghost). Restore then deletes
//! the key and WirePlumber elects from availability.

use anyhow::Result;
use std::sync::Mutex;

/// `node.name` prefix for every host-owned stream sink. Full names uniqued
/// per capturer (`punktfunk-speaker-<pid>-<seq>`) so overlapping capturers
/// never alias. The staleness rule matches this prefix.
pub(super) const SINK_NAME_PREFIX: &str = "punktfunk-speaker";

/// WirePlumber preferred-sink key (subject 0 on `default` metadata;
/// value is `{"name":"<node.name>"}` typed `Spa:String:JSON`).
const CONFIGURED_SINK_KEY: &str = "default.configured.audio.sink";
/// WirePlumber's elected sink (same subject and shape): what the operator
/// hears when nothing is configured.
const EFFECTIVE_SINK_KEY: &str = "default.audio.sink";

#[derive(Debug, PartialEq)]
enum Restore {
    /// Re-set the saved pre-claim JSON (`{"name":"..."}`).
    Value(String),
    /// No pre-claim preference, or the saved one was a stale punktfunk claim.
    Delete,
}

/// What a release writes: the newest remaining claim's sink, or the pre-claim default.
#[derive(Debug, PartialEq)]
enum Release {
    Repoint(String),
    Restore(Restore),
}

/// Claim bookkeeping, split from PipeWire I/O so the restore rules unit-test
/// on every platform.
struct Ledger {
    /// Held claims' sink names, oldest first; the last one is the configured default.
    claims: Vec<String>,
    restore: Option<Restore>,
    /// `node.name` the operator heard on before the first claim: the host
    /// bridge's playthrough and voice-chat target. Kept across releases.
    host: Option<String>,
}

impl Ledger {
    const fn new() -> Ledger {
        Ledger {
            claims: Vec::new(),
            restore: None,
            host: None,
        }
    }

    /// Remember the elected sink the first claim found. A stale punktfunk
    /// name (crash leftover) is not an output; the last real one stands.
    fn note_host(&mut self, effective: Option<&str>) {
        if let Some(name) = effective
            .and_then(json_name)
            .filter(|n| !n.contains(SINK_NAME_PREFIX))
        {
            self.host = Some(name.to_owned());
        }
    }

    /// Count a new claim. `true` = first holder; caller must [`note_previous`].
    fn on_claim(&mut self, sink: &str) -> bool {
        self.claims.push(sink.to_owned());
        self.claims.len() == 1
    }

    /// Apply the staleness rule to what the first claim found.
    fn note_previous(&mut self, prev: Option<String>) {
        self.restore = Some(match prev {
            Some(v) if !v.contains(SINK_NAME_PREFIX) => Restore::Value(v),
            _ => Restore::Delete,
        });
    }

    /// Count a release. The newest leaving re-points to the next newest; the last holder
    /// returns the restore action. An older holder leaving writes nothing.
    fn on_release(&mut self, sink: &str) -> Option<Release> {
        let i = self.claims.iter().rposition(|c| c == sink)?;
        let newest = i + 1 == self.claims.len();
        self.claims.remove(i);
        match self.claims.last() {
            None => self.restore.take().map(Release::Restore),
            Some(top) if newest && top != sink => Some(Release::Repoint(top.clone())),
            Some(_) => None,
        }
    }
}

static LEDGER: Mutex<Ledger> = Mutex::new(Ledger::new());

/// The `name` inside WirePlumber's `{"name":"…"}` sink value.
fn json_name(value: &str) -> Option<&str> {
    let rest = value.split_once(r#""name":""#)?.1;
    rest.split_once('"').map(|(name, _)| name)
}

/// The output the operator heard before the stream claimed the default, if a
/// claim has seen one. `None` until then, or with no session manager.
pub(super) fn host_sink() -> Option<String> {
    LEDGER.lock().unwrap().host.clone()
}

/// Point the configured default at `sink_name` (refcounted; see module docs).
/// Never fails the caller: missing WirePlumber still captures; apps just
/// are not rerouted (legacy behaviour).
pub(super) fn claim(sink_name: &str) {
    let mut ledger = LEDGER.lock().unwrap();
    let first = ledger.on_claim(sink_name);
    // Latest claim wins: even with an existing holder, route to the newest session's sink.
    match set_configured_sink(Some(&format!(r#"{{"name":"{sink_name}"}}"#))) {
        Ok(seen) => {
            if first {
                ledger.note_previous(seen.configured);
                ledger.note_host(seen.effective.as_deref());
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
                "default sink not claimed — host apps may keep playing to the previous output");
        }
    }
}

/// Release `sink_name`'s claim. The newest leaving hands the default to the next newest
/// session's sink; the last release writes the pre-claim default back.
pub(super) fn release(sink_name: &str) {
    let mut ledger = LEDGER.lock().unwrap();
    let restore = match ledger.on_release(sink_name) {
        None => return,
        Some(Release::Restore(r)) => r,
        Some(Release::Repoint(top)) => {
            match set_configured_sink(Some(&format!(r#"{{"name":"{top}"}}"#))) {
                Ok(_) => tracing::info!(
                    sink = top,
                    "default sink back on the remaining stream session"
                ),
                Err(e) => tracing::warn!(error = %format!("{e:#}"),
                    "default sink not handed back — host apps may keep playing to the ended session"),
            }
            return;
        }
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
            "default sink not restored — set it manually (wpctl set-default)"),
    }
}

/// What the metadata held before a write: the configured value this claim
/// replaces, and the elected sink the operator was hearing on.
struct Seen {
    configured: Option<String>,
    effective: Option<String>,
}

/// Read both sink keys from `default` metadata, then set `value` on [`CONFIGURED_SINK_KEY`]
/// (`None` deletes). Own short-lived main loop on the calling thread — claims come from
/// session start/end, never a PW callback. The ledger lock is held across this call, so
/// the one-shot's timeout is what bounds a sick-but-connected daemon.
fn set_configured_sink(value: Option<&str>) -> Result<Seen> {
    let session = super::pw_oneshot::OneShot::connect("claim", super::pw_oneshot::TIMEOUT)?;
    let (metadata, mut defaults) = session.default_metadata()?;
    metadata.set_property(
        0,
        CONFIGURED_SINK_KEY,
        value.map(|_| "Spa:String:JSON"),
        value,
    );
    session.round()?; // flush the write before the proxies drop
    Ok(Seen {
        configured: defaults.remove(CONFIGURED_SINK_KEY),
        effective: defaults.remove(EFFECTIVE_SINK_KEY),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_release_roundtrip() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"), "first claim must save the previous value");
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()));
        assert_eq!(
            l.on_release("a"),
            Some(Release::Restore(Restore::Value(
                r#"{"name":"alsa_output.hdmi"}"#.into()
            )))
        );
    }

    #[test]
    fn nested_claims_restore_once() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(Some(r#"{"name":"alsa_output.hdmi"}"#.into()));
        assert!(
            !l.on_claim("a"),
            "second claim must not overwrite the saved value"
        );
        assert_eq!(l.on_release("a"), None, "inner release must not restore");
        assert_eq!(
            l.on_release("a"),
            Some(Release::Restore(Restore::Value(
                r#"{"name":"alsa_output.hdmi"}"#.into()
            )))
        );
    }

    /// The newest session leaving hands the default to the one still streaming; an older one
    /// leaving writes nothing, since the default is not its sink.
    #[test]
    fn the_newest_leaving_repoints_to_the_next_newest() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(None);
        assert!(!l.on_claim("b"));
        assert_eq!(l.on_release("b"), Some(Release::Repoint("a".into())));
        assert!(!l.on_claim("c"));
        assert_eq!(l.on_release("a"), None, "c still holds the default");
        assert_eq!(l.on_release("c"), Some(Release::Restore(Restore::Delete)));
    }

    /// A leftover `punktfunk-speaker-*` name must not become the restore target.
    #[test]
    fn stale_own_claim_degrades_to_delete() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(Some(r#"{"name":"punktfunk-speaker-4242-0"}"#.into()));
        assert_eq!(l.on_release("a"), Some(Release::Restore(Restore::Delete)));
    }

    #[test]
    fn unset_previous_deletes() {
        let mut l = Ledger::new();
        assert!(l.on_claim("a"));
        l.note_previous(None);
        assert_eq!(l.on_release("a"), Some(Release::Restore(Restore::Delete)));
    }

    /// The host output is the elected sink's bare name; a stale punktfunk
    /// election or a missing key keeps the last real one.
    #[test]
    fn host_output_is_the_elected_sink_name() {
        assert_eq!(
            json_name(r#"{"name":"alsa_output.hdmi"}"#),
            Some("alsa_output.hdmi")
        );
        assert_eq!(json_name("garbage"), None);
        let mut l = Ledger::new();
        l.note_host(Some(r#"{"name":"alsa_output.hdmi"}"#));
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
        l.note_host(Some(r#"{"name":"punktfunk-speaker-4242-0"}"#));
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
        l.note_host(None);
        assert_eq!(l.host.as_deref(), Some("alsa_output.hdmi"));
    }

    /// Unbalanced release must not underflow or restore.
    #[test]
    fn unbalanced_release_is_harmless() {
        let mut l = Ledger::new();
        assert_eq!(l.on_release("a"), None);
        assert!(
            l.on_claim("a"),
            "ledger must stay usable after an unbalanced release"
        );
    }
}
