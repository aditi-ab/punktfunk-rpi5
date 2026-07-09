//! GPU inventory + operator GPU preference for multi-GPU hosts (web-console GPU selection).
//!
//! Three concerns, one module:
//! - **Enumeration** ([`enumerate`]): the machine's hardware GPUs — DXGI adapters on Windows
//!   (WARP/Basic-Render and indirect-display/display-only adapters filtered out — an IddCx
//!   adapter like our own pf-vdisplay mirrors its render GPU's whole DXGI identity and would
//!   list every GPU twice), `/dev/dri/renderD*` + sysfs PCI ids on Linux, empty elsewhere.
//!   Compiled on every platform so the management endpoints (and the checked-in OpenAPI
//!   document) are identical everywhere.
//! - **Preference** ([`prefs`]): the operator's persisted auto/manual choice
//!   (`<config>/gpu-settings.json`, written by the mgmt API). A manual preference is stored by
//!   *stable identity* — PCI vendor:device + occurrence + name — NOT by LUID (Windows LUIDs are
//!   reassigned every boot) or adapter index (enumeration order can change across driver updates).
//! - **Selection** ([`selected_gpu`] / [`pick`]): the one place that turns (inventory, preference,
//!   `PUNKTFUNK_RENDER_ADAPTER`) into the render/encode GPU. Precedence: **manual preference >
//!   env substring > auto (max dedicated VRAM)**, with graceful fall-through — a preferred GPU
//!   that vanished (unplugged eGPU, disabled iGPU) logs a warning and auto-selects so the host
//!   keeps streaming, and the mgmt API surfaces the fallback instead of hiding it.
//!
//! A preference change applies to the **next session**: selection is read at capture/encode setup
//! (`win_adapter::resolve_render_adapter_luid`, the encoder-backend dispatch, the codec probes), a
//! running session keeps the device it opened on. [`session_begin`]/[`active`] record which GPU a
//! live session actually encodes on, for the console's "in use" display.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// PCI vendor ids of the GPU vendors the encode backends know (NVENC / AMF / QSV, VAAPI on Linux).
pub(crate) const VENDOR_NVIDIA: u32 = 0x10DE;
pub(crate) const VENDOR_AMD: u32 = 0x1002;
pub(crate) const VENDOR_INTEL: u32 = 0x8086;

/// Platform handle of an enumerated GPU — how the pipeline actually addresses it. Not part of the
/// stable identity (Windows LUIDs are per-boot; a render node can renumber across kernel updates).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GpuHandle {
    /// DXGI `AdapterLuid` of this adapter (this boot only).
    #[cfg(target_os = "windows")]
    pub luid_low: u32,
    #[cfg(target_os = "windows")]
    pub luid_high: i32,
    /// DRM render node (`/dev/dri/renderD*`) of this GPU.
    #[cfg(target_os = "linux")]
    pub render_node: Option<PathBuf>,
}

/// One hardware GPU as enumerated on this host.
#[derive(Clone, Debug)]
pub(crate) struct GpuInfo {
    /// Stable identifier for the API/UI: `"{vendor:04x}-{device:04x}-{occurrence}"`. Occurrence
    /// disambiguates identical cards (two of the same model) by enumeration order among their
    /// twins — the best available tiebreaker (PCI order), imperfect but honest.
    pub id: String,
    /// Adapter description (Windows) / synthesized vendor label + node (Linux).
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Index among enumerated GPUs with the same (vendor_id, device_id).
    pub occurrence: u32,
    /// Dedicated VRAM in bytes (0 where the platform doesn't expose it — non-amdgpu Linux sysfs).
    pub vram_bytes: u64,
    pub handle: GpuHandle,
}

/// Lowercase vendor tag for the API (`nvidia` / `amd` / `intel` / `other`).
pub(crate) fn vendor_tag(vendor_id: u32) -> &'static str {
    match vendor_id {
        VENDOR_NVIDIA => "nvidia",
        VENDOR_AMD => "amd",
        VENDOR_INTEL => "intel",
        _ => "other",
    }
}

