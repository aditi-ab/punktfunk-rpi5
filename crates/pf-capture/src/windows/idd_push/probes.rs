//! Phase A.2 micro-probes (stall attribution, `vdisplay-disturbance-immunity.md` §4.2): a small
//! engine of watchdogged sacrificial threads that continuously measure the legs a capture stall
//! could hide in, so the stall reporter can read back "what was alive during the hole":
//!
//! - **fence** (one thread per hardware adapter): a tiny CopyResource + D3D11 fence round-trip —
//!   engine liveness. A Level-Two/Three adapter freeze ("graphics hardware is idle") stalls this
//!   for the freeze's duration on EVERY engine of that adapter.
//! - **dwm-tick**: `DwmGetCompositionTimingInfo(NULL)` `cRefresh` advance — the compositor's own
//!   clock. Frozen tick with live fences = DWM (or something it waits on) is blocked, not the GPU.
//!   The same sample also tracks `cFrame` (the COMPOSED-frame counter) — the compose-silence
//!   discriminator: a clock that ticks while `cFrame` freezes means DWM had NOTHING to compose
//!   (the foreground content stopped presenting); a `cFrame` that keeps advancing through a hole
//!   means DWM built frames that never reached OUR swap-chain (the OS frame-generation leg).
//! - **dwm-flush**: a watchdogged `DwmFlush` — its latency IS the composition-wait measurement.
//! - **scanline**: `D3DKMTGetScanLine` on an active output (Level-Zero reentrant — documented safe
//!   against every miniport lock, so a BLOCKED call here convicts the KMD itself). Prefers a
//!   physical head; on an exclusive topology only our IDD is active and the call's latency is
//!   still the KMD-liveness signal ([`pf_win_display::win_display::active_scanline_target`]).
//! - **cpu**: a high-res sleeper measuring overshoot — the DPC-storm / CPU-starvation
//!   discriminator (a machine-wide scheduling hole inflates every other probe too).
//!
//! The blocking of a probe is its measurement — none of these ever run on the capture/encode
//! path. Threads are DETACHED (never joined): a probe wedged inside a kernel call for the stall's
//! duration exits at its next loop turn after the stop flag flips. One engine per process
//! ([`acquire`]), refcounted across parallel capturers; probes sample at 20 Hz or slower and cost
//! microseconds each, so the engine is invisible next to a streaming session.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use windows::core::{s, w, Interface};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_FENCE_FLAG_NONE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dwm::{DwmFlush, DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::stall::ProbeWindow;

/// One probe's sample ring: `(completed_at, span, value_us)` — `value` is the measurement (a call
/// latency or a frozen-span/overshoot), `span` the wall interval it describes ending at
/// `completed_at`. Capped; ~20 Hz per probe → several minutes of coverage.
struct Ring {
    samples: Mutex<VecDeque<(Instant, Duration, u64)>>,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(512)),
        }
    }

    fn push(&self, completed_at: Instant, span: Duration, value_us: u64) {
        let mut s = self.samples.lock().unwrap();
        if s.len() == 512 {
            s.pop_front();
        }
        s.push_back((completed_at, span, value_us));
    }

    /// Max `value` among samples whose described interval `[completed_at - span, completed_at]`
    /// intersects `[from, to]`; `None` when the ring holds nothing for the window (probe absent,
    /// or it started after the window).
    fn window_max(&self, from: Instant, to: Instant) -> Option<u64> {
        let s = self.samples.lock().unwrap();
        let mut max: Option<u64> = None;
        for (end, span, value) in s.iter() {
            let start = end.checked_sub(*span).unwrap_or(*end);
            if start <= to && *end >= from {
                max = Some(max.map_or(*value, |m| m.max(*value)));
            }
        }
        max
    }
}

/// A blocking probe's "currently inside the call since" marker: a call still blocked when the
/// reporter reads the window is exactly the interesting case, and its ring sample does not exist
/// yet — the marker's age stands in for it.
struct InFlight {
    since: Mutex<Option<Instant>>,
}

impl InFlight {
    fn new() -> Self {
        Self {
            since: Mutex::new(None),
        }
    }

    fn enter(&self) -> Instant {
        let now = Instant::now();
        *self.since.lock().unwrap() = Some(now);
        now
    }

    fn exit(&self) {
        *self.since.lock().unwrap() = None;
    }

