//! The GameStream control stream: an ENet host on UDP 47999. Moonlight connects this
//! BEFORE the video stream starts (`STAGE_CONTROL_STREAM_START` precedes
//! `STAGE_VIDEO_STREAM_START`), so it must be up or the whole connection aborts. P1.4 here
//! just accepts the connection + services ENet (keepalive/timeouts) so video can flow;
//! decoding control messages into input injection (mouse/keyboard/gamepad) is the next step.
//!
//! Plaintext for now (we negotiate `encryptionEnabled=0` in DESCRIBE); the encrypted
//! SS_ENC_CONTROL_V2 framing is P1.5. Runs on its own native thread for the host's lifetime.

use super::CONTROL_PORT;
use anyhow::{anyhow, Context, Result};
use rusty_enet::{Event, Host, HostSettings};
use std::net::UdpSocket;
use std::time::Duration;

/// Bind the ENet control host on 47999 and service it forever on a dedicated thread.
pub fn spawn() -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", CONTROL_PORT)).context("bind control UDP")?;
    socket
        .set_nonblocking(true)
        .context("control socket nonblocking")?;
    let mut host = Host::new(
        socket,
        HostSettings {
            peer_limit: 4,
            channel_limit: 8,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("ENet host init: {e:?}"))?;
    tracing::info!(port = CONTROL_PORT, "ENet control listening");

    std::thread::Builder::new()
        .name("lumen-control".into())
        .spawn(move || loop {
            loop {
                match host.service() {
                    Ok(Some(event)) => match event {
                        Event::Connect { .. } => {
                            tracing::info!("control: client connected");
                        }
                        Event::Disconnect { .. } => {
                            tracing::info!("control: client disconnected");
                        }
                        Event::Receive {
                            channel_id, packet, ..
                        } => {
                            let d = packet.data();
                            let opcode = if d.len() >= 2 {
                                u16::from_le_bytes([d[0], d[1]])
                            } else {
                                0
                            };
                            // TODO(P1.4): decode input events (mouse/keyboard/gamepad) → inject.rs.
                            tracing::debug!(
                                channel_id,
                                len = d.len(),
                                opcode = format!("0x{opcode:04x}"),
                                "control: message"
                            );
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:?}"), "control: service error");
                        break;
                    }
                }
            }
            // ENet needs frequent servicing for handshake/keepalive/retransmit.
            std::thread::sleep(Duration::from_millis(2));
        })
        .context("spawn control thread")?;
    Ok(())
}
