//! The shell↔session handoff: every stream runs in the spawned `punktfunk-session`
//! Vulkan binary (the legacy in-process presenter is gone — phase 5 of
//! punktfunk-planning `linux-client-rearchitecture.md`). What is left here is the
//! TRANSLATION: a [`ConnectRequest`] becomes a `ConnectPlan`, and the session's typed
//! lifecycle events become the [`AppMsg`]s the relm4 app consumes — spinner until
//! `{"ready":true}`, banner from the `{"error"|"ended": …}` line, exit code 3 +
//! `trust_rejected` routed to the re-pair PIN ceremony.
//!
//! Spawning, the argv, the stdout contract and the child handle live in
//! `pf_client_core::orchestrate` (design/client-architecture-split.md §3) — the WinUI shell
//! and the coming CLI spawn through the same code, so "what flags does a stream get" and
//! "what does ready mean" have exactly one answer.

use crate::app::AppMsg;
use crate::ui_hosts::ConnectRequest;
use pf_client_core::orchestrate::{self, ConnectPlan, HostTarget, SessionEvent};
use pf_client_core::trust::Settings;

/// Spawn tunables beyond a plain connect.
#[derive(Debug, Default)]
pub struct SpawnOpts {
    /// Handshake budget override (`--connect-timeout`) — the request-access flow passes
    /// ~185 s because the host PARKS the connection until the operator approves.
    pub connect_timeout_secs: Option<u64>,
    /// Persist the host as *paired* once the child reports ready (request-access: the
    /// operator's approval IS the pairing). Plain TOFU persists unpaired.
    pub persist_paired: bool,
    /// A cancel handle to arm (request-access's waiting dialog): killing the child is
    /// the only abort a parked connect has.
    pub cancel: Option<CancelHandle>,
}

pub use orchestrate::{session_binary, CancelHandle};

/// Spawn the session binary for a connect with `fp_hex` pinned and translate its
/// lifecycle into [`AppMsg`]s. `tofu` = the fingerprint came from the host's advert
/// rather than the store — the app persists it once the child reports ready (the child
/// connects pinned to it, so ready proves the host really holds that identity).
///
/// The caller has already taken `busy`; [`AppMsg::SessionExited`] releases it. `Err` =
/// the spawn itself failed (binary missing?) — surfaced as a connect error.
pub fn spawn_session(
    sender: relm4::Sender<AppMsg>,
    req: ConnectRequest,
    fp_hex: String,
    tofu: bool,
    fullscreen_on_stream: bool,
    opts: SpawnOpts,
) -> Result<(), String> {
    // The plan this connect resolves to. A plain card click carries no `--profile`: it honors
    // the host's own binding, which the session resolves through the same helper this shell
    // would have used (design/client-settings-profiles.md §4.6) — passing it here would be a
    // second source of truth for one decision. Only a "Connect with ▸" pick (or a URL's
    // `profile=`) sets one, and it applies to this session alone.
    //
    // Two fields are deliberately not the shell's state yet, because nothing in the spawn path
    // reads them: `settings` carries only what the argv needs (the fullscreen flag), and `wake`
    // is false because this shell still runs its own dial-first wake fallback in `app.rs`. Both
    // become real when the GTK connect path moves onto `ConnectOrchestrator` (arch-split C0).
    let plan = ConnectPlan {
        host: HostTarget {
            name: req.name.clone(),
            addr: req.addr.clone(),
            port: req.port,
            fp_hex: Some(fp_hex.clone()),
            mac: req.mac.clone(),
            id: None,
        },
        launch: req.launch.as_ref().map(|(id, _)| id.clone()),
        profile: None,
        // A one-off pick rides the flag; without one the session resolves the host's own
        // binding through the same helper this shell would have used.
        profile_override: req.profile.clone(),
        settings: Settings {
            fullscreen_on_stream,
            ..Default::default()
        },
        wake: false,
        connect_timeout_secs: opts.connect_timeout_secs,
        tofu,
        // The per-host clipboard decision, resolved here so the child doesn't look it up
        // again — matched by address, the way every other per-host lookup matches.
        clipboard: pf_client_core::trust::KnownHosts::load()
            .hosts
            .iter()
            .any(|h| h.addr == req.addr && h.port == req.port && h.clipboard_sync),
    };

    let persist_paired = opts.persist_paired;
    let (mut error, mut ended) = (None::<(String, bool)>, None::<String>);
    orchestrate::spawn_session(&plan, opts.cancel, move |ev| match ev {
        SessionEvent::Ready => {
            let _ = sender.send(AppMsg::SessionReady {
                req: req.clone(),
                fp_hex: fp_hex.clone(),
                tofu,
                persist_paired,
            });
        }
        SessionEvent::Error {
            msg,
            trust_rejected,
        } => error = Some((msg, trust_rejected)),
        SessionEvent::Ended(msg) => ended = Some(msg),
        // The brain persists the window size; the shell has nothing to do with it.
        SessionEvent::Window { .. } => {}
        SessionEvent::Exited(code) => {
            let _ = sender.send(AppMsg::SessionExited {
                req: req.clone(),
                code,
                error: error.take(),
                ended: ended.take(),
                tofu,
            });
        }
    })?;
    Ok(())
}
