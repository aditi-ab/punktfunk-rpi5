//! A USB/IP server (simulation path only).
//!
//! Vendored + trimmed from `usbip` v0.8.0 (jiegec/usbip, MIT); the USB *host* modules and the
//! `rusb`/`nusb` device constructors are removed so this carries no libusb dependency. See `NOTICE`.

use log::*;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use std::any::Any;
use std::collections::HashMap;
use std::io::{ErrorKind, Result};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use usbip_protocol::UsbIpCommand;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

mod consts;
mod device;
mod endpoint;
mod interface;
mod setup;
pub mod usbip_protocol;
mod util;
pub use consts::*;
pub use device::*;
pub use endpoint::*;
pub use interface::*;
pub use setup::*;
pub use util::*;

use crate::usbip_protocol::{UsbIpResponse, USBIP_RET_SUBMIT, USBIP_RET_UNLINK};

/// Main struct of a USB/IP server
#[derive(Default, Debug)]
pub struct UsbIpServer {
    available_devices: RwLock<Vec<UsbDevice>>,
    used_devices: RwLock<HashMap<String, UsbDevice>>,
}

impl UsbIpServer {
    /// Create a [UsbIpServer] with simulated devices
    pub fn new_simulated(devices: Vec<UsbDevice>) -> Self {
        Self {
            available_devices: RwLock::new(devices),
            used_devices: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add_device(&self, device: UsbDevice) {
        self.available_devices.write().await.push(device);
    }

    pub async fn remove_device(&self, bus_id: &str) -> Result<()> {
        let mut available_devices = self.available_devices.write().await;

        if let Some(device) = available_devices.iter().position(|d| d.bus_id == bus_id) {
            available_devices.remove(device);
            Ok(())
        } else if let Some(device) = self
            .used_devices
            .read()
            .await
            .values()
            .find(|d| d.bus_id == bus_id)
        {
            Err(std::io::Error::other(format!(
                "Device {} is in use",
                device.bus_id
            )))
        } else {
            Err(std::io::Error::new(
                ErrorKind::NotFound,
                format!("Device {bus_id} not found"),
            ))
        }
    }
}

/// Answer one isochronous `USBIP_CMD_SUBMIT` (punktfunk addition).
///
/// The wire's `iso_packet_descriptor` is `number_of_packets` × 16 bytes of big-endian
/// `offset / length / actual_length / status`. On an OUT transfer the payload for each packet lives
/// at `offset` in the URB's transfer buffer — packets are **not** necessarily contiguous, which is
/// why the offsets must be honoured rather than assuming a flat stride.
async fn handle_iso_submit(
    device: &UsbDevice,
    header: &usbip_protocol::UsbIpHeaderBasic,
    real_ep: u8,
    start_frame: u32,
    number_of_packets: u32,
    data: &[u8],
    iso_packet_descriptor: &[u8],
) -> UsbIpResponse {
    let n = number_of_packets as usize;
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);

    let mut requested = Vec::with_capacity(n);
    let mut packets = Vec::with_capacity(n);
    for i in 0..n {
        let d = &iso_packet_descriptor[i * 16..i * 16 + 16];
        let offset = be(&d[0..4]) as usize;
        let length = be(&d[4..8]);
        requested.push(length);
        // Slice the packet out of the transfer buffer, tolerating a short/absent buffer (an IN
        // transfer carries none) rather than panicking on a malformed table.
        let end = offset.saturating_add(length as usize).min(data.len());
        let payload = data.get(offset..end).unwrap_or(&[]);
        packets.push(IsoPacket {
            data: payload,
            requested_len: length as usize,
        });
    }

    match device.find_ep(real_ep) {
        None => {
            warn!("Endpoint {real_ep:02x?} not found (iso)");
            UsbIpResponse::usbip_ret_submit_fail(header)
        }
        Some((ep, intf)) => match device.handle_iso_urb(ep, intf, &packets).await {
            Ok(replies) => {
                UsbIpResponse::usbip_ret_submit_iso(header, start_frame, &requested, &replies)
            }
            Err(err) => {
                warn!("Error handling iso URB: {err}");
                UsbIpResponse::usbip_ret_submit_fail(header)
            }
        },
    }
}

/// Force a non-isochronous reply into the shape `vhci_hcd` will accept (punktfunk addition).
///
/// **A reply may be shorter than the host asked for, never longer.** A short IN transfer is
/// ordinary USB — the device had less to say — but an over-long one is a babble condition and the
/// kernel does not forgive it: `usbip_recv_xbuff()` compares the reply's `actual_length` against
/// the URB's `transfer_buffer_length` and, on `>`, treats it as a malicious packet, logs
/// `recv xbuf, 0` (that `0` is the untouched initialiser, not a byte count) and raises
/// `VDEV_EVENT_ERROR_TCP` — which tears down the **whole connection**, so the device disappears
/// rather than one URB failing. Real hardware truncates here, so we do too: a handler bug then
/// costs one wrong reply instead of the pad.
///
/// An OUT transfer returns no payload at all. `usbip_recv_xbuff()` returns early for
/// `usb_pipeout`, so any payload appended to an OUT reply is bytes the kernel never reads — and
/// every byte after it in the stream is then misframed. (Its `actual_length` is another matter:
/// that must still count the bytes accepted, see
/// [`UsbIpResponse::usbip_ret_submit_out_success`].)
///
/// Field-diagnosed 2026-08-17: a 42-byte DualSense calibration report answering a 41-byte request
/// killed `hid-playstation`'s probe with `-EPROTO` and took the controller with it. Every backend
/// that is not USB (uhid, Windows `hidclass`) truncates silently, which is why the same constant
/// had looked correct for months.
pub(crate) fn clamp_reply(mut resp: Vec<u8>, requested: u32, out: bool) -> Vec<u8> {
    if out {
        resp.clear();
    } else if resp.len() > requested as usize {
        resp.truncate(requested as usize);
    }
    resp
}

pub async fn handler<T: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut socket: &mut T,
    server: Arc<UsbIpServer>,
) -> Result<()> {
    let mut current_import_device_id: Option<String> = None;
    loop {
        let command = UsbIpCommand::read_from_socket(&mut socket).await;
        if let Err(err) = command {
            if let Some(dev_id) = current_import_device_id {
                let mut used_devices = server.used_devices.write().await;
                let mut available_devices = server.available_devices.write().await;
                match used_devices.remove(&dev_id) {
                    Some(dev) => available_devices.push(dev),
                    None => unreachable!(),
                }
            }

            if err.kind() == ErrorKind::UnexpectedEof {
                info!("Remote closed the connection");
                return Ok(());
            } else {
                return Err(err);
            }
        }

        let used_devices = server.used_devices.read().await;
        let mut current_import_device = current_import_device_id
            .clone()
            .and_then(|ref id| used_devices.get(id));

        match command.unwrap() {
            UsbIpCommand::OpReqDevlist { .. } => {
                trace!("Got OP_REQ_DEVLIST");
                let devices = server.available_devices.read().await;

                // OP_REP_DEVLIST
                UsbIpResponse::op_rep_devlist(&devices)
                    .write_to_socket(socket)
                    .await?;
                trace!("Sent OP_REP_DEVLIST");
            }
            UsbIpCommand::OpReqImport { busid, .. } => {
                trace!("Got OP_REQ_IMPORT");

                current_import_device_id = None;
                current_import_device = None;
                std::mem::drop(used_devices);

                let mut used_devices = server.used_devices.write().await;
                let mut available_devices = server.available_devices.write().await;
                let busid_compare =
                    &busid[..busid.iter().position(|&x| x == 0).unwrap_or(busid.len())];
                for (i, dev) in available_devices.iter().enumerate() {
                    if busid_compare == dev.bus_id.as_bytes() {
                        let dev = available_devices.remove(i);
                        let dev_id = dev.bus_id.clone();
                        used_devices.insert(dev.bus_id.clone(), dev);
                        current_import_device_id = dev_id.clone().into();
                        current_import_device = Some(used_devices.get(&dev_id).unwrap());
                        break;
                    }
                }

                let res = if let Some(dev) = current_import_device {
                    UsbIpResponse::op_rep_import_success(dev)
                } else {
                    UsbIpResponse::op_rep_import_fail()
                };
                res.write_to_socket(socket).await?;
                trace!("Sent OP_REP_IMPORT");
            }
            UsbIpCommand::UsbIpCmdSubmit {
                mut header,
                transfer_buffer_length,
                start_frame,
                number_of_packets,
                setup,
                data,
                iso_packet_descriptor,
                ..
            } => {
                trace!("Got USBIP_CMD_SUBMIT");
                let device = current_import_device.unwrap();

                let out = header.direction == 0;
                let real_ep = if out { header.ep } else { header.ep | 0x80 };

                header.command = USBIP_RET_SUBMIT.into();

                // Isochronous URBs carry a packet table and must be answered packet-by-packet
                // (punktfunk addition — upstream dropped the table, which stalls USB audio).
                // `0xFFFFFFFF` is the kernel's documented "not ISO" sentinel; the real
                // implementation sends 0, and `read_from_socket` treats both as no table.
                if !iso_packet_descriptor.is_empty() {
                    let res = handle_iso_submit(
                        device,
                        &header,
                        real_ep as u8,
                        start_frame,
                        number_of_packets,
                        &data,
                        &iso_packet_descriptor,
                    )
                    .await;
                    res.write_to_socket(socket).await?;
                    trace!("Sent USBIP_RET_SUBMIT (iso)");
                    continue;
                }

                let res = match device.find_ep(real_ep as u8) {
                    None => {
                        warn!("Endpoint {real_ep:02x?} not found");
                        UsbIpResponse::usbip_ret_submit_fail(&header)
                    }
                    Some((ep, intf)) => {
                        trace!("->Endpoint {ep:02x?}");
                        trace!("->Setup {setup:02x?}");
                        trace!("->Request {data:02x?}");
                        let resp = device
                            .handle_urb(
                                ep,
                                intf,
                                transfer_buffer_length,
                                SetupPacket::parse(&setup),
                                &data,
                            )
                            .await;

                        match resp {
                            Ok(resp) => {
                                let over = resp.len() > transfer_buffer_length as usize;
                                let resp = clamp_reply(resp, transfer_buffer_length, out);
                                if over {
                                    warn!(
                                        "handler returned more than the {transfer_buffer_length}-byte \
                                         request on ep {real_ep:02x?} — truncated; an over-long reply \
                                         tears down the whole usbip connection"
                                    );
                                }
                                if out {
                                    trace!("<-Wrote {}", data.len());
                                    // Acknowledge the bytes we took, not the (empty) reply:
                                    // `actual_length` is what `write()` on the device node
                                    // returns to the process that wrote it. (punktfunk fix —
                                    // upstream said 0 here, and winebus read that as failure.)
                                    UsbIpResponse::usbip_ret_submit_out_success(
                                        &header,
                                        data.len() as u32,
                                    )
                                } else {
                                    trace!("<-Resp {resp:02x?}");
                                    UsbIpResponse::usbip_ret_submit_success(
                                        &header,
                                        0,
                                        0,
                                        resp,
                                        vec![],
                                    )
                                }
                            }
                            Err(err) => {
                                warn!("Error handling URB: {err}");
                                UsbIpResponse::usbip_ret_submit_fail(&header)
                            }
                        }
                    }
                };
                res.write_to_socket(socket).await?;
                trace!("Sent USBIP_RET_SUBMIT");
            }
            UsbIpCommand::UsbIpCmdUnlink {
                mut header,
                unlink_seqnum,
            } => {
                trace!("Got USBIP_CMD_UNLINK for {unlink_seqnum:10x?}");

                header.command = USBIP_RET_UNLINK.into();

                let res = UsbIpResponse::usbip_ret_unlink_success(&header);
                res.write_to_socket(socket).await?;
                trace!("Sent USBIP_RET_UNLINK");
            }
        }
    }
}

