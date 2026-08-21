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
//! - `os` — the host's OS identity chain (`windows` | `macos` | `linux[/<family>][/<id>]`, e.g.
//!   `linux/fedora/bazzite` — see [`crate::osinfo`]), so a client can show an OS icon on the host
//!   card. Advisory/unauthenticated like `mac`: a wrong value only draws a wrong icon.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The native-protocol mDNS service type. Clients browse this to find punktfunk/1 hosts.
pub const NATIVE_SERVICE: &str = "_punktfunk._udp.local.";

/// mDNS advertisement gate — `PUNKTFUNK_MDNS`. Default ON; `0|false|off|no` (the
/// `PUNKTFUNK_ZEROCOPY` off-grammar) disables BOTH the native and GameStream adverts, for
/// environments where multicast is dead or unwanted (bridged Docker, CI network namespaces,
/// locked-down VLANs): the advert there reaches nobody — or fails outright and aborts the
/// GameStream plane — while clients can always dial a manually-added host (mDNS-blind
/// host-add works since the 0.8.4 dial-first fix). CLI `--no-mdns` sets the same knob.
pub(crate) fn mdns_enabled() -> bool {
    !std::env::var("PUNKTFUNK_MDNS")
        .map(|s| mdns_off_value(&s))
        .unwrap_or(false)
}

/// `true` iff the `PUNKTFUNK_MDNS` value means "off". Split from the env read for testability
/// (env vars are process-global; tests must not race the parallel suite by setting them).
fn mdns_off_value(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Wire protocol id advertised in the `proto` TXT record.
pub const NATIVE_PROTO: &str = "punktfunk/1";

/// The DNS label a display name advertises its A record under (`<label>.local.`), which is a
/// different thing from the service *instance* name: an instance name may be free text
/// ("Living Room PC" — see `PUNKTFUNK_HOST_NAME`), a host name may not, and `mdns-sd` rejects the
/// whole `ServiceInfo` if the target isn't a legal name — which would take discovery down entirely
/// rather than just look wrong. Anything outside `[A-Za-z0-9.-]` becomes `-`; a name that is
/// already legal (every machine hostname, i.e. every host without the override) passes through
/// byte-for-byte, so this changes nothing for hosts that don't set the knob.
pub(crate) fn dns_label(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if out.len() >= 63 {
            break;
        }
        out.push(if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            c
        } else {
            '-'
        });
    }
    let out = out.trim_matches(['-', '.']).to_string();
    if out.is_empty() {
        "punktfunk-host".to_string()
    } else {
        out
    }
}

/// Holds the mDNS daemon; dropping it unregisters the service and stops the re-announce loop.
pub struct Advert {
    _daemon: ServiceDaemon,
    stop: Arc<AtomicBool>,
}

