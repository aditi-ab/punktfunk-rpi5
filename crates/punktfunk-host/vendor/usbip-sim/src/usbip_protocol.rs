//! USB/IP protocol structs
//!
//! This module contains declarations of all structs used in the USB/IP protocol,
//! as well as functions to serialize and deserialize them to/from byte arrays,
//! and functions to send and receive them over a socket.
//!
//! They are based on the [Linux kernel documentation](https://docs.kernel.org/usb/usbip_protocol.html).

use log::trace;
use std::io::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::UsbDevice;

/// USB/IP protocol version
///
/// This is currently the only supported version of USB/IP
/// for this library.
pub const USBIP_VERSION: u16 = 0x0111;

/// Command code: Retrieve the list of exported USB devices
pub const OP_REQ_DEVLIST: u16 = 0x8005;
/// Command code: import a remote USB device
pub const OP_REQ_IMPORT: u16 = 0x8003;
/// Reply code: The list of exported USB devices
pub const OP_REP_DEVLIST: u16 = 0x0005;
/// Reply code: Reply to import
pub const OP_REP_IMPORT: u16 = 0x0003;

/// Command code: Submit an URB
pub const USBIP_CMD_SUBMIT: u16 = 0x0001;
/// Command code: Unlink an URB
pub const USBIP_CMD_UNLINK: u16 = 0x0002;
/// Reply code: Reply for submitting an URB
pub const USBIP_RET_SUBMIT: u16 = 0x0003;
/// Reply code: Reply for URB unlink
pub const USBIP_RET_UNLINK: u16 = 0x0004;

/// USB/IP direction
///
/// NOTE: Must not be confused with rusb::Direction,
/// which has the opposite enum values. This is only for
/// internal use in the USB/IP protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Out = 0,
    In = 1,
}

/// Common header for all context sensitive packets
///
/// All commands/responses which rely on a device being attached
/// to a client use this header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UsbIpHeaderBasic {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl UsbIpHeaderBasic {
    /// Converts a byte array into a [UsbIpHeaderBasic].
    pub fn from_bytes(bytes: &[u8; 20]) -> Self {
        let result = UsbIpHeaderBasic {
            command: u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
            seqnum: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            devid: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            direction: u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
            ep: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        };
        // The direction should be 0 or 1
        debug_assert!(result.direction & 1 == result.direction);
        result
    }

    /// Converts the [UsbIpHeaderBasic] into a byte array.
    pub fn to_bytes(&self) -> [u8; 20] {
        let mut result = [0u8; 20];
        result[0..4].copy_from_slice(&self.command.to_be_bytes());
        result[4..8].copy_from_slice(&self.seqnum.to_be_bytes());
        result[8..12].copy_from_slice(&self.devid.to_be_bytes());
        result[12..16].copy_from_slice(&self.direction.to_be_bytes());
        result[16..20].copy_from_slice(&self.ep.to_be_bytes());
        result
    }

    pub(crate) async fn read_from_socket_with_command<T: AsyncReadExt + Unpin>(
        socket: &mut T,
        command: u16,
    ) -> Result<Self> {
        let seqnum = socket.read_u32().await?;
        let devid = socket.read_u32().await?;
        let direction = socket.read_u32().await?;
        // The direction should be 0 or 1
        debug_assert!(direction & 1 == direction);
        let ep = socket.read_u32().await?;

        Ok(UsbIpHeaderBasic {
            command: command.into(),
            seqnum,
            devid,
            direction,
            ep,
        })
    }
}

