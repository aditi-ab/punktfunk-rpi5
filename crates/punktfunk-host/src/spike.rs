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
    /// PyroWave datagram-aligned packetization at this shard payload
    /// ([`Encoder::set_wire_chunking`], plan §4.4) — what a real session passes from its
    /// negotiated `shard_payload`. `None` = the dense one-packet-per-AU shape.
    ///
    /// This is also the switch that makes the STREAMED-AU wire reachable from the spike: with
    /// it set and `PUNKTFUNK_PYROWAVE_STREAMED_AU=1` armed, the encoder's `poll_chunk` hands the
    /// AU out in window-aligned pieces and the loopback seals them through
    /// `begin_streamed_frame_at`/`seal_streamed_chunk`/`seal_streamed_finish` — the same path a
    /// `VIDEO_CAP_STREAMED_AU` client drives. Without it there is no way to exercise PW6 end to
    /// end outside a real client session.
    pub wire_chunk: Option<usize>,
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
            // `resolve` is the shared GameStream/spike constructor and hard-codes `pyrowave: false`
            // (GameStream never negotiates it). The spike DOES know its codec, and on Linux that
            // flag is what puts the capture on the raw-dmabuf passthrough
            // (`ZeroCopyPolicy::pyrowave_session`, set from the same comparison in
            // `session_plan::output_format`). Left false, `--codec pyrowave` encoded PyroWave off a
            // capture negotiated for somebody else, and the only way to exercise the real path was
            // the host-global `PUNKTFUNK_ENCODER=pyrowave` lever — which ALSO flips
            // `backend_is_vaapi`, so it cannot reproduce a per-session PyroWave negotiation on an
            // auto/NVENC host at all. That is precisely the configuration PW2 exists for.
            let mut want =
                capture::OutputFormat::resolve(false, crate::encode::resolved_backend_is_gpu());
            want.pyrowave = opts.codec == Codec::PyroWave;
            capture::capture_virtual_output(
                vout,
                want,
                crate::session_plan::CaptureBackend::resolve(),
                compositor == crate::vdisplay::Compositor::Kwin,
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
        4,                            // no client decoder — keep the backend's multi-slice default
    )
    .context("open encoder")?;

    // Datagram-aligned packetization (§4.4) — and, with the PW6 knob armed, the gate that makes
    // `supports_chunked_poll()` true so the drain below takes the streamed-AU path.
    if let Some(c) = opts.wire_chunk {
        encoder.set_wire_chunking(c);
        tracing::info!(
            shard_payload = c,
            chunked_poll = encoder.supports_chunked_poll(),
            "spike: wire chunking on (chunked_poll=false means PUNKTFUNK_PYROWAVE_STREAMED_AU \
             is not armed — the AU still goes out whole)"
        );
    }

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
        // 0 = the whole-AU drain; > encoded = the streamed drain actually cut AUs into pieces.
        chunks = stats.chunks,
        chunks_per_au = format!(
            "{:.1}",
            stats.chunks as f64 / (stats.encoded.max(1)) as f64
        ),
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
    /// Streamed-AU drain only: total chunks polled across all AUs (1 per AU means the cut never
    /// engaged — the knob is off or the AU fits one chunk).
    chunks: u64,
}

