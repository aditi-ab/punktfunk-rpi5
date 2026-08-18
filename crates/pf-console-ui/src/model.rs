//! The console's shared binary↔overlay state and command bus — the widened sibling of
//! [`crate::library::LibraryShared`]. The session binary's service threads (discovery,
//! probing, pairing, waking, persistence) WRITE snapshots in; the shell reads them per
//! frame by generation stamp. The overlay never blocks: anything that touches the
//! network or disk rides a [`ConsoleCmd`] to the binary instead.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A settings profile as the console shows it (design client-settings-profiles.md §5.2a):
/// the resolved name and accent of a catalog entry, keyed by its stable id. The service
/// thread resolves these against the catalog; the shell never opens the profiles file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileChip {
    pub id: String,
    pub name: String,
    /// `#RRGGBB`, the catalog's optional tint for pinned cards.
    pub accent: Option<String>,
}

/// One row on the console home carousel — a saved host, a discovered-but-unsaved one,
/// a pinned profile card, or (client-side) the trailing Add Host tile. Fully resolved by
/// the service thread; the shell renders it verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostRow {
    /// Stable identity across refreshes: the pinned fingerprint when known, else
    /// `addr:port` — keeps the cursor on "the same host" as snapshots churn.
    pub key: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Pinned certificate fingerprint (lowercase hex); empty = not pinned.
    pub fp_hex: String,
    pub paired: bool,
    /// In the known-hosts store (vs. discovered-only).
    pub saved: bool,
    /// Advertising on mDNS or proven reachable by the probe sweep.
    pub online: bool,
    /// The management API's port (mDNS TXT or store), for the library fetch.
    pub mgmt_port: u16,
    /// Offline + a stored MAC → activating wakes first ("Wake & Connect").
    pub can_wake: bool,
    /// Last successful connect (UNIX seconds) — the most-recent accent.
    pub last_used: Option<u64>,
    /// The host's OS-identity chain (live advert preferred, else the stored one), for a
    /// future tile OS glyph. Empty = unknown (older host). Plumbed now; drawing is a
    /// follow-up — the Skia glyph set doesn't exist yet.
    pub os: String,
    /// `Some` = this row is a pinned profile card (§5.2a): a shortcut tile rendered right
    /// after its host's primary tile, sharing its live state, that connects with THIS
    /// profile. `None` = the host's primary tile.
    pub pin: Option<ProfileChip>,
    /// The primary tile's default-profile chip: the profile bound as this host's default
    /// (`KnownHost::profile_id`), resolved, so the tile can say what a plain A-press uses.
    /// Always `None` on pinned rows — there the profile IS `pin`.
    pub bound_profile: Option<ProfileChip>,
}

/// The pairing ceremony's observable state (one at a time — the ceremony is modal).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub enum PairPhase {
    #[default]
    Idle,
    /// The SPAKE2 exchange is running (up to ~90 s on a mistyped-then-fixed PIN).
    Busy,
    Failed(String),
    /// Paired and persisted; `key` addresses the host's refreshed row.
    Paired {
        key: String,
    },
}

/// A wake-and-wait in progress (one at a time). The service thread re-sends magic
/// packets and probes; the shell renders the card and acts on `online`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WakeStatus {
    pub key: String,
    pub name: String,
    /// Seconds since the wake started (the card's counter).
    pub seconds: u32,
    pub timed_out: bool,
    /// The host answered a probe — the shell launches if the wake wanted a connect.
    pub online: bool,
    /// Connect once awake (A on an offline host) vs. a bare wake.
    pub then_connect: bool,
}

#[derive(Default)]
struct ConsoleState {
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    pair: PairPhase,
    wake: Option<WakeStatus>,
    /// A one-shot toast from a service worker (e.g. the log-upload result). The shell
    /// takes it on its next sync; unlike [`PairPhase`] there is no modal state to track,
    /// so a plain take-once string is the whole protocol.
    notice: Option<String>,
}

/// The shared handle. Service threads write; the shell polls per frame (cheap locks,
/// no rendering data inside).
#[derive(Clone, Default)]
pub struct ConsoleShared(Arc<Mutex<ConsoleState>>);

impl ConsoleShared {
    pub fn set_hosts(&self, hosts: Vec<HostRow>) {
        let mut s = self.0.lock().unwrap();
        if s.hosts != hosts {
            s.hosts = hosts;
            s.hosts_gen += 1;
        }
    }

    pub(crate) fn hosts_gen(&self) -> u64 {
        self.0.lock().unwrap().hosts_gen
    }

    pub(crate) fn hosts_snapshot(&self) -> (Vec<HostRow>, u64) {
        let s = self.0.lock().unwrap();
        (s.hosts.clone(), s.hosts_gen)
    }

    pub fn set_pair(&self, phase: PairPhase) {
        self.0.lock().unwrap().pair = phase;
    }

    pub(crate) fn pair(&self) -> PairPhase {
        self.0.lock().unwrap().pair.clone()
    }

    pub fn set_wake(&self, wake: Option<WakeStatus>) {
        self.0.lock().unwrap().wake = wake;
    }

