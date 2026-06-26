//! punktfunk Vulkan implicit layer — `VK_LAYER_PUNKTFUNK_hdr_inject`.
//!
//! ## Why
//! On Windows, NVIDIA/AMD Vulkan ICDs do **not** advertise any HDR color space
//! (`HDR10_ST2084_EXT`, `EXTENDED_SRGB_LINEAR_EXT`) for a surface on an IddCx *indirect / virtual*
//! display — even when Windows "Use HDR" is enabled and the desktop is composited at 10-bit. So
//! Vulkan games (Doom: The Dark Ages and the rest of id Tech, Indiana Jones, …) query
//! `vkGetPhysicalDeviceSurfaceFormatsKHR`, find no HDR color space, and refuse HDR
//! ("This device does not support HDR"). DX11/DX12 HDR works on the same display because the OS
//! compositor drives it; only the Vulkan WSI *enumeration* is gated. An on-box spike proved the ICD
//! *accepts and presents* a forced HDR swapchain on that exact surface — it just won't *advertise*
//! the format. So the entire fix is to append the HDR surface formats to the enumeration the game
//! queries; once the game asks for that swapchain, the ICD honors it.
//!
//! ## What this layer does
//! Intercepts `vkGetPhysicalDeviceSurfaceFormatsKHR` / `...2KHR`, calls down to the ICD, and appends
//! `{A2B10G10R10_UNORM_PACK32, HDR10_ST2084_EXT}` and `{R16G16B16A16_SFLOAT, EXTENDED_SRGB_LINEAR_EXT}`
//! (deduped). **Self-gating:** it only injects when the surface's monitor actually has Windows
//! advanced-color (HDR) *enabled* — so it is a complete no-op on SDR sessions and on real monitors
//! (which already advertise HDR, and dedup drops the duplicate). It tracks `VkSurfaceKHR -> HWND` by
//! intercepting `vkCreateWin32SurfaceKHR`. Everything else is pass-through dispatch chaining.
//!
//! Off-switches: the loader-standard `DISABLE_PF_VKHDR=1` (disables the whole layer), and
//! `PF_VKHDR_EXCLUDE` (comma/semicolon list of exe basenames to skip — defaults include known
//! kernel-anti-cheat titles). `PF_VKHDR_LOG=1` enables a debug log in `%TEMP%\pf_vkhdr_layer.log`.

#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]
// HWND / HMONITOR etc. deliberately mirror the Win32 names.
#![allow(clippy::upper_case_acronyms)]

use ash::vk;
use ash::vk::Handle;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::{Mutex, OnceLock};

// ---- Vulkan loader<->layer glue (vk_layer.h; not in the core registry / ash) ----
// The loader's private create-info sTypes squat on 47/48 (NOT the 1000211xxx range).
const LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: i32 = 0;
const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;

type PfnGipa = vk::PFN_vkGetInstanceProcAddr;
type PfnGdpa = vk::PFN_vkGetDeviceProcAddr;
type PfnGpdpa = unsafe extern "system" fn(vk::Instance, *const c_char) -> vk::PFN_vkVoidFunction;

#[repr(C)]
struct BaseIn {
    s_type: vk::StructureType,
    p_next: *const c_void,
}
#[repr(C)]
struct LayerInstanceLink {
    p_next: *mut LayerInstanceLink,
    next_gipa: PfnGipa,
    next_gpdpa: Option<PfnGpdpa>,
}
#[repr(C)]
struct LayerInstanceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    u: *mut LayerInstanceLink,
}
#[repr(C)]
struct LayerDeviceLink {
    p_next: *mut LayerDeviceLink,
    next_gipa: PfnGipa,
    next_gdpa: PfnGdpa,
}
#[repr(C)]
struct LayerDeviceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    u: *mut LayerDeviceLink,
}
#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    pfn_gipa: Option<PfnGipa>,
    pfn_gdpa: Option<PfnGdpa>,
    pfn_gpdpa: Option<PfnGpdpa>,
}

// raw mirror of VkSurfaceFormat2KHR (avoid ash lifetime generics in fn-pointer types)
#[repr(C)]
#[derive(Clone, Copy)]
struct SurfaceFormat2Raw {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    surface_format: vk::SurfaceFormatKHR,
}

// ---- ICD function-pointer typedefs we call down to (raw pointers, no lifetimes) ----
type FnCreateInstance =
    unsafe extern "system" fn(*const c_void, *const c_void, *mut vk::Instance) -> vk::Result;
