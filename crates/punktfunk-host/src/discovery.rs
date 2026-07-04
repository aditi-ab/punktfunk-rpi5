//! mDNS advertisement of the native punktfunk/1 service so native clients auto-discover the
//! host — the native-protocol analogue of the GameStream `_nvstream._tcp` advert
//! ([`crate::gamestream::mdns`]).
//!
//! The service type is **`_punktfunk._udp.local.`** (UDP because punktfunk/1 is QUIC, and the
//! advertised port is the QUIC control/data port a client `--connect`s). TXT records carry:
//! - `proto` — the wire protocol id ([`NATIVE_PROTO`]), so a future incompatible revision is
//!   distinguishable by discovery alone;
//! - `fp` — the host certificate SHA-256 (lowercase hex), the exact value a client pins. mDNS is
//!   unauthenticated, so this is advisory — TOFU/pinning still verifies it on connect — but it
//!   lets a picker show the fingerprint and pre-pin a chosen host;
//! - `pair` — `required` or `optional`, so a client can tell up front whether it must run the PIN
//!   pairing ceremony before it can stream;
//! - `id` — the stable host uniqueid (dedup across IPs / re-advertises);
//! - `mgmt` — the management API's TCP port (when it serves one), so a client can fetch the host's
//!   game library (`GET /api/v1/library`, mTLS) on the SAME IP without assuming the default port.
//!   Omitted by a host with no mgmt API (the standalone `punktfunk1-host`).
//! - `mac` — the host's wake-capable NIC MAC(s) (comma-separated, routed NIC first), which a client
//!   persists so it can Wake-on-LAN this host after it sleeps. Advisory/unauthenticated (a wrong
//!   MAC only makes a wake fail). Omitted when none can be read.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use std::net::IpAddr;

/// The native-protocol mDNS service type. Clients browse this to find punktfunk/1 hosts.
pub const NATIVE_SERVICE: &str = "_punktfunk._udp.local.";

/// Wire protocol id advertised in the `proto` TXT record.
pub const NATIVE_PROTO: &str = "punktfunk/1";

/// Holds the mDNS daemon; dropping it unregisters the service.
pub struct Advert {
    _daemon: ServiceDaemon,
}

/// Advertise the native host on the LAN. `fingerprint` is the host cert SHA-256 (lowercase hex);
/// `require_pairing` tells a discovering client whether it must pair before it can stream;
/// `mgmt_port` is the management API's port (`Some` when this host serves one — the client browses
/// the library there over mTLS on the advertised IP), `None` for a host with no mgmt API.
pub fn advertise_native(
    hostname: &str,
    ip: IpAddr,
    port: u16,
    fingerprint: &str,
    require_pairing: bool,
    uniqueid: &str,
    mgmt_port: Option<u16>,
) -> Result<Advert> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let host_name = format!("{hostname}.local.");
    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("proto".into(), NATIVE_PROTO.into());
    props.insert("fp".into(), fingerprint.to_string());
    props.insert(
        "pair".into(),
        if require_pairing {
            "required"
        } else {
            "optional"
        }
        .into(),
    );
    props.insert("id".into(), uniqueid.to_string());
    if let Some(mgmt) = mgmt_port {
        props.insert("mgmt".into(), mgmt.to_string());
    }
    // `mac` — the host's wake-capable NIC MAC(s), comma-separated `aa:bb:cc:dd:ee:ff`, routed NIC
    // first. A client persists these while the host is awake so it can send a Wake-on-LAN magic
    // packet to wake it later (when it's asleep and no longer advertising). Unauthenticated like
    // the rest of the advert, but a wrong MAC only makes a wake fail — the magic packet is inert
    // and the cert fingerprint still gates the actual connection. Omitted when none can be read.
    let macs = crate::wol::wake_macs(ip);
    if !macs.is_empty() {
        props.insert("mac".into(), macs.join(","));
    }
    // Detect & warn (never modifies) if the routed NIC isn't armed to wake — the usual reason WoL
    // silently fails.
    crate::wol::warn_if_not_armed(ip);
    let service = ServiceInfo::new(NATIVE_SERVICE, hostname, &host_name, ip, port, props)
        .context("build native mDNS ServiceInfo")?;
    daemon
        .register(service)
        .context("register native mDNS service")?;
    tracing::info!(
        service = "_punktfunk._udp",
        port,
        host = %host_name,
        pair = if require_pairing { "required" } else { "optional" },
        "native punktfunk/1 mDNS advertising"
    );
    Ok(Advert { _daemon: daemon })
}
