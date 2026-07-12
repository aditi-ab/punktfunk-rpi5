//! DXGI adapter enumeration for the Settings "GPU" picker.
//!
//! Streaming (decode + present) runs in the spawned `punktfunk-session` binary; the shell only
//! needs the list of real (hardware) adapters to offer on a multi-GPU box (a hybrid laptop or an
//! eGPU). The picked adapter description is persisted (`crate::trust::Settings::adapter`) and read
//! by the session child at connect (`PUNKTFUNK_ADAPTER` remains the session binary's env override).

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

/// The adapter's human-readable description.
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

/// Every DXGI adapter, in enumeration order.
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
