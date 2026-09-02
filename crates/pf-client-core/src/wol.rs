//! Client Wake-on-LAN: parse stored MAC strings and send via `punktfunk_core::wol`.
//! A sleeping host has no ARP entry; the core's broadcast is what actually wakes it.

use std::net::Ipv4Addr;

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
