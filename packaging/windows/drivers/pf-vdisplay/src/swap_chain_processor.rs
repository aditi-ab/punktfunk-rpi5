//! The swap-chain processor (STEP 5 + STEP 6): a worker thread that DRAINS the IddCx swap-chain (so the
//! virtual monitor stays a usable display) and PUBLISHES each acquired surface into the host-created
//! shared ring (the IDD-push path).
//!
//! The OS presents the composited desktop to the driver through a swap-chain; the driver MUST consume it
//! (acquire → finished-processing) or the monitor stalls. STEP 5 binds our render device to the swap-chain
//! (`IddCxSwapChainSetDevice`) and loops acquire/finish. STEP 6 lazily attaches a [`FramePublisher`] to
//! the host's shared ring and, on each acquired frame, `CopyResource`s `out.MetaData.pSurface` into the
//! next ring slot before finishing the frame (a non-IDD-push session simply never attaches and keeps
//! draining). Frames the ring can NOT take feed the [`FrameStash`] instead, which every fresh attach
//! republishes as its instant first frame — the first-frame guarantee that makes a session opened onto
//! an IDLE desktop show a picture without waiting for anything to dirty the display.
//!
//! Ported from the proven oracle (`packaging/windows/vdisplay-driver/pf-vdisplay/src/
//! swap_chain_processor.rs`) onto wdk-sys + wdk-iddcx. The oracle's `wdf_umdf`/`wdf_umdf_sys` are
//! replaced by `wdk_sys::iddcx::*` + the `wdk_iddcx` DDI wrappers. Those wrappers return a RAW
//! `NTSTATUS` (`i32`) that is HRESULT-shaped for the swap-chain DDIs, so we classify it by hand
//! (`hr >= 0` = success; `0x8000_000A` = E_PENDING; `hr < 0 && != E_PENDING` = error) rather than with
//! `nt_success`.

use std::{
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wdk_sys::iddcx::{
    self, IDARG_IN_RELEASEANDACQUIREBUFFER2, IDARG_IN_REPORTFRAMESTATISTICS,
    IDARG_IN_SETREALTIMEGPUPRIORITY, IDARG_IN_SWAPCHAINSETDEVICE,
    IDARG_OUT_RELEASEANDACQUIREBUFFER2, IDDCX_SWAPCHAIN,
};
// `HANDLE` is the shared wdk-sys typedef (`crate::types`) re-used by the iddcx bindings — take it from
// the crate root, which is guaranteed to export it (the iddcx module only re-exports it if bindgen
// re-declared it there). It is the same type as `IDARG_IN_SETSWAPCHAIN.hNextSurfaceAvailable`.
use wdk_sys::{HANDLE, NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding};
use windows::{
    Win32::{
        Foundation::HANDLE as WHANDLE,
        Graphics::{
            Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D},
            Dxgi::{IDXGIDevice, IDXGIResource},
        },
        System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        System::Threading::{
            AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
            SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
        },
    },
    core::{Interface, w},
};

use crate::{
    direct_3d_device::Direct3DDevice,
    frame_transport::{FramePublisher, FrameStash, PublishOutcome},
};

/// E_PENDING — `ReleaseAndAcquireBuffer2` returns this (HRESULT-shaped) when the swap-chain is valid but
/// DWM has composed no new frame yet; wait on the surface-available event and retry.
const E_PENDING: u32 = 0x8000_000A;
/// `WAIT_TIMEOUT` from `WaitForSingleObject` (defined locally to avoid pulling a windows-crate constant
/// type into the comparison — the raw `WAIT_EVENT.0` is just a `u32`).
const WAIT_TIMEOUT_U32: u32 = 0x0000_0102;

/// HRESULT-shaped success test for the swap-chain DDIs (raw `NTSTATUS`/HRESULT: success iff non-negative).
#[inline]
fn hr_success(hr: NTSTATUS) -> bool {
    hr >= 0
}

/// Whether the swap-chain processing device's GPU scheduling is raised to REALTIME (the IddCx
/// 1.9 `IddCxSetRealtimeGPUPriority` DDI — "higher priority than any regular application can
/// set"). Default **ON**: minimum latency at every layer — a GPU-saturating game must not starve
/// the leg that feeds every captured frame into the ring, and the 2026-08 default-OFF (after an
/// RX 9070 XT field A/B blamed this raise for a metronomic ~1.8 s capture-stall class) did not
/// fix those AMD reports and did regress loaded NVIDIA boxes into feed starvation. The AMD
/// metronome keeps its per-box escape hatch: `setx /M PFVD_NO_RT_GPU 1` (any value) + a device
/// restart disables the raise — read via [`machine_env`] as well as the process environment,
/// because WUDFHost's own environment is stale until a reboot. The old `PFVD_RT_GPU` opt-in
/// ladder (off/thread/realtime) is gone; a stale `PFVD_RT_GPU` now just matches the default.
fn rt_gpu_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("PFVD_NO_RT_GPU").is_none() && machine_env("PFVD_NO_RT_GPU").is_none()
    })
}

