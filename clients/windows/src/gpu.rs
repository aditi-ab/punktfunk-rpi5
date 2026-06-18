//! The single Direct3D 11 device shared by the video decoder (D3D11VA hardware decode) and the
//! presenter (the `SwapChainPanel` composition swapchain + the present draw).
//!
//! Zero-copy hardware decode requires FFmpeg to decode HEVC into `ID3D11Texture2D`s created by the
//! **same** device the presenter binds as shader resources and draws with — a texture from one
//! device can't be sampled by another. So the device is created once, here, and both subsystems
//! pull it from a process-global `OnceLock` (initialised on whichever thread asks first: the
//! session pump when it builds the decoder, or the UI thread when it builds the presenter).
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
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};

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

fn create_device() -> Result<SharedDevice> {
    // Preference order: a hardware adapter with video support (enables D3D11VA); the same without
    // the VIDEO flag (a driver that rejects it still presents + software-decodes); finally WARP for
    // the GPU-less box. BGRA_SUPPORT is required for the composition swapchain in every case.
    let attempts = [
        (D3D_DRIVER_TYPE_HARDWARE, true, true),
        (D3D_DRIVER_TYPE_HARDWARE, false, true),
        (D3D_DRIVER_TYPE_WARP, false, false),
    ];
    for (driver, video, hardware) in attempts {
        let flags = if video {
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT
        } else {
            D3D11_CREATE_DEVICE_BGRA_SUPPORT
        };
        let mut device = None;
        let mut context = None;
        let r = unsafe {
            D3D11CreateDevice(
                None,
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
            // Make the device + immediate context free-threaded: the decoder (D3D11VA video context,
            // pump thread) and the presenter (immediate context, UI thread) both touch this device.
            // FFmpeg also sets this during hwdevice init, but doing it up front keeps the
            // cross-thread `Send`/`Sync` sound from the moment the device exists.
            if let Ok(mt) = context.cast::<ID3D11Multithread>() {
                unsafe {
                    let _ = mt.SetMultithreadProtected(true); // returns the prior state; ignore
                }
            }
            tracing::info!(
                driver = if hardware {
                    "hardware"
                } else {
                    "WARP (software)"
                },
                video,
                "shared D3D11 device created"
            );
            return Ok(SharedDevice {
                device,
                context,
                hardware,
            });
        }
    }
    Err(anyhow!(
        "D3D11CreateDevice failed for both hardware and WARP"
    ))
}
