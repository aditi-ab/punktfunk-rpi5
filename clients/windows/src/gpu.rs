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
//! an NVIDIA dGPU): `PUNKTFUNK_ADAPTER` (index or case-insensitive name substring) wins; else the
//! adapter whose output owns the monitor our window is on — that's the adapter DWM composes that
//! monitor with, so presents are copy-free and decode runs on the near GPU; else the default
//! adapter. Deliberately NOT "the adapter with the best decoder": if the monitor's adapter can't
//! decode the codec we demote to software, which beats a per-frame cross-adapter present copy.
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
use std::sync::OnceLock;
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

static SHARED: OnceLock<Option<SharedDevice>> = OnceLock::new();

/// The process-wide shared D3D11 device, created on first call. `None` only if D3D11 device
/// creation fails for both a hardware adapter and WARP (effectively never — WARP is always present).
pub fn shared() -> Option<&'static SharedDevice> {
    SHARED.get_or_init(create).as_ref()
}

fn create() -> Option<SharedDevice> {
    match create_device() {
        Ok(d) => Some(d),
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

/// Resolve an explicit adapter: `PUNKTFUNK_ADAPTER` (index or case-insensitive name substring)
/// wins; else the adapter whose output owns the monitor the app window is on (see module docs);
/// else `None` → the default adapter (also the headless-CLI path, where no window exists).
fn resolve_adapter() -> Option<IDXGIAdapter> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let adapters: Vec<IDXGIAdapter> = {
        let mut v = Vec::new();
        let mut i = 0u32;
        while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            if let Ok(a) = a.cast::<IDXGIAdapter>() {
                v.push(a);
            }
        }
        v
    };

    if let Ok(pref) = std::env::var("PUNKTFUNK_ADAPTER") {
        let pref = pref.trim();
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
            Some(a) => {
                tracing::info!(pref, adapter = %adapter_name(a), "PUNKTFUNK_ADAPTER matched")
            }
            None => tracing::warn!(pref, "PUNKTFUNK_ADAPTER matched no adapter — using default"),
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

fn create_device() -> Result<SharedDevice> {
    // Preference order: the resolved adapter (or the default hardware adapter) with video support
    // (enables D3D11VA); the same without the VIDEO flag (a driver that rejects it still presents +
    // software-decodes); finally WARP for the GPU-less box. BGRA_SUPPORT is required for the
    // composition swapchain in every case. An explicit adapter requires D3D_DRIVER_TYPE_UNKNOWN.
    let adapter = resolve_adapter();
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