    /// Age (µs) of a call still in flight that started before `to`; 0 when idle.
    fn blocked_us(&self, to: Instant) -> u64 {
        match *self.since.lock().unwrap() {
            Some(since) if since <= to => to.duration_since(since).as_micros() as u64,
            _ => 0,
        }
    }
}

/// A blocking probe = ring + in-flight marker; `window_max` folds both.
struct BlockingProbe {
    ring: Ring,
    inflight: InFlight,
}

impl BlockingProbe {
    fn new() -> Self {
        Self {
            ring: Ring::new(),
            inflight: InFlight::new(),
        }
    }

    fn measure(&self, f: impl FnOnce()) {
        let t0 = self.inflight.enter();
        f();
        let dur = t0.elapsed();
        self.inflight.exit();
        self.ring.push(Instant::now(), dur, dur.as_micros() as u64);
    }

    fn window_max(&self, from: Instant, to: Instant) -> Option<u64> {
        let ring = self.ring.window_max(from, to);
        let blocked = self.inflight.blocked_us(to);
        match (ring, blocked) {
            (None, 0) => None,
            (r, b) => Some(r.unwrap_or(0).max(b)),
        }
    }
}

struct Inner {
    stop: AtomicBool,
    /// One per hardware adapter, labelled by LUID (hybrids run two).
    fences: Vec<(String, BlockingProbe)>,
    dwm_tick: Ring,
    /// `cFrame` (composed-frame counter) frozen spans, sampled by the same dwm-tick thread.
    dwm_frame: Ring,
    dwm_flush: BlockingProbe,
    scanline: BlockingProbe,
    /// Whether the scanline probe currently targets a PHYSICAL head (see the module docs).
    scanline_physical: AtomicBool,
    scanline_running: AtomicBool,
    cpu: Ring,
}

/// The process-wide micro-probe engine. Hold the `Arc` for the session's lifetime; the last drop
/// stops every thread at its next loop turn.
pub(super) struct ProbeEngine {
    inner: Arc<Inner>,
}

impl Drop for ProbeEngine {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
    }
}

static ENGINE: Mutex<Weak<ProbeEngine>> = Mutex::new(Weak::new());

/// The engine, started on first use and shared across parallel capturers (probe threads are
/// per-PROCESS facts — two capturers asking the same questions twice would only add noise).
pub(super) fn acquire() -> Arc<ProbeEngine> {
    let mut g = ENGINE.lock().unwrap();
    if let Some(e) = g.upgrade() {
        return e;
    }
    let e = Arc::new(ProbeEngine::start());
    *g = Arc::downgrade(&e);
    e
}

impl ProbeEngine {
    /// What the probes saw across `[from, to]` — the stall reporter's evidence read. Cheap: five
    /// short mutex-held ring scans.
    pub(super) fn window(&self, from: Instant, to: Instant) -> ProbeWindow {
        let i = &self.inner;
        ProbeWindow {
            fence_max_us: i
                .fences
                .iter()
                .filter_map(|(_, p)| p.window_max(from, to))
                .max(),
            dwm_tick_frozen_us: i.dwm_tick.window_max(from, to),
            dwm_frame_frozen_us: i.dwm_frame.window_max(from, to),
            dwm_flush_max_us: i.dwm_flush.window_max(from, to),
            scanline_max_us: if i.scanline_running.load(Ordering::Relaxed) {
                i.scanline.window_max(from, to)
            } else {
                None
            },
            scanline_physical: i.scanline_physical.load(Ordering::Relaxed),
            cpu_max_overshoot_us: i.cpu.window_max(from, to),
        }
    }

    fn start() -> Self {
        let fences = enumerate_hardware_adapters()
            .into_iter()
            .map(|(label, _)| (label, BlockingProbe::new()))
            .collect();
        let inner = Arc::new(Inner {
            stop: AtomicBool::new(false),
            fences,
            dwm_tick: Ring::new(),
            dwm_frame: Ring::new(),
            dwm_flush: BlockingProbe::new(),
            scanline: BlockingProbe::new(),
            scanline_physical: AtomicBool::new(false),
            scanline_running: AtomicBool::new(false),
            cpu: Ring::new(),
        });
        // Fence probes re-enumerate to pair each adapter with its probe slot by index (the two
        // enumerations run back-to-back; a hot-plugged GPU between them only skips a probe).
        for (idx, (label, adapter)) in enumerate_hardware_adapters().into_iter().enumerate() {
            let inner_t = inner.clone();
            spawn_detached(&format!("pf-probe-fence-{idx}"), move || {
                fence_probe_loop(&inner_t, idx, &label, &adapter);
            });
        }
        let inner_t = inner.clone();
        spawn_detached("pf-probe-dwm-tick", move || dwm_tick_loop(&inner_t));
        let inner_t = inner.clone();
        spawn_detached("pf-probe-dwm-flush", move || dwm_flush_loop(&inner_t));
        let inner_t = inner.clone();
        spawn_detached("pf-probe-scanline", move || scanline_loop(&inner_t));
        let inner_t = inner.clone();
        spawn_detached("pf-probe-cpu", move || cpu_sentinel_loop(&inner_t));
        tracing::debug!(
            fence_probes = inner.fences.len(),
            "stall-attribution micro-probes started (fence/dwm-tick/dwm-flush/scanline/cpu)"
        );
        Self { inner }
    }
}

