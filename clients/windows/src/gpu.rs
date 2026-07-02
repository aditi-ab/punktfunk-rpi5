//! The single Direct3D 11 device shared by the video decoder (D3D11VA hardware decode) and the
//! presenter (the `SwapChainPanel` composition swapchain + the present draw).
//!
//! Zero-copy hardware decode requires FFmpeg to decode HEVC into `ID3D11Texture2D`s created by the
//! **same** device the presenter binds as shader resources and draws with — a texture from one
//! device can't be sampled by another. So the device is created once, here, and both subsystems
//! pull it from a process-global `OnceLock` (initialised on whichever thread asks first: the
//! session pump when it builds the decoder, or the UI thread when it builds the presenter).
//!
//! **Adapter selection** (matters on hybrid boxes — e.g. an Intel iGPU driving the panel next to
//! an NVIDIA dGPU): `PUNKTFUNK_ADAPTER` (index or case-insensitive name substring, a debugging
//! override) wins; else the persisted Settings GPU pick ([`crate::trust::Settings::adapter`], the
//! Settings-page selector on multi-GPU boxes); else the adapter whose output owns the monitor our
//! window is on — that's the adapter DWM composes that monitor with, so presents are copy-free
//! and decode runs on the near GPU; else the default adapter. Deliberately NOT "the adapter with
//! the best decoder": if the monitor's adapter can't decode the codec we demote to software,
//! which beats a per-frame cross-adapter present copy. The device is cached **keyed by the
//! resolved preference**, so a Settings change takes effect at the next session (the pump and the
//! presenter both resolve at session start and read the same value) without an app restart.
//!
//! `PUNKTFUNK_D3D_DEBUG=1` adds the D3D11 debug layer (validation messages in the debugger /
//! DebugView) — invaluable for present-path bugs, which D3D11 otherwise drops silently.
//!
//! **Thread-safety.** windows-rs COM interfaces are deliberately `!Send`/`!Sync` — thread-safety
//! is per-object, not universal. An `ID3D11Device` and its immediate context become free-threaded
//! once `ID3D11Multithread::SetMultithreadProtected(TRUE)` is set, which FFmpeg's D3D11VA backend
//! does inside `av_hwdevice_ctx_init` (it installs an `ID3D11Multithread`-based default lock when we
//! leave `AVD3D11VADeviceContext.lock` null). The decoder then uses FFmpeg's separate
//! `ID3D11VideoContext` for decode while the presenter uses the immediate context for draw; under
//! multithread protection D3D serialises the two internally, and decode/draw touch disjoint context
//! state. That makes the `unsafe impl Send + Sync` below sound for exactly this usage.

use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

pub struct SharedDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// True when this is a real GPU (hardware) adapter — a precondition for D3D11VA decode. WARP
    /// (the GPU-less dev box) creates fine for present but cannot hardware-decode HEVC, so the
    /// decoder skips straight to the software path there.
    pub hardware: bool,
}

// Sound for our usage — see the module docs: the device + immediate context are free-threaded under
// the multithread protection FFmpeg installs, and decode (video context) / present (immediate
// context) never share mutable context state.
unsafe impl Send for SharedDevice {}
unsafe impl Sync for SharedDevice {}

/// The shared device, cached with the GPU preference it was resolved from (empty = automatic).
/// Re-created when the preference changes — in practice only between sessions: within one session
/// the decoder and the presenter both call [`shared`] at session start with the same value.
static SHARED: Mutex<Option<(String, Arc<SharedDevice>)>> = Mutex::new(None);

