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
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct HelperOptions {
    pub target_id: u32,
    pub gdi_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Negotiated encode bit depth (8, or 10 = HEVC Main10). HDR auto-upgrades to 10 from the
    /// captured frame's `Rgb10a2` format regardless.
    pub bit_depth: u8,
}

/// AU framing magic + version, so the host can resync / detect a helper crash on its stdout stream.
const AU_MAGIC: u32 = 0x5046_4155; // "PFAU"

/// Control byte the host writes on our stdin to force the next frame to be an IDR. Must match
/// `wgc_relay::CTL_KEYFRAME`.
const CTL_KEYFRAME: u8 = 0x01;

pub fn run(opts: HelperOptions) -> Result<()> {
    tracing::info!(
        target_id = opts.target_id,
        gdi = %opts.gdi_name,
        mode = format!("{}x{}@{}", opts.width, opts.height, opts.fps),
        "WGC helper starting (user session)"
    );

    // This thread does WGC capture + video-processor convert + NVENC submit — the GPU-submitting hot
    // path. Elevate its OS priority so a CPU-heavy game can't deschedule it and delay submission (which
    // would leave our HIGH GPU priority with nothing queued to prioritise). Apollo's capture thread is
    // likewise CRITICAL.
    crate::m3::boost_thread_priority(true);

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
        false,          // not cuda
        opts.bit_depth, // 8, or 10 = Main10 (HDR auto-upgrades from the Rgb10a2 frame regardless)
    )
    .context("open NVENC")?;

    // Control channel: the host writes a single byte on our stdin to force an IDR (client decode
    // recovery), mirroring `enc.request_keyframe()` in the single-process path. A reader thread sets
    // a flag the encode loop checks; stdin EOF (host gone) just stops the thread.
    let kf = Arc::new(AtomicBool::new(false));
    {
        let kf = kf.clone();
        std::thread::Builder::new()
            .name("wgc-helper-ctl".into())
            .spawn(move || {
                let mut stdin = std::io::stdin();
                let mut byte = [0u8; 1];
                while let Ok(n) = stdin.read(&mut byte) {
                    if n == 0 {
                        break; // host closed our stdin
                    }
                    if byte[0] == CTL_KEYFRAME {
                        kf.store(true, Ordering::Relaxed);
                    }
                }
            })
            .ok();
    }

    // Binary stdout — lock it once + write framed AUs. A short write / broken pipe means the host
    // (parent) went away → exit cleanly so the host's relaunch watchdog can respawn us.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Encode pipeline depth. The loop keeps DEPTH frames in flight so per-frame GPU-scheduling waits
    // can overlap. NOTE: depth > 1 was measured to REGRESS under a GPU-saturating game — the encodes
    // serialize on the contended GPU anyway, so a deeper queue just stacks latency (≈ depth × frame
    // time) without raising throughput. Default 1 (the validated-best); `PUNKTFUNK_ENCODE_DEPTH` (1..=6)
    // can raise it if a future workload is genuinely encode-throughput-bound rather than scheduling-bound.
    let depth: usize = std::env::var("PUNKTFUNK_ENCODE_DEPTH")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&d| (1..=6).contains(&d))
        .unwrap_or(1);
    tracing::info!(depth, "WGC helper: encode pipeline depth");

    let perf = std::env::var_os("PUNKTFUNK_PERF").is_some();
    let mut frames = 0u64;
    let mut cap_wait_ns = 0u64;
    let mut encode_ns = 0u64; // time blocked in lock_bitstream (the oldest in-flight encode)
    let mut write_ns = 0u64; // time blocked writing the AU to the stdout pipe (relay backpressure)
    let mut window = std::time::Instant::now();

    // Prime: submit `depth` frames before the first poll so NVENC has that many encodes in flight.
    // We don't hold the `CapturedFrame`s past `submit`: NVENC keeps its own registered texture clone
    // and the capturer's ring/held-set own the canonical refs (sized for `depth`), so the in-flight
    // inputs stay valid after our clones drop.
    enc.submit(&first).context("first encoder submit")?;
    drop(first);
    for _ in 1..depth {
        let f = cap.next_frame().context("WGC prime frame")?;
        enc.submit(&f).context("prime encoder submit")?;
    }
    loop {
        if kf.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }
        // Pop + forward the OLDEST in-flight frame (FIFO). With `depth` outstanding it has had
        // depth-1 frames' worth of GPU slots to finish, so this rarely blocks under load.
        let p0 = std::time::Instant::now();
        let polled = enc.poll().context("encoder poll")?;
        if perf {
            encode_ns += p0.elapsed().as_nanos() as u64;
        }
        if let Some(au) = polled {
            let w0 = std::time::Instant::now();
            let wrote = write_au(&mut out, &au);
            if perf {
                write_ns += w0.elapsed().as_nanos() as u64;
            }
            if wrote.is_err() {
                tracing::info!("WGC helper: stdout closed (host gone) — exiting");
                return Ok(());
            }
        }
        // Refill: capture + submit to keep `depth` frames in flight.
        let t0 = std::time::Instant::now();
        let next = cap.next_frame().context("WGC next frame")?;
        if perf {
            cap_wait_ns += t0.elapsed().as_nanos() as u64;
        }
        enc.submit(&next).context("encoder submit")?;

        if perf {
            frames += 1;
            let since = window.elapsed();
            if since.as_secs() >= 2 {
                let secs = since.as_secs_f64();
                let per = |ns: u64| format!("{:.2}", ns as f64 / frames as f64 / 1e6);
                tracing::info!(
                    fps = format!("{:.1}", frames as f64 / secs),
                    cap_wait_ms = per(cap_wait_ns),
                    encode_ms = per(encode_ns),
                    write_ms = per(write_ns),
                    "WGC helper perf (depth-pipelined; encode_ms=lock_bitstream on the oldest)"
                );
                frames = 0;
                cap_wait_ns = 0;
                encode_ns = 0;
                write_ns = 0;
                window = std::time::Instant::now();
            }
        }
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