    pub(crate) fn wake(&self) -> Option<WakeStatus> {
        self.0.lock().unwrap().wake.clone()
    }

    /// Post a one-shot toast from a service worker. A newer notice replaces an unshown
    /// older one — the shell polls per frame, so in practice nothing is ever dropped.
    pub fn set_notice(&self, text: String) {
        self.0.lock().unwrap().notice = Some(text);
    }

    pub(crate) fn take_notice(&self) -> Option<String> {
        self.0.lock().unwrap().notice.take()
    }
}

/// Work the shell asks the binary to do. Everything here blocks (network/disk), so it
/// runs on the binary's service thread, never on the render path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConsoleCmd {
    /// (Re)fetch a host's game library into the shared library model.
    FetchLibrary {
        addr: String,
        mgmt: u16,
        fp_hex: String,
    },
    /// Re-read only WHICH titles the host has up (`GET /api/v1/status`), leaving the catalog
    /// on screen untouched — raised when a stream ends onto a shelf, the one moment the
    /// running set is most likely to have changed under it.
    ///
    /// Its own command rather than a `FetchLibrary`: that one sets the phase to `Loading`,
    /// which would replace the shelf the player just came back to with a spinner.
    RefreshRunning {
        addr: String,
        mgmt: u16,
        fp_hex: String,
    },
    /// Run the SPAKE2 PIN ceremony; on success persist the pin and refresh hosts.
    Pair {
        addr: String,
        port: u16,
        pin: String,
        device_name: String,
    },
    /// Upload the client's recent log ring to this PAIRED host's management API — the
    /// "send logs to host" escape hatch for platforms whose own logs are unreachable
    /// (Deck Gaming Mode, tvOS). Same transport + trust as `FetchLibrary`; the result
    /// comes back as a shared-model notice toast.
    SendLogs {
        addr: String,
        mgmt: u16,
        fp_hex: String,
        host_name: String,
    },
    /// Save a manually entered host (unpaired) and refresh the rows.
    SaveHost {
        name: String,
        addr: String,
        port: u16,
    },
    /// Rename / re-address a saved host (the host menu's "Edit…"). `key` addresses the
    /// row; the fingerprint, pins and MACs already stored against it are kept — this edits
    /// a host, it doesn't replace one.
    UpdateHost {
        key: String,
        name: String,
        addr: String,
        port: u16,
    },
    /// Drop a saved host (the host menu's "Forget"). The next connect to that address
    /// starts from scratch: no pin, no pairing, no pinned cards.
    ForgetHost { key: String },
    /// Start the wake-and-wait loop for this saved host.
    Wake { key: String, then_connect: bool },
    /// Stop the wake loop (B on the wake card) and clear its status.
    CancelWake,
    /// Sweep reachability now (the home screen refreshes its presence pips).
    Probe,
    /// Pin (or unpin) a profile as an extra connect card on a saved host
    /// (`KnownHost::pinned_profiles`, design §5.2a). `key` is the HOST row's key
    /// (fingerprint or `addr:port`); presentation only — never touches the host's
    /// default binding or the profile itself. Idempotent: re-pinning a pinned profile
    /// (or unpinning an absent one) is a no-op.
    SetPin {
        key: String,
        profile_id: String,
        pin: bool,
    },
}

/// The overlay→binary command queue. A plain deque under the same locking discipline as
/// the shared models — the service thread drains it on a short cadence (it's never
/// latency-critical: every command's effect arrives via a model snapshot anyway).
#[derive(Clone, Default)]
pub struct ConsoleBus(Arc<Mutex<VecDeque<ConsoleCmd>>>);

impl ConsoleBus {
    /// Queue a command. Normally the shell's side; the binary may also seed one (the
    /// direct-entry library fetch) — same lane, same handler.
    pub fn send(&self, cmd: ConsoleCmd) {
        self.0.lock().unwrap().push_back(cmd);
    }

    /// Binary side: drain everything queued since the last call.
    pub fn drain(&self) -> Vec<ConsoleCmd> {
        self.0.lock().unwrap().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_generation_bumps_only_on_change() {
        let shared = ConsoleShared::default();
        let row = HostRow {
            key: "aa".into(),
            name: "Tower".into(),
            addr: "10.0.0.2".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: false,
            mgmt_port: 47990,
            can_wake: false,
            last_used: None,
            os: String::new(),
            pin: None,
            bound_profile: None,
        };
        shared.set_hosts(vec![row.clone()]);
        let g1 = shared.hosts_gen();
        shared.set_hosts(vec![row.clone()]);
        assert_eq!(shared.hosts_gen(), g1, "identical snapshot doesn't churn");
        shared.set_hosts(vec![HostRow {
            online: true,
            ..row
        }]);
        assert_eq!(shared.hosts_gen(), g1 + 1);
    }

    #[test]
    fn bus_drains_in_order() {
        let bus = ConsoleBus::default();
        bus.send(ConsoleCmd::Probe);
        bus.send(ConsoleCmd::CancelWake);
        assert_eq!(bus.drain(), vec![ConsoleCmd::Probe, ConsoleCmd::CancelWake]);
        assert!(bus.drain().is_empty());
    }
}
