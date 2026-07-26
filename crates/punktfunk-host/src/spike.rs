//! The pipeline spike (plan §8): capture → NVENC encode → playable file, with the
//! encoded access units also fed through a `punktfunk_core` host→client `Session` over an
//! in-process loopback to prove the core's FEC + packetize + reassemble path on real
//! encoder output.
//!
//! This is the spike runner, not the production host path: it drives the stages on one thread (the
//! per-stage-thread pipeline with bounded channels is [`crate::pipeline`]). Source is
//! either a synthetic BGRx test pattern (no capture session needed) or the live xdg
//! ScreenCast portal monitor.

use crate::capture::{self, Capturer, SyntheticCapturer};
use crate::encode::{self, Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, Context, Result};
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF};
use punktfunk_core::{Config, Role, Session};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Deterministic moving BGRx test pattern — no capture session required.
    Synthetic,
    /// Deterministic moving NV12 texture on the GPU (Windows only) — no capture session required.
    /// Feeds the native AMF / D3D11 zero-copy encoders, which demand an NV12 GPU texture the CPU
    /// `Synthetic` source can't give them. Used to validate GPU-encoder behaviour (e.g. AMF
    /// intra-refresh) headlessly.
    SyntheticNv12,
    /// Live monitor via the xdg ScreenCast portal + PipeWire.
    Portal,
    /// KWin virtual output created at `width`x`height` (zkde_screencast). Lets us validate
    /// capture (and zero-copy) at an arbitrary client resolution against a headless KWin.
    KwinVirtual,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub source: Source,
    /// Synthetic-only; the portal source uses the PipeWire-negotiated size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub codec: Codec,
    pub bitrate_bps: u64,
    /// Raw Annex-B elementary-stream sink (`.h265`/`.h264`/`.ivf-less .obu`); playable.
    pub out: PathBuf,
    /// Also round-trip every AU through a `punktfunk_core` host→client loopback and verify.
    pub loopback: bool,
}