impl GpuInfo {
    /// Lowercase vendor tag for the API (`nvidia` / `amd` / `intel` / `other`).
    pub fn vendor_tag(&self) -> &'static str {
        vendor_tag(self.vendor_id)
    }

    /// The DXGI LUID this adapter had at enumeration time.
    #[cfg(target_os = "windows")]
    pub fn luid(&self) -> windows::Win32::Foundation::LUID {
        windows::Win32::Foundation::LUID {
            LowPart: self.handle.luid_low,
            HighPart: self.handle.luid_high,
        }
    }
}

/// Assign the stable `id` + `occurrence` fields after enumeration (occurrence = index among
/// same-(vendor,device) twins, in enumeration order).
fn assign_ids(gpus: &mut [GpuInfo]) {
    for i in 0..gpus.len() {
        let occ = gpus[..i]
            .iter()
            .filter(|g| g.vendor_id == gpus[i].vendor_id && g.device_id == gpus[i].device_id)
            .count() as u32;
        gpus[i].occurrence = occ;
        gpus[i].id = format!(
            "{:04x}-{:04x}-{}",
            gpus[i].vendor_id, gpus[i].device_id, occ
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

/// Which kernel adapter types ([`D3DKMT_ADAPTERTYPE`] bit-field words) never belong in the GPU
/// inventory. Pure bit math on every platform so the classification is unit-tested with words
/// captured from real hardware; only the Windows [`enumerate`] consumes it at runtime.
///
/// [`D3DKMT_ADAPTERTYPE`]: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmt_adaptertype
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod adapter_type {
    /// `RenderSupported` — clear on display-only adapters (an IDD ghost has no render engine).
    const RENDER_SUPPORTED: u32 = 1 << 0;
    /// `SoftwareDevice` — WARP/Basic Render (normally already dropped via the DXGI flag).
    const SOFTWARE_DEVICE: u32 = 1 << 2;
    /// `IndirectDisplayDevice` — an IddCx virtual-display adapter (pf-vdisplay, Parsec VDD, …).
    const INDIRECT_DISPLAY_DEVICE: u32 = 1 << 6;

    /// True when these bits describe an adapter that can never be the render/encode GPU:
    /// indirect-display, software, or anything without render support.
    pub(crate) fn hidden(bits: u32) -> bool {
        bits & INDIRECT_DISPLAY_DEVICE != 0
            || bits & SOFTWARE_DEVICE != 0
            || bits & RENDER_SUPPORTED == 0
    }
}

/// Kernel-side adapter-type query (`D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERTYPE)`) via raw
/// gdi32 FFI — the `windows` crate's `Wdk_*` bindings aren't enabled, and one 3-call query
/// doesn't justify them.
#[cfg(target_os = "windows")]
mod kmt {
    /// `KMTQUERYADAPTERINFOTYPE::KMTQAITYPE_ADAPTERTYPE` (d3dkmdt.h).
    const KMTQAITYPE_ADAPTERTYPE: u32 = 15;

    /// `D3DKMT_OPENADAPTERFROMLUID`: LUID in, kernel adapter handle out.
    #[repr(C)]
    struct OpenAdapterFromLuid {
        luid_low: u32,
        luid_high: i32,
        adapter: u32,
    }
    /// `D3DKMT_QUERYADAPTERINFO`.
    #[repr(C)]
    struct QueryAdapterInfo {
        adapter: u32,
        ty: u32,
        private_data: *mut core::ffi::c_void,
        private_data_size: u32,
    }
    /// `D3DKMT_CLOSEADAPTER`.
    #[repr(C)]
    struct CloseAdapter {
        adapter: u32,
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn D3DKMTOpenAdapterFromLuid(arg: *mut OpenAdapterFromLuid) -> i32;
        fn D3DKMTQueryAdapterInfo(arg: *mut QueryAdapterInfo) -> i32;
        fn D3DKMTCloseAdapter(arg: *mut CloseAdapter) -> i32;
    }

    /// The `D3DKMT_ADAPTERTYPE` bits for the adapter with this LUID, `None` when the kernel
    /// query fails (callers fail open — better a listed twin than a hidden real GPU).
    pub(crate) fn adapter_type_bits(luid_low: u32, luid_high: i32) -> Option<u32> {
        // SAFETY: every pointer handed to the three D3DKMT calls addresses a stack local that
        // outlives the call; NTSTATUS >= 0 is success. The kernel handle is closed on every
        // path that opened it, including a failed query.
        unsafe {
            let mut open = OpenAdapterFromLuid {
                luid_low,
                luid_high,
                adapter: 0,
            };
            if D3DKMTOpenAdapterFromLuid(&mut open) < 0 {
                return None;
            }
            let mut bits: u32 = 0;
            let mut query = QueryAdapterInfo {
                adapter: open.adapter,
                ty: KMTQAITYPE_ADAPTERTYPE,
                private_data: (&mut bits as *mut u32).cast(),
                private_data_size: size_of::<u32>() as u32,
            };
            let status = D3DKMTQueryAdapterInfo(&mut query);
            let mut close = CloseAdapter {
                adapter: open.adapter,
            };
            let _ = D3DKMTCloseAdapter(&mut close);
            (status >= 0).then_some(bits)
        }
    }
}

/// Enumerate this host's hardware GPUs. Windows: DXGI adapters minus WARP/Basic-Render (the same
/// filter `win_adapter` always applied) and minus indirect-display/display-only adapters (see the
/// ghost-twin filter inside). Linux: `/dev/dri/renderD*` with PCI ids from sysfs.
/// Other platforms (the macOS dev/test host build): empty — the endpoints still exist, they just
/// report no GPUs.
#[cfg(target_os = "windows")]
pub(crate) fn enumerate() -> Vec<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    let mut out = Vec::new();
    // SAFETY: `CreateDXGIFactory1` returns an owned COM factory (Released when the local drops) or
    // errs (→ empty inventory). `EnumAdapters1(i)` yields owned `IDXGIAdapter1`s until it errors
    // past the last adapter; `GetDesc1()` returns the descriptor by value. Only locals are touched,
    // nothing escapes, no raw pointer is dereferenced.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = adapter.GetDesc1() else { continue };
            if (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue; // Microsoft Basic Render / WARP
            }
            let name = String::from_utf16_lossy(&d.Description)
                .trim_end_matches('\u{0}')
                .to_string();
            let lname = name.to_ascii_lowercase();
            if lname.contains("basic render") || lname.contains("warp") {
                continue;
            }
            // pf-vdisplay's IddCx adapter enumerates as a perfect DXGI twin of the GPU it renders
            // on — same Description, VendorId/DeviceId/SubSysId, even DedicatedVideoMemory; only
            // the LUID differs (verified on an RTX 4090 host). Left in, every punktfunk host
            // lists each render GPU twice: the picker offers a dead twin (display-only — D3D11
            // device creation on it fails) and auto-select's max-VRAM tie between the twins is
            // settled by enumeration order. The kernel adapter type is the only field that
            // separates them, so drop indirect-display (any vendor's virtual display, ours
            // included), display-only, and software adapters here. A failed query keeps the
            // adapter (fail open).
            match kmt::adapter_type_bits(d.AdapterLuid.LowPart, d.AdapterLuid.HighPart) {
                Some(bits) if adapter_type::hidden(bits) => {
                    tracing::debug!(
                        name = %name,
                        bits = %format!("{bits:#x}"),
                        "skipping non-render DXGI adapter (indirect display / display-only / software)"
                    );
                    continue;
                }
                _ => {}
            }
            out.push(GpuInfo {
                id: String::new(),
                name,
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                occurrence: 0,
                vram_bytes: d.DedicatedVideoMemory as u64,
                handle: GpuHandle {
                    luid_low: d.AdapterLuid.LowPart,
                    luid_high: d.AdapterLuid.HighPart,
                },
            });
        }
    }
    assign_ids(&mut out);
    out
}