/// Read a MACHINE environment variable from the registry — where `setx /M` writes it.
///
/// `std::env` is NOT enough in this process: WUDFHost inherits its environment from WUDFSvc,
/// which inherited it from the SCM at boot, and the SCM never refreshes on `WM_SETTINGCHANGE`.
/// So a `setx /M` set today is invisible to `std::env::var_os` until a REBOOT — measured on
/// .173 (2026-08-29): the toggle below had no effect across device restarts with fresh WUDFHost
/// PIDs until this read was added. [`rt_gpu_enabled`]'s opt-out reads through here for the same
/// reason.
fn machine_env(name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};
    const KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    let (subkey, value) = (HSTRING::from(KEY), HSTRING::from(name));
    let mut buf = [0u16; 256];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: both name pointers address NUL-terminated HSTRING buffers alive for the call;
    // `buf`/`size` are a matched out-buffer and its byte length. RRF_RT_REG_SZ makes the call
    // reject any non-string value rather than write a foreign type into the buffer.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    // `size` is bytes INCLUDING the terminator; trim to chars and drop trailing NULs.
    let chars = (size as usize / 2).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..chars])
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// Whether the drain loop reports per-frame statistics to the OS
/// (`IddCxSwapChainReportFrameStatistics`). Default **ON**: the canonical MS sample leaves the
/// call as a TODO and this driver never made it, but the statistics feed the OS's per-output
/// scheduling, and the 2026-08 case-#4 evidence shape is DWM starving exactly one head — this
/// one — while composing a lit sibling at full rate (virtual-head-selective compose
/// starvation). Reporting is the contract-following default; `setx /M PFVD_FRAME_STATS 0` (or
/// `off`) + a device restart is the A/B escape hatch — read via [`machine_env`], because the
/// process environment never sees it.
fn frame_stats_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let off = |v: &str| v == "0" || v.eq_ignore_ascii_case("off");
        let set = std::env::var("PFVD_FRAME_STATS")
            .ok()
            .or_else(|| machine_env("PFVD_FRAME_STATS"));
        let on = !set.as_deref().is_some_and(off);
        dbglog!(
            "[pf-vd] frame statistics: {} (PFVD_FRAME_STATS={set:?})",
            if on { "ON" } else { "OFF" }
        );
        on
    })
}

/// Current QPC tick count (the clock `IDDCX_FRAME_STATISTICS`' `*QpcTime` fields are stamped
/// in). Never fails on XP+; a zero on the impossible error path is a harmless "no timing" stat.
fn qpc() -> u64 {
    let mut t = 0i64;
    // SAFETY: out-pointer to a valid local.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    #[allow(clippy::cast_sign_loss)]
    {
        t as u64
    }
}

/// QPC ticks → whole milliseconds. Signed: a frame's display time can sit AFTER our acquire.
fn qpc_ms(ticks: i64) -> i64 {
    use std::sync::OnceLock;
    static FREQ: OnceLock<i64> = OnceLock::new();
    let f = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: out-pointer to a valid local; the frequency is fixed at boot.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    ticks.saturating_mul(1000) / f
}

/// Wall-clock milliseconds since the Unix epoch — lines a driver-log witness up with host.log.
fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// A minimal newtype to move a raw pointer / handle across the thread boundary. The wrapped value is a
/// raw IddCx swap-chain handle or an event HANDLE (both raw pointers, framework-managed) — sending them
/// to the worker is sound because only this thread touches them and the framework synchronises lifetime.
struct Sendable<T>(T);
// SAFETY: see the type doc — the wrapped raw handle is owned by the worker for its lifetime.
unsafe impl<T> Send for Sendable<T> {}