/// Client side commands from the Virtual Host Controller
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum UsbIpCommand {
    OpReqDevlist {
        status: u32,
    },
    OpReqImport {
        status: u32,
        busid: [u8; 32],
    },
    UsbIpCmdSubmit {
        header: UsbIpHeaderBasic,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup: [u8; 8],
        data: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpCmdUnlink {
        header: UsbIpHeaderBasic,
        unlink_seqnum: u32,
    },
}

impl UsbIpCommand {
    /// Constructs a [UsbIpCommand] from a socket
    ///
    /// This will consume a variable amount of bytes from the socket.
    /// It might fail if the bytes does not follow the USB/IP protocol properly.
    pub async fn read_from_socket<T: AsyncReadExt + Unpin>(socket: &mut T) -> Result<UsbIpCommand> {
        let version: u16 = socket.read_u16().await?;

        if version != 0 && version != USBIP_VERSION {
            return Err(std::io::Error::other(format!(
                "Unknown version: {version:#04X}"
            )));
        }

        let command: u16 = socket.read_u16().await?;

        trace!(
            "Received command: {:#04X} ({}), parsing...",
            command,
            match command {
                OP_REQ_DEVLIST => "OP_REQ_DEVLIST",
                OP_REQ_IMPORT => "OP_REQ_IMPORT",
                USBIP_CMD_SUBMIT => "USBIP_CMD_SUBMIT",
                USBIP_CMD_UNLINK => "USBIP_CMD_UNLINK",
                _ => "Unknown",
            }
        );

        match command {
            OP_REQ_DEVLIST => {
                let status = socket.read_u32().await?;
                debug_assert!(status == 0);

                Ok(UsbIpCommand::OpReqDevlist { status })
            }
            OP_REQ_IMPORT => {
                let status = socket.read_u32().await?;
                debug_assert!(status == 0);
                let mut busid = [0; 32];
                socket.read_exact(&mut busid).await?;

                Ok(UsbIpCommand::OpReqImport { status, busid })
            }
            USBIP_CMD_SUBMIT => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_SUBMIT)
                        .await?;
                let transfer_flags = socket.read_u32().await?;
                let transfer_buffer_length = socket.read_u32().await?;
                let start_frame = socket.read_u32().await?;
                let number_of_packets = socket.read_u32().await?;
                let interval = socket.read_u32().await?;

                let mut setup = [0; 8];
                socket.read_exact(&mut setup).await?;

                let data = if header.direction == Direction::In as u32 {
                    vec![]
                } else {
                    let mut data = vec![0; transfer_buffer_length as usize];
                    socket.read_exact(&mut data).await?;
                    data
                };

                // The kernel docs specifies that this should be set to 0xFFFFFFFF for all
                // non-ISO packets, however the actual implementation resorts to 0x00000000
                // https://stackoverflow.com/questions/76899798/usb-ip-what-is-the-size-of-the-iso-packet-descriptor
                let iso_packet_descriptor =
                    if number_of_packets != 0 && number_of_packets != 0xFFFFFFFF {
                        let mut result = vec![0; 16 * number_of_packets as usize];
                        socket.read_exact(&mut result).await?;
                        result
                    } else {
                        vec![]
                    };

                Ok(UsbIpCommand::UsbIpCmdSubmit {
                    header,
                    transfer_flags,
                    transfer_buffer_length,
                    start_frame,
                    number_of_packets,
                    interval,
                    setup,
                    data,
                    iso_packet_descriptor,
                })
            }
            USBIP_CMD_UNLINK => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_UNLINK)
                        .await?;
                let unlink_seqnum = socket.read_u32().await?;

                let mut _padding = [0; 24];
                socket.read_exact(&mut _padding).await?;

                Ok(UsbIpCommand::UsbIpCmdUnlink {
                    header,
                    unlink_seqnum,
                })
            }
            _ => Err(std::io::Error::other(format!(
                "Unknown command: {command:#04X}"
            ))),
        }
    }

    /// Converts the [UsbIpCommand] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            UsbIpCommand::OpReqDevlist { status } => {
                let mut result = Vec::with_capacity(8);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result
            }
            UsbIpCommand::OpReqImport { status, busid } => {
                let mut result = Vec::with_capacity(40);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&busid);
                result
            }
            UsbIpCommand::UsbIpCmdSubmit {
                ref header,
                transfer_flags,
                transfer_buffer_length,
                start_frame,
                number_of_packets,
                interval,
                setup,
                ref data,
                ref iso_packet_descriptor,
            } => {
                debug_assert!(
                    header.direction != Direction::Out as u32
                        || transfer_buffer_length == data.len() as u32
                );

                let mut result = Vec::with_capacity(48 + data.len() + iso_packet_descriptor.len());
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&transfer_flags.to_be_bytes());
                result.extend_from_slice(&transfer_buffer_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&interval.to_be_bytes());
                result.extend_from_slice(&setup);
                result.extend_from_slice(data);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            UsbIpCommand::UsbIpCmdUnlink {
                ref header,
                unlink_seqnum,
            } => {
                let mut result = Vec::with_capacity(48);
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&unlink_seqnum.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }
}

