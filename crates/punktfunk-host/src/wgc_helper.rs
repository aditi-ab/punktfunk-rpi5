//! USER-session WGC helper (Windows) — part of the two-process secure-desktop design
//! (docs/windows-secure-desktop.md).
//!
//! WGC won't activate under the SYSTEM account, but the host must run as SYSTEM for the secure
//! desktop. So the SYSTEM host spawns THIS helper in the interactive user session
//! (`CreateProcessAsUserW`) to do the WGC capture + NVENC encode that needs the user token, and the
//! helper ships the encoded Annex-B access units back over its **stdout** pipe (which the host
//! inherits + reads). The host relays them on the live QUIC session while the normal desktop is up,
//! and switches to its own DDA encoder on the secure desktop. The helper captures the SAME SudoVDA
//! output **by GDI name only** — it never creates a virtual output / touches display topology (a
//! second topology owner would re-trigger the ACCESS_LOST born-lost storm).
//!
//! Wire framing on stdout, per AU: `[u32 len LE][u64 pts_ns LE][u8 keyframe][len bytes data]`.

use crate::capture::{dxgi::WinCaptureTarget, wgc::WgcCapturer, Capturer};
use crate::encode::{self, Codec};
use anyhow::{Context, Result};
use std::io::Write;

pub struct HelperOptions {
    pub target_id: u32,
    pub gdi_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// AU framing magic + version, so the host can resync / detect a helper crash on its stdout stream.
const AU_MAGIC: u32 = 0x5046_4155; // "PFAU"

pub fn run(opts: HelperOptions) -> Result<()> {
    tracing::info!(
        target_id = opts.target_id,
        gdi = %opts.gdi_name,
        mode = format!("{}x{}@{}", opts.width, opts.height, opts.fps),
        "WGC helper starting (user session)"
    );

    // Capture the EXISTING SudoVDA output by GDI name / target id — do NOT create one (the host owns
    // the virtual output + its isolate/restore; a second topology owner breaks DDA recovery).
    let target = WinCaptureTarget {
        adapter_luid: 0,
        gdi_name: opts.gdi_name.clone(),
        target_id: opts.target_id,
    };
    let mut cap =
        WgcCapturer::open(target, Some((opts.width, opts.height, opts.fps))).context("WGC open")?;
    cap.set_active(true);

    // First frame establishes the real dimensions + whether the desktop is HDR (the encoder derives
    // Main10/HDR from the frame's PixelFormat::Rgb10a2). Then open NVENC on the capture device.
    let first = cap.next_frame().context("first WGC frame")?;
    let (w, h) = (first.width, first.height);
    let mut enc = encode::open_video(
        Codec::H265,
        first.format,
        w,
        h,
        opts.fps,
        opts.bitrate_kbps as u64 * 1000,
        false, // not cuda
        8,     // bit depth: HDR auto-upgrades to Main10 from the Rgb10a2 frame
    )
    .context("open NVENC")?;

    // Binary stdout — lock it once + write framed AUs. A short write / broken pipe means the host
    // (parent) went away → exit cleanly so the host's relaunch watchdog can respawn us.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut frame = first;
    loop {
        enc.submit(&frame).context("encoder submit")?;
        while let Some(au) = enc.poll().context("encoder poll")? {
            if write_au(&mut out, &au).is_err() {
                tracing::info!("WGC helper: stdout closed (host gone) — exiting");
                return Ok(());
            }
        }
        frame = cap.next_frame().context("WGC next frame")?;
    }
}

fn write_au(out: &mut impl Write, au: &encode::EncodedFrame) -> std::io::Result<()> {
    out.write_all(&AU_MAGIC.to_le_bytes())?;
    out.write_all(&(au.data.len() as u32).to_le_bytes())?;
    out.write_all(&au.pts_ns.to_le_bytes())?;
    out.write_all(&[au.keyframe as u8])?;
    out.write_all(&au.data)?;
    out.flush()
}