#[cfg(target_os = "linux")]
pub(crate) fn enumerate() -> Vec<GpuInfo> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    let mut out = Vec::new();
    for node in nodes {
        let sys = format!("/sys/class/drm/{node}/device");
        let read_hex = |f: &str| -> u32 {
            std::fs::read_to_string(format!("{sys}/{f}"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                .unwrap_or(0)
        };
        let vendor_id = read_hex("vendor");
        let device_id = read_hex("device");
        // Only amdgpu exposes a VRAM total in sysfs; 0 elsewhere (display-only — Linux auto
        // selection is NVIDIA-presence + render node, not VRAM).
        let vram_bytes = std::fs::read_to_string(format!("{sys}/mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let vendor_label = match vendor_id {
            VENDOR_NVIDIA => "NVIDIA".to_string(),
            VENDOR_AMD => "AMD".to_string(),
            VENDOR_INTEL => "Intel".to_string(),
            other => format!("GPU 0x{other:04x}"),
        };
        out.push(GpuInfo {
            id: String::new(),
            name: format!("{vendor_label} GPU ({node})"),
            vendor_id,
            device_id,
            occurrence: 0,
            vram_bytes,
            handle: GpuHandle {
                render_node: Some(PathBuf::from(format!("/dev/dri/{node}"))),
            },
        });
    }
    assign_ids(&mut out);
    out
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(crate) fn enumerate() -> Vec<GpuInfo> {
    Vec::new()
}

// ---------------------------------------------------------------------------------------------
// Persisted preference
// ---------------------------------------------------------------------------------------------

/// Operator GPU-selection mode: `Auto` (env substring, else max VRAM — today's behavior) or
/// `Manual` (an explicit GPU chosen in the web console).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GpuMode {
    #[default]
    Auto,
    Manual,
}

/// Stable identity of the manually preferred GPU (see [`GpuInfo::id`] for why not LUID/index).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreferredGpu {
    pub vendor_id: u32,
    pub device_id: u32,
    #[serde(default)]
    pub occurrence: u32,
    /// Adapter name at the time of selection — the last-resort matcher and the label the API
    /// shows when the preferred GPU is currently absent.
    #[serde(default)]
    pub name: String,
}

/// The persisted GPU preference (`<config>/gpu-settings.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GpuPreference {
    #[serde(default)]
    pub mode: GpuMode,
    /// `Some` when `mode == Manual` (kept when switching back to Auto so the console can offer
    /// "return to your previous manual pick").
    #[serde(default)]
    pub gpu: Option<PreferredGpu>,
}

