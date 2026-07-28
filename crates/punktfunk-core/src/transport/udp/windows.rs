//! Windows batched UDP send: `WSASendMsg` UDP Send Offload (USO). The platform body of
//! [`super::UdpTransport`]'s `send_gso` override, plus the standalone [`send_uso_all`].

use super::{is_transient_io, UdpTransport};
use crate::transport::Transport;

/// Windows UDP Send Offload (USO) enable state (process-wide). The Windows analogue of Linux UDP
/// GSO: `WSASendMsg` + `UDP_SEND_MSG_SIZE`. **On by default** (the 1 Gbps+ send lever — Windows
/// otherwise does one `send` syscall per packet, which caps throughput at high packet rates). Kill
/// switch `PUNKTFUNK_GSO=0`; auto-fallback latches it off the first time a send reports it
/// unsupported (old OS / NIC / path). We detect support from the send error rather than a
/// `setsockopt` probe — the probe sets a socket-wide default segment size that would fragment plain
/// `send`s of larger-than-segment packets.
#[cfg(target_os = "windows")]
mod uso {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 = uninit, 1 = on, 2 = off

    pub fn active() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let off = std::env::var_os("PUNKTFUNK_GSO")
                    .map(|v| v == "0")
                    .unwrap_or(false);
                STATE.store(if off { 2 } else { 1 }, Ordering::Relaxed);
                tracing::info!(
                    enabled = !off,
                    "Windows UDP Send Offload (USO) resolved (the 1 Gbps+ send lever; PUNKTFUNK_GSO=0 disables)"
                );
                !off
            }
        }
    }
    /// Latch USO off for the process after a send that means it isn't usable on this OS/NIC/path.
    pub fn disable() {
        if STATE.swap(2, Ordering::Relaxed) != 2 {
            tracing::warn!(
                "Windows USO unsupported on this path — falling back to per-packet sends"
            );
        }
    }
}

/// True if a `WSASendMsg` USO error means USO isn't usable here (vs a transient full-buffer
/// `WouldBlock`, handled by [`is_transient_io`]) — latch it off and fall back to per-packet sends.
/// 10022 WSAEINVAL, 10042 WSAENOPROTOOPT, 10045 WSAEOPNOTSUPP, 10040 WSAEMSGSIZE.
#[cfg(target_os = "windows")]
fn uso_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(10022) | Some(10042) | Some(10045) | Some(10040)
    )
}

