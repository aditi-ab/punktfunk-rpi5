//! Client-side Wake-on-LAN: parse stored MAC strings and hand them to the shared core sender
//! (`punktfunk_core::wol`). A sleeping host has no ARP entry, so the broadcast the core sends is
//! what actually wakes it; this is called just before connecting to an offline saved host and
//! from the explicit "Wake host" menu item.

use std::net::Ipv4Addr;

/// Fire a Wake-on-LAN magic packet at `macs` (each `aa:bb:cc:dd:ee:ff`), also unicasting
/// `last_ip` when given. Best-effort — logs the outcome and returns promptly (the core sends a
/// short burst of datagrams).
pub fn wake(macs: &[String], last_ip: Option<Ipv4Addr>) {
    let parsed: Vec<[u8; 6]> = macs
        .iter()
        .filter_map(|s| punktfunk_core::wol::parse_mac(s))
        .collect();
    if parsed.is_empty() {
        tracing::warn!("wake requested but no valid MAC is known for this host");
        return;
    }
    match punktfunk_core::wol::send_magic_packet(&parsed, last_ip) {
        Ok(()) => tracing::info!(count = parsed.len(), "sent Wake-on-LAN magic packet"),
        Err(e) => tracing::warn!(error = %e, "Wake-on-LAN send failed"),
    }
}
