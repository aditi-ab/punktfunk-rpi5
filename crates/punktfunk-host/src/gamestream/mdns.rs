//! mDNS advertisement of `_nvstream._tcp.local.` so Moonlight auto-discovers the host.
//! (Manual "add host by IP" also works as a fallback, which is what we test with first.)

use super::Host;
use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

/// Holds the mDNS daemon; dropping it unregisters the service.
pub struct Advert {
    _daemon: ServiceDaemon,
}

pub fn advertise(host: &Host) -> Result<Advert> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    // Instance name = the display name (what Moonlight lists); A-record target = the sanitized
    // DNS label, so a free-text `PUNKTFUNK_HOST_NAME` can't produce an illegal record.
    let host_name = format!("{}.local.", crate::discovery::dns_label(&host.hostname));
    // No TXT records are required for Moonlight discovery; it resolves the A record and then
    // GETs /serverinfo for capabilities.
    let props: HashMap<String, String> = HashMap::new();
    let service = ServiceInfo::new(
        "_nvstream._tcp.local.",
        &host.hostname,
        &host_name,
        host.local_ip,
        host.http_port,
        props,
    )
    .context("build mDNS ServiceInfo")?;
    daemon.register(service).context("register mDNS service")?;
    tracing::info!(
        service = "_nvstream._tcp",
        port = host.http_port,
        host = %host_name,
        "mDNS advertising"
    );
    Ok(Advert { _daemon: daemon })
}