pub struct SwapChainProcessor {
    terminate: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: Raw ptr is managed by external library; access is serialised by the worker thread + the
// terminate flag.
unsafe impl Send for SwapChainProcessor {}
// SAFETY: as above — the raw pointer is only touched by the serialised worker, so a shared
// `&SwapChainProcessor` reference exposes no unsynchronised access.
unsafe impl Sync for SwapChainProcessor {}

impl SwapChainProcessor {
    pub fn new() -> Self {
        Self {
            terminate: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn run(
        &mut self,
        swap_chain: IDDCX_SWAPCHAIN,
        device: Arc<Direct3DDevice>,
        available_buffer_event: HANDLE,
        target_id: u32,
        render_luid_low: u32,
        render_luid_high: i32,
    ) {
        let available_buffer_event = Sendable(available_buffer_event);
        let swap_chain = Sendable(swap_chain);
        let terminate = self.terminate.clone();

        let join_handle = thread::spawn(move || {
            // Rust 2021 disjoint closure captures would otherwise grab the raw `swap_chain.0` /
            // `available_buffer_event.0` FIELDS directly (defeating the `Sendable` Send wrapper, since the
            // inner `*mut IDDCX_SWAPCHAIN__` / `HANDLE` are `!Send`). Rebind the WHOLE wrappers here so the
            // closure captures them as `Sendable<_>` (which IS `Send`), then unwrap from the locals.
            let swap_chain = swap_chain;
            let available_buffer_event = available_buffer_event;
            // It is very important to prioritize this thread by making use of the Multimedia Scheduler
            // Service. It will intelligently prioritize the thread for improved throughput in high
            // CPU-load scenarios.
            let mut av_task = 0u32;
            // SAFETY: `w!("Distribution")` is a 'static null-terminated UTF-16 task name; `av_task` is a
            // valid local out-param. The returned handle is reverted with AvRevertMmThreadCharacteristics.
            let res = unsafe { AvSetMmThreadCharacteristicsW(w!("Distribution"), &mut av_task) };
            // MMCSS can fail under the restricted WUDFHost token ('Distribution' task unregistered /
            // service unavailable). The MS sample CONTINUES unprioritized — never abort: returning
            // here would leave the assigned swap-chain undrained (the monitor stalls, DWM blocks on
            // it) and leak the WDF swap-chain object until device teardown. But "unprioritized" is
            // not acceptable either: this thread is the whole display's frame pump, and at normal
            // priority a display-stack disturbance (DDC/HPD servicing DPC pressure, poller-software
            // storms) can starve it into multi-hundred-ms delivery holes. Fall back to
            // TIME_CRITICAL — the highest band available without the realtime priority class, and
            // the closest to what MMCSS 'Distribution' would have granted. The thread spends its
            // life blocked on the surface-available event / keyed mutex, so it cannot starve others.
            let av_handle = match res {
                Ok(h) => Some(h),
                Err(e) => {
                    // SAFETY: plain FFI; GetCurrentThread returns a pseudo-handle (never fails,
                    // nothing to close), SetThreadPriority on it affects only this thread.
                    let fallback = unsafe {
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)
                    };
                    dbglog!(
                        "[pf-vd] swap-chain: MMCSS prioritization failed ({e:?}) — fell back to \
                         TIME_CRITICAL thread priority (ok={})",
                        fallback.is_ok()
                    );
                    None
                }
            };

            Self::run_core(
                swap_chain.0,
                &device,
                available_buffer_event.0,
                &terminate,
                target_id,
                render_luid_low,
                render_luid_high,
            );

            dbglog!(
                "[pf-vd] swap-chain run_core RETURNED (target={target_id}) — deleting swap-chain, device drops next"
            );

            // Delete the swap-chain WDF object BEFORE the `Arc<Direct3DDevice>` drops (the swap-chain
            // referenced our device). `WdfObjectDelete` takes a WDFOBJECT.
            // SAFETY: `swap_chain` is a live IddCx swap-chain handle; we own the sole reference here and
            // the drain loop has exited.
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, swap_chain.0 as WDFOBJECT);
            }

            // Revert the thread to normal once it's done (only if MMCSS was actually engaged).
            if let Some(h) = av_handle {
                // SAFETY: `h` is the live characteristics handle returned by
                // AvSetMmThreadCharacteristicsW above, reverted exactly once here at thread exit.
                let res = unsafe { AvRevertMmThreadCharacteristics(h) };
                if let Err(e) = res {
                    dbglog!("[pf-vd] swap-chain: failed to revert prioritized thread: {e:?}");
                }
            }
        });

        self.thread = Some(join_handle);
    }

