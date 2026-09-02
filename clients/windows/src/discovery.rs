//! Windows adapter for the shared [`pf_client_core::discovery`] browser.
//!
//! The WinUI shell still consumes resolved hosts rather than discovery events,
//! so [`browse`] forwards `Resolved` and ignores `Removed`. The core owns the
//! mDNS daemon, address selection, rescan backoff, and shutdown.
//!
//! Dropped UI receivers stop the adapter even when the LAN stays empty.

use std::time::Duration;

pub use pf_client_core::discovery::{DiscoveredHost, Rescan};

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
                        if tx.send_blocking(host).is_err() {
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