/// Server side responses from the USB Host
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum UsbIpResponse {
    OpRepDevlist {
        status: u32,
        device_count: u32,
        devices: Vec<UsbDevice>,
    },
    OpRepImport {
        status: u32,
        device: Option<UsbDevice>,
    },
    UsbIpRetSubmit {
        header: UsbIpHeaderBasic,
        status: u32,
        actual_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        error_count: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpRetUnlink {
        header: UsbIpHeaderBasic,
        status: u32,
    },
}

impl UsbIpResponse {
    /// Converts the [UsbIpResponse] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            Self::OpRepDevlist {
                status,
                device_count,
                ref devices,
            } => {
                let mut result = Vec::with_capacity(
                    12 + devices.len() * 312
                        + devices
                            .iter()
                            .map(|d| d.interfaces.len() * 4)
                            .sum::<usize>(),
                );
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&device_count.to_be_bytes());
                for dev in devices {
                    result.extend_from_slice(&dev.to_bytes_with_interfaces());
                }
                result
            }
            Self::OpRepImport { status, ref device } => {
                let mut result = Vec::with_capacity(320);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                if let Some(device) = device {
                    result.extend_from_slice(&device.to_bytes());
                }
                result
            }
            Self::UsbIpRetSubmit {
                ref header,
                status,
                actual_length,
                start_frame,
                number_of_packets,
                error_count,
                ref transfer_buffer,
                ref iso_packet_descriptor,
            } => {
                let mut result =
                    Vec::with_capacity(48 + transfer_buffer.len() + iso_packet_descriptor.len());

                debug_assert!(header.command == USBIP_RET_SUBMIT.into());
                debug_assert!(if header.direction == Direction::In as u32 {
                    actual_length == transfer_buffer.len() as u32
                } else {
                    actual_length == 0
                });

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&actual_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&error_count.to_be_bytes());
                result.extend_from_slice(&[0; 8]);
                result.extend_from_slice(transfer_buffer);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            Self::UsbIpRetUnlink { ref header, status } => {
                let mut result = Vec::with_capacity(48);

                debug_assert!(header.command == USBIP_RET_UNLINK.into());

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }

    pub async fn write_to_socket<T: AsyncWriteExt + Unpin>(&self, socket: &mut T) -> Result<()> {
        socket.write_all(&self.to_bytes()).await
    }

    /// Constructs a OP_REP_DEVLIST response
    pub fn op_rep_devlist(devices: &[UsbDevice]) -> Self {
        Self::OpRepDevlist {
            status: 0,
            device_count: devices.len() as u32,
            devices: devices.to_vec(),
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn op_rep_import_success(device: &UsbDevice) -> Self {
        Self::OpRepImport {
            status: 0,
            device: Some(device.clone()),
        }
    }

    /// Constructs a failed OP_REP_IMPORT response
    pub fn op_rep_import_fail() -> Self {
        Self::OpRepImport {
            status: 1,
            device: None,
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn usbip_ret_submit_success(
        header: &UsbIpHeaderBasic,
        start_frame: u32,
        number_of_packets: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    ) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 0,
            actual_length: transfer_buffer.len() as u32,
            start_frame,
            number_of_packets,
            error_count: 0,
            transfer_buffer,
            iso_packet_descriptor,
        }
    }

    /// Constructs a failed OP_REP_IMPORT response
    pub fn usbip_ret_submit_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 1,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            transfer_buffer: vec![],
            iso_packet_descriptor: vec![],
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn usbip_ret_unlink_success(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: 0,
        }
    }

    /// Constructs a failed OP_REP_IMPORT response.
    pub fn usbip_ret_unlink_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: 1,
        }
    }
}

// (In-crate test module removed in the vendored copy — see NOTICE.)
