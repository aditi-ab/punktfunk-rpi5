//! Host-side Wake-on-LAN support.
//!
//! Two jobs, both best-effort (a failure here never affects streaming):
//!  1. [`wake_macs`] — report the host's wake-capable NIC MAC(s) so a client can persist them
//!     (from the mDNS `mac` TXT record, [`crate::discovery`]) and wake this host later, once it's
//!     asleep and no longer advertising.
//!  2. [`warn_if_not_armed`] — *detect & warn only* whether the NIC is actually armed to wake on a
//!     magic packet. We never change NIC settings (that's the user's call); we just surface the
//!     single most common reason WoL silently fails.

use std::net::IpAddr;

/// Upper bound on advertised MACs — keeps the mDNS TXT record small. A host has at most a couple
/// of wake-capable NICs; the routed one is always first.
const MAX_MACS: usize = 4;

/// MAC(s) of the host's wake-capable NIC(s), lowercase `aa:bb:cc:dd:ee:ff`, with the NIC that
/// bears `primary_ip` (the address clients reach us on) FIRST, then other non-loopback NICs as
/// fallbacks. Best-effort — an empty list just means clients can't auto-wake (they fall back to
/// manual MAC entry). Deduped; all-zero MACs skipped; capped at [`MAX_MACS`].
pub fn wake_macs(primary_ip: IpAddr) -> Vec<String> {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();

    // Interface names in priority order: the one holding `primary_ip` first, then every other
    // non-loopback interface that has an IP, de-duplicated by name (an iface has one MAC but may
    // appear once per address).
    let mut names: Vec<String> = Vec::new();
    if let Some(primary) = ifaces.iter().find(|i| i.ip() == primary_ip) {
        names.push(primary.name.clone());
    }
    for i in &ifaces {
        if i.is_loopback() {
            continue;
        }
        if !names.contains(&i.name) {
            names.push(i.name.clone());
        }
    }

    let mut out: Vec<String> = Vec::new();
    for name in names {
        let Ok(Some(mac)) = mac_address::mac_address_by_name(&name) else {
            continue;
        };
        let b = mac.bytes();
        if b == [0u8; 6] {
            continue; // unset / virtual
        }
        let s = format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        );
        if !out.contains(&s) {
            out.push(s);
        }
        if out.len() >= MAX_MACS {
            break;
        }
    }
    out
}

/// Log whether the host NIC bearing `primary_ip` is armed to wake on a magic packet. Detect &
/// warn only — never modifies settings. Linux-only (reads `ethtool <iface>`); a no-op elsewhere
/// and silent when it can't tell (no `ethtool`, insufficient privilege).
#[cfg(target_os = "linux")]
pub fn warn_if_not_armed(primary_ip: IpAddr) {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let Some(iface) = ifaces
        .iter()
        .find(|i| i.ip() == primary_ip)
        .map(|i| i.name.clone())
    else {
        return;
    };
    match ethtool_wol_has_magic(&iface) {
        Some(true) => {
            tracing::info!(iface = %iface, "Wake-on-LAN armed (magic packet) on host NIC")
        }
        Some(false) => tracing::warn!(
            iface = %iface,
            "Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep. \
             Enable it with: sudo ethtool -s {iface} wol g  (and turn on 'Wake on LAN'/'Wake on \
             PCIe' in BIOS). Wired Ethernet is required; Wi-Fi wake is unreliable.",
        ),
        None => {} // couldn't determine — stay quiet rather than cry wolf
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warn_if_not_armed(_primary_ip: IpAddr) {}

/// Parse `ethtool <iface>` for the *current* Wake-on setting and report whether it includes `g`
/// (wake on MagicPacket). Returns `None` if ethtool is missing/failed or the field is absent.
#[cfg(target_os = "linux")]
fn ethtool_wol_has_magic(iface: &str) -> Option<bool> {
    let out = std::process::Command::new("ethtool")
        .arg(iface)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let t = line.trim();
        // The current setting is "Wake-on: <flags>"; skip the "Supports Wake-on: ..." capability
        // line. `g` = MagicPacket, `d` = disabled.
        if let Some(flags) = t.strip_prefix("Wake-on:") {
            return Some(flags.trim().contains('g'));
        }
    }
    None
}
