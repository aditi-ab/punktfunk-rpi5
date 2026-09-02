//! Windows adapter for the shared [`pf_client_core::discovery`] browser.
//!
//! The WinUI shell still consumes resolved hosts rather than discovery events,
//! so [`browse`] maps `Resolved` and ignores `Removed`. The core owns the mDNS
//! daemon, address selection, rescan backoff, and shutdown. This module keeps
//! only the Windows-facing host shape used by saved-host and wake flows.
//!
//! Dropped UI receivers stop the adapter even when the LAN stays empty.

use std::time::Duration;

pub use pf_client_core::discovery::Rescan;

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
    /// Persisted like `mac` (`trust::learn_from_advert`), and load-bearing rather than cosmetic:
    /// a host moved off 47990 loses its library once mDNS is gone unless we write this down.
    /// `None` if absent (older host) — resolve via `library::DEFAULT_MGMT_PORT`.
    pub mgmt_port: Option<u16>,
}

impl From<pf_client_core::discovery::DiscoveredHost> for DiscoveredHost {
    fn from(host: pf_client_core::discovery::DiscoveredHost) -> Self {
        Self {
            key: host.key,
            name: host.name,
            addr: host.addr,
            port: host.port,
            fp_hex: host.fp_hex,
            pair: host.pair,
            mac: host.mac,
            os: host.os,
            mgmt_port: host.mgmt_port,
        }
    }
}

/// Adapt shared discovery to the host-only receiver the Windows UI consumes.
/// Removal stays ignored until the shell owns an offline-host state.
pub fn browse() -> (async_channel::Receiver<DiscoveredHost>, Rescan) {
    let (events, rescan) = pf_client_core::discovery::browse();
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("punktfunk-mdns-windows".into())
        .spawn(move || {
            while !tx.is_closed() {
                match events.try_recv() {
                    Ok(pf_client_core::discovery::DiscoveryEvent::Resolved(host)) => {
                        if tx.send_blocking(host.into()).is_err() {
                            break;
                        }
                    }
                    Ok(pf_client_core::discovery::DiscoveryEvent::Removed { .. }) => {}
                    Err(async_channel::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                }
            }
        })
        .expect("spawn mDNS adapter thread");
    (rx, rescan)
}
