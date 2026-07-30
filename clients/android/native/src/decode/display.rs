//! Display/frame-rendered tracking, render-callback registration, HDR dataspace mapping.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use super::latency::now_realtime_ns;
use super::RENDERED_CAP;

/// `CLOCK_MONOTONIC` now in nanoseconds — the base of the `systemNano` render timestamp the
/// `OnFrameRendered` callback reports (Android's `System.nanoTime`), read only to re-base that
/// stamp onto `CLOCK_REALTIME` (see [`on_frame_rendered`]).
fn now_monotonic_ns() -> i128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` with a valid out-pointer is an always-safe syscall.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
}

/// State shared between the decode loop and the `AMediaCodec` `OnFrameRendered` callback (which
/// fires on a codec-internal thread): rendered frames awaiting their render timestamp, so the HUD
/// gets the spec's `display` stage (decoded→displayed) and the `capture→displayed` end-to-end
/// headline (`design/stats-unification.md` — this replaces Android's v1 `capture→decoded`
/// endpoint whenever the platform delivers render callbacks).
pub(super) struct DisplayTracker {
    stats: Arc<crate::stats::VideoStats>,
    /// Live host-minus-client clock offset (ns) for the skew-corrected end-to-end sample —
    /// loaded per callback so mid-stream re-syncs apply. Holding the handle (not the client)
    /// keeps the leaked render-callback refcount from pinning the whole session alive.
    clock_offset: Arc<AtomicI64>,
    /// Always-on latch/display accumulator for the presenter's 1 Hz `pf-present` line —
    /// independent of the HUD gate, so a HUD-off A/B stays measurable from logcat.
    meter: Arc<super::presenter::PresentMeter>,
    /// `(pts_us, decoded_real_ns, released_real_ns)` of frames released with `render = true`, in
    /// release order, awaiting their callback. Pushed on EVERY render (no HUD gate — the ring is
    /// a 64-tuple bound and the latch metric wants to exist when nobody is watching).
    rendered: Mutex<VecDeque<(u64, i128, i128)>>,
}

impl DisplayTracker {
    pub(super) fn new(
        stats: Arc<crate::stats::VideoStats>,
        clock_offset: Arc<AtomicI64>,
        meter: Arc<super::presenter::PresentMeter>,
    ) -> Arc<DisplayTracker> {
        Arc::new(DisplayTracker {
            stats,
            clock_offset,
            meter,
            rendered: Mutex::new(VecDeque::new()),
        })
    }

    /// Park one just-rendered frame's `(pts, decoded stamp, release stamp)` for the render
    /// callback to pair — the release stamp is the latch metric's start (release→displayed).
    pub(super) fn note_rendered(&self, pts_us: u64, decoded_ns: i128, released_ns: i128) {
        let mut g = self
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.push_back((pts_us, decoded_ns, released_ns));
        if g.len() > RENDERED_CAP {
            g.pop_front(); // render callbacks stopped coming (allowed under load) — evict
        }
    }
}

/// Register [`on_frame_rendered`] on the codec (`AMediaCodec_setOnFrameRenderedCallback`,
/// **API 33** — "Available since Android T" per the NDK header; only the *Java* listener dates
/// back further). That sits above the API-28 floor, so the entry point is dlsym-resolved at
/// runtime like [`try_set_frame_rate`] — hard-linking it (as 0.9.0 shipped) made
/// `System.loadLibrary` fail on every pre-Android-13 device, taking down all of `NativeBridge`.
/// The `ndk` wrapper has no binding and the call needs the raw codec pointer, which is what the
/// vendored crate's public `as_ptr` patch is for. Returns the userdata pointer holding a leaked
/// `Arc<DisplayTracker>` refcount; the caller MUST reclaim it with [`release_render_callback`]
/// AFTER dropping the codec (`AMediaCodec_delete` is what guarantees no further callback can
/// fire). `None` (nothing to reclaim) if the symbol is absent (API < 33) or the platform refused —
/// the HUD then simply has no `display` stage, exactly the pre-callback behaviour.
pub(super) fn install_render_callback(
    codec: &MediaCodec,
    tracker: &Arc<DisplayTracker>,
) -> Option<*const DisplayTracker> {
    // media_status_t AMediaCodec_setOnFrameRenderedCallback(
    //     AMediaCodec*, AMediaCodecOnFrameRendered, void*)                            (API 33)
    type SetOnFrameRenderedFn = unsafe extern "C" fn(
        *mut ndk_sys::AMediaCodec,
        ndk_sys::AMediaCodecOnFrameRendered,
        *mut c_void,
    ) -> ndk_sys::media_status_t;
    // SAFETY: `dlopen` of `libmediandk.so`, which the `ndk` media wrapper already links — always
    // mapped, so this only bumps its refcount (never closed — process-lifetime handle). `dlsym`
    // returns null when the symbol is absent (device below API 33), checked before transmuting the
    // non-null pointer to its fn-pointer type.
    let set_on_frame_rendered = unsafe {
        let lib = libc::dlopen(c"libmediandk.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let sym = libc::dlsym(lib, c"AMediaCodec_setOnFrameRenderedCallback".as_ptr());
        if sym.is_null() {
            log::info!("decode: no render callback on this API level (<33) — no display stage");
            return None;
        }
        std::mem::transmute::<*mut c_void, SetOnFrameRenderedFn>(sym)
    };
    let ud = Arc::into_raw(tracker.clone());
    // SAFETY: `codec.as_ptr()` is the live codec this thread owns; `ud` outlives the registration
    // (reclaimed only after the codec is deleted, per this function's contract).
    let status = unsafe {
        set_on_frame_rendered(codec.as_ptr(), Some(on_frame_rendered), ud as *mut c_void)
    };
    if status == ndk_sys::media_status_t::AMEDIA_OK {
        Some(ud)
    } else {
        log::warn!("decode: setOnFrameRenderedCallback failed ({status:?}) — no display stage");
        // SAFETY: registration failed, so the codec never took the reference — reclaim it now.
        unsafe { drop(Arc::from_raw(ud)) };
        None
    }
}

/// Reclaim [`install_render_callback`]'s leaked `Arc` refcount.
///
/// # Safety
/// Call exactly once, and only after the codec the callback was registered on has been dropped —
/// deleting the codec stops its internal threads, so no callback can still be running (or run
/// later) against this pointer.
pub(super) unsafe fn release_render_callback(ud: *const DisplayTracker) {
    // SAFETY: `ud` is the pointer `install_render_callback` leaked from `Arc::into_raw`, and this
    // function's contract is that it is reclaimed exactly once, after the codec is gone.
    unsafe { drop(Arc::from_raw(ud)) };
}

/// The `AMediaCodecOnFrameRendered` trampoline: fires (possibly batched) on a codec-internal
/// thread once per output frame actually placed on the output surface, with SurfaceFlinger's
/// render timestamp. That timestamp (`system_nano`) is on `CLOCK_MONOTONIC`, so it is re-based
/// onto `CLOCK_REALTIME` here — against monotonic-now at callback time, which also cancels any lag
/// between the frame rendering and the (batchable) callback delivery — to subtract against the
/// receipt/decode stamps and the host capture pts. Records the HUD's `displayed` point:
/// `end-to-end` = capture→displayed (skew-corrected) and `display` = decoded→displayed
/// (single-clock local). Panic-free by construction (poison-proof lock, saturating math) — an
/// unwind out of an `extern "C"` fn would abort the process.
unsafe extern "C" fn on_frame_rendered(
    _codec: *mut ndk_sys::AMediaCodec,
    userdata: *mut c_void,
    media_time_us: i64,
    system_nano: i64,
) {
    // SAFETY: the platform hands back exactly the `userdata` registered with the callback — the
    // `Arc::into_raw` pointer from `install_render_callback`, whose refcount is held for as long as
    // the codec exists, and the codec is what delivers this call.
    let t = unsafe { &*(userdata as *const DisplayTracker) };
    let displayed_ns = now_realtime_ns() - (now_monotonic_ns() - system_nano as i128);
    let pts_us = media_time_us.max(0) as u64;
    // Pair the frame back to its release record, evicting older entries (their callbacks were
    // dropped by the platform) — same monotonic-eviction discipline as `note_decoded_pts`.
    let mut paired = None;
    {
        let mut g = t
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(&(p, d, r)) = g.front() {
            if p > pts_us {
                break; // future frame — leave it for its own callback
            }
            g.pop_front();
            if p == pts_us {
                paired = Some((d, r));
                break;
            }
        }
    }
    // Clamped to (0, 10 s) like the e2e sample: a vendor's first render callbacks can carry a
    // garbage `system_nano` (observed on-glass: an epoch-sized latch max on the session's first
    // window), and one such sample would poison every max/percentile it lands in.
    let clamp = |v: i128| (v > 0 && v < 10_000_000_000).then_some((v / 1000) as u64);
    let display_us = paired.and_then(|(d, _)| clamp(displayed_ns - d));
    let latch_us = paired.and_then(|(_, r)| clamp(displayed_ns - r));
    // Always-on half: the presenter's pf-present line reads these with the HUD off.
    t.meter.note_latch(latch_us);
    if !t.stats.enabled() {
        return; // HUD hidden — skip the skew math + the stats lock
    }
    let e2e_ns =
        displayed_ns + t.clock_offset.load(Ordering::Relaxed) as i128 - pts_us as i128 * 1000;
    let e2e_us = (e2e_ns > 0 && e2e_ns < 10_000_000_000).then_some((e2e_ns / 1000) as u64);
    t.stats.note_displayed(e2e_us, display_us, latch_us);
}

/// React to an output-format change by signalling the stream's HDR dataspace on the Surface (SDR
/// streams leave the default alone). The AMediaCodec analogue of the sync loop's `OutputFormatChanged`
/// handling; safe to call repeatedly (`applied_ds` dedups).
pub(super) fn apply_hdr_dataspace(
    codec: &MediaCodec,
    window: &NativeWindow,
    applied_ds: &mut Option<DataSpace>,
) {
    if let Some(ds) = hdr_dataspace(codec) {
        if *applied_ds != Some(ds) {
            match window.set_buffers_data_space(ds) {
                Ok(()) => {
                    *applied_ds = Some(ds);
                    log::info!("decode: HDR stream → Surface dataspace {ds}");
                }
                Err(e) => {
                    log::warn!("decode: set_buffers_data_space({ds}) failed (non-fatal): {e}")
                }
            }
        }
    }
}

/// Map the decoder's reported output colour to a BT.2020 HDR dataspace, or `None` for SDR. The
/// integer values are the Android MediaFormat colour constants the NDK shares: COLOR_TRANSFER
/// ST2084 = 6 (PQ/HDR10), HLG = 7; COLOR_RANGE FULL = 1, LIMITED = 2 (the host encodes limited).
pub(super) fn hdr_dataspace(codec: &MediaCodec) -> Option<DataSpace> {
    let fmt = codec.output_format();
    let full_range = fmt.i32("color-range") == Some(1);
    match fmt.i32("color-transfer") {
        Some(6) => Some(if full_range {
            DataSpace::Bt2020Pq
        } else {
            DataSpace::Bt2020ItuPq
        }),
        Some(7) => Some(if full_range {
            DataSpace::Bt2020Hlg
        } else {
            DataSpace::Bt2020ItuHlg
        }),
        _ => None, // SDR (BT.709 / SDR_VIDEO) or unspecified
    }
}
