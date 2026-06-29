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
                setup,
                data,
                ..
            } => {
                trace!("Got USBIP_CMD_SUBMIT");
                let device = current_import_device.unwrap();

                let out = header.direction == 0;
                let real_ep = if out { header.ep } else { header.ep | 0x80 };

                header.command = USBIP_RET_SUBMIT.into();

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
                                if out {
                                    trace!("<-Wrote {}", data.len());
                                } else {
                                    trace!("<-Resp {resp:02x?}");
                                }
                                UsbIpResponse::usbip_ret_submit_success(&header, 0, 0, resp, vec![])
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