/// Spawn a detached probe thread — never joined by design (see the module docs).
fn spawn_detached(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        tracing::debug!(name, error = %e, "micro-probe thread failed to spawn — probe absent");
    }
}

/// Hardware (non-software) DXGI adapters, labelled by LUID. Display-only/indirect adapters are
/// filtered later by their device-creation failure, not here.
fn enumerate_hardware_adapters() -> Vec<(String, IDXGIAdapter1)> {
    let mut out = Vec::new();
    // SAFETY: plain factory creation; the result is checked.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    for i in 0.. {
        // SAFETY: `factory` is live; enumeration ends at DXGI_ERROR_NOT_FOUND (the Err arm).
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: `adapter` is live; `desc` is a valid out-param.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        out.push((
            format!(
                "{:08x}:{:08x}",
                desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
            ),
            adapter,
        ));
    }
    out
}

/// One adapter's engine-liveness loop: submit a trivial copy, signal a fence, wait on its event.
/// The wait latency is the sample. Device-creation failure = probe absent for this adapter
/// (display-only/indirect adapters land here).
fn fence_probe_loop(inner: &Inner, idx: usize, label: &str, adapter: &IDXGIAdapter1) {
    let Some((context, ctx4, fence, event, (a, b))) = fence_probe_setup(adapter) else {
        tracing::debug!(
            adapter = label,
            "fence probe: no device on this adapter — absent"
        );
        return;
    };
    let mut value = 0u64;
    let Some((_, probe)) = inner.fences.get(idx) else {
        return;
    };
    while !inner.stop.load(Ordering::Relaxed) {
        value += 1;
        probe.measure(|| {
            // SAFETY: all COM objects are live for this loop's lifetime (owned above); `a`/`b`
            // are same-device, same-desc textures; the event handle is ours. The fence signal
            // orders after the queued copy, so the wait measures the engine actually processing
            // work; the 10 s ceiling only bounds a truly wedged adapter (the timeout sample still
            // records — that IS the measurement).
            unsafe {
                context.CopyResource(&a, &b);
                let _ = ctx4.Signal(&fence, value);
                if fence.SetEventOnCompletion(value, event).is_ok() {
                    let _ = WaitForSingleObject(event, 10_000);
                }
            }
        });
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: the loop exited; this thread owns `event` and closes it exactly once.
    unsafe {
        let _ = CloseHandle(event);
    }
}

/// The fence probe's D3D plumbing: a device on `adapter`, its fence, the wait event, and two tiny
/// textures kept alive for the copies. `None` when any piece is unavailable (pre-FL11 or
/// display-only adapters, fence-less runtimes).
#[allow(clippy::type_complexity)]
fn fence_probe_setup(
    adapter: &IDXGIAdapter1,
) -> Option<(
    windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    ID3D11DeviceContext4,
    ID3D11Fence,
    HANDLE,
    (ID3D11Texture2D, ID3D11Texture2D),
)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: adapter is live; UNKNOWN driver type is the documented mode for an explicit
    // adapter; out-params are valid locals checked below.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .ok()?;
    }
    let device = device?;
    let context = context?;
    let device5: ID3D11Device5 = device.cast().ok()?;
    let ctx4: ID3D11DeviceContext4 = context.cast().ok()?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 16,
        Height: 16,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        ..Default::default()
    };
    let mut a = None;
    let mut b = None;
    // SAFETY: `device` is live, `desc` fully initialized, out-params valid locals checked below.
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut a)).ok()?;
        device.CreateTexture2D(&desc, None, Some(&mut b)).ok()?;
    }
    let (a, b) = (a?, b?);
    let mut fence = None;
    // SAFETY: `device5` is live; out-param is a valid local checked below.
    unsafe {
        device5
            .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)
            .ok()?;
    }
    let fence: ID3D11Fence = fence?;
    // SAFETY: plain event creation (auto-reset, unnamed); checked below, closed by the loop.
    let event = unsafe { CreateEventW(None, false, false, None) }.ok()?;
    Some((context, ctx4, fence, event, (a, b)))
}