fn drain_encoder(
    encoder: &mut dyn Encoder,
    sink: &mut impl Write,
    mut lb: Option<&mut Loopback>,
    stats: &mut Stats,
) -> Result<()> {
    // Streamed-AU drain (PW6): the encoder hands the finished AU out in shard-aligned pieces and
    // the loopback seals each piece as it arrives, exactly as the native host's send thread does.
    // Re-queried per drain, never cached — the trait's contract.
    if encoder.supports_chunked_poll() {
        return drain_encoder_chunked(encoder, sink, lb, stats);
    }
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

/// The streamed-AU drain. Each chunk is sealed into the open wire frame the moment it is polled;
/// the concatenation is kept only so the completed AU can still be written to the file sink and
/// byte-compared against what the client reassembled — which is the point of the leg: it proves
/// the chunks the encoder cut, sealed through the sentinel-block wire, reassemble to EXACTLY the
/// AU `poll()` would have produced.
fn drain_encoder_chunked(
    encoder: &mut dyn Encoder,
    sink: &mut impl Write,
    mut lb: Option<&mut Loopback>,
    stats: &mut Stats,
) -> Result<()> {
    let mut whole: Vec<u8> = Vec::new();
    let mut chunks = 0u32;
    while let Some(c) = encoder.poll_chunk().context("encoder poll_chunk")? {
        if c.first {
            whole.clear();
            chunks = 0;
            if let Some(lb) = lb.as_deref_mut() {
                lb.streamed_begin(c.pts_ns, c.keyframe)?;
            }
        }
        whole.extend_from_slice(&c.data);
        chunks += 1;
        if let Some(lb) = lb.as_deref_mut() {
            lb.streamed_chunk(&c.data)?;
        }
        if !c.last {
            continue;
        }
        sink.write_all(&whole).context("write AU to file")?;
        stats.encoded += 1;
        stats.bytes_out += whole.len() as u64;
        stats.chunks += chunks as u64;
        if c.keyframe {
            stats.keyframes += 1;
        }
        if let Some(lb) = lb.as_deref_mut() {
            lb.streamed_finish(&whole)?;
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
    /// The streamed AU currently open (PW6). `Some` strictly between `streamed_begin` and
    /// `streamed_finish`, mirroring the native send thread's `StreamedOpen`.
    open: Option<punktfunk_core::packet::StreamedAu>,
    /// Wire frame index for the streamed path. `submit_frame` uses the packetizer's internal
    /// counter and `begin_streamed_frame_at` takes an explicit one; a session must use ONE
    /// numbering style, and the spike never mixes them (`supports_chunked_poll()` is constant
    /// for a PyroWave session, so every AU takes the same route).
    next_index: u32,
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
            open: None,
            next_index: 0,
        })
    }

    /// Open a streamed AU on the wire (PW6). The client side needs no opt-in: a streamed frame
    /// completes exactly like a whole one and is handed up as a single `Frame` — which is the
    /// finding this leg exists to demonstrate rather than assert.
    fn streamed_begin(&mut self, pts_ns: u64, keyframe: bool) -> Result<()> {
        if self.open.is_some() {
            return Err(anyhow!(
                "streamed AU still open at begin — a previous AU never sent its `last` chunk"
            ));
        }
        let mut flags = FLAG_PIC as u32;
        if keyframe {
            flags |= FLAG_SOF as u32;
        }
        let idx = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        self.open = Some(
            self.host
                .begin_streamed_frame_at(pts_ns, flags, idx)
                .map_err(|e| anyhow!("begin_streamed_frame_at: {e:?}"))?,
        );
        Ok(())
    }

    /// Seal + send one encoder chunk. The returned batch is often EMPTY (the sealer buffers
    /// until a whole FEC block accumulates) — that is the normal case, not an error.
    fn streamed_chunk(&mut self, data: &[u8]) -> Result<()> {
        let au = self
            .open
            .as_mut()
            .ok_or_else(|| anyhow!("streamed chunk with no open AU"))?;
        let wires = self
            .host
            .seal_streamed_chunk(au, data, false)
            .map_err(|e| anyhow!("seal_streamed_chunk: {e:?}"))?;
        self.send(wires)
    }

    /// Close the AU (final block carries the real totals) and verify what the client got.
    fn streamed_finish(&mut self, expect: &[u8]) -> Result<()> {
        let au = self
            .open
            .take()
            .ok_or_else(|| anyhow!("streamed finish with no open AU"))?;
        let wires = self
            .host
            .seal_streamed_finish(au)
            .map_err(|e| anyhow!("seal_streamed_finish: {e:?}"))?;
        self.send(wires)?;
        self.submitted += 1;
        self.bytes += expect.len() as u64;
        self.verify(expect)
    }

    fn send(&mut self, wires: Vec<Vec<u8>>) -> Result<()> {
        if wires.is_empty() {
            return Ok(());
        }
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        self.host
            .send_sealed(&refs)
            .map_err(|e| anyhow!("send_sealed: {e:?}"))?;
        drop(refs);
        self.host.reclaim_wires(wires);
        Ok(())
    }

    /// Drain whatever the client can now reassemble and byte-compare it to `expect`.
    fn verify(&mut self, expect: &[u8]) -> Result<()> {
        loop {
            match self.client.poll_frame() {
                Ok(frame) => {
                    self.recovered += 1;
                    if frame.data != expect {
                        self.mismatches += 1;
                        tracing::warn!(
                            recovered = self.recovered,
                            got = frame.data.len(),
                            expected = expect.len(),
                            complete = frame.complete,
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
