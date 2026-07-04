//! LAN host discovery: browse the host's mDNS advert (`_punktfunk._udp`, TXT keys
//! `fp`/`pair`/`id` — see the host crate's `discovery.rs`) on a worker thread and stream
//! results to the UI. Ported verbatim from the GTK client (`mdns-sd` is cross-platform).

use mdns_sd::{ServiceDaemon, ServiceEvent};

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
}

/// Browse continuously for the app's lifetime. The thread exits when the receiver is
/// dropped (the send fails) or the daemon dies.
pub fn browse() -> async_channel::Receiver<DiscoveredHost> {
    let (tx, rx) = async_channel::unbounded();
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
            let receiver = match daemon.browse("_punktfunk._udp.local.") {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS browse failed — discovery disabled");
                    return;
                }
            };
            while let Ok(event) = receiver.recv() {
                if let ServiceEvent::ServiceResolved(info) = event {
                    let props = info.get_properties();
                    let val = |k: &str| props.get_property_val_str(k).unwrap_or("").to_string();
                    let Some(addr) = info.get_addresses().iter().next().map(|a| a.to_string())
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
                    };
                    if tx.send_blocking(host).is_err() {
                        break; // UI gone — stop browsing
                    }
                }
            }
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    rx
}