    fn run_core(
        swap_chain: IDDCX_SWAPCHAIN,
        device: &Direct3DDevice,
        available_buffer_event: HANDLE,
        terminate: &AtomicBool,
        target_id: u32,
        render_luid_low: u32,
        render_luid_high: i32,
    ) {
        // SetDevice fails (0x887A0026, FACILITY_DXGI) when the monitor briefly flaps INACTIVE during
        // topology activation — the OS unassigns + re-assigns the swap-chain, and a fresh run_core thread
        // can lose the race to the unassign. Retry briefly so a stable re-assign binds the device instead
        // of giving up on the first transient failure. `terminate` (set when the OS unassigns + drops the
        // processor) breaks us out promptly.
        //
        // Cast to IDXGIDevice ONCE and BORROW it to the swap-chain across all retries. Re-casting +
        // `into_raw()`'ing on EVERY attempt — and a flapping monitor fails several attempts per session —
        // orphans an IDXGIDevice reference per failure, pinning the D3D device (and its ~dozen worker
        // threads + tens of MB of VRAM) so it is NEVER freed when the processor drops. `as_raw()` keeps
        // our single reference (released right after the loop); IddCx AddRefs its own on success, and
        // `device` keeps the object alive for the drain loop regardless.
        let dxgi_device = match device.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                dbglog!("[pf-vd] swap-chain: failed to cast ID3D11Device to IDXGIDevice: {e:?}");
                return;
            }
        };
        // Built zeroed + field-assigned (driver style) — robust against a bindgen field-set difference.
        let mut set_device = pod_init!(IDARG_IN_SWAPCHAINSETDEVICE);
        set_device.pDevice = dxgi_device.as_raw().cast();
        let mut set_ok = false;
        let mut terminated = false;
        for attempt in 0..60u32 {
            if terminate.load(Ordering::Relaxed) {
                dbglog!(
                    "[pf-vd] swap-chain run_core: terminated during SetDevice (attempt {attempt}, target={target_id})"
                );
                terminated = true;
                break;
            }
            // SAFETY: driver is loaded; `swap_chain` is valid; `set_device` points to valid local storage.
            let hr = unsafe { wdk_iddcx::IddCxSwapChainSetDevice(swap_chain, &set_device) };
            if hr_success(hr) {
                set_ok = true;
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice OK (target={target_id}, attempt={attempt}) — entering drain loop"
                );
                break;
            }
            if attempt == 0 {
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice attempt 0 failed ({hr:#x}) — retrying up to 60x@50ms (monitor may be flapping)"
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
        // GPU-scheduling raise for the swap-chain processing device — default ON, so the leg
        // feeding every captured frame into the ring outranks a GPU-saturating game (history +
        // `PFVD_NO_RT_GPU` escape hatch: [`rt_gpu_enabled`]). Best-effort, never fatal, issued
        // while our borrowed device reference is still alive (IddCx uses it synchronously); the
        // DDI may still decline (e.g. E_NOTIMPL on pre-WDDM-3.0 hardware).
        if set_ok && rt_gpu_enabled() {
            let mut rt = pod_init!(IDARG_IN_SETREALTIMEGPUPRIORITY);
            rt.pDevice = dxgi_device.as_raw().cast();
            // SAFETY: driver is loaded; `swap_chain` is the live assigned swap-chain whose
            // device bind just succeeded; `rt.pDevice` is that same bound DXGI device,
            // alive across the synchronous call; `rt` points to valid local storage.
            let hr = unsafe { wdk_iddcx::IddCxSetRealtimeGPUPriority(swap_chain, &rt) };
            if hr_success(hr) {
                dbglog!(
                    "[pf-vd] swap-chain: processing device raised to REALTIME GPU priority (default; PFVD_NO_RT_GPU disables) (target={target_id})"
                );
            } else {
                dbglog!(
                    "[pf-vd] swap-chain: realtime GPU priority declined ({hr:#x}) — normal scheduling (target={target_id})"
                );
            }
        }
        // Release our borrowed device reference — IddCx holds its own now, or we gave up. (Explicit drop
        // so NLL can't release it mid-loop while the swap-chain still references the raw ptr.)
        drop(dxgi_device);
        if !set_ok {
            if !terminated {
                dbglog!(
                    "[pf-vd] swap-chain run_core: SetDevice never succeeded after retries (target={target_id}) — giving up"
                );
            }
            return;
        }

        // STEP 6 IDD-push: lazily ATTACH to the HOST-created shared ring over the SEALED channel. The
        // frame objects are unnamed — the host duplicates their handles into this process and delivers
        // the values via IOCTL_SET_FRAME_CHANNEL, which the control plane stashes on our monitor
        // (`monitor::take_frame_channel`). Until a delivery lands we just drain — exactly the STEP-5
        // behaviour — so a non-IDD-push session never stalls. The frame-channel stash is polled every
        // iteration (attach latency = first-frame latency, since attach republishes the FrameStash).
        // STEP 6 sibling-join fix: re-adopt a FramePublisher PRESERVED across a swap-chain
        // unassign→reassign flap. When a SIBLING display churns the desktop topology (a second client
        // joining / leaving / resizing), the OS reassigns THIS monitor's swap-chain and the previous
        // worker exited — but the host-owned ring it published into is still live, and the host only
        // re-delivers the frame channel on a ring RECREATE (a descriptor change). Without this the
        // fresh worker has nothing to attach to and the first client's stream freezes (repeat frames
        // forever). Re-adopt ONLY when the freshly-assigned swap-chain renders on the SAME adapter as
        // the preserved publisher (same pooled Direct3DDevice → its immediate context + opened ring
        // textures are valid); a mismatch drops it and falls back to a fresh channel delivery. A
        // preserved publisher that the host superseded meanwhile (ring recreate → is_stale, or a
        // pending delivery) is dropped + replaced by the existing re-attach logic at the loop top.
        let mut publisher: Option<FramePublisher> =
            crate::monitor::take_preserved_publisher(target_id).and_then(|p| {
                if p.render_adapter() == (render_luid_low, render_luid_high) {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: re-adopted preserved publisher (target={target_id}) — resuming the host ring across the swap-chain flap"
                    );
                    Some(p)
                } else {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: preserved publisher's render adapter changed (target={target_id}) — dropping it, will re-attach from a fresh channel"
                    );
                    None
                }
            });
        // The FIRST-FRAME stash (see `FrameStash`): the retained last composed frame, republished
        // into every fresh ring at attach so a session opening onto an idle desktop is never black.
        // Re-adopted across a swap-chain flap on the same adapter (`take_preserved_stash` drops a
        // cross-adapter one — its texture lives on the other adapter's pooled device).
        let mut stash: FrameStash =
            crate::monitor::take_preserved_stash(target_id, render_luid_low, render_luid_high)
                .unwrap_or_default();

        let mut logged_pending = false;
        let mut logged_frame = false;
        // First `IddCxSwapChainReportFrameStatistics` outcome, logged once (the rest of the
        // session reports silently) so a field log answers "did the stats path engage".
        let mut logged_stats = false;
        // Frame witness (see the success branch): the previous acquire's OS frame number and
        // wall time, plus the last late-frame line's time (its throttle).
        let mut last_pfn: u32 = 0;
        let mut last_frame_at: Option<Instant> = None;
        let mut last_late_log: Option<Instant> = None;
        // The frame-channel delivery gate (see `monitor::frame_channel_gen`): the loop only takes
        // the monitors mutex when a delivery LANDED since it last looked. Seeded one behind the
        // current generation so a delivery that arrived before this worker started (host delivered
        // ahead of the swap-chain assign) is checked on the very first pass.
        let mut seen_chan_gen = crate::monitor::frame_channel_gen().wrapping_sub(1);
        loop {
            // Check terminate at the TOP, every iteration. The success branch below does NOT re-check it,
            // so during a CONTINUOUS frame burst (DWM rendering the freshly-activated desktop) a thread the
            // OS unassigns — or that the processor is dropping — never sees the flag and loops on, pinning
            // its D3D device (and ~36 NVIDIA worker threads). That is THE reconnect leak; it only
            // reproduced at full speed (E_PENDING gaps DO check terminate and masked it under a debugger).
            // Without this, `SwapChainProcessor::drop`'s join can also block until the burst ends.
            if terminate.load(Ordering::Relaxed) {
                break;
            }

            // The lock-free delivery gate: `chan_pending` is true only when a `set_frame_channel`
            // landed since the last pass — the two mutex-taking checks below (`has_frame_channel`,
            // `take_frame_channel`) used to run EVERY pass (≥60 locks/s per worker on a mutex
            // contended by the whole control plane, the mode DDIs and the watchdog); now the
            // steady state takes no lock at all.
            let chan_gen = crate::monitor::frame_channel_gen();
            let chan_pending = chan_gen != seen_chan_gen;
            // Re-attach triggers, either of:
            // * `is_stale` — the host recreated the ring mid-session (HDR flip): it bumps OUR header's
            //   generation and re-delivers; without dropping here we'd keep CopyResource'ing into the
            //   stale ring, whose format now mismatches the surface → the publish() format-guard drops
            //   every frame and the stream freezes until the next swap-chain recreate.
            // * a PENDING delivery (newest-wins) — a host build-retry creates a whole NEW ring with a
            //   DIFFERENT header mapping; the old publisher's header never changes, so `is_stale` can't
            //   fire. The host only delivers after fully (re)creating a ring, so a pending delivery
            //   always supersedes whatever we're attached to.
            if publisher.as_ref().is_some_and(FramePublisher::is_stale)
                || (publisher.is_some()
                    && chan_pending
                    && crate::monitor::has_frame_channel(target_id))
            {
                // Harvest the superseded ring's last-published frame into the stash BEFORE dropping
                // the publisher: between sessions the driver keeps publishing into the (host-side
                // dead) previous ring, so that slot holds the CURRENT desktop image — exactly what
                // the new ring's attach below republishes as its instant first frame.
                if let Some(p) = publisher.take() {
                    p.harvest_into(&device.device, &mut stash);
                }
            }
            // Lazy-attach at the loop TOP so we keep trying even while the display is idle (E_PENDING /
            // no frames presented yet), not only when a frame is acquired — gated by `chan_pending`
            // (a delivery can only appear via `set_frame_channel`, which bumps the generation):
            // attach latency is still first-frame latency, since the attach itself republishes the
            // stash. A taken delivery is consumed whether the attach succeeds or not (on failure its
            // handles are closed inside from_channel, the host's wait-for-attach reads the status
            // code, and any retry is a NEW delivery — with its own bump). `target_id` binds the
            // attach: the mapped ring must name THIS monitor (proto v3 validation inside
            // from_channel — a cross-delivered ring is refused, never published into).
            if publisher.is_none()
                && chan_pending
                && let Some(channel) = crate::monitor::take_frame_channel(target_id)
                && let Ok(mut p) = FramePublisher::from_channel(
                    channel,
                    target_id,
                    render_luid_low,
                    render_luid_high,
                    &device.device,
                    &device.device_context,
                )
            {
                // FIRST-FRAME GUARANTEE: republish the retained desktop image into the fresh ring
                // immediately. On an idle desktop DWM composes nothing, so without this the host
                // would wait (and kick synthetic input) for a frame that may never come. A
                // stale-descriptor stash (e.g. pre-HDR-flip) is rejected by publish()'s guard —
                // at worst the old wait-for-compose path.
                if let Some(t) = stash.texture()
                    && p.publish(t) == PublishOutcome::Published
                {
                    dbglog!(
                        "[pf-vd] frame-push(driver): republished the retained frame into the fresh ring (target={target_id}) — instant first frame, no compose needed"
                    );
                }
                publisher = Some(p);
            }
            // The pending generation was serviced above — whichever branch ran, a lock-taking
            // check happened (`has_frame_channel` and/or `take_frame_channel`), so this pass has
            // seen everything up to `chan_gen`. A delivery racing in between bumps past it and
            // re-arms the gate on the next pass.
            if chan_pending {
                seen_chan_gen = chan_gen;
            }

            // ...Buffer2 is required once CAN_PROCESS_FP16 is set. AcquireSystemMemoryBuffer=FALSE keeps
            // the GPU surface (out.MetaData.pSurface) — STEP 6 publishes it into the shared ring in the
            // success branch below. Built zeroed + field-assigned (driver style) so a bindgen field-set
            // difference can't break a positional struct literal.
            let mut in_args = pod_init!(IDARG_IN_RELEASEANDACQUIREBUFFER2);
            #[allow(clippy::cast_possible_truncation)]
            {
                in_args.Size = size_of::<IDARG_IN_RELEASEANDACQUIREBUFFER2>() as u32;
            }
            in_args.AcquireSystemMemoryBuffer = 0;
            // `pod_init!` (zeroed, not `::default()`) — consistent with every other IddCx out-struct
            // in this driver, and robust whether or not bindgen derives `Default` for this type (its
            // `MetaData` field carries a raw `pSurface` pointer + union which can suppress the derive).
            let mut buffer = pod_init!(IDARG_OUT_RELEASEANDACQUIREBUFFER2);
            // SAFETY: driver is loaded; `swap_chain` is valid; in/out point to valid local storage.
            let hr: NTSTATUS = unsafe {
                wdk_iddcx::IddCxSwapChainReleaseAndAcquireBuffer2(
                    swap_chain,
                    &mut in_args,
                    &mut buffer,
                )
            };

            // v2 telemetry (stall attribution): stamp the drain heartbeat — plus the last-acquire
            // on a pass that got a composed frame — into the shared header EVERY pass, E_PENDING
            // included (the wait below is ≤16 ms, so the heartbeat cadence bounds how stale it can
            // read while this thread is scheduled). What lets the host split a capture stall into
            // worker-starved / DWM-composed-nothing / our-delivery-leg.
            if let Some(p) = publisher.as_ref() {
                p.note_drain(hr_success(hr));
            }

            if (hr as u32) == E_PENDING {
                if !logged_pending {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: E_PENDING (target={target_id}) — swap-chain valid but DWM has composed NO frame yet"
                    );
                    logged_pending = true;
                }
                // SAFETY: `available_buffer_event` is the framework-provided surface-available event.
                let wait_result =
                    unsafe { WaitForSingleObject(WHANDLE(available_buffer_event.cast()), 16).0 };

                // thread requested an end
                if terminate.load(Ordering::Relaxed) {
                    break;
                }

                // WAIT_OBJECT_0 | WAIT_TIMEOUT
                if matches!(wait_result, 0 | WAIT_TIMEOUT_U32) {
                    // We have a new buffer (or timed out), so try the AcquireBuffer again.
                    continue;
                }

                // The wait was cancelled or something unexpected happened.
                break;
            } else if hr_success(hr) {
                let acquire_qpc = qpc();
                // Frame witness: the two stamps the OS puts on every frame (`IDDCX_METADATA2`),
                // which split a FRAME-GENERATION hole from inside the swap-chain. Logged when a
                // frame ends >= 1 s of silence, or arrives >= 250 ms after its display time (at
                // most one line per 2 s). Read the line as: a PresentationFrameNumber JUMP = DWM
                // composed frames this swap-chain never received; +1 with a large `late` = the
                // frames were composed on time and delivered late; +1 and fresh = DWM composed
                // nothing for this head. `dirty=1` on a static desktop is the OS's no-update repeat.
                let pfn = buffer.MetaData.PresentationFrameNumber;
                let display_qpc = buffer.MetaData.PresentDisplayQPCTime;
                #[allow(clippy::cast_possible_wrap)]
                let late_ms =
                    (display_qpc != 0).then(|| qpc_ms(acquire_qpc as i64 - display_qpc as i64));
                let silence = last_frame_at.map_or(Duration::ZERO, |t| t.elapsed());
                if silence >= Duration::from_secs(1)
                    || (late_ms.is_some_and(|l| l >= 250)
                        && last_late_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)))
                {
                    dbglog!(
                        "[pf-vd] frame-witness (target={target_id}) t={} silence={}ms pfn={pfn} (+{}) late={} dirty={}",
                        unix_ms(),
                        silence.as_millis(),
                        pfn.wrapping_sub(last_pfn),
                        late_ms.map_or("n/a".to_string(), |l| format!("{l}ms")),
                        buffer.MetaData.DirtyRectCount
                    );
                    last_late_log = Some(Instant::now());
                }
                last_pfn = pfn;
                last_frame_at = Some(Instant::now());
                if !logged_frame {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: FIRST FRAME acquired (target={target_id}) — DWM IS compositing the virtual display!"
                    );
                    logged_frame = true;
                }
                // STEP 6: copy the acquired surface into the shared ring BEFORE FinishedProcessingFrame
                // (the surface is valid until the next ReleaseAndAcquire). Every successful acquire
                // TRANSFERS one surface reference to the driver — the MS sample `Attach`es it into a
                // ComPtr and `Reset`s BEFORE FinishedProcessingFrame, warning that a driver which
                // "forgets to release the reference" leaves the surfaces alive after the swap-chain is
                // destroyed. Holding it (the old `from_raw_borrowed`) leaked the swap-chain's whole
                // surface set per assign/unassign cycle (reconnect, mode change, HDR flip) — so adopt
                // the reference UNCONDITIONALLY (publisher or not); it is released when `res` drops at
                // the end of this block. (Publisher attach happens at the loop top.)
                // Frame-statistics inputs, harvested alongside the publish: our "send" is the ring
                // publish (the GPU copy into the shared section), and the pixel/byte counts come
                // from the surface descriptor.
                let send_start_qpc = qpc();
                let mut stat_pixels: u32 = 0;
                let mut stat_bytes: u32 = 0;
                {
                    let raw = buffer.MetaData.pSurface as *mut core::ffi::c_void;
                    if !raw.is_null() {
                        // SAFETY: `raw` is the live surface IddCx just handed us, carrying the acquire's
                        // transferred reference; `from_raw` adopts exactly that reference (released on
                        // drop, below — the queued GPU copy is unaffected: D3D defers destruction, and
                        // the copy is ordered before the consumer via the slot keyed mutex).
                        let res = unsafe { IDXGIResource::from_raw(raw) };
                        if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                            let mut desc = D3D11_TEXTURE2D_DESC::default();
                            // SAFETY: `tex` is the live acquired surface; GetDesc fills the local.
                            unsafe { tex.GetDesc(&mut desc) };
                            stat_pixels = desc.Width.saturating_mul(desc.Height);
                            // Assumes the common 32-bpp swap-chain format (an FP16 surface is
                            // really 8 bytes/px — a 2× understatement there, acceptable for a
                            // statistic: the OS wants magnitude, not an invoice).
                            stat_bytes = stat_pixels.saturating_mul(4);
                            match publisher.as_mut().map(|p| p.publish(&tex)) {
                                // Ring took it (or the host is alive and busy) — nothing to retain.
                                Some(PublishOutcome::Published | PublishOutcome::Dropped) => {}
                                // No ring, or the surface's descriptor doesn't match it (a mode-set /
                                // HDR flip racing the host's ring recreate): RETAIN the frame — it is
                                // the desktop image the next attach republishes as its first frame.
                                // Unattached/mismatched composes are damage-driven and transient, so
                                // the extra copy costs nothing at steady state.
                                Some(PublishOutcome::DescMismatch) | None => {
                                    stash.store(
                                        &device.device,
                                        &device.device_context,
                                        &tex,
                                        Instant::now(),
                                    );
                                }
                            }
                        }
                        // `res` drops here → the acquire's surface reference is released, pre-Finished.
                    }
                }

                // Report the frame's statistics BEFORE FinishedProcessingFrame — while the frame
                // is still the swap-chain's open frame (the case-#4 A/B — see
                // `frame_stats_enabled` and the wdk-iddcx wrapper doc). COMPLETED status, the
                // metadata's present number (64→32-bit truncation wraps after ~4 billion frames —
                // irrelevant), acquire and publish QPC brackets, and the surface's pixel/byte
                // magnitudes. Best-effort: a failing HRESULT is logged once (with the sent
                // values) and never breaks the drain loop — draining is load-bearing, statistics
                // are advisory.
                if frame_stats_enabled() {
                    let mut stats_in = pod_init!(IDARG_IN_REPORTFRAMESTATISTICS);
                    let s = &mut stats_in.FrameStatistics;
                    let pfn_raw = buffer.MetaData.PresentationFrameNumber;
                    let send_stop_qpc = qpc();
                    #[allow(clippy::cast_possible_truncation)]
                    let step_size = size_of::<iddcx::IDDCX_FRAME_STATISTICS_STEP>() as u32;
                    // The processing-step array. The DDI's validator REJECTS an empty one
                    // (`FrameProcessingStepsCount = 0` + null pointer) with
                    // STATUS_INVALID_PARAMETER even though every documented field is set per
                    // contract — measured on .173, 2026-08-29, by probing the shapes. Our one
                    // "processing step" is the copy into the host ring, reported as the
                    // ENCODE start/end interval. Element-wise init: bindgen derives no `Copy`.
                    let mut steps = [
                        pod_init!(iddcx::IDDCX_FRAME_STATISTICS_STEP),
                        pod_init!(iddcx::IDDCX_FRAME_STATISTICS_STEP),
                    ];
                    steps[0].Size = step_size;
                    steps[0].Type = iddcx::IDDCX_FRAME_STATISTICS_STEP_TYPE::IDDCX_FRAME_STATISTICS_STEP_TYPE_ENCODE_START;
                    steps[0].QpcTime = send_start_qpc;
                    steps[1].Size = step_size;
                    steps[1].Type = iddcx::IDDCX_FRAME_STATISTICS_STEP_TYPE::IDDCX_FRAME_STATISTICS_STEP_TYPE_ENCODE_END;
                    steps[1].QpcTime = send_stop_qpc;
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        s.Size = size_of::<iddcx::IDDCX_FRAME_STATISTICS>() as u32;
                        s.PresentationFrameNumber = pfn_raw as u32;
                    }
                    s.FrameStatus = iddcx::IDDCX_FRAME_STATUS::IDDCX_FRAME_STATUS_COMPLETED;
                    // An unsliced frame reports FrameSliceTotal=1 with CurrentSlice=0.
                    s.FrameSliceTotal = 1;
                    s.FrameAcquireQpcTime = acquire_qpc;
                    s.SendStartQpcTime = send_start_qpc;
                    s.SendStopQpcTime = send_stop_qpc;
                    // SendCompleteQpcTime stays zero: the contract reserves it for drivers with
                    // an asynchronous transmit completion routine, which this publish is not.
                    s.FrameProcessingStepsCount = 2;
                    s.pFrameProcessingStep = steps.as_mut_ptr();
                    s.ProcessedPixelCount = stat_pixels;
                    s.FrameSizeInBytes = stat_bytes;
                    // Remaining fields stay zeroed: no re-encodes, `CurrentSlice` 0, default flags.
                    // SAFETY: driver is loaded; `swap_chain` is valid; `stats_in` is a filled
                    // local and `steps` outlives the synchronous call.
                    let hr = unsafe {
                        wdk_iddcx::IddCxSwapChainReportFrameStatistics(swap_chain, &stats_in)
                    };
                    if !logged_stats {
                        dbglog!(
                            "[pf-vd] swap-chain run_core: first frame-statistics report (target={target_id}) rc={hr:#x} pfn={pfn_raw} px={stat_pixels} bytes={stat_bytes}"
                        );
                        logged_stats = true;
                    }
                }

                // SAFETY: driver is loaded; `swap_chain` is valid.
                let hr = unsafe { wdk_iddcx::IddCxSwapChainFinishedProcessingFrame(swap_chain) };
                if !hr_success(hr) {
                    break;
                }
            } else {
                // The swap-chain was likely abandoned (e.g. DXGI_ERROR_ACCESS_LOST) — exit the loop.
                break;
            }
        }

        // STEP 6 sibling-join fix: the drain loop exited (the OS unassigned this swap-chain — typically
        // because a SIBLING display churned the desktop topology — or it errored), but the host-owned
        // ring the publisher holds is still live. Hand it to the monitor so the NEXT worker assigned to
        // this monitor resumes publishing into the same ring instead of freezing (the host re-delivers
        // the channel only on a ring recreate). If the monitor is GONE (a genuine teardown, not a flap),
        // `preserve_publisher` hands the publisher back inside the `Err` and dropping the returned
        // `Result` closes the ring handles here — no leak, no stale ring left behind.
        if let Some(p) = publisher.take() {
            let _ = crate::monitor::preserve_publisher(target_id, p);
        }
        // Preserve the first-frame stash alongside (dropped inside if empty or the monitor is gone
        // — it owns no handles, just a driver-private texture). The next worker on this adapter
        // re-adopts it, so the retained frame survives the flap too.
        crate::monitor::preserve_stash(target_id, render_luid_low, render_luid_high, stash);
    }
}

impl Drop for SwapChainProcessor {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.take() {
            // signal the worker to end
            self.terminate.store(true, Ordering::Relaxed);
            // wait until the worker is finished (it deletes the swap-chain object before returning)
            let _ = handle.join();
        }
    }
}
