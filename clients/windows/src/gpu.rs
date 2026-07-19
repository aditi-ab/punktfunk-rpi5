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
///
/// **Deduplicated by description**, because the description IS the identity everywhere
/// downstream: the pick is persisted as that string (`Settings::adapter`) and matched by
/// name in the session binary (`PUNKTFUNK_VK_ADAPTER`). So two entries with the same name
/// are one selectable choice however many times DXGI enumerates them — listing it twice
/// only offers the user a meaningless coin flip. Seen live on an Intel Arc laptop
/// (2026-07-19), whose Vulkan ICD likewise enumerates the one physical iGPU twice.
pub fn adapter_names() -> Vec<String> {
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2; // dxgi.h; not in this windows-rs feature set
    let mut names: Vec<String> = Vec::new();
    for a in all_adapters() {
        let desc1 = a
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIAdapter1>()
            .and_then(|a1| unsafe { a1.GetDesc1() })
            .ok();
        let name = adapter_name(&a);
        // Forensics for the next duplicate/oddity report — which adapters DXGI actually
        // returned, and whether the repeats share a LUID (one adapter enumerated twice)
        // or are distinct devices that merely present the same description.
        if let Some(d) = &desc1 {
            tracing::debug!(
                name = %name,
                luid = format!("{:08x}-{:08x}", d.AdapterLuid.HighPart, d.AdapterLuid.LowPart),
                vendor = format_args!("{:#06x}", d.VendorId),
                device = format_args!("{:#06x}", d.DeviceId),
                flags = d.Flags,
                "DXGI adapter"
            );
        }
        if desc1.is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0) {
            continue; // WARP / software renderer — never a streaming target
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}
