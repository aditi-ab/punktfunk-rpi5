//! The pyrowave-sys calls both PyroWave encoders make the same way: the status check and the
//! packetize step that turns an encoded frame into codec packets for [`crate::pyrowave_wire`].

use anyhow::{bail, Result};
use pyrowave_sys as pw;

pub fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

/// Packetize the frame `enc` last encoded into `bitstream` (resized to `cap`), at the boundary
/// `wire_chunk` implies, and stamp the colour bits on the first packet. `(offset, size)` per
/// packet. Dense mode (`None`) is exactly one packet.
///
/// # Safety
/// `enc` is a live encoder whose last encode has completed.
pub unsafe fn packetize(
    enc: pw::pyrowave_encoder,
    bitstream: &mut Vec<u8>,
    cap: usize,
    wire_chunk: Option<usize>,
    pq: bool,
) -> Result<Vec<(usize, usize)>> {
    bitstream.resize(cap, 0);
    // Chunked mode reserves the 4-byte window prefix from the packetize boundary.
    let boundary = crate::pyrowave_wire::packet_boundary(wire_chunk, cap);
    let mut n: usize = 0;
    // SAFETY: the caller's contract; `n` outlives the call.
    let r = unsafe { pw::pyrowave_encoder_compute_num_packets(enc, boundary, &mut n) };
    pw_check(r, "compute_num_packets")?;
    if n == 0 || (wire_chunk.is_none() && n != 1) {
        bail!("pyrowave: unexpected packet count {n} at boundary {boundary}");
    }
    let mut packets = vec![pw::pyrowave_packet { offset: 0, size: 0 }; n];
    let mut out_n: usize = 0;
    // SAFETY: the caller's contract; `packets` holds the `n` entries the encoder asked for and
    // `bitstream` the `cap` bytes it is told about.
    let r = unsafe {
        pw::pyrowave_encoder_packetize(
            enc,
            packets.as_mut_ptr(),
            boundary,
            &mut out_n,
            bitstream.as_mut_ptr() as *mut std::ffi::c_void,
            cap,
        )
    };
    pw_check(r, "packetize")?;
    packets.truncate(out_n.max(1));
    // Pyrowave's C API signals FULL range and centred siting; both CSCs emit limited-range,
    // left-sited codes (BT.2020/PQ when `pq`). Stamp them so VUI-honouring clients keep blacks.
    if let Some(p) = packets.first() {
        crate::pyrowave_wire::stamp_color_bits(bitstream, p.offset, pq);
    }
    Ok(packets.iter().map(|p| (p.offset, p.size)).collect())
}