pub fn run(opts: Options) -> Result<()> {
    let mut capturer: Box<dyn Capturer> = match opts.source {
        Source::Synthetic => {
            tracing::info!(
                width = opts.width,
                height = opts.height,
                fps = opts.fps,
                "spike source: synthetic BGRx test pattern"
            );
            Box::new(SyntheticCapturer::new(opts.width, opts.height, opts.fps))
        }
        Source::SyntheticNv12 => {
            #[cfg(target_os = "windows")]
            {
                tracing::info!(
                    width = opts.width,
                    height = opts.height,
                    fps = opts.fps,
                    "spike source: synthetic NV12 GPU texture (moving luma ramp)"
                );
                Box::new(
                    capture::synthetic_nv12::SyntheticNv12Capturer::new(
                        opts.width,
                        opts.height,
                        opts.fps,
                    )
                    .context("open synthetic NV12 capturer")?,
                )
            }
            #[cfg(not(target_os = "windows"))]
            {
                anyhow::bail!(
                    "--source synthetic-nv12 is Windows-only (native AMF / D3D11 encoders)"
                );
            }
        }
        Source::Portal => {
            // PUNKTFUNK_SPIKE_HDR=1: run the GNOME 50+ HDR offer (10-bit PQ dmabufs) — the dev
            // validation lever for the Linux HDR capture path without a full GameStream client.
            let want_hdr = std::env::var("PUNKTFUNK_SPIKE_HDR").as_deref() == Ok("1");
            tracing::info!(
                want_hdr,
                "spike source: xdg ScreenCast portal (live monitor)"
            );
            // Embedded cursor: the spike passes `cursor_blend = false` to its encoder open, so
            // a metadata pointer would be composited by nothing.
            capture::open_portal_monitor(want_hdr, false).context("open portal capturer")?
        }
        Source::KwinVirtual => {
            let compositor = crate::vdisplay::detect().unwrap_or(crate::vdisplay::Compositor::Kwin);
            tracing::info!(
                width = opts.width,
                height = opts.height,
                ?compositor,
                "spike source: virtual output (PUNKTFUNK_COMPOSITOR)"
            );
            let mut vd = crate::vdisplay::open(compositor).context("open virtual display")?;
            let vout = vd
                .create(punktfunk_core::Mode {
                    width: opts.width,
                    height: opts.height,
                    refresh_hz: opts.fps,
                })
                .context("create virtual output")?;
            capture::capture_virtual_output(
                vout,
                capture::OutputFormat::resolve(false, crate::encode::resolved_backend_is_gpu()),
                crate::session_plan::CaptureBackend::resolve(),
            )
            .context("capture virtual output")?
        }
    };

    // Activate the capturer so the portal/PipeWire process callback actually delivers frames
    // (it gates the per-frame de-pad on `active`; idle by default so reconnects are cheap).
    capturer.set_active(true);

    // The first frame establishes the authoritative dimensions (the portal's negotiated
    // size, or the synthetic size) used to configure the encoder.
    let first = capturer.next_frame().context("capture first frame")?;
    let (w, h) = (first.width, first.height);
    tracing::info!(
        width = w,
        height = h,
        format = ?first.format,
        codec = ?opts.codec,
        bitrate_bps = opts.bitrate_bps,
        "opening video encoder"
    );
    let mut encoder = encode::open_video(
        opts.codec,
        first.format,
        w,
        h,
        opts.fps,
        opts.bitrate_bps,
        first.is_cuda(),
        8,                            // spike synthetic harness: 8-bit
        encode::ChromaFormat::Yuv420, // ...and 4:2:0
        false,                        // synthetic frames carry no cursor
    )
    .context("open encoder")?;

    let mut sink = BufWriter::new(
        File::create(&opts.out).with_context(|| format!("create {}", opts.out.display()))?,
    );

    let mut lb = if opts.loopback {
        Some(Loopback::new().context("build punktfunk-core loopback")?)
    } else {
        None
    };

    let target_frames = (opts.seconds as u64) * (opts.fps as u64);
    let started = Instant::now();
    let mut stats = Stats::default();

    let mut frame = first;
    loop {
        encoder.submit(&frame).context("encoder submit")?;
        stats.submitted += 1;
        drain_encoder(encoder.as_mut(), &mut sink, lb.as_mut(), &mut stats)?;
        if stats.submitted >= target_frames {
            break;
        }
        frame = capturer.next_frame().context("capture frame")?;
    }

    // NVENC buffers frames internally even at delay=0 — flush and drain the tail.
    encoder.flush().context("encoder flush")?;
    drain_encoder(encoder.as_mut(), &mut sink, lb.as_mut(), &mut stats)?;
    sink.flush().context("flush output file")?;

    let elapsed = started.elapsed().as_secs_f64();
    tracing::info!(
        submitted = stats.submitted,
        encoded = stats.encoded,
        keyframes = stats.keyframes,
        bytes_out = stats.bytes_out,
        out = %opts.out.display(),
        elapsed_s = format!("{elapsed:.2}"),
        encode_fps = format!("{:.1}", stats.encoded as f64 / elapsed.max(1e-9)),
        "spike capture→encode→file complete"
    );

    if let Some(lb) = lb {
        lb.report();
        if lb.mismatches > 0 || lb.recovered != lb.submitted {
            return Err(anyhow!(
                "punktfunk-core loopback verification FAILED: {} mismatches, {}/{} AUs recovered",
                lb.mismatches,
                lb.recovered,
                lb.submitted
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Stats {
    submitted: u64,
    encoded: u64,
    keyframes: u64,
    bytes_out: u64,
}

fn drain_encoder(
    encoder: &mut dyn Encoder,
    sink: &mut impl Write,
    mut lb: Option<&mut Loopback>,
    stats: &mut Stats,
) -> Result<()> {
    while let Some(au) = encoder.poll().context("encoder poll")? {
        sink.write_all(&au.data).context("write AU to file")?;
        stats.encoded += 1;
        stats.bytes_out += au.data.len() as u64;
        if au.keyframe {
            stats.keyframes += 1;
        }
        if let Some(lb) = lb.as_deref_mut() {
            lb.submit(&au)?;
        }
    }
    Ok(())
}

/// A host↔client `punktfunk_core` pair over a lossless in-process loopback. Each encoded AU is
/// FEC-protected, packetized, sent, then reassembled on the client and byte-compared to the
/// original — exercising the core on real encoder output (the spike "feed into a Session" goal).
struct Loopback {
    host: Session,
    client: Session,
    submitted: u64,
    recovered: u64,
    mismatches: u64,
    bytes: u64,
}

impl Loopback {
    fn new() -> Result<Loopback> {
        let (host_tx, client_tx) = punktfunk_core::transport::loopback_pair(0, 0);
        let host = Session::new(Config::p1_defaults(Role::Host), Box::new(host_tx))
            .map_err(|e| anyhow!("host session: {e:?}"))?;
        let client = Session::new(Config::p1_defaults(Role::Client), Box::new(client_tx))
            .map_err(|e| anyhow!("client session: {e:?}"))?;
        Ok(Loopback {
            host,
            client,
            submitted: 0,
            recovered: 0,
            mismatches: 0,
            bytes: 0,
        })
    }

    fn submit(&mut self, au: &EncodedFrame) -> Result<()> {
        let mut flags = FLAG_PIC as u32;
        if au.keyframe {
            flags |= FLAG_SOF as u32;
        }
        self.host
            .submit_frame(&au.data, au.pts_ns, flags)
            .map_err(|e| anyhow!("host submit_frame: {e:?}"))?;
        self.submitted += 1;
        self.bytes += au.data.len() as u64;

        // Lossless + in-order loopback: each submit yields exactly the AU just sent.
        loop {
            match self.client.poll_frame() {
                Ok(frame) => {
                    self.recovered += 1;
                    if frame.data != au.data {
                        self.mismatches += 1;
                        tracing::warn!(
                            recovered = self.recovered,
                            got = frame.data.len(),
                            expected = au.data.len(),
                            "loopback AU mismatch"
                        );
                    }
                }
                Err(punktfunk_core::PunktfunkError::NoFrame) => break,
                Err(e) => return Err(anyhow!("client poll_frame: {e:?}")),
            }
        }
        Ok(())
    }

    fn report(&self) {
        tracing::info!(
            submitted = self.submitted,
            recovered = self.recovered,
            mismatches = self.mismatches,
            bytes = self.bytes,
            "punktfunk-core loopback: AUs FEC-packetized → sent → reassembled & verified"
        );
    }
}