/// `cRefresh` + `cFrame` advance sampling: each tick records how long the compositor's refresh
/// counter and its COMPOSED-frame counter have been unchanged. A frozen refresh span covering a
/// stall (with live fences) = DWM blocked, not the GPU. A LIVE refresh with a frozen `cFrame` =
/// DWM ticked but composed nothing — no damage anywhere, i.e. the foreground content itself
/// stopped presenting; a `cFrame` that advances through a capture hole = DWM composed frames the
/// IDD swap-chain never received (the OS frame-generation leg dropped them).
fn dwm_tick_loop(inner: &Inner) {
    let mut last_refresh = 0u64;
    let mut last_change = Instant::now();
    let mut last_frame = 0u64;
    let mut last_frame_change = Instant::now();
    while !inner.stop.load(Ordering::Relaxed) {
        let mut info = DWM_TIMING_INFO {
            cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
            ..Default::default()
        };
        // SAFETY: a NULL hwnd asks for composition-wide timing; `info` is a valid local with
        // `cbSize` stamped (the API's byte-count contract).
        if unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.is_ok() {
            let now = Instant::now();
            if info.cRefresh != last_refresh {
                last_refresh = info.cRefresh;
                last_change = now;
            }
            let frozen = now.duration_since(last_change);
            inner.dwm_tick.push(now, frozen, frozen.as_micros() as u64);
            if info.cFrame != last_frame {
                last_frame = info.cFrame;
                last_frame_change = now;
            }
            let frame_frozen = now.duration_since(last_frame_change);
            inner
                .dwm_frame
                .push(now, frame_frozen, frame_frozen.as_micros() as u64);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Watchdogged `DwmFlush`: blocks until the next composition — the wait IS the measurement.
fn dwm_flush_loop(inner: &Inner) {
    while !inner.stop.load(Ordering::Relaxed) {
        inner.dwm_flush.measure(|| {
            // SAFETY: no arguments, no state — the call blocks until DWM composes (or fails
            // immediately when composition is unavailable; both durations are valid samples).
            let _ = unsafe { DwmFlush() };
        });
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `D3DKMT_OPENADAPTERFROMLUID` (d3dkmthk.h): LUID in, kernel adapter handle out.
#[repr(C)]
struct D3dkmtOpenAdapterFromLuid {
    adapter_luid: LUID,
    h_adapter: u32,
}

/// `D3DKMT_GETSCANLINE` (d3dkmthk.h): `InVerticalBlank` is a C `BOOLEAN` (u8) — the pad bytes
/// before the 4-aligned `scan_line` mirror the C layout exactly (size assert below).
#[repr(C)]
struct D3dkmtGetScanline {
    h_adapter: u32,
    vidpn_source_id: u32,
    in_vertical_blank: u8,
    _pad: [u8; 3],
    scan_line: u32,
}

/// `D3DKMT_CLOSEADAPTER` (d3dkmthk.h).
#[repr(C)]
struct D3dkmtCloseAdapter {
    h_adapter: u32,
}

const _: () = {
    assert!(std::mem::size_of::<D3dkmtOpenAdapterFromLuid>() == 12);
    assert!(std::mem::size_of::<D3dkmtGetScanline>() == 16);
    assert!(std::mem::size_of::<D3dkmtCloseAdapter>() == 4);
};

type D3dkmtFn<T> = unsafe extern "system" fn(*mut T) -> i32;

/// Resolve a `D3DKMT*` entry point from gdi32 (no stable windows-rs bindings — the same
/// GetProcAddress route as pf-frame's scheduling-priority call).
///
/// # Safety
/// `T` must be the exact d3dkmthk.h argument struct for `name` — the transmute binds the
/// signature, and the layout asserts above pin the three structs this module uses.
unsafe fn d3dkmt<T>(name: windows::core::PCSTR) -> Option<D3dkmtFn<T>> {
    // SAFETY: gdi32 is a permanent system module in any process that touched GDI/D3D; the name is
    // a static nul-terminated literal from the caller.
    let gdi32 = unsafe { GetModuleHandleW(w!("gdi32.dll")) }.ok()?;
    // SAFETY: `gdi32` is the live module handle just obtained.
    let p = unsafe { GetProcAddress(gdi32, name) }?;
    // SAFETY: the caller's contract (doc above) — `p` is the named export, whose ABI is
    // `NTSTATUS fn(struct*)` for every D3DKMT entry point this module names.
    Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, D3dkmtFn<T>>(p) })
}

/// Level-Zero KMD-liveness loop: `D3DKMTGetScanLine` against an active output, retargeted every
/// ~2 s (topology moves mid-session). The call's LATENCY is the signal; a blocked Level-Zero DDI
/// convicts the KMD below every OS lever.
fn scanline_loop(inner: &Inner) {
    // SAFETY: `D3dkmtOpenAdapterFromLuid` is the exact d3dkmthk.h struct for this export
    // (layout-asserted above) — the `d3dkmt` safety contract.
    let open = unsafe { d3dkmt::<D3dkmtOpenAdapterFromLuid>(s!("D3DKMTOpenAdapterFromLuid")) };
    // SAFETY: `D3dkmtGetScanline` is the exact d3dkmthk.h struct for this export (asserted above).
    let get = unsafe { d3dkmt::<D3dkmtGetScanline>(s!("D3DKMTGetScanLine")) };
    // SAFETY: `D3dkmtCloseAdapter` is the exact d3dkmthk.h struct for this export (asserted above).
    let close = unsafe { d3dkmt::<D3dkmtCloseAdapter>(s!("D3DKMTCloseAdapter")) };
    let (Some(open), Some(get), Some(close)) = (open, get, close) else {
        tracing::debug!("scanline probe: D3DKMT entry points unavailable — absent");
        return;
    };
    let mut target: Option<(u32, u32)> = None; // (h_adapter, vidpn_source_id)
    let mut ticks = 0u32;
    while !inner.stop.load(Ordering::Relaxed) {
        if target.is_none() || ticks % 20 == 0 {
            if let Some((h, _)) = target.take() {
                let mut req = D3dkmtCloseAdapter { h_adapter: h };
                // SAFETY: `req` is a valid local for the asserted layout; `h` came from a
                // successful open below and is closed exactly once here.
                unsafe { close(&mut req) };
            }
            if let Some((lo, hi, source, physical)) =
                pf_win_display::win_display::active_scanline_target()
            {
                let mut req = D3dkmtOpenAdapterFromLuid {
                    adapter_luid: LUID {
                        LowPart: lo,
                        HighPart: hi,
                    },
                    h_adapter: 0,
                };
                // SAFETY: `req` is a valid local for the asserted layout; the returned handle is
                // owned by this loop and closed on retarget/exit.
                if unsafe { open(&mut req) } >= 0 && req.h_adapter != 0 {
                    target = Some((req.h_adapter, source));
                    inner.scanline_physical.store(physical, Ordering::Relaxed);
                }
            }
            inner
                .scanline_running
                .store(target.is_some(), Ordering::Relaxed);
        }
        ticks = ticks.wrapping_add(1);
        if let Some((h, source)) = target {
            inner.scanline.measure(|| {
                let mut req = D3dkmtGetScanline {
                    h_adapter: h,
                    vidpn_source_id: source,
                    in_vertical_blank: 0,
                    _pad: [0; 3],
                    scan_line: 0,
                };
                // SAFETY: `req` is a valid local for the asserted layout; `h` is the live adapter
                // handle owned by this loop. A failed status is still a timed (valid) sample.
                unsafe { get(&mut req) };
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((h, _)) = target.take() {
        let mut req = D3dkmtCloseAdapter { h_adapter: h };
        // SAFETY: as the retarget close above — owned handle, closed exactly once at exit.
        unsafe { close(&mut req) };
    }
}

/// DPC/starvation sentinel: 5 ms sleeps, overshoot aggregated to one max per ~100 ms bucket.
fn cpu_sentinel_loop(inner: &Inner) {
    let mut bucket_max = 0u64;
    let mut bucket_start = Instant::now();
    while !inner.stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        let overshoot = t0.elapsed().saturating_sub(Duration::from_millis(5));
        bucket_max = bucket_max.max(overshoot.as_micros() as u64);
        let now = Instant::now();
        let span = now.duration_since(bucket_start);
        if span >= Duration::from_millis(100) {
            inner.cpu.push(now, span, bucket_max);
            bucket_max = 0;
            bucket_start = now;
        }
    }
}