/// The preference store: in-memory current value + its JSON file. Mirrors `native_pairing`'s
/// persistence discipline (private dir, secret-file temp write + atomic rename, in-memory
/// rollback if the disk write fails).
pub(crate) struct GpuPrefStore {
    path: PathBuf,
    cur: Mutex<GpuPreference>,
}

impl GpuPrefStore {
    /// Load the store from `path` (missing/corrupt file ⇒ default Auto, with a warning for the
    /// corrupt case — never fail host startup over a settings file).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<GpuPreference>(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "gpu-settings.json unreadable — using Auto: {e}");
                    GpuPreference::default()
                }
            },
            Err(_) => GpuPreference::default(),
        };
        GpuPrefStore {
            path,
            cur: Mutex::new(cur),
        }
    }

    pub fn get(&self) -> GpuPreference {
        self.cur.lock().unwrap().clone()
    }

    /// Persist + apply a new preference. The in-memory value only changes if the disk write
    /// succeeds, so a full disk can't leave memory and file disagreeing.
    pub fn set(&self, pref: GpuPreference) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            crate::gamestream::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        crate::gamestream::write_secret_file(&tmp, &serde_json::to_vec_pretty(&pref)?)?;
        std::fs::rename(&tmp, &self.path)?;
        *self.cur.lock().unwrap() = pref;
        Ok(())
    }
}

/// The process-wide preference store (config-dir file), loaded once on first access — the same
/// global-accessor shape as [`crate::config::config`], because selection happens deep inside
/// capture/encode setup where no app state is threaded.
pub(crate) fn prefs() -> &'static GpuPrefStore {
    static STORE: OnceLock<GpuPrefStore> = OnceLock::new();
    STORE.get_or_init(|| {
        GpuPrefStore::load_from(crate::gamestream::config_dir().join("gpu-settings.json"))
    })
}