/// One `WSASendMsg` carrying a `UDP_SEND_MSG_SIZE` control message: Winsock splits `buf` (a
/// back-to-back concatenation of equal-size datagrams, only the final one allowed shorter) into
/// `seg_size`-byte UDP datagrams to the connected peer in ONE syscall — the analogue of
/// `send_one_gso`. The `WSA_CMSG_*` helpers are C macros not exported by the `windows` crate, so
/// the cmsg layout math is reimplemented here (ported from quinn-udp's Windows backend).
#[cfg(target_os = "windows")]
fn send_one_uso(socket: &std::net::UdpSocket, buf: &[u8], seg_size: u16) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        WSASendMsg, CMSGHDR, IPPROTO_UDP, UDP_SEND_MSG_SIZE, WSABUF, WSAMSG,
    };
    let align_usize = std::mem::align_of::<usize>();
    let align_hdr = std::mem::align_of::<CMSGHDR>();
    let cmsgdata_align = |n: usize| (n + align_usize - 1) & !(align_usize - 1);
    let cmsghdr_align = |n: usize| (n + align_hdr - 1) & !(align_hdr - 1);
    let hdr = std::mem::size_of::<CMSGHDR>();

    // 8-byte-aligned control buffer; 32 B holds one u32 cmsg (WSA_CMSG_SPACE(4) = 24 on x64).
    #[repr(align(8))]
    struct Aligned([u8; 32]);
    let mut ctrl = Aligned([0u8; 32]);

    let mut data = WSABUF {
        len: buf.len() as u32,
        buf: buf.as_ptr() as *mut u8, // WSASendMsg only reads it
    };
    let mut msg = WSAMSG {
        name: std::ptr::null_mut(),
        namelen: 0,
        lpBuffers: &mut data,
        dwBufferCount: 1,
        Control: WSABUF {
            len: 0,
            buf: ctrl.0.as_mut_ptr(),
        },
        dwFlags: 0,
    };
    let cmsg_len = cmsgdata_align(hdr) + std::mem::size_of::<u32>(); // WSA_CMSG_LEN(4)
    let space = cmsgdata_align(hdr + cmsghdr_align(std::mem::size_of::<u32>())); // WSA_CMSG_SPACE(4)
                                                                                 // SAFETY: `ctrl` is a local control buffer sized by `WSA_CMSG_SPACE(4)` — computed as `space`
                                                                                 // just above — so the header plus its 4-byte payload fit inside it and neither the field stores
                                                                                 // nor the unaligned data write can run past the end. `write_unaligned` is used because the
                                                                                 // payload sits at a `WSA_CMSG_DATA` offset with no alignment guarantee.
    unsafe {
        let cmsg = ctrl.0.as_mut_ptr() as *mut CMSGHDR;
        (*cmsg).cmsg_len = cmsg_len;
        (*cmsg).cmsg_level = IPPROTO_UDP;
        (*cmsg).cmsg_type = UDP_SEND_MSG_SIZE;
        let data_ptr = (cmsg as usize + cmsgdata_align(hdr)) as *mut u32;
        std::ptr::write_unaligned(data_ptr, seg_size as u32);
        msg.Control.len = space as u32;
        let mut sent = 0u32;
        let rc = WSASendMsg(
            socket.as_raw_socket() as usize,
            &msg,
            0,
            &mut sent,
            std::ptr::null_mut(),
            None,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Reusable Windows USO batch send for callers that own their OWN connected `UdpSocket` and are not
/// the [`UdpTransport`] data plane — specifically the GameStream video sender, whose paced bursts of
/// equal-size RTP/FEC packets are otherwise sent one `send` syscall at a time on Windows. Coalesces
/// the LEADING run of uniform-size packets into ≤512-segment `WSASendMsg(UDP_SEND_MSG_SIZE)`
/// super-buffers and returns how many packets it sent that way; the caller sends any remainder with
/// its own per-packet path. Returns `Ok(0)` (caller sends everything scalar) when USO is disabled
/// (`PUNKTFUNK_GSO=0`) or the packets aren't uniform-size. On a USO-unsupported error it latches USO
/// off process-wide and returns the count sent so far; a transient full-buffer also returns the
/// count-so-far. Same uniform-size rule and `seg`/512 chunking as the [`UdpTransport`] `send_gso`
/// Windows path, reusing its [`send_one_uso`] primitive.
#[cfg(target_os = "windows")]
pub fn send_uso_all(socket: &std::net::UdpSocket, packets: &[&[u8]]) -> std::io::Result<usize> {
    if packets.is_empty() || !uso::active() {
        return Ok(0);
    }
    // USO needs every segment but the last to be exactly `seg` bytes; bail to the scalar caller path
    // otherwise (a frame's final/short packet or a size-mixed burst).
    let seg = packets[0].len();
    let last = packets.len() - 1;
    if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
        return Ok(0);
    }
    let max_seg = 512usize; // Win11 x64 accepts up to ~512 segments per WSASendMsg
    let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
    let mut sent = 0usize;
    for chunk in packets.chunks(max_seg) {
        scratch.clear();
        for p in chunk {
            scratch.extend_from_slice(p);
        }
        match send_one_uso(socket, &scratch, seg as u16) {
            Ok(()) => sent += chunk.len(),
            // Send buffer momentarily full — stop here; the caller sends the rest (and the pacing
            // loop / blocking socket absorbs it). Never block or tear down here.
            Err(e) if is_transient_io(&e) => break,
            // USO unsupported on this OS/NIC/path — latch off; the caller sends the rest scalar and
            // every later burst skips USO via `uso::active()`.
            Err(e) if uso_unsupported(&e) => {
                uso::disable();
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}

#[cfg(target_os = "windows")]
pub(super) fn send_gso(t: &UdpTransport, packets: &[&[u8]]) -> std::io::Result<usize> {
    if packets.is_empty() {
        return Ok(0);
    }
    if !uso::active() {
        return t.send_batch(packets);
    }
    // USO needs every segment but the last to be exactly `seg` bytes (same as Linux GSO).
    let seg = packets[0].len();
    let last = packets.len() - 1;
    if seg == 0 || packets[..last].iter().any(|p| p.len() != seg) || packets[last].len() > seg {
        return t.send_batch(packets);
    }
    // Win11 x64 accepts up to ~512 segments per WSASendMsg.
    let max_seg = 512usize;
    let mut scratch: Vec<u8> = Vec::with_capacity(seg * packets.len().min(max_seg));
    let mut sent = 0usize;
    for chunk in packets.chunks(max_seg) {
        scratch.clear();
        for p in chunk {
            scratch.extend_from_slice(p);
        }
        match send_one_uso(&t.socket, &scratch, seg as u16) {
            Ok(()) => sent += chunk.len(),
            // Send buffer momentarily full / connected-socket ICMP blip — drop the rest, never
            // block, never tear down (see is_transient_io).
            Err(e) if is_transient_io(&e) => break,
            // USO unsupported on this OS/NIC/path — latch off and finish via scalar send_batch.
            Err(e) if uso_unsupported(&e) => {
                uso::disable();
                return Ok(sent + t.send_batch(&packets[sent..])?);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}
