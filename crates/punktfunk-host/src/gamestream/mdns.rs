//! mDNS advertisement of `_nvstream._tcp.local.` so Moonlight auto-discovers the host.
//! (Manual "add host by IP" also works as a fallback, which is what we test with first.)

use super::Host;
use anyhow::{Context, Result};
use mdns_sd::ServiceInfo;
use std::collections::HashMap;

// One `Advert` for both service types: holds the mDNS daemon plus the re-announce loop that
// keeps the record pointed at the host's current address.
use crate::discovery::Advert;

const SERVICE: &str = "_nvstream._tcp.local.";

pub fn advertise(host: &Host) -> Result<Advert> {
    // Instance name = the display name (what Moonlight lists); A-record target = the sanitized
    // DNS label, so a free-text `PUNKTFUNK_HOST_NAME` can't produce an illegal record.
    let host_name = format!("{}.local.", crate::discovery::dns_label(&host.hostname));
    let instance = host.hostname.clone();
    let port = host.http_port;
    tracing::info!(
        service = "_nvstream._tcp",
        port,
        host = %host_name,
        "mDNS advertising"
    );
    // The advertised address is supplied per-registration so the record follows the host onto a
    // network that only came up after boot — see [`crate::discovery::advertise_live`].
    crate::discovery::advertise_live(SERVICE, move |ip| {
        // No TXT records are required for Moonlight discovery; it resolves the A record and then
        // GETs /serverinfo for capabilities.
        let props: HashMap<String, String> = HashMap::new();
        ServiceInfo::new(SERVICE, &instance, &host_name, ip, port, props)
            .context("build mDNS ServiceInfo")
    })
}