// ---------------------------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------------------------

/// Why a GPU was selected — surfaced by the mgmt API so the console can explain the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickSource {
    /// The operator's manual preference matched a present GPU.
    Preference,
    /// `PUNKTFUNK_RENDER_ADAPTER` substring matched.
    Env,
    /// Auto: max dedicated VRAM (Windows) / platform default (Linux display).
    Auto,
    /// A manual preference is set but that GPU is absent — fell back to auto so the host keeps
    /// streaming (logged; the console shows the fallback).
    PreferenceMissing,
}

impl PickSource {
    pub fn tag(self) -> &'static str {
        match self {
            PickSource::Preference => "preference",
            PickSource::Env => "env",
            PickSource::Auto => "auto",
            PickSource::PreferenceMissing => "preference_missing",
        }
    }
}

/// A resolved selection: the GPU the next session's pipeline will be created on, and why.
#[derive(Clone, Debug)]
pub(crate) struct SelectedGpu {
    pub info: GpuInfo,
    pub source: PickSource,
}

/// Find the manually preferred GPU in the inventory. Match order: exact stable identity
/// (vendor, device, occurrence) → same model (vendor, device; a twin renumbered) → exact name
/// (ids changed across a driver/firmware quirk but the marketing name survived).
pub(crate) fn find_preferred(gpus: &[GpuInfo], want: &PreferredGpu) -> Option<usize> {
    gpus.iter()
        .position(|g| {
            g.vendor_id == want.vendor_id
                && g.device_id == want.device_id
                && g.occurrence == want.occurrence
        })
        .or_else(|| {
            gpus.iter()
                .position(|g| g.vendor_id == want.vendor_id && g.device_id == want.device_id)
        })
        .or_else(|| {
            if want.name.is_empty() {
                return None;
            }
            gpus.iter().position(|g| g.name == want.name)
        })
}

/// Pure selection over an inventory: **manual preference > env substring > max VRAM**. Returns
/// the index into `gpus` plus the reason. `None` only when `gpus` is empty. A set-but-unmatched
/// env substring falls through to max-VRAM (same outcome as env unset — deliberately more robust
/// than the old `resolve_render_adapter_luid`, which returned *no* adapter on a stale substring).
pub(crate) fn pick(
    gpus: &[GpuInfo],
    pref: &GpuPreference,
    env_substr: Option<&str>,
) -> Option<(usize, PickSource)> {
    let mut preference_missing = false;
    if pref.mode == GpuMode::Manual {
        if let Some(want) = &pref.gpu {
            match find_preferred(gpus, want) {
                Some(i) => return Some((i, PickSource::Preference)),
                None => preference_missing = true,
            }
        }
    }
    if let Some(sub) = env_substr.filter(|s| !s.is_empty()) {
        let sub = sub.to_ascii_lowercase();
        if let Some(i) = gpus
            .iter()
            .position(|g| g.name.to_ascii_lowercase().contains(&sub))
        {
            return Some((i, PickSource::Env));
        }
    }
    let i = gpus
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.vram_bytes)
        .map(|(i, _)| i)?;
    Some((
        i,
        if preference_missing {
            PickSource::PreferenceMissing
        } else {
            PickSource::Auto
        },
    ))
}

/// The GPU the next session will run on. Windows: the full precedence over the DXGI inventory —
/// this is what `win_adapter::resolve_render_adapter_luid` (capture ring + IddCx render pin) and
/// the encoder-vendor dispatch both consume, so capture, encode, and the advertisement agree by
/// construction. Pure query — callers log (this runs per serverinfo poll).
#[cfg(target_os = "windows")]
pub(crate) fn selected_gpu() -> Option<SelectedGpu> {
    let gpus = enumerate();
    let pref = prefs().get();
    let env = crate::config::config()
        .render_adapter
        .clone()
        .filter(|s| !s.is_empty());
    let (i, source) = pick(&gpus, &pref, env.as_deref())?;
    Some(SelectedGpu {
        info: gpus.into_iter().nth(i)?,
        source,
    })
}

