//! LAN host discovery: browse the host's mDNS advert (`_punktfunk._udp`, TXT keys
//! `fp`/`pair`/`id` — see the host crate's `discovery.rs`) on a worker thread and stream
//! results to the UI. Ported verbatim from the GTK client (`mdns-sd` is cross-platform).

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// DNS-SD service type punktfunk hosts advertise (host side: `punktfunk_host::discovery`).
const SERVICE_TYPE: &str = "_punktfunk._udp.local.";

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredHost {
    /// Stable row key: the advertised host id, falling back to the mDNS fullname.
    pub key: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Host certificate fingerprint to pin (lowercase hex), empty if not advertised.
    pub fp_hex: String,
    /// Pairing requirement: `"required"` or `"optional"`.
    pub pair: String,
    /// Wake-on-LAN MAC(s) from the mDNS `mac` TXT (comma-separated `aa:bb:cc:dd:ee:ff`), which the
    /// hosts page persists onto the matching saved host so it can wake it later. Empty if absent.
    pub mac: Vec<String>,
    /// The host's OS-identity chain from the mDNS `os` TXT (`windows` | `macos` |
    /// `linux[/<family>][/<id>]`), sanitized — drives the host tile's OS mark and is
    /// persisted like `mac`. Empty if absent (older host).
    pub os: String,
    /// The management API's port from the mDNS `mgmt` TXT — where the game library is served.
    /// Persisted like `mac` (`trust::learn_mgmt_port`), and load-bearing rather than cosmetic:
    /// a host moved off 47990 loses its library once mDNS is gone unless we write this down.
    /// `None` if absent (older host) — resolve via `library::DEFAULT_MGMT_PORT`.
    pub mgmt_port: Option<u16>,
}

/// Forces the running browse to re-query now — the hosts page's Refresh. Mirrors
/// `pf_client_core::discovery::Rescan`; see there for why a client needs one (`mdns-sd` re-queries
/// on a backoff that doubles out to an hour, so a long-lived browse is effectively passive).
#[derive(Clone, Debug)]
pub struct Rescan(Arc<AtomicBool>);

impl Rescan {
    /// Ask the browse thread to put a fresh query on the wire. Coalesces; returns immediately.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Browse continuously for the app's lifetime, with a handle that forces an immediate re-query.
/// The thread exits when the receiver is dropped (the send fails) or the daemon dies.
pub fn browse() -> (async_channel::Receiver<DiscoveredHost>, Rescan) {
    let (tx, rx) = async_channel::unbounded();
    let flag = Arc::new(AtomicBool::new(false));
    let requested = flag.clone();
    std::thread::Builder::new()
        .name("punktfunk-mdns".into())
        .spawn(move || {
            let daemon = match ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS daemon failed — discovery disabled");
                    return;
                }
            };
            let mut receiver = match daemon.browse(SERVICE_TYPE) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS browse failed — discovery disabled");
                    return;
                }
            };
            loop {
                // The worker has to notice that its consumer went away even when NOTHING is
                // arriving — the normal state of a LAN with no hosts on it. The old blocking
                // `recv()` only ever learned that from a failed send, so a bounded consumer (the
                // wake-and-wait below spawns one browse per wake) left this thread and its daemon
                // — another thread, and a socket bound to :5353 — running for the app's lifetime.
                // Checked at the TOP so the `continue` arms below can't skip it either.
                if tx.is_closed() {
                    break;
                }
                // Re-browsing the same type replaces the daemon's listener: it replays the cache
                // into the new channel, queries immediately, and resets the backoff.
                if requested.swap(false, Ordering::Relaxed) {
                    match daemon.browse(SERVICE_TYPE) {
                        Ok(r) => receiver = r,
                        Err(e) => tracing::warn!(error = %e, "mDNS rescan failed"),
                    }
                }
                let event = match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => event,
                    Err(_) if receiver.is_disconnected() && receiver.is_empty() => break,
                    Err(_) => continue, // timed out — go round and look for a rescan request
                };
                if let ServiceEvent::ServiceResolved(info) = event {
                    let props = info.get_properties();
                    let val = |k: &str| props.get_property_val_str(k).unwrap_or("").to_string();
                    // IPv4 only, like every other client (`pf_client_core::discovery`): the core
                    // dials `format!("{host}:{port}").parse::<SocketAddr>()`, which cannot parse a
                    // bare IPv6 literal, and the host stack binds IPv4 sockets exclusively. Taking
                    // an arbitrary first address here rendered cards that failed on every click,
                    // because a host's OS responder commonly answers AAAA for its hostname.
                    let Some(addr) = info.get_addresses_v4().iter().next().map(|a| a.to_string())
                    else {
                        continue;
                    };
                    let id = val("id");
                    let host = DiscoveredHost {
                        key: if id.is_empty() {
                            info.get_fullname().to_string()
                        } else {
                            id
                        },
                        name: info
                            .get_fullname()
                            .split('.')
                            .next()
                            .unwrap_or("?")
                            .to_string(),
                        addr,
                        port: info.get_port(),
                        fp_hex: val("fp"),
                        pair: val("pair"),
                        mac: val("mac")
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        os: pf_client_core::os::sanitize_os(&val("os")),
                        mgmt_port: val("mgmt").parse().ok(),
                    };
                    if tx.send_blocking(host).is_err() {
                        break; // UI gone — stop browsing
                    }
                }
            }
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    (rx, Rescan(flag))
}