/// The user's decode/present GPU preference: the `PUNKTFUNK_ADAPTER` env (debugging override)
/// wins, else the persisted Settings pick; empty = automatic.
fn adapter_pref() -> String {
    std::env::var("PUNKTFUNK_ADAPTER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::trust::Settings::load().adapter)
}

/// The process-shared D3D11 device for the current GPU preference, created (or re-created after
/// a preference change) on demand. `None` only if D3D11 device creation fails for both a hardware
/// adapter and WARP (effectively never — WARP is always present).
pub fn shared() -> Option<Arc<SharedDevice>> {
    let pref = adapter_pref();
    let mut cached = SHARED.lock().unwrap();
    if let Some((key, dev)) = cached.as_ref() {
        if *key == pref {
            return Some(dev.clone());
        }
    }
    match create_device(&pref) {
        Ok(d) => {
            let d = Arc::new(d);
            *cached = Some((pref, d.clone()));
            Some(d)
        }
        Err(e) => {
            tracing::error!(error = %e, "shared D3D11 device creation failed — no present/decode");
            None
        }
    }
}

/// The adapter's human-readable description, for the logs.
fn adapter_name(adapter: &IDXGIAdapter) -> String {
    unsafe {
        adapter
            .GetDesc()
            .map(|d| {
                String::from_utf16_lossy(&d.Description)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_else(|_| "<unknown adapter>".into())
    }
}

/// Every DXGI adapter, in enumeration order (`PUNKTFUNK_ADAPTER=<index>` uses these indices).
fn all_adapters() -> Vec<IDXGIAdapter> {
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut v = Vec::new();
    let mut i = 0u32;
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        if let Ok(a) = a.cast::<IDXGIAdapter>() {
            v.push(a);
        }
    }
    v
}

/// Descriptions of the real (hardware, non-WARP) GPUs — the Settings GPU picker's option list.
/// The picker only shows when this has more than one entry.
pub fn adapter_names() -> Vec<String> {
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2; // dxgi.h; not in this windows-rs feature set
    all_adapters()
        .iter()
        .filter(|a| {
            a.cast::<windows::Win32::Graphics::Dxgi::IDXGIAdapter1>()
                .and_then(|a1| unsafe { a1.GetDesc1() })
                .map(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE == 0)
                .unwrap_or(true)
        })
        .map(adapter_name)
        .collect()
}

/// Resolve an explicit adapter: a non-empty `pref` (index or case-insensitive name substring, from
/// env or Settings) wins; else the adapter whose output owns the monitor the app window is on (see
/// module docs); else `None` → the default adapter (also the headless-CLI path with no window).
fn resolve_adapter(pref: &str) -> Option<IDXGIAdapter> {
    let adapters = all_adapters();

    if !pref.is_empty() {
        let found = if let Ok(idx) = pref.parse::<usize>() {
            adapters.get(idx).cloned()
        } else {
            let needle = pref.to_lowercase();
            adapters
                .iter()
                .find(|a| adapter_name(a).to_lowercase().contains(&needle))
                .cloned()
        };
        match &found {
            Some(a) => tracing::info!(pref, adapter = %adapter_name(a), "GPU preference matched"),
            None => tracing::warn!(pref, "GPU preference matched no adapter — using automatic"),
        }
        if found.is_some() {
            return found;
        }
    }

    // The adapter driving the monitor our window sits on: DWM composes that monitor with it, so
    // presenting from it is copy-free (a hybrid box's other adapter would pay a cross-adapter
    // copy per frame).
    let monitor = unsafe {
        use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
        use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
        let hwnd = FindWindowW(None, windows::core::w!("Punktfunk")).ok()?;
        MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL)
    };
    if monitor.is_invalid() {
        return None;
    }
    for adapter in &adapters {
        let mut oi = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(oi) } {
            oi += 1;
            if let Ok(desc) = unsafe { output.GetDesc() } {
                if desc.Monitor == monitor {
                    tracing::info!(adapter = %adapter_name(adapter), "using the window's monitor adapter");
                    return Some(adapter.clone());
                }
            }
        }
    }
    None
}

fn create_device(pref: &str) -> Result<SharedDevice> {
    // Preference order: the resolved adapter (or the default hardware adapter) with video support
    // (enables D3D11VA); the same without the VIDEO flag (a driver that rejects it still presents +
    // software-decodes); finally WARP for the GPU-less box. BGRA_SUPPORT is required for the
    // composition swapchain in every case. An explicit adapter requires D3D_DRIVER_TYPE_UNKNOWN.
    let adapter = resolve_adapter(pref);
    let attempts: [(Option<&IDXGIAdapter>, D3D_DRIVER_TYPE, bool, bool); 3] = match &adapter {
        Some(a) => [
            (Some(a), D3D_DRIVER_TYPE_UNKNOWN, true, true),
            (Some(a), D3D_DRIVER_TYPE_UNKNOWN, false, true),
            (None, D3D_DRIVER_TYPE_WARP, false, false),
        ],
        None => [
            (None, D3D_DRIVER_TYPE_HARDWARE, true, true),
            (None, D3D_DRIVER_TYPE_HARDWARE, false, true),
            (None, D3D_DRIVER_TYPE_WARP, false, false),
        ],
    };
    // The debug layer needs the SDK layers installed (Graphics Tools); when they're missing the
    // creation fails, so each attempt retries without the flag rather than failing the ladder.
    let debug = std::env::var("PUNKTFUNK_D3D_DEBUG").is_ok_and(|v| v == "1");
    for (adapter, driver, video, hardware) in attempts {
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if video {
            flags |= D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        }
        let flag_sets: &[D3D11_CREATE_DEVICE_FLAG] = if debug {
            &[flags | D3D11_CREATE_DEVICE_DEBUG, flags]
        } else {
            &[flags]
        };
        for &flags in flag_sets {
            let mut device = None;
            let mut context = None;
            let r = unsafe {
                D3D11CreateDevice(
                    adapter,
                    driver,
                    None,
                    flags,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };
            if r.is_ok() {
                let (device, context) = (device.unwrap(), context.unwrap());
                // Make the device + immediate context free-threaded: the decoder (D3D11VA video
                // context, pump thread) and the presenter (immediate context, render thread) both
                // touch this device. FFmpeg also sets this during hwdevice init, but doing it up
                // front keeps the cross-thread `Send`/`Sync` sound from the moment the device exists.
                if let Ok(mt) = context.cast::<ID3D11Multithread>() {
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true); // returns the prior state; ignore
                    }
                }
                tracing::info!(
                    adapter = %adapter.map(adapter_name).unwrap_or_else(|| if hardware {
                        "default".into()
                    } else {
                        "WARP (software)".into()
                    }),
                    video,
                    debug = (flags & D3D11_CREATE_DEVICE_DEBUG).0 != 0,
                    "shared D3D11 device created"
                );
                return Ok(SharedDevice {
                    device,
                    context,
                    hardware,
                });
            }
        }
    }
    Err(anyhow!(
        "D3D11CreateDevice failed for both hardware and WARP"
    ))
}