/// The GPU the next session will run on (Linux). Mirrors the encode dispatch for display: a
/// matched manual preference wins; otherwise NVIDIA-presence → the NVIDIA GPU, else the GPU that
/// owns the VAAPI render node. (The *authoritative* Linux switches stay in `encode::open_video` /
/// [`linux_render_node`] — this is the console's view of them.)
#[cfg(target_os = "linux")]
pub(crate) fn selected_gpu() -> Option<SelectedGpu> {
    let gpus = enumerate();
    let pref = prefs().get();
    let mut preference_missing = false;
    if pref.mode == GpuMode::Manual {
        if let Some(want) = &pref.gpu {
            match find_preferred(&gpus, want) {
                Some(i) => {
                    return Some(SelectedGpu {
                        info: gpus.into_iter().nth(i)?,
                        source: PickSource::Preference,
                    })
                }
                None => preference_missing = true,
            }
        }
    }
    let source = if preference_missing {
        PickSource::PreferenceMissing
    } else {
        PickSource::Auto
    };
    if linux_nvidia_present() {
        if let Some(i) = gpus.iter().position(|g| g.vendor_id == VENDOR_NVIDIA) {
            return Some(SelectedGpu {
                info: gpus.into_iter().nth(i)?,
                source,
            });
        }
    }
    let node = linux_render_node();
    let i = gpus
        .iter()
        .position(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
        .unwrap_or(0);
    Some(SelectedGpu {
        info: gpus.into_iter().nth(i)?,
        source,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub(crate) fn selected_gpu() -> Option<SelectedGpu> {
    None
}

/// The manually preferred GPU, only when `mode == Manual` **and** it is currently present.
/// The Linux encode dispatch consults this (auto mode keeps today's NVIDIA-presence behavior
/// exactly).
pub(crate) fn manual_selection() -> Option<GpuInfo> {
    let pref = prefs().get();
    if pref.mode != GpuMode::Manual {
        return None;
    }
    let want = pref.gpu?;
    let gpus = enumerate();
    let i = find_preferred(&gpus, &want)?;
    gpus.into_iter().nth(i)
}

/// The VAAPI/DRM render node for this host: matched manual preference > `PUNKTFUNK_RENDER_NODE`
/// (a deliberate live env read — see `config.rs` module docs) > `/dev/dri/renderD128`.
#[cfg(target_os = "linux")]
pub(crate) fn linux_render_node() -> PathBuf {
    if let Some(g) = manual_selection() {
        if let Some(node) = g.handle.render_node {
            return node;
        }
    }
    std::env::var("PUNKTFUNK_RENDER_NODE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

/// NVIDIA-presence probe (same device-node check as `encode::nvidia_present` — duplicated two
/// lines rather than widening that private fn's visibility).
#[cfg(target_os = "linux")]
fn linux_nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// A cache key that changes whenever the *selection* changes (preference edits included), for the
/// per-GPU probe caches (`can_encode_444`, `windows_codec_support`) that were process-lifetime
/// `OnceLock`s back when selection was env-only.
pub(crate) fn selection_key() -> String {
    match selected_gpu() {
        Some(sel) => {
            #[cfg(target_os = "windows")]
            {
                format!(
                    "{}:{:08x}{:08x}",
                    sel.info.id, sel.info.handle.luid_high as u32, sel.info.handle.luid_low
                )
            }
            #[cfg(not(target_os = "windows"))]
            {
                sel.info.id
            }
        }
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Live "in use" record
// ---------------------------------------------------------------------------------------------

/// What a live session encodes on — the console's "currently used GPU".
#[derive(Clone, Debug)]
pub(crate) struct ActiveGpu {
    /// Stable id of the GPU ([`GpuInfo::id`]; empty for the CPU/software path) so a UI can match
    /// it against the inventory.
    pub id: String,
    pub name: String,
    pub vendor_id: u32,
    /// The encode backend the session opened (`nvenc` / `amf` / `qsv` / `vaapi` / `software`).
    pub backend: &'static str,
}

struct ActiveState {
    gpu: ActiveGpu,
    sessions: u32,
}

static ACTIVE: Mutex<Option<ActiveState>> = Mutex::new(None);

/// RAII marker for one live encode session; dropping it decrements the session count. Held by the
/// encoder wrapper `open_video` returns, so the count is correct by construction (every successful
/// open is paired with a drop).
pub(crate) struct ActiveSession(());

impl Drop for ActiveSession {
    fn drop(&mut self) {
        let mut st = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = st.as_mut() {
            st.sessions = st.sessions.saturating_sub(1);
        }
    }
}

/// Record a session opening on `gpu`. Concurrent sessions share one GPU (the Windows pipeline is
/// single-GPU by construction; Linux sessions share the selection), so the latest record wins and
/// a counter tracks liveness.
pub(crate) fn session_begin(gpu: ActiveGpu) -> ActiveSession {
    let mut st = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = st.as_ref().map(|s| s.sessions).unwrap_or(0) + 1;
    *st = Some(ActiveState { gpu, sessions });
    ActiveSession(())
}

/// The GPU live sessions encode on + how many sessions hold it. `Some` with `sessions == 0` means
/// "last used, idle now" — the mgmt API distinguishes the two.
pub(crate) fn active() -> Option<(ActiveGpu, u32)> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| (s.gpu.clone(), s.sessions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(vendor: u32, device: u32, name: &str, vram_gb: u64) -> GpuInfo {
        GpuInfo {
            id: String::new(),
            name: name.into(),
            vendor_id: vendor,
            device_id: device,
            occurrence: 0,
            vram_bytes: vram_gb * 1024 * 1024 * 1024,
            handle: GpuHandle::default(),
        }
    }

    /// The dev-box shape: NVIDIA dGPU + Intel Arc iGPU.
    fn hybrid() -> Vec<GpuInfo> {
        let mut v = vec![
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut v);
        v
    }

    fn manual(vendor: u32, device: u32, occurrence: u32, name: &str) -> GpuPreference {
        GpuPreference {
            mode: GpuMode::Manual,
            gpu: Some(PreferredGpu {
                vendor_id: vendor,
                device_id: device,
                occurrence,
                name: name.into(),
            }),
        }
    }

    #[test]
    fn auto_picks_max_vram() {
        let (i, src) = pick(&hybrid(), &GpuPreference::default(), None).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Auto);
    }

    #[test]
    fn manual_preference_beats_env_and_vram() {
        let pref = manual(VENDOR_INTEL, 0x7d55, 0, "Intel(R) Arc(TM) Graphics");
        let (i, src) = pick(&hybrid(), &pref, Some("nvidia")).unwrap();
        assert_eq!(i, 0);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn env_substring_beats_vram_and_is_case_insensitive() {
        let mut gpus = vec![
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
        ];
        assign_ids(&mut gpus);
        let (i, src) = pick(&gpus, &GpuPreference::default(), Some("ARC")).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Env);
    }

    #[test]
    fn unmatched_env_falls_back_to_max_vram() {
        let (i, src) = pick(&hybrid(), &GpuPreference::default(), Some("radeon")).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Auto);
    }

    #[test]
    fn missing_preferred_gpu_falls_back_and_says_so() {
        let pref = manual(VENDOR_AMD, 0x744c, 0, "AMD Radeon RX 7900 XTX");
        let (i, src) = pick(&hybrid(), &pref, None).unwrap();
        assert_eq!(i, 1); // max VRAM
        assert_eq!(src, PickSource::PreferenceMissing);
    }

    #[test]
    fn preferred_matches_same_model_when_occurrence_gone() {
        // Stored occurrence 1 (was the second of two twins); only one twin remains.
        let mut gpus = vec![
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut gpus);
        let pref = manual(VENDOR_NVIDIA, 0x2c05, 1, "NVIDIA GeForce RTX 5070 Ti");
        let (i, src) = pick(&gpus, &pref, None).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn preferred_matches_by_name_when_ids_changed() {
        let pref = manual(VENDOR_NVIDIA, 0xffff, 0, "Intel(R) Arc(TM) Graphics");
        let (i, src) = pick(&hybrid(), &pref, None).unwrap();
        assert_eq!(i, 0);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn empty_inventory_selects_nothing() {
        assert!(pick(&[], &GpuPreference::default(), Some("nvidia")).is_none());
    }

    #[test]
    fn ids_disambiguate_twins() {
        let mut gpus = vec![
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut gpus);
        assert_eq!(gpus[0].id, "10de-2c05-0");
        assert_eq!(gpus[1].id, "10de-2c05-1");
    }

    #[test]
    fn store_round_trips_and_survives_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu-settings.json");
        let store = GpuPrefStore::load_from(path.clone());
        assert_eq!(store.get(), GpuPreference::default());
        let pref = manual(VENDOR_INTEL, 0x7d55, 0, "Intel(R) Arc(TM) Graphics");
        store.set(pref.clone()).unwrap();
        assert_eq!(store.get(), pref);
        // A fresh load sees the persisted value…
        assert_eq!(GpuPrefStore::load_from(path.clone()).get(), pref);
        // …and a corrupt file degrades to Auto instead of failing startup.
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            GpuPrefStore::load_from(path).get(),
            GpuPreference::default()
        );
    }

    #[test]
    fn session_counter_tracks_begin_and_drop() {
        // Serialize against other tests via the ACTIVE mutex being process-global: this is the
        // only test touching it.
        let a = session_begin(ActiveGpu {
            id: "10de-2c05-0".into(),
            name: "GPU A".into(),
            vendor_id: VENDOR_NVIDIA,
            backend: "nvenc",
        });
        let (gpu0, n0) = active().unwrap();
        assert_eq!((gpu0.name.as_str(), n0), ("GPU A", 1));
        let b = session_begin(ActiveGpu {
            id: "10de-2c05-0".into(),
            name: "GPU A".into(),
            vendor_id: VENDOR_NVIDIA,
            backend: "nvenc",
        });
        assert_eq!(active().unwrap().1, 2);
        drop(a);
        assert_eq!(active().unwrap().1, 1);
        drop(b);
        assert_eq!(active().unwrap().1, 0); // idle, last-used retained
    }

    /// `D3DKMT_ADAPTERTYPE` words captured on a real host (RTX 4090 + pf-vdisplay + Raphael
    /// iGPU, 2026-07): the IDD ghost and Basic Render hide, the real GPUs stay.
    #[test]
    fn adapter_type_hides_idd_ghost_keeps_real_gpus() {
        assert!(adapter_type::hidden(0x0342)); // pf-vdisplay ghost twin: indirect + display-only
        assert!(!adapter_type::hidden(0x031b)); // NVIDIA GeForce RTX 4090
        assert!(!adapter_type::hidden(0x2323)); // AMD Radeon(TM) Graphics (Raphael iGPU)
        assert!(adapter_type::hidden(0x0105)); // Microsoft Basic Render Driver (software)
    }

    /// End-to-end smoke of the ghost-twin filter + the raw D3DKMT FFI (struct layout, linking)
    /// on whatever GPUs this Windows machine has: nothing the filter would hide may survive
    /// [`enumerate`]. On a host with pf-vdisplay installed this actively exercises ghost
    /// exclusion; on a GPU-less CI runner it passes vacuously.
    #[cfg(target_os = "windows")]
    #[test]
    fn enumerate_excludes_non_render_adapters() {
        for g in enumerate() {
            let bits = kmt::adapter_type_bits(g.handle.luid_low, g.handle.luid_high);
            eprintln!(
                "enumerated: {} ({}) kmt_bits={}",
                g.name,
                g.id,
                bits.map_or("<query failed>".into(), |b| format!("{b:#x}")),
            );
            if let Some(bits) = bits {
                assert!(
                    !adapter_type::hidden(bits),
                    "{} ({}) should have been filtered (bits {bits:#x})",
                    g.name,
                    g.id
                );
            }
        }
    }
}