/// Spawn a USB/IP server at `addr` using [TcpListener]
pub async fn server(addr: SocketAddr, server: Arc<UsbIpServer>) {
    let listener = TcpListener::bind(addr).await.expect("bind to addr");

    let server = async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, _addr)) => {
                    info!("Got connection from {:?}", socket.peer_addr());
                    let new_server = server.clone();
                    tokio::spawn(async move {
                        let res = handler(&mut socket, new_server).await;
                        info!("Handler ended with {res:?}");
                    });
                }
                Err(err) => {
                    warn!("Got error {err:?}");
                }
            }
        }
    };

    server.await
}

// (Host-mode constructors and in-crate tests removed in the vendored copy — see NOTICE.)

/// Covers only the punktfunk reply-shaping addition; see [`clamp_reply`] for why the kernel treats
/// an over-long reply as fatal to the connection rather than to the URB.
#[cfg(test)]
mod clamp_tests {
    use super::clamp_reply;

    /// The exact 2026-08-17 field failure: a 42-byte calibration blob against `wLength` 41.
    /// Un-truncated this is `actual_length = 42 > transfer_buffer_length = 41`, which makes
    /// `usbip_recv_xbuff()` raise `VDEV_EVENT_ERROR_TCP` and disconnect the pad entirely.
    #[test]
    fn an_over_long_in_reply_is_truncated_to_the_request() {
        let reply = vec![0xAB; 42];
        assert_eq!(clamp_reply(reply, 41, false).len(), 41);
    }

    /// A device returning less than asked is a short packet — ordinary USB, and the host is told
    /// the true count. Padding it out would fabricate data the device never sent.
    #[test]
    fn a_short_in_reply_is_left_alone() {
        assert_eq!(clamp_reply(vec![1, 2, 3], 64, false), vec![1, 2, 3]);
    }

    /// An OUT reply carries no payload back whatever the handler returns: the kernel does not read
    /// one, so those bytes would stay in the stream and misframe every PDU after them.
    #[test]
    fn an_out_reply_never_carries_a_payload() {
        assert!(clamp_reply(vec![1, 2, 3, 4], 4, true).is_empty());
        assert!(clamp_reply(vec![], 0, true).is_empty());
    }
}
