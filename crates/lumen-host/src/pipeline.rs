//! The host hot path (plan §7), wiring the platform stages to `lumen_core`:
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
use lumen_core::packet::{FLAG_PIC, FLAG_SOF};
use lumen_core::Session;

/// Drive one capture→encode→submit step. The real pipeline spawns threads and uses
/// bounded channels; this documents the data flow and the `lumen_core` submit contract.
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
    }) = encoder.poll()?
    {
        let mut flags = FLAG_PIC as u32;
        if keyframe {
            flags |= FLAG_SOF as u32;
        }
        // core does FEC + packetize + pace + send.
        session.submit_frame(&data, pts_ns, flags)?;
    }
    Ok(())
}