type FnDestroyInstance = unsafe extern "system" fn(vk::Instance, *const c_void);
type FnCreateDevice = unsafe extern "system" fn(
    vk::PhysicalDevice,
    *const c_void,
    *const c_void,
    *mut vk::Device,
) -> vk::Result;
type FnGetSurfFmts = unsafe extern "system" fn(
    vk::PhysicalDevice,
    vk::SurfaceKHR,
    *mut u32,
    *mut vk::SurfaceFormatKHR,
) -> vk::Result;
type FnGetSurfFmts2 = unsafe extern "system" fn(
    vk::PhysicalDevice,
    *const c_void,
    *mut u32,
    *mut c_void,
) -> vk::Result;
type FnCreateWin32Surface = unsafe extern "system" fn(
    vk::Instance,
    *const c_void,
    *const c_void,
    *mut vk::SurfaceKHR,
) -> vk::Result;
type FnDestroySurface = unsafe extern "system" fn(vk::Instance, vk::SurfaceKHR, *const c_void);

struct InstanceData {
    instance: vk::Instance,
    next_gipa: PfnGipa,
    next_gpdpa: Option<PfnGpdpa>,
    destroy_instance: Option<FnDestroyInstance>,
    get_surface_formats: Option<FnGetSurfFmts>,
    get_surface_formats2: Option<FnGetSurfFmts2>,
    create_win32_surface: Option<FnCreateWin32Surface>,
    destroy_surface: Option<FnDestroySurface>,
}
unsafe impl Send for InstanceData {}

struct DeviceData {
    next_gdpa: PfnGdpa,
}
unsafe impl Send for DeviceData {}

fn instances() -> &'static Mutex<HashMap<usize, InstanceData>> {
    static M: OnceLock<Mutex<HashMap<usize, InstanceData>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
fn devices() -> &'static Mutex<HashMap<usize, DeviceData>> {
    static M: OnceLock<Mutex<HashMap<usize, DeviceData>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
/// VkSurfaceKHR handle -> the HWND it was created from (so we can resolve its monitor's HDR state).
fn surface_hwnds() -> &'static Mutex<HashMap<u64, isize>> {
    static M: OnceLock<Mutex<HashMap<u64, isize>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

// dispatch key = first pointer word of a dispatchable handle (loader dispatch table ptr).
#[inline]
unsafe fn key(raw: u64) -> usize {
    *(raw as usize as *const usize)
}
#[inline]
unsafe fn as_pfn(p: *const c_void) -> vk::PFN_vkVoidFunction {
    Some(std::mem::transmute::<
        *const c_void,
        unsafe extern "system" fn(),
    >(p))
}
#[inline]
unsafe fn resolve<T: Copy>(gipa: PfnGipa, inst: vk::Instance, name: &CStr) -> Option<T> {
    gipa(inst, name.as_ptr()).map(|f| std::mem::transmute_copy::<_, T>(&f))
}

fn log(msg: &str) {
    if std::env::var_os("PF_VKHDR_LOG").is_none() {
        return;
    }
    use std::io::Write;
    let mut path = std::env::temp_dir();
    path.push("pf_vkhdr_layer.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

fn hdr_extra() -> [vk::SurfaceFormatKHR; 2] {
    [
        vk::SurfaceFormatKHR {
            format: vk::Format::A2B10G10R10_UNORM_PACK32,
            color_space: vk::ColorSpaceKHR::HDR10_ST2084_EXT,
        },
        vk::SurfaceFormatKHR {
            format: vk::Format::R16G16B16A16_SFLOAT,
            color_space: vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
        },
    ]
}

/// `false` if this process is on the anti-cheat exclude list (built-in + `PF_VKHDR_EXCLUDE`).
/// Computed once per process.
fn injection_allowed_for_process() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if exe.is_empty() {
            return true;
        }
        // Conservative default skip-list for kernel-level anti-cheat titles. Users can extend or
        // clear via PF_VKHDR_EXCLUDE. (HDR injection is benign, but we err toward not being present
        // in these process' WSI path at all.)
        const DENY: &[&str] = &[
            "cs2.exe",
            "rainbowsix.exe",
            "rainbowsixgame.exe",
            "r5apex.exe",
        ];
        let mut denied = false;
        for d in DENY {
            if *d == exe {
                denied = true;
            }
        }
        if let Ok(extra) = std::env::var("PF_VKHDR_EXCLUDE") {
            for e in extra.split([',', ';']) {
                if e.trim().to_lowercase() == exe {
                    denied = true;
                }
            }
        }
        if denied {
            log(&format!("injection disabled for excluded process: {exe}"));
        }
        !denied
    })
}

