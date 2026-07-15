//! The host hot path (plan §7), wiring the platform stages to `punktfunk_core`:
//!
//! ```text
//! capture(dmabuf) → encode(NVENC/VAAPI) → core[FEC+packetize+pace+send]
//! ```
//!
//! Each stage runs on its own native OS thread, connected by bounded SPSC channels with
//! drop-oldest on overflow so the encoder is never blocked. No async runtime here.

use crate::capture::Capturer;
use crate::encode::{EncodedFrame, Encoder};
use anyhow::Result;
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF};
use punktfunk_core::Session;

/// Drive one capture→encode→submit step. The real pipeline spawns threads and uses
/// bounded channels; this documents the data flow and the `punktfunk_core` submit contract.
pub fn pump_once(
    capturer: &mut dyn Capturer,
    encoder: &mut dyn Encoder,
    session: &mut Session,
) -> Result<()> {
    let frame = capturer.next_frame()?;
    encoder.submit(&frame)?;
    while let Some(EncodedFrame {
        data,
        pts_ns,
        keyframe,
        recovery_anchor,
        chunk_aligned: _,
    }) = encoder.poll()?
    {
        let mut flags = FLAG_PIC as u32;
        if keyframe {
            flags |= FLAG_SOF as u32;
        }
        if recovery_anchor {
            flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
        }
        // core does FEC + packetize + pace + send.
        session.submit_frame(&data, pts_ns, flags)?;
    }
    Ok(())
}
