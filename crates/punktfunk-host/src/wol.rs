//! Host-side Wake-on-LAN / Wake-on-Wireless-LAN support.
//!
//! Two jobs, both best-effort (a failure here never affects streaming):
//!  1. [`wake_macs`] — report the host's wake-capable NIC MAC(s) so a client can persist them
//!     (from the mDNS `mac` TXT record, [`crate::discovery`]) and wake this host later, once it's
//!     asleep and no longer advertising. Wired and Wi-Fi NICs alike: a magic packet is the same
//!     packet either way, and an associated station in WoWLAN sleep receives the broadcast the
//!     AP buffers for it.
//!  2. [`warn_if_not_armed`] — *detect & warn only* whether the NIC is actually armed to wake on a
//!     magic packet. We never change NIC settings (that's the user's call); we just surface the
//!     single most common reason WoL silently fails.
//!
//! Wired and wireless are armed through completely different interfaces, so the check follows the
//! NIC: `ethtool <iface>` reports the wired `Wake-on: g` bit, while a Wi-Fi NIC's magic-packet
//! trigger lives in nl80211's WoWLAN state and is read with `iw phy <phy> wowlan show`. Asking
//! ethtool about a Wi-Fi NIC is what the previous version did, and it is actively misleading:
//! most wireless drivers print `Wake-on: d` whether or not WoWLAN is armed, so an armed host got
//! warned that it wasn't — with a fix command (`ethtool -s wlan0 wol g`) that its driver rejects.

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
/// warn only — never modifies settings. Linux-only (shells out to `iw`/`ethtool`); a no-op
/// elsewhere and silent when it can't tell (tool missing, insufficient privilege).
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

    // A NIC with an nl80211 phy is wireless: ask nl80211 about WoWLAN, not ethtool about WoL.
    if let Some(phy) = wireless_phy(&iface) {
        match wowlan_has_magic(phy.as_deref(), &iface) {
            Some(true) => tracing::info!(
                iface = %iface,
                phy = phy.as_deref().unwrap_or("?"),
                "Wake-on-WLAN armed (magic packet) on host Wi-Fi NIC"
            ),
            Some(false) => {
                let phy = phy.as_deref().unwrap_or("phy0");
                // A device the kernel won't arm can't wake on anything, so name that separately
                // — enabling a WoWLAN trigger alone would not fix it.
                let extra = if device_wakeup_enabled(&iface) == Some(false) {
                    " The kernel also has wake-up switched off for this device \
                     (/sys/class/net/<iface>/device/power/wakeup reads `disabled`), which blocks \
                     a network wake by itself."
                } else {
                    ""
                };
                tracing::warn!(
                    iface = %iface,
                    "Wake-on-WLAN is NOT armed on this host's Wi-Fi NIC — clients cannot wake it \
                     from sleep. Enable it with: sudo iw phy {phy} wowlan enable magic-packet \
                     (NetworkManager resets that on every re-connect; make it stick with: sudo \
                     nmcli connection modify <connection> 802-11-wireless.wake-on-wlan magic). \
                     The adapter must also stay powered and associated while the host sleeps, and \
                     be allowed to wake the machine in BIOS/UEFI.{extra}",
                )
            }
            None => {} // couldn't determine — stay quiet rather than cry wolf
        }
        return;
    }

    match ethtool_wol_has_magic(&iface) {
        Some(true) => {
            tracing::info!(iface = %iface, "Wake-on-LAN armed (magic packet) on host NIC")
        }
        Some(false) => tracing::warn!(
            iface = %iface,
            "Wake-on-LAN is NOT armed on this host's NIC — clients cannot wake it from sleep. \
             Enable it with: sudo ethtool -s {iface} wol g  (and turn on 'Wake on LAN'/'Wake on \
             PCIe' in BIOS).",
        ),
        None => {} // couldn't determine — stay quiet rather than cry wolf
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warn_if_not_armed(_primary_ip: IpAddr) {}

/// Is `iface` a Wi-Fi NIC, and if so which nl80211 phy backs it? `Some(Some("phy0"))` = wireless
/// and we know the phy (so we can query and name it); `Some(None)` = wireless but the phy name
/// couldn't be read; `None` = wired (or sysfs is unavailable, which reads the same way — the
/// ethtool path then applies, exactly as before).
#[cfg(target_os = "linux")]
fn wireless_phy(iface: &str) -> Option<Option<String>> {
    let dir = format!("/sys/class/net/{iface}/phy80211");
    if !std::path::Path::new(&dir).exists() {
        return None;
    }
    let name = std::fs::read_to_string(format!("{dir}/name"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(name)
}

/// Whether a Wi-Fi NIC is armed for a magic-packet wake. `iw` is authoritative — it reads the
/// live nl80211 WoWLAN state, which is where the trigger actually lives.
///
/// Two fallbacks for when `iw` can't answer (binary missing, driver without the WoWLAN command,
/// no phy name, or a kernel that wants privilege we don't have — the host runs as a plain user
/// service, so that last one is not hypothetical):
///  * a *positive* ethtool reading counts, a negative one never does — a handful of drivers
///    (brcmfmac and friends, i.e. most Raspberry Pi / SoC Wi-Fi) really do expose the
///    magic-packet bit through ethtool, while the far more common `Wake-on: d` from a wireless
///    driver means nothing at all;
///  * failing that, sysfs `device/power/wakeup` — world-readable, and a `disabled` there is
///    conclusive in the negative direction: the kernel will not arm this device to wake the
///    machine, so whatever WoWLAN triggers the firmware holds can never fire.
#[cfg(target_os = "linux")]
fn wowlan_has_magic(phy: Option<&str>, iface: &str) -> Option<bool> {
    if let Some(v) = phy.and_then(iw_wowlan_has_magic) {
        return Some(v);
    }
    if let Some(true) = ethtool_wol_has_magic(iface) {
        return Some(true);
    }
    // Only the negative is meaningful: `enabled` says the device may wake the machine, not that a
    // magic packet is one of the things that will do it.
    match device_wakeup_enabled(iface) {
        Some(false) => Some(false),
        _ => None,
    }
}

/// sysfs `/sys/class/net/<iface>/device/power/wakeup` — `enabled`/`disabled`, i.e. whether the
/// kernel will arm this device to wake the system at all. `None` when the attribute isn't there
/// (platform/SDIO devices often have none) or can't be read.
#[cfg(target_os = "linux")]
fn device_wakeup_enabled(iface: &str) -> Option<bool> {
    let text =
        std::fs::read_to_string(format!("/sys/class/net/{iface}/device/power/wakeup")).ok()?;
    match text.trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

/// Ask nl80211 (via `iw phy <phy> wowlan show`) whether the magic-packet trigger is enabled.
/// `None` if `iw` is missing or the driver doesn't implement WoWLAN.
#[cfg(target_os = "linux")]
fn iw_wowlan_has_magic(phy: &str) -> Option<bool> {
    let out = std::process::Command::new("iw")
        .args(["phy", phy, "wowlan", "show"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_iw_wowlan(&String::from_utf8_lossy(&out.stdout))
}

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
    parse_ethtool_wol(&String::from_utf8_lossy(&out.stdout))
}

/// `ethtool <iface>` output → does the *current* Wake-on setting include `g` (MagicPacket)?
/// `None` when the field is absent. Split out from the command so it can be unit-tested on any
/// platform.
fn parse_ethtool_wol(text: &str) -> Option<bool> {
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

/// `iw phy <phy> wowlan show` output → is the magic-packet trigger enabled? The two shapes are
///
/// ```text
/// WoWLAN is disabled
/// ```
/// ```text
/// WoWLAN is enabled:
///  * wake up on magic packet
///  * wake up on pattern match, up to 20 patterns of 16 - 128 bytes
/// ```
///
/// `* wake up on anything` (the nl80211 `any` trigger) counts too — that NIC wakes on every frame
/// it receives, magic packets included. Enabled with only other triggers reads as NOT armed,
/// which is the honest answer: a magic packet won't wake it. `None` when the output says nothing
/// about WoWLAN at all. Split out from the command so it can be unit-tested on any platform.
fn parse_iw_wowlan(text: &str) -> Option<bool> {
    let mut seen = false;
    let mut magic = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(state) = t.strip_prefix("WoWLAN is ") {
            seen = true;
            if state
                .trim()
                .trim_end_matches(':')
                .eq_ignore_ascii_case("disabled")
            {
                return Some(false);
            }
        } else if seen && t.starts_with('*') {
            let l = t.to_ascii_lowercase();
            if l.contains("magic packet") || l.contains("anything") {
                magic = true;
            }
        }
    }
    seen.then_some(magic)
}

#[cfg(test)]
mod tests {
    use super::{parse_ethtool_wol, parse_iw_wowlan};

    #[test]
    fn ethtool_current_setting_not_capability_line() {
        let armed =
            "Settings for enp5s0:\n\tSupports Wake-on: pumbg\n\tWake-on: g\n\tLink detected: yes\n";
        assert_eq!(parse_ethtool_wol(armed), Some(true));
        // "Supports Wake-on: ...g..." must NOT be read as the current setting.
        let off = "Settings for enp5s0:\n\tSupports Wake-on: pumbg\n\tWake-on: d\n";
        assert_eq!(parse_ethtool_wol(off), Some(false));
        assert_eq!(
            parse_ethtool_wol("Settings for lo:\n\tLink detected: yes\n"),
            None
        );
    }

    #[test]
    fn iw_wowlan_states() {
        assert_eq!(parse_iw_wowlan("WoWLAN is disabled\n"), Some(false));
        assert_eq!(
            parse_iw_wowlan("WoWLAN is enabled:\n * wake up on magic packet\n"),
            Some(true)
        );
        // Enabled, but not for magic packets — a magic packet will not wake this NIC.
        assert_eq!(
            parse_iw_wowlan(
                "WoWLAN is enabled:\n * wake up on pattern match, up to 20 patterns of 16 - 128 bytes\n"
            ),
            Some(false)
        );
        // The `any` trigger wakes on every received frame, magic packets included.
        assert_eq!(
            parse_iw_wowlan("WoWLAN is enabled:\n * wake up on anything (device continues operating normally)\n"),
            Some(true)
        );
        // Nothing to go on — the driver has no WoWLAN command.
        assert_eq!(parse_iw_wowlan(""), None);
        assert_eq!(
            parse_iw_wowlan("Wiphy phy0\n\tmax # scan SSIDs: 20\n"),
            None
        );
    }
}