impl Drop for Advert {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How often a live advert re-checks the address it is announcing.
const IP_RECHECK: Duration = Duration::from_secs(10);

/// The address to advertise right now — loopback only while the machine still has none.
fn current_ip() -> IpAddr {
    crate::gamestream::primary_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Register `build(ip)` for the host's current address, and re-register it whenever that address
/// changes. Shared by both adverts ([`advertise_native`] and [`crate::gamestream::mdns`]).
///
/// mDNS records are PUSHED, not polled: whatever address was true at `register()` keeps being
/// announced until something registers a newer one. The host process comes up during boot, which
/// on a cold start is before the machine has an address — so the first registration could be
/// `127.0.0.1`, and it stayed that way until the host was restarted by hand. `mdns-sd` documents a
/// second `register()` of the same fullname as an update, so re-announcing is just calling it
/// again.
///
/// Polls the *routed* address rather than subscribing to the daemon's `IpAdd` events, because the
/// boot race usually resolves without one: the NIC often has its address before we register and
/// only the default route lands late, so no interface event ever fires.
pub(crate) fn advertise_live(
    service: &'static str,
    build: impl Fn(IpAddr) -> Result<ServiceInfo> + Send + 'static,
) -> Result<Advert> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let registered = current_ip();
    daemon
        .register(build(registered)?)
        .with_context(|| format!("register {service} mDNS service"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let (bg_daemon, bg_stop) = (daemon.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut announced = registered;
        while !bg_stop.load(Ordering::Relaxed) {
            std::thread::sleep(IP_RECHECK);
            let now = current_ip();
            if now == announced {
                continue;
            }
            match build(now)
                .and_then(|info| bg_daemon.register(info).context("re-register mDNS service"))
            {
                Ok(()) => {
                    tracing::info!(service, from = %announced, to = %now, "host address changed — re-announced");
                    announced = now;
                }
                // Leave the previous record standing and retry next tick rather than going dark.
                Err(e) => {
                    tracing::warn!(service, error = %format!("{e:#}"), "mDNS re-announce failed");
                }
            }
        }
    });
    Ok(Advert {
        _daemon: daemon,
        stop,
    })
}

/// Advertise the native host on the LAN. `fingerprint` is the host cert SHA-256 (lowercase hex);
/// `require_pairing` tells a discovering client whether it must pair before it can stream;
/// `mgmt_port` is the management API's port (`Some` when this host serves one — the client browses
/// the library there over mTLS on the advertised IP), `None` for a host with no mgmt API.
// One parameter per TXT key, single call site — a params struct would just restate the
// module doc's key list with extra ceremony.
#[allow(clippy::too_many_arguments)]
pub fn advertise_native(
    hostname: &str,
    port: u16,
    fingerprint: &str,
    require_pairing: bool,
    uniqueid: &str,
    mgmt_port: Option<u16>,
    os_chain: &str,
) -> Result<Advert> {
    // `hostname` is the DISPLAY name (the instance label clients read back); the A-record target
    // has to be a legal DNS name, hence the separate sanitized label.
    let host_name = format!("{}.local.", dns_label(hostname));
    // Owned, because the record is rebuilt whenever the host's address changes — see
    // [`advertise_live`]. Everything except the address (and the MACs derived from it) is fixed,
    // so it is computed once here and moved into the builder.
    let instance = hostname.to_string();
    let mut fixed: HashMap<String, String> = HashMap::new();
    fixed.insert("proto".into(), NATIVE_PROTO.into());
    fixed.insert("fp".into(), fingerprint.to_string());
    fixed.insert(
        "pair".into(),
        if require_pairing {
            "required"
        } else {
            "optional"
        }
        .into(),
    );
    fixed.insert("id".into(), uniqueid.to_string());
    if let Some(mgmt) = mgmt_port {
        fixed.insert("mgmt".into(), mgmt.to_string());
    }
    // `os` — advisory OS-identity chain for the client's host-card icon (see module doc).
    if !os_chain.is_empty() {
        fixed.insert("os".into(), os_chain.to_string());
    }
    tracing::info!(
        service = "_punktfunk._udp",
        port,
        host = %host_name,
        pair = if require_pairing { "required" } else { "optional" },
        "native punktfunk/1 mDNS advertising"
    );
    advertise_live(NATIVE_SERVICE, move |ip| {
        let mut props = fixed.clone();
        // `mac` — the host's wake-capable NIC MAC(s), comma-separated `aa:bb:cc:dd:ee:ff`, routed
        // NIC first. A client persists these while the host is awake so it can send a
        // Wake-on-LAN magic packet to wake it later (when it's asleep and no longer advertising).
        // Unauthenticated like the rest of the advert, but a wrong MAC only makes a wake fail —
        // the magic packet is inert and the cert fingerprint still gates the actual connection.
        // Omitted when none can be read, which is what a host that came up before its network did
        // used to report forever.
        let macs = crate::wol::wake_macs(ip);
        if !macs.is_empty() {
            props.insert("mac".into(), macs.join(","));
        }
        // Detect & warn (never modifies) if the routed NIC isn't armed to wake — the usual reason
        // WoL silently fails. Re-checked on an address change because the routed NIC may be a
        // different one now.
        crate::wol::warn_if_not_armed(ip);
        ServiceInfo::new(NATIVE_SERVICE, &instance, &host_name, ip, port, props)
            .context("build native mDNS ServiceInfo")
    })
}

#[cfg(test)]
mod tests {
    use super::{dns_label, mdns_off_value};

    #[test]
    fn dns_label_passes_machine_names_through_and_tames_display_names() {
        // Every host WITHOUT PUNKTFUNK_HOST_NAME must advertise exactly what it did before.
        for plain in ["bazzite-htpc", "DESKTOP-1A2B3C", "box.lan", "steamdeck"] {
            assert_eq!(dns_label(plain), plain);
        }
        // A free-text display name becomes a legal label instead of poisoning the record.
        assert_eq!(dns_label("Living Room PC"), "Living-Room-PC");
        assert_eq!(dns_label("  Ben's Rig!  "), "Ben-s-Rig");
        assert_eq!(dns_label("Wohnzimmer-PC ☕"), "Wohnzimmer-PC");
        // Degenerate input still yields something registerable.
        assert_eq!(dns_label("***"), "punktfunk-host");
        assert_eq!(dns_label(""), "punktfunk-host");
        // DNS caps a label at 63 bytes.
        assert!(dns_label(&"a".repeat(200)).len() <= 63);
    }

    #[test]
    fn mdns_off_grammar() {
        for off in ["0", "false", "off", "no", " OFF ", "False"] {
            assert!(mdns_off_value(off), "{off:?} should disable mDNS");
        }
        // Anything else — including set-but-empty — keeps the advert on (matches the
        // PUNKTFUNK_ZEROCOPY grammar: only an explicit off-value turns it off).
        for on in ["", "1", "true", "yes", "on", "banana"] {
            assert!(!mdns_off_value(on), "{on:?} should keep mDNS on");
        }
    }
}
