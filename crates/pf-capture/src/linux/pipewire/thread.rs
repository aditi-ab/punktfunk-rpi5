//! The PipeWire loop thread: connect, offer, negotiate, then run `.process` until quit.

use super::consume::{consume_frame, realtime_minus_monotonic_ns, FenceWaitStats};
use super::hold::{pool_ask, DeferredRequeue, HoldBook, PoolCensus};
use super::offers::{hdr_modifier_offers, offer_pacing, packed_modifier_offers, probe_producer};
use super::pacer::{wire_interval, Pacer, RawTimer, RequestListener, HEARTBEAT};
use super::plan::{
    consumer_kind, resolved_capture_arm, ImportState, NegotiationPlan, PassthroughFallbacks,
};
use super::{map_format, UserData};
use crate::linux::pw_cursor::{update_cursor_meta, CursorState};
use crate::linux::pw_pods::{
    build_cursor_meta_param, build_default_format_obj, build_dmabuf_buffers, build_dmabuf_format,
    build_hdr_dmabuf_format, build_header_meta_param, build_mappable_buffers,
    build_shm_only_buffers, build_sync_timeline_meta_param, serialize_pod, video_raw, Extent,
    Pacing, HDR_FORMAT_ORDER,
};
use crate::linux::sync_timeline::{hand_back, SyncDevice};
use crate::ZeroCopyPolicy;
use anyhow::{Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

/// The PipeWire loop thread for one capture session: connects, builds the
/// modifier offers ([`packed_modifier_offers`], [`hdr_modifier_offers`]),
/// negotiates, and runs `.process` until `quit_rx`, `broken`, or disconnect.
#[allow(clippy::too_many_arguments)]
pub(in crate::linux) fn pipewire_thread(
    fd: Option<OwnedFd>,
    node_id: u32,
    // One-deep mailbox: publish overwrites, so a stalled consumer loses intermediates, never the latest.
    slot: crate::linux::FrameSlot,
    wake: SyncSender<()>,
    signals: crate::linux::CaptureSignals,
    // Zero-copy decision, resolved once by `spawn_pipewire` — never re-derived here.
    plan: NegotiationPlan,
    // `want_444`/`want_hdr` pick the pod family; `expect_exact_dims` arms the birth-mode gate.
    opts: crate::linux::CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    quit_rx: pw::channel::Receiver<()>,
    // Encode-backend facts from the facade — never re-derived here.
    policy: ZeroCopyPolicy,
) -> Result<()> {
    let crate::linux::CaptureOpts {
        want_444,
        want_hdr,
        expect_exact_dims,
        cursor_id0_hides,
        producer_is_gamescope,
        pool_min,
        pool_max,
        unpaced,
        lazy,
        ..
    } = opts;
    // Node ids and remote fds do not identify a compositor: Mutter and gamescope
    // can both use the default daemon. Keep the producer contract explicit.
    let pool_min = pool_ask(pool_min, pool_max, plan.nvenc_raw || plan.vaapi_passthrough);
    let offer_cursor_meta = !producer_is_gamescope;
    crate::pwinit::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    // Capturer `Drop` lands here on the loop thread and stops `run()` so the thread unwinds
    // instead of blocking to process exit. Hold the attachment for the loop's life. The
    // registry probe below also runs the loop; `quit_seen` keeps a quit during it terminal.
    let quit_seen = std::rc::Rc::new(std::cell::Cell::new(false));
    let quit_loop = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), {
        let quit_seen = quit_seen.clone();
        move |()| {
            tracing::debug!("pipewire: quit signal received — stopping capture loop");
            quit_seen.set(true);
            quit_loop.quit();
        }
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    // Portal source: fd to a sandboxed PipeWire remote. KWin virtual-output: no fd, default daemon.
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?,
        None => context
            .connect_rc(None)
            .context("pw connect (default daemon)")?,
    };
    // Lazy driver (PipeWire ≥ 1.2.7 "headless server" scheduling): the producer paints only
    // in a graph cycle this stream starts, so the encode loop owns the tick and no second
    // clock beats against it. Only a producer that emits RequestProcess (Mutter ≥ 49 virtual
    // monitors) may be driven this way; any other stays the driver as before.
    let probe = probe_producer(&core, &mainloop, node_id);
    let lazy = lazy && probe.supports_request;
    if quit_seen.get() {
        return Ok(());
    }

    let backend_is_vaapi = policy.backend_is_vaapi;
    let force_shm = plan.force_shm;
    let vaapi_passthrough = plan.vaapi_passthrough;
    let prefer_native_nv12 = plan.prefer_native_nv12;
    let prefer_native_p010 = plan.prefer_native_p010;
    // Isolated worker (design/zerocopy-worker-isolation.md): a driver fault kills the worker,
    // not this host. Construction failure → CPU path (no dmabuf request). `plan.build_importer`
    // already encodes when to try.
    if plan.gpu_import_latched {
        tracing::warn!(
            "zero-copy GPU import disabled for this capture identity (repeated import-worker \
             deaths or a previous dmabuf negotiation timeout) — using CPU path"
        );
    }
    let mut importer = if plan.build_importer {
        match pf_zerocopy::Importer::new_for_capture() {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                None
            }
        }
    } else {
        None
    };
    if prefer_native_nv12 || prefer_native_p010 {
        tracing::info!(
            container = if prefer_native_p010 { "P010" } else { "NV12" },
            "zero-copy: preferring gamescope's producer-side planar LINEAR DMA-BUF (no host \
             RGB CSC; PUNKTFUNK_PIPEWIRE_NV12=0 restores the packed-RGB negotiation)"
        );
    }
    // Per-fourcc offers: importer lists plus the encoder-proved gamescope seed
    // and the PyroWave Vulkan list, finalized by `dmabuf_modifiers_for_producer`.
    let (modifiers, modifiers_bgra, extend_pyrowave) = packed_modifier_offers(
        &policy,
        &signals.health,
        importer.as_mut(),
        vaapi_passthrough,
        producer_is_gamescope,
    );
    let hdr_modifiers = hdr_modifier_offers(
        &policy,
        &signals.health,
        importer.as_mut(),
        want_hdr,
        vaapi_passthrough,
        producer_is_gamescope,
        plan.nvenc_raw,
    );
    if extend_pyrowave {
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    let want_dmabuf = plan.want_dmabuf(importer.is_some(), &modifiers);
    // Latch must fire only for an offer actually made — `plan.build_importer` cannot know
    // the importer constructed.
    signals.gpu_dmabuf_offer.store(
        want_dmabuf && !vaapi_passthrough && !want_hdr,
        Ordering::Relaxed,
    );
    // One line for the resolved arm and its consumer. Detail lines below explain an arm;
    // they do not state which one this session took.
    let consumer = consumer_kind(
        policy.pyrowave_session,
        backend_is_vaapi,
        policy.backend_is_gpu,
    );
    let arm = resolved_capture_arm(&plan, importer.is_some(), want_dmabuf);
    tracing::info!(
        capture_arm = arm.as_str(),
        consumer = consumer.as_str(),
        modifier_count = if want_hdr {
            hdr_modifiers
                .iter()
                .map(|(_, m)| m.len())
                .max()
                .unwrap_or(0)
        } else {
            modifiers.len()
        },
        // Latch state belongs on the same line as the arm: `cpu` is either "never dmabuf"
        // or "a prior failure we are still living with" — only the second is a bug.
        raw_dmabuf_latch = signals.health.raw_state(),
        "capture pipeline resolved: {} → {}",
        arm.as_str(),
        consumer.as_str()
    );
    if force_shm {
        tracing::info!(
            "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
        );
    } else if plan.raw_dmabuf_latched {
        tracing::warn!(
            "zero-copy raw-dmabuf passthrough disabled for this capture identity (repeated \
             encoder import failures or a negotiation timeout) — capturing CPU frames instead"
        );
    } else if !want_dmabuf && (plan.build_importer || plan.vaapi_passthrough) {
        tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
    } else if vaapi_passthrough {
        // PyroWave remains raw passthrough when its tiled lists are empty: LINEAR is valid.
        tracing::info!(
            native_nv12_preferred = prefer_native_nv12,
            native_p010_preferred = prefer_native_p010,
            modifier_count = modifiers.len(),
            pyrowave_extended = extend_pyrowave,
            "zero-copy: advertising DMA-BUF modifiers for direct encoder import (LINEAR \
             always; native NV12 first when enabled, packed RGB fallback)"
        );
    } else if want_dmabuf {
        tracing::info!(
            bgrx_count = modifiers.len(),
            bgra_count = modifiers_bgra.len(),
            // Sample is truncated to 6, LINEAR pushed last — reading the sample as the whole
            // list makes a good offer look tiled-only.
            linear_offered = modifiers.contains(&0),
            sample = ?&modifiers[..modifiers.len().min(6)],
            "zero-copy: advertising EGL-importable dmabuf modifiers (BGRx + BGRA pods)"
        );
    } else if consumer.cpu_is_downgrade() {
        // No dmabuf advertised: this is the CPU path. `raw_dmabuf_latched` already caught a
        // latched downgrade. Warn for every GPU consumer; software wants CPU frames.
        // `consumer_kind` is per-session so a PyroWave session on an NVIDIA host still warns
        // (the host-global encoder pref would have called it NVENC and logged nothing).
        tracing::warn!(
            consumer = consumer.as_str(),
            "{} encode with the CPU capture path (per-frame de-pad + CSC + upload) — \
             zero-copy is off for this capture ({}); set PUNKTFUNK_ZEROCOPY=1 to restore the \
             dmabuf default",
            consumer.as_str(),
            if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                "PUNKTFUNK_ZEROCOPY is set falsy"
            } else if want_hdr && !policy.hdr_cuda_ok {
                // `build_importer` drops HDR when the encoder cannot take packed 10-bit
                // CUDA. Naming the output format would send the reader to the wrong knob.
                "this HDR session's encoder cannot ingest a 10-bit CUDA payload, so the capture \
                 stays on CPU frames"
            } else {
                "this session's output format asked for CPU frames"
            }
        );
    }
    if want_dmabuf && !vaapi_passthrough && want_444 {
        tracing::info!(
            "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
             NVENC fed native full-chroma YUV, no CPU pixel path"
        );
    } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
        tracing::info!(
            "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709 limited) on the GPU — NVENC \
             fed native YUV (no internal RGB→YUV CSC)"
        );
    }

    // Holds on published frames park their release from whichever thread drops last and wake
    // this channel; a withheld buffer rejoins only on the loop thread — the receiver (attached
    // after the stream exists) or `try_defer` drains the parked releases.
    let (requeue_tx, requeue_rx) = pw::channel::channel::<()>();
    // Explicit sync needs a dmabuf lane and a DRM node that serves syncobjs; whether a buffer
    // then carries sync points is the producer's call at negotiation.
    let sync = (crate::explicit_sync() && (want_hdr || want_dmabuf))
        .then(SyncDevice::open)
        .flatten()
        .map(std::sync::Arc::new);
    let defer = std::sync::Arc::new(DeferredRequeue {
        book: std::sync::Mutex::new(HoldBook::default()),
        pending: std::sync::Mutex::new(Vec::new()),
        wake: requeue_tx,
        logged_active: std::sync::atomic::AtomicBool::new(false),
        logged_shallow: std::sync::atomic::AtomicBool::new(false),
        sync: sync.clone(),
    });

    // The heartbeat timer reads `driving` after `signals` moves into the listener's state.
    let signals_hb = signals.clone();
    // A driven producer paints only in cycles this stream starts; the pacer starts one per
    // request, no sooner than a wire interval after the last. Every entry point runs on this
    // thread. The stream pointer lands once the stream exists.
    let pacer = lazy.then(|| Pacer::new(wire_interval(preferred)));
    // Shared with the consumer, which imports held frames at its own tick.
    signals
        .has_importer
        .store(importer.is_some(), Ordering::Relaxed);
    *signals.importer.lock().unwrap_or_else(|e| e.into_inner()) = importer;
    let signals_exit = signals.clone();
    let hdr_tiled_raw =
        want_hdr && policy.gamescope_tiled && plan.nvenc_raw && !signals.health.hdr_tiled_refused();
    let data = UserData {
        info: VideoInfoRaw::default(),
        format: None,
        modifier: 0,
        slot,
        wake,
        signals,
        vaapi_passthrough,
        // Same predicate `hdr_modifier_offers` used for the NVENC raw lane.
        hdr_tiled_raw,
        import_policy: plan.import_policy.for_ten_bit_sdr(opts.ten_bit_sdr),
        import_state: ImportState::default(),
        dbg_log_n: 0,
        pts: crate::pts_provenance::PtsProvenance::new(),
        pts_reported: std::time::Instant::now(),
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
        hdr_pts_enabled: pf_host_config::env_on("PUNKTFUNK_CAPTURE_HDR_PTS").unwrap_or(true),
        fence_wait: FenceWaitStats::default(),
        pool: PoolCensus::default(),
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(cursor_id0_hides),
        expect_dims: if expect_exact_dims {
            preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
        defer: defer.clone(),
        pacer: pacer.clone(),
        held_drops: 0,
        sync: sync.clone(),
    };

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
        // Do not let the session manager re-target this stream: an orphaned auto-link to
        // a fresh Video/Source wedges that node and head-blocks the daemon work queue,
        // stalling all new link negotiation system-wide.
        "node.dont-reconnect"     => "true",
    };
    if lazy {
        // "2" outranks the producer's supports-request, so PipeWire picks this node as the
        // driver and the producer becomes a requesting follower.
        props.insert("node.supports-lazy", "2");
    }
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-screencast", props).context("pw Stream")?;
    if let Some(p) = &pacer {
        p.stream.set(stream.as_raw_ptr());
    }

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, ud, old, new| {
            let streaming = matches!(new, pw::stream::StreamState::Streaming);
            // Valid only while Streaming. True = the pacer's triggers start every cycle;
            // false = the producer kept the tick.
            let driving = streaming && stream.is_driving();
            tracing::info!(?old, ?new, driving, "pipewire stream state");
            // `Streaming` with no buffers is a static desktop. Anything else means the source
            // went away; `try_latest` turns a sustained non-Streaming state into capture-loss
            // so the encode loop rebuilds instead of freezing on the last frame.
            ud.signals.streaming.store(streaming, Ordering::Relaxed);
            ud.signals.driving.store(driving, Ordering::Relaxed);
            if matches!(new, pw::stream::StreamState::Error(_)) {
                ud.signals.errored.store(true, Ordering::Relaxed);
            }
            if let Some(p) = &ud.pacer {
                p.on_streaming(driving);
            }
        })
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) =
                pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            // Parse once (`parse` takes `&mut self`) and report failure. On `Err`, `negotiated`
            // stays false so the timeout looks like "no accepted format" — a malformed pod we
            // accepted, not a format mismatch.
            let parsed = ud.info.parse(param);
            if let Err(e) = &parsed {
                tracing::error!(
                    error = %e,
                    "pipewire: the negotiated Format pod does not parse — capture will time out \
                     with no usable format"
                );
            }
            if parsed.is_ok() {
                ud.signals.negotiated.store(true, Ordering::Relaxed);
                // Renegotiation replaces the pool: cached per-buffer imports key on buffers
                // that no longer exist, and a recycled fd/inode must not resolve to a stale import.
                if let Some(imp) = ud
                    .signals
                    .importer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    imp.clear_cache();
                }
                let sz = ud.info.size();
                // Gamescope cursor source scales root→frame (`xfixes_cursor::scale_to_frame`).
                ud.signals.frame_size.store(
                    (u64::from(sz.width) << 32) | u64::from(sz.height),
                    Ordering::Relaxed,
                );
                ud.format = map_format(ud.info.format());
                ud.modifier = ud.info.modifier();
                // 10-bit PQ is only offered with MANDATORY BT.2020/PQ, so a 10-bit negotiation
                // is HDR — still log the producer's fixated transfer/primaries.
                let hdr = ud.format.is_some_and(|f| f.is_hdr());
                ud.signals.hdr_negotiated.store(hdr, Ordering::Relaxed);
                tracing::info!(
                    width = sz.width,
                    height = sz.height,
                    spa_format = ?ud.info.format(),
                    mapped = ?ud.format,
                    modifier = ud.modifier,
                    hdr,
                    transfer_function = ud.info.transfer_function(),
                    color_primaries = ud.info.color_primaries(),
                    "pipewire format negotiated"
                );
                if ud.format.is_none() {
                    tracing::error!(
                        spa_format = ?ud.info.format(),
                        "negotiated a pixel format the encoder cannot consume — frames will be skipped"
                    );
                }
            }
        })
        // Pool census. `remove_buffer` also purges the deferred-requeue book: the buffer is
        // being freed under any hold, so that hold's later release must be a no-op (generation
        // in `HoldBook::complete` also covers the address being reused by a new pool).
        .add_buffer(|_stream, ud, _buf| ud.pool.add())
        .remove_buffer(|_stream, ud, buf| {
            ud.pool.remove();
            if let Ok(mut book) = ud.defer.book.lock() {
                book.purge(buf as usize);
            }
        })
        .process(|stream, ud| {
            // Latest-frame-only: Mutter bursts, older queued buffers are stale. Drain, read the
            // older ones' cursor meta, requeue them, keep newest. Dequeue/requeue stay outside
            // `catch_unwind` — a panic inside would strand `newest` and shrink the fixed pool.

            // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on the
            // loop thread; `dequeue_raw_buffer` returns a stream-owned `*mut pw_buffer` or null
            // (null-checked), single-threaded so no concurrent access.
            let mut newest = unsafe { stream.dequeue_raw_buffer() };
            if newest.is_null() {
                return;
            }
            let mut drained = 1u32;
            loop {
                // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
                let next = unsafe { stream.dequeue_raw_buffer() };
                if next.is_null() {
                    break;
                }
                // A new cursor bitmap rides only the buffer of the shape change; read it before
                // the stale pixels go back. Not while gated: that meta is in the doomed size.
                if ud.expect_dims.is_none() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // SAFETY: `newest` is dequeued and not yet requeued, as below.
                        update_cursor_meta(&mut ud.cursor, unsafe { (*newest).buffer });
                    }));
                }
                // SAFETY: `newest` was dequeued from this stream and not yet requeued; we immediately
                // overwrite it, so the requeued pointer is never touched again.
                unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
                newest = next;
                drained += 1;
            }
            // Producer's actual pool depth, once per distinct value. `build_dmabuf_buffers`
            // asks for a range; the producer picks. Depth is the deferred-requeue budget:
            // ≤ HOLD_POOL_RESERVE cannot defer, and a requeued buffer may be rewritten mid-encode.
            if let Some(depth) = ud.pool.note_frame() {
                tracing::info!(
                    pool_depth = depth,
                    high_water = ud.pool.high_water,
                    drained,
                    "pipewire buffer pool negotiated — the producer's ACTUAL count \
                     (add_buffer/remove_buffer): the deferred-requeue budget, and the rewrite \
                     window for any frame published without a hold"
                );
            }
            // Sacrificial birth mode (kwin.rs `create`): frame and cursor meta are in the doomed
            // size until renegotiation. Self-disarms on match, or after `GATE_DEADLINE` — degraded
            // dims beat a first-frame-timeout retry loop if the promised renegotiation never comes.
            if let Some((ew, eh)) = ud.expect_dims {
                /// Renegotiation normally lands within a frame or two; past this, stop starving
                /// the pipeline (the real mode never applied).
                const GATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
                let sz = ud.info.size();
                if sz.width == ew && sz.height == eh {
                    tracing::info!(
                        skipped = ud.gate_skips,
                        width = ew,
                        height = eh,
                        "producer renegotiated to the expected mode — frames flow"
                    );
                    ud.expect_dims = None;
                } else if ud
                    .gate_since
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed()
                    > GATE_DEADLINE
                {
                    tracing::warn!(
                        negotiated_w = sz.width,
                        negotiated_h = sz.height,
                        expected_w = ew,
                        expected_h = eh,
                        skipped = ud.gate_skips,
                        "producer never renegotiated to the expected mode — accepting its \
                         dims (session runs degraded rather than wedged)"
                    );
                    ud.expect_dims = None;
                } else {
                    ud.gate_skips += 1;
                    if ud.gate_skips == 1 || ud.gate_skips.is_power_of_two() {
                        tracing::info!(
                            negotiated_w = sz.width,
                            negotiated_h = sz.height,
                            expected_w = ew,
                            expected_h = eh,
                            n = ud.gate_skips,
                            "holding frames until the producer renegotiates to the expected mode"
                        );
                    }
                    // SAFETY: `newest` was dequeued from this stream and not yet requeued;
                    // requeued exactly once here, then never touched (mirrors the null path).
                    unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
                    return;
                }
            }
            // PipeWire dispatches from a C trampoline with no catch_unwind; a panic across that
            // FFI aborts the host. Contain inspect/consume — the only Rust here that can panic.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                // load through a valid pointer — no mutation or aliasing.
                let spa_buf = unsafe { (*newest).buffer };

                // Cursor meta before the stale-frame skip: Mutter pointer-only moves arrive as
                // metadata-only CORRUPTED buffers we drop for pixels, but the cursor is fresh.
                update_cursor_meta(&mut ud.cursor, spa_buf);
                // Publish the live overlay so pointer-only motion on a static desktop still
                // moves. Skip when `overlay()` is `None`: gamescope has no `SPA_META_Cursor`,
                // and writing `None` at frame rate would clobber the XFixes `Some` in this
                // same slot (pointer strobes). Hidden is still `Some(visible:false)`.
                if let Some(overlay) = ud.cursor.overlay() {
                    if let Ok(mut slot) = ud.signals.cursor_live.lock() {
                        *slot = Some(overlay);
                    }
                }

                // Header + first chunk for the CORRUPTED skip. SPA_META_Header is optional.

                // SAFETY: `spa_buf` is the `*mut spa_buffer` of the buffer we still hold.
                // `spa_buffer_find_meta_data` scans that buffer's metadata array for a `SPA_META_Header`
                // of at least `size_of::<spa_meta_header>()` bytes and returns a pointer into the held
                // buffer's metadata (or null). The size argument matches the struct the result is cast
                // to, and the pointer stays valid as long as the buffer is held (until requeue). Null is
                // handled below.
                let hdr = unsafe {
                    spa::sys::spa_buffer_find_meta_data(
                        spa_buf,
                        spa::sys::SPA_META_Header,
                        std::mem::size_of::<spa::sys::spa_meta_header>(),
                    ) as *const spa::sys::spa_meta_header
                };
                let hdr_flags = if hdr.is_null() {
                    0u32
                } else {
                    // SAFETY: reached only when `hdr` is non-null; it points to a `spa_meta_header`
                    // inside the live buffer's metadata (returned for a size >=
                    // `size_of::<spa_meta_header>()`, so `.flags` is in bounds). A single field read
                    // while the buffer is still held.
                    unsafe { (*hdr).flags }
                };
                // Compositor stamp, upstream of delivery jitter `SystemTime::now()` cannot see.
                // Whether it is worth shipping is what the provenance line measures.
                let hdr_pts = if hdr.is_null() {
                    None
                } else {
                    // SAFETY: as for `.flags` — non-null, from a lookup that demanded at least
                    // `size_of::<spa_meta_header>()` bytes (so `.pts` is in bounds), read while
                    // the buffer is still held.
                    Some(unsafe { (*hdr).pts })
                };
                // Size + flags for the CORRUPTED skip. dmabuf legitimately reports chunk size
                // 0, so the size-0 stale skip is SHM-only.

                // SAFETY: every dereference is guarded in order before any field read — `spa_buf`
                // non-null, `n_datas > 0`, the `datas` (`*mut spa_data`) array non-null, and the first
                // element's `chunk` (`*mut spa_chunk`) non-null. `d0` is that first `spa_data` and `c`
                // its chunk; reading `(*d0).type_`, `(*c).size`, `(*c).flags` are in-bounds field loads
                // of libspa structs inside the buffer we still hold. Single-threaded loop, no mutation.
                let (chunk_size, chunk_flags, is_dmabuf) = unsafe {
                    if !spa_buf.is_null()
                        && (*spa_buf).n_datas > 0
                        && !(*spa_buf).datas.is_null()
                        && !(*(*spa_buf).datas).chunk.is_null()
                    {
                        let d0 = (*spa_buf).datas;
                        let c = (*d0).chunk;
                        let is_dmabuf =
                            (*d0).type_ == spa::sys::SPA_DATA_DmaBuf;
                        ((*c).size, (*c).flags, is_dmabuf)
                    } else {
                        (0u32, 0i32, false)
                    }
                };

                let corrupted = (hdr_flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED) != 0
                    || (chunk_flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32) != 0;

                // Skip Mutter CORRUPTED / size-0 cursor-update buffers. Pointer motion sends
                // metadata-only buffers flagged CORRUPTED (chunk size 0) that still reference
                // a recycled old frame — encoding that is the flash. Size-0 skip is SHM-only.
                if corrupted || (chunk_size == 0 && !is_dmabuf) {
                    ud.dbg_log_n += 1;
                    if ud.dbg_log_n.is_power_of_two() {
                        tracing::debug!(
                            skipped = ud.dbg_log_n,
                            drained,
                            "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                        );
                    }
                    return;
                }

                if let Some(p) = &ud.pacer {
                    p.on_paint();
                }
                consume_frame(ud, spa_buf, newest, stream.as_raw_ptr(), hdr_pts);
            }));
            // Requeue `newest` exactly once on every path unless `try_defer` withheld it —
            // then `BufferHold` owns the requeue; doing both hands the producer the buffer
            // twice. `newest`'s entry is stable here: only this thread removes entries, never
            // `newest`'s inside `.process`. A panic after publish still leaves the hold live.
            let withheld = ud
                .defer
                .book
                .lock()
                .map(|b| b.contains(newest as usize))
                .unwrap_or(false);
            if !withheld {
                // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame)
                // completed inside the closure above; `newest` was dequeued from this stream,
                // not yet requeued, and — per the `withheld` check — carries no hold that would
                // requeue it a second time.
                unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
            }
            if outcome.is_err() {
                // `.process` is per-frame; a deterministic panic would flood. Power-of-two throttle.
                static PANICS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::error!(count = n, "panic in pipewire process callback — frame dropped");
                }
            }
        })
        .register()
        .context("register stream listener")?;

    // A `BufferHold` dropping on any thread only parks and wakes; this loop-thread callback
    // (or `try_defer`, whichever runs first) is where a withheld buffer rejoins.
    let defer_cb = defer.clone();
    let stream_ptr = stream.as_raw_ptr() as usize;
    let _requeue_attach = requeue_rx.attach(mainloop.loop_(), move |()| {
        // SAFETY: the loop thread dispatches this. The stream outlives this attached receiver
        // (declared after it, dropped before it), and the loop stops dispatching once `run()`
        // returns.
        unsafe { defer_cb.drain(stream_ptr as *mut pw::sys::pw_stream) };
    });

    // `PUNKTFUNK_PW_FIXED_POD="WxH"`: one fixed format, to bisect against a producer's EnumFormat.
    let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));

    let obj = if let Some((fw, fh)) = fixed_pod {
        tracing::info!(
            fw,
            fh,
            "pipewire: offering a fixed BGRx format pod (PUNKTFUNK_PW_FIXED_POD)"
        );
        video_raw(
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Id,
                VideoFormat::BGRx
            ),
            Extent::Fixed(fw, fh),
            Pacing::Producer,
        )
    } else {
        build_default_format_obj(preferred, Pacing::Producer)
    };

    // gamescope paints the Steam overlay into this node only when negotiated
    // `gamescope_focus_appid` is 0 (the default). Do not advertise a non-zero focus-appid —
    // that is the Remote-Play branch, which drops the overlay.

    if want_hdr {
        tracing::info!(
            "HDR capture: offering xBGR_210LE/xRGB_210LE DMA-BUF modifiers (LINEAR always) \
             with MANDATORY BT.2020 + SMPTE-2084 (PQ) colorimetry"
        );
    }
    // Zero-copy: offer only BGRx dmabuf with our EGL-importable modifiers (offering shm
    // makes the compositor pick shm). Modifiers go out as MANDATORY `ChoiceEnum::Enum`;
    // this is not the two-step DONT_FIXATE handshake (`ChoiceFlags` cannot express it).
    let build_pods = |unpaced: bool| -> Result<Vec<Vec<u8>>> {
        let pacing = offer_pacing(
            unpaced,
            probe.framerate_mhz,
            producer_is_gamescope,
            preferred,
        );
        if want_hdr {
            // Offering SDR alongside lets the producer pick it, and a timeout latches SDR
            // downgrade. Order is the fix — see the NVIDIA note on `HDR_FORMAT_ORDER`. First
            // compatible pod wins, so gamescope's P010 pass leads when the encoder takes it.
            let mut pods = Vec::with_capacity(HDR_FORMAT_ORDER.len() + 1);
            if prefer_native_p010 {
                pods.push(build_hdr_dmabuf_format(
                    VideoFormat::P010_10LE,
                    &[0],
                    preferred,
                    pacing,
                )?);
            }
            for (fmt, list) in &hdr_modifiers {
                pods.push(build_hdr_dmabuf_format(*fmt, list, preferred, pacing)?);
            }
            return Ok(pods);
        }
        if !want_dmabuf {
            // The fixed bisect pod stays exactly what the operator typed.
            let o = if unpaced && fixed_pod.is_none() {
                build_default_format_obj(preferred, Pacing::Unpaced)
            } else {
                obj.clone()
            };
            return Ok(vec![serialize_pod(o)?]);
        }
        let mut pods = Vec::with_capacity(if prefer_native_nv12 { 3 } else { 2 });
        if prefer_native_nv12 {
            // First compatible consumer pod wins. Pinning BT.709 limited selects gamescope's
            // RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(
                VideoFormat::NV12,
                &[0],
                preferred,
                pacing,
            )?);
        }
        if !modifiers.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &modifiers,
                preferred,
                pacing,
            )?);
        }
        // xdph (Hyprland/sway) lists only BGRA on its dmabuf EnumFormat (BGRA+BGRx on SHM).
        // A BGRx-only dmabuf offer intersects nothing and the link fails as if modifiers
        // mismatched. Same 32-bit layout; listed after BGRx so a producer offering both
        // still takes the existing path (first compatible consumer pod wins).
        if !modifiers_bgra.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRA,
                &modifiers_bgra,
                preferred,
                pacing,
            )?);
        }
        Ok(pods)
    };
    // Unpaced pods first, the plain set behind them. A KWin before 6.7 floors `maxFramerate`
    // at 1/1, so a fixed 0/1 fails every intersection and the plain set is what fixates.
    let mut format_pods = build_pods(unpaced)?;
    if unpaced {
        format_pods.extend(build_pods(false)?);
    }
    let buffers_values = if want_hdr || want_dmabuf {
        // Dmabuf-only. HDR: Mutter's SHM path paints 8-bit ARGB32 regardless of format, so a
        // MemFd buffer under a 10-bit format would carry mislabeled bytes.
        Some(build_dmabuf_buffers(pool_min, false)?)
    } else if force_shm {
        // Exclude DmaBuf so Mutter must download (glReadPixels orders against render).
        Some(build_shm_only_buffers()?)
    } else {
        // CPU path still accepts mappable dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins).
        Some(build_mappable_buffers()?)
    };

    let cursor_meta = if offer_cursor_meta {
        Some(build_cursor_meta_param()?)
    } else {
        None
    };
    // Explicit sync: a Buffers twin that demands the meta, ahead of the plain one, and the
    // meta itself. Both sides listing the meta is what puts the two syncobj datas on a buffer.
    let sync_buffers = match &sync {
        Some(_) => Some(build_dmabuf_buffers(pool_min, true)?),
        None => None,
    };
    let sync_meta = match &sync {
        Some(_) => Some(build_sync_timeline_meta_param()?),
        None => None,
    };
    // Any meta listed here narrows the producer's set to the intersection, so the header
    // rides along with the first one; a producer left unlisted keeps its whole set.
    let header_meta = if cursor_meta.is_some() || sync_meta.is_some() {
        Some(build_header_meta_param()?)
    } else {
        None
    };
    let mut byte_slices: Vec<&[u8]> = Vec::new();
    for pod in &format_pods {
        byte_slices.push(pod);
    }
    if let Some(b) = &sync_buffers {
        byte_slices.push(b);
    }
    if let Some(b) = &buffers_values {
        byte_slices.push(b);
    }
    if let Some(m) = &cursor_meta {
        byte_slices.push(m);
    }
    if let Some(m) = &sync_meta {
        byte_slices.push(m);
    }
    if let Some(m) = &header_meta {
        byte_slices.push(m);
    }
    let mut params: Vec<&Pod> = byte_slices
        .iter()
        .map(|&b| Pod::from_bytes(b).context("pod from bytes"))
        .collect::<Result<_>>()?;

    let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    if lazy {
        flags |= pw::stream::StreamFlags::DRIVER;
    }
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            flags,
            &mut params,
        )
        .context("pw stream connect")?;

    let cap_timer = pacer.as_ref().map(|p| {
        let p = p.clone();
        mainloop.loop_().add_timer(move |_| p.schedule())
    });
    if let (Some(p), Some(t)) = (&pacer, &cap_timer) {
        use pw::loop_::IsSource;
        p.timer.set(Some(RawTimer {
            utils: mainloop.loop_().as_raw().utils,
            source: t.as_ptr(),
        }));
    }
    let _requests = pacer
        .as_ref()
        .map(|p| RequestListener::attach(&stream, p.clone()));
    let heartbeat = pacer.as_ref().map(|p| {
        let (p, signals) = (p.clone(), signals_hb.clone());
        mainloop.loop_().add_timer(move |_| {
            // Re-read the role: PipeWire may assign the driver after the Streaming edge.
            // SAFETY: the stream outlives this timer source (declared after it).
            let driving = signals.streaming.load(Ordering::Relaxed)
                && unsafe { pw::sys::pw_stream_is_driving(p.stream.get()) };
            if driving != signals.driving.swap(driving, Ordering::Relaxed) {
                p.on_streaming(driving);
            }
            if driving {
                p.heartbeat();
            }
        })
    });
    if let Some(t) = &heartbeat {
        let _ = t.update_timer(Some(HEARTBEAT), Some(HEARTBEAT));
    }

    // Blocks until capturer `Drop` fires the quit channel. The importer goes here, not with
    // the last `CaptureSignals` clone: the next pipeline must find the EGL/CUDA state gone.
    mainloop.run();
    signals_exit.has_importer.store(false, Ordering::Relaxed);
    *signals_exit
        .importer
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}