// ---- Win32 / DisplayConfig: is the surface's monitor HDR-enabled right now? ----
mod hdr {
    use std::ffi::c_void;
    pub type HWND = isize;
    pub type HMONITOR = isize;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Rect {
        pub l: i32,
        pub t: i32,
        pub r: i32,
        pub b: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MonitorInfoExW {
        pub cb: u32,
        pub rc_monitor: Rect,
        pub rc_work: Rect,
        pub flags: u32,
        pub sz_device: [u16; 32],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Luid {
        pub low: u32,
        pub high: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Rational {
        pub num: u32,
        pub den: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Source {
        pub adapter: Luid,
        pub id: u32,
        pub mode_idx: u32,
        pub status: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Target {
        pub adapter: Luid,
        pub id: u32,
        pub mode_idx: u32,
        pub tech: i32,
        pub rotation: i32,
        pub scaling: i32,
        pub refresh: Rational,
        pub scanline: i32,
        pub available: i32,
        pub status: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PathInfo {
        pub src: Source,
        pub tgt: Target,
        pub flags: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ModeInfo {
        pub _b: [u8; 64],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Header {
        pub typ: i32,
        pub size: u32,
        pub adapter: Luid,
        pub id: u32,
    }
    #[repr(C)]
    pub struct AdvInfo {
        pub header: Header,
        pub value: u32,
        pub enc: i32,
        pub bpc: i32,
    }
    #[repr(C)]
    pub struct SourceName {
        pub header: Header,
        pub gdi: [u16; 32],
    }

    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromWindow(h: HWND, flags: u32) -> HMONITOR;
        fn GetMonitorInfoW(h: HMONITOR, mi: *mut MonitorInfoExW) -> i32;
        fn GetDisplayConfigBufferSizes(flags: u32, np: *mut u32, nm: *mut u32) -> i32;
        fn QueryDisplayConfig(
            flags: u32,
            np: *mut u32,
            pa: *mut PathInfo,
            nm: *mut u32,
            ma: *mut ModeInfo,
            topo: *mut c_void,
        ) -> i32;
        fn DisplayConfigGetDeviceInfo(p: *mut c_void) -> i32;
    }
    const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
    const GET_SOURCE_NAME: i32 = 1;
    const GET_ADVANCED_COLOR_INFO: i32 = 9;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    unsafe fn active_paths() -> Vec<PathInfo> {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != 0 || np == 0 {
            return Vec::new();
        }
        let mut pa: Vec<PathInfo> = vec![std::mem::zeroed(); np as usize];
        let mut ma: Vec<ModeInfo> = vec![std::mem::zeroed(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            pa.as_mut_ptr(),
            &mut nm,
            ma.as_mut_ptr(),
            std::ptr::null_mut(),
        ) != 0
        {
            return Vec::new();
        }
        pa.truncate(np as usize);
        pa
    }

    unsafe fn target_hdr_enabled(p: &PathInfo) -> bool {
        let mut ai: AdvInfo = std::mem::zeroed();
        ai.header.typ = GET_ADVANCED_COLOR_INFO;
        ai.header.size = std::mem::size_of::<AdvInfo>() as u32;
        ai.header.adapter = p.tgt.adapter;
        ai.header.id = p.tgt.id;
        if DisplayConfigGetDeviceInfo(&mut ai as *mut _ as *mut c_void) != 0 {
            return false;
        }
        // value bitfield: bit0 advancedColorSupported, bit1 advancedColorEnabled.
        (ai.value & 0b10) != 0
    }

    unsafe fn source_gdi(p: &PathInfo) -> [u16; 32] {
        let mut sn: SourceName = std::mem::zeroed();
        sn.header.typ = GET_SOURCE_NAME;
        sn.header.size = std::mem::size_of::<SourceName>() as u32;
        sn.header.adapter = p.src.adapter;
        sn.header.id = p.src.id;
        let _ = DisplayConfigGetDeviceInfo(&mut sn as *mut _ as *mut c_void);
        sn.gdi
    }

    /// Is HDR (Windows advanced color) currently enabled on the display this surface lives on?
    /// `hwnd == 0`/unknown falls back to "any active display has HDR enabled".
    pub unsafe fn enabled_for(hwnd: HWND) -> bool {
        let paths = active_paths();
        if hwnd != 0 {
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi: MonitorInfoExW = std::mem::zeroed();
            mi.cb = std::mem::size_of::<MonitorInfoExW>() as u32;
            if GetMonitorInfoW(mon, &mut mi) != 0 {
                for p in &paths {
                    if source_gdi(p) == mi.sz_device {
                        return target_hdr_enabled(p);
                    }
                }
            }
        }
        paths.iter().any(|p| target_hdr_enabled(p))
    }
}

/// Should we inject HDR formats for this surface right now?
unsafe fn should_inject(surface: vk::SurfaceKHR) -> bool {
    if !injection_allowed_for_process() {
        return false;
    }
    let hwnd = surface_hwnds()
        .lock()
        .ok()
        .and_then(|m| m.get(&surface.as_raw()).copied())
        .unwrap_or(0);
    hdr::enabled_for(hwnd)
}

// ---- entry point ----

#[no_mangle]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    p: *mut NegotiateLayerInterface,
) -> vk::Result {
    if p.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let s = &mut *p;
    if s.s_type != LAYER_NEGOTIATE_INTERFACE_STRUCT {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    if s.loader_layer_interface_version > 2 {
        s.loader_layer_interface_version = 2;
    }
    s.pfn_gipa = Some(layer_gipa);
    s.pfn_gdpa = Some(layer_gdpa);
    s.pfn_gpdpa = Some(layer_gpdpa);
    log("negotiate: VK_LAYER_PUNKTFUNK_hdr_inject active (v2)");
    vk::Result::SUCCESS
}

// ---- proc-addr dispatch ----

unsafe extern "system" fn layer_gipa(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    match CStr::from_ptr(p_name).to_bytes() {
        b"vkGetInstanceProcAddr" => as_pfn(layer_gipa as *const c_void),
        b"vkGetDeviceProcAddr" => as_pfn(layer_gdpa as *const c_void),
        b"vkCreateInstance" => as_pfn(create_instance as *const c_void),
        b"vkDestroyInstance" => as_pfn(destroy_instance as *const c_void),
        b"vkCreateDevice" => as_pfn(create_device as *const c_void),
        b"vkGetPhysicalDeviceSurfaceFormatsKHR" => as_pfn(get_surface_formats as *const c_void),
        b"vkGetPhysicalDeviceSurfaceFormats2KHR" => as_pfn(get_surface_formats2 as *const c_void),
        b"vkCreateWin32SurfaceKHR" => as_pfn(create_win32_surface as *const c_void),
        b"vkDestroySurfaceKHR" => as_pfn(destroy_surface as *const c_void),
        _ => {
            if instance == vk::Instance::null() {
                return None;
            }
            let next = {
                let g = instances().lock().ok()?;
                g.get(&key(instance.as_raw())).map(|d| d.next_gipa)
            };
            next.and_then(|gipa| gipa(instance, p_name))
        }
    }
}

unsafe extern "system" fn layer_gpdpa(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    match CStr::from_ptr(p_name).to_bytes() {
        b"vkGetPhysicalDeviceSurfaceFormatsKHR" => as_pfn(get_surface_formats as *const c_void),
        b"vkGetPhysicalDeviceSurfaceFormats2KHR" => as_pfn(get_surface_formats2 as *const c_void),
        _ => {
            if instance == vk::Instance::null() {
                return None;
            }
            let next = {
                let g = instances().lock().ok()?;
                g.get(&key(instance.as_raw())).and_then(|d| d.next_gpdpa)
            };
            next.and_then(|gpdpa| gpdpa(instance, p_name))
        }
    }
}

unsafe extern "system" fn layer_gdpa(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    if CStr::from_ptr(p_name).to_bytes() == b"vkGetDeviceProcAddr" {
        return as_pfn(layer_gdpa as *const c_void);
    }
    if device == vk::Device::null() {
        return None;
    }
    let next = {
        let g = match devices().lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        g.get(&key(device.as_raw())).map(|d| d.next_gdpa)
    };
    next.and_then(|gdpa| gdpa(device, p_name))
}

// ---- instance chain ----

unsafe fn find_instance_link(p_ci: *const c_void) -> *mut LayerInstanceCreateInfo {
    let mut node = (*(p_ci as *const BaseIn)).p_next as *const BaseIn;
    while !node.is_null() {
        if (*node).s_type.as_raw() == LOADER_INSTANCE_CREATE_INFO {
            let lci = node as *mut LayerInstanceCreateInfo;
            if (*lci).function == VK_LAYER_LINK_INFO {
                return lci;
            }
        }
        node = (*node).p_next as *const BaseIn;
    }
    ptr::null_mut()
}

unsafe extern "system" fn create_instance(
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_inst: *mut vk::Instance,
) -> vk::Result {
    let lci = find_instance_link(p_ci);
    if lci.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let link = (*lci).u;
    if link.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).next_gipa;
    let next_gpdpa = (*link).next_gpdpa;
    (*lci).u = (*link).p_next;

    let create: FnCreateInstance =
        match resolve(next_gipa, vk::Instance::null(), c"vkCreateInstance") {
            Some(f) => f,
            None => return vk::Result::ERROR_INITIALIZATION_FAILED,
        };
    let res = create(p_ci, p_alloc, p_inst);
    if res != vk::Result::SUCCESS {
        return res;
    }
    let inst = *p_inst;
    let data = InstanceData {
        instance: inst,
        next_gipa,
        next_gpdpa,
        destroy_instance: resolve(next_gipa, inst, c"vkDestroyInstance"),
        get_surface_formats: resolve(next_gipa, inst, c"vkGetPhysicalDeviceSurfaceFormatsKHR"),
        get_surface_formats2: resolve(next_gipa, inst, c"vkGetPhysicalDeviceSurfaceFormats2KHR"),
        create_win32_surface: resolve(next_gipa, inst, c"vkCreateWin32SurfaceKHR"),
        destroy_surface: resolve(next_gipa, inst, c"vkDestroySurfaceKHR"),
    };
    if let Ok(mut g) = instances().lock() {
        g.insert(key(inst.as_raw()), data);
    }
    log("create_instance: hooked");
    vk::Result::SUCCESS
}

unsafe extern "system" fn destroy_instance(inst: vk::Instance, p_alloc: *const c_void) {
    if inst == vk::Instance::null() {
        return;
    }
    let data = instances()
        .lock()
        .ok()
        .and_then(|mut g| g.remove(&key(inst.as_raw())));
    if let Some(d) = data {
        if let Some(f) = d.destroy_instance {
            f(inst, p_alloc);
        }
    }
}

// ---- device chain (pass-through; keeps device-level dispatch working) ----

unsafe fn find_device_link(p_ci: *const c_void) -> *mut LayerDeviceCreateInfo {
    let mut node = (*(p_ci as *const BaseIn)).p_next as *const BaseIn;
    while !node.is_null() {
        if (*node).s_type.as_raw() == LOADER_DEVICE_CREATE_INFO {
            let lci = node as *mut LayerDeviceCreateInfo;
            if (*lci).function == VK_LAYER_LINK_INFO {
                return lci;
            }
        }
        node = (*node).p_next as *const BaseIn;
    }
    ptr::null_mut()
}

unsafe extern "system" fn create_device(
    pdev: vk::PhysicalDevice,
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_dev: *mut vk::Device,
) -> vk::Result {
    let lci = find_device_link(p_ci);
    if lci.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let link = (*lci).u;
    if link.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).next_gipa;
    let next_gdpa = (*link).next_gdpa;
    (*lci).u = (*link).p_next;

    let inst = instances()
        .lock()
        .ok()
        .and_then(|g| g.get(&key(pdev.as_raw())).map(|d| d.instance))
        .unwrap_or(vk::Instance::null());

    let create: FnCreateDevice = match resolve(next_gipa, inst, c"vkCreateDevice") {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };
    let res = create(pdev, p_ci, p_alloc, p_dev);
    if res != vk::Result::SUCCESS {
        return res;
    }
    let dev = *p_dev;
    if let Ok(mut g) = devices().lock() {
        g.insert(key(dev.as_raw()), DeviceData { next_gdpa });
    }
    vk::Result::SUCCESS
}

// ---- surface tracking (so we can resolve a surface's monitor) ----

unsafe extern "system" fn create_win32_surface(
    inst: vk::Instance,
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    let down = instances().lock().ok().and_then(|g| {
        g.get(&key(inst.as_raw()))
            .and_then(|d| d.create_win32_surface)
    });
    let down = match down {
        Some(f) => f,
        None => return vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    };
    let res = down(inst, p_ci, p_alloc, p_surface);
    if res == vk::Result::SUCCESS {
        // VkWin32SurfaceCreateInfoKHR: sType@0, pNext@8, flags@16, hinstance@24, hwnd@32
        let hwnd = *((p_ci as *const u8).add(32) as *const isize);
        if let Ok(mut m) = surface_hwnds().lock() {
            m.insert((*p_surface).as_raw(), hwnd);
        }
    }
    res
}

unsafe extern "system" fn destroy_surface(
    inst: vk::Instance,
    surface: vk::SurfaceKHR,
    p_alloc: *const c_void,
) {
    if let Ok(mut m) = surface_hwnds().lock() {
        m.remove(&surface.as_raw());
    }
    let down = instances()
        .lock()
        .ok()
        .and_then(|g| g.get(&key(inst.as_raw())).and_then(|d| d.destroy_surface));
    if let Some(f) = down {
        f(inst, surface, p_alloc);
    }
}

// ---- the actual fix: append HDR surface formats (self-gated on display HDR state) ----

unsafe extern "system" fn get_surface_formats(
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_count: *mut u32,
    p_formats: *mut vk::SurfaceFormatKHR,
) -> vk::Result {
    let down = instances().lock().ok().and_then(|g| {
        g.get(&key(pdev.as_raw()))
            .and_then(|d| d.get_surface_formats)
    });
    let down = match down {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };

    let mut n = 0u32;
    let r = down(pdev, surface, &mut n, ptr::null_mut());
    if r != vk::Result::SUCCESS {
        return r;
    }
    let mut real = vec![vk::SurfaceFormatKHR::default(); n as usize];
    if n > 0 {
        let r = down(pdev, surface, &mut n, real.as_mut_ptr());
        if r != vk::Result::SUCCESS {
            return r;
        }
    }
    real.truncate(n as usize);

    let mut aug = real;
    if !aug.is_empty() && should_inject(surface) {
        for e in hdr_extra() {
            if !aug
                .iter()
                .any(|x| x.format == e.format && x.color_space == e.color_space)
            {
                aug.push(e);
            }
        }
    }

    if p_formats.is_null() {
        *p_count = aug.len() as u32;
        return vk::Result::SUCCESS;
    }
    let m = (*p_count as usize).min(aug.len());
    ptr::copy_nonoverlapping(aug.as_ptr(), p_formats, m);
    *p_count = m as u32;
    if m < aug.len() {
        vk::Result::INCOMPLETE
    } else {
        vk::Result::SUCCESS
    }
}

unsafe extern "system" fn get_surface_formats2(
    pdev: vk::PhysicalDevice,
    p_info: *const c_void,
    p_count: *mut u32,
    p_formats: *mut c_void,
) -> vk::Result {
    let down = instances().lock().ok().and_then(|g| {
        g.get(&key(pdev.as_raw()))
            .and_then(|d| d.get_surface_formats2)
    });
    let down = match down {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };

    let mut n = 0u32;
    let r = down(pdev, p_info, &mut n, ptr::null_mut());
    if r != vk::Result::SUCCESS {
        return r;
    }
    let mut real: Vec<SurfaceFormat2Raw> = (0..n)
        .map(|_| SurfaceFormat2Raw {
            s_type: vk::StructureType::SURFACE_FORMAT_2_KHR,
            p_next: ptr::null_mut(),
            surface_format: vk::SurfaceFormatKHR::default(),
        })
        .collect();
    if n > 0 {
        let r = down(pdev, p_info, &mut n, real.as_mut_ptr() as *mut c_void);
        if r != vk::Result::SUCCESS {
            return r;
        }
    }
    real.truncate(n as usize);

    // VkPhysicalDeviceSurfaceInfo2KHR: sType@0, pNext@8, surface@16
    let surface = vk::SurfaceKHR::from_raw(*((p_info as *const u8).add(16) as *const u64));

    let mut extras: Vec<vk::SurfaceFormatKHR> = Vec::new();
    if !real.is_empty() && should_inject(surface) {
        for e in hdr_extra() {
            if !real.iter().any(|x| {
                x.surface_format.format == e.format && x.surface_format.color_space == e.color_space
            }) {
                extras.push(e);
            }
        }
    }
    let total = real.len() + extras.len();

    if p_formats.is_null() {
        *p_count = total as u32;
        return vk::Result::SUCCESS;
    }
    let m = (*p_count as usize).min(total);
    let out = p_formats as *mut SurfaceFormat2Raw;
    for i in 0..m {
        let sf = if i < real.len() {
            real[i].surface_format
        } else {
            extras[i - real.len()]
        };
        let dst = out.add(i);
        (*dst).s_type = vk::StructureType::SURFACE_FORMAT_2_KHR;
        (*dst).surface_format = sf;
    }
    *p_count = m as u32;
    if m < total {
        vk::Result::INCOMPLETE
    } else {
        vk::Result::SUCCESS
    }
}
