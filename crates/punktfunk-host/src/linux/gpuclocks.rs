//! GPU clock / P-state hygiene for the Linux host — the "easy-scene p99" lever (host-latency
//! plan Tier 1B): the driver's adaptive P-state ramps clocks down between bursty encode frames,
//! so every frame re-pays a spin-up. This is NOT theoretical — measured on the 780M (VCN 4),
//! a 1440p HEVC encode takes ~4.4 ms/frame with the clocks hot (120 fps pacing) but ~8 ms/frame
//! at a 60 fps duty cycle: the sag doubles per-frame encode latency.
//!
//! **AMD** (`PUNKTFUNK_PIN_CLOCKS=1`, root-gated by sysfs ownership): write `high` into each
//! amdgpu card's `power_dpm_force_performance_level` for the host lifetime, restoring the prior
//! value on exit. Non-root gets EACCES → logged once with the privilege recipe. Deliberately
//! opt-in: it defeats power management box-wide and is wrong on battery (Steam Deck!).
//!
//! **NVIDIA** — two independent halves, both no-ops off NVIDIA:
//!
//! 1. **`CudaNoStablePerfLimit` application profile** (no root needed): a CUDA/NVENC context
//!    clamps GeForce into the P2 performance state — reduced *memory* clock — for the process
//!    lifetime. NVIDIA's supported opt-out is an application profile keyed on the process name
//!    (shipped by default for `obs`/`Discord` since R595; the raw key `0x166c5e = 0` "should work
//!    with all supported driver versions" — NVIDIA engineer, open-gpu-kernel-modules#333). We drop
//!    a rule for `punktfunk-host` into `~/.nv/nvidia-application-profiles-rc.d/`; the driver's
//!    user-space component reads it at load, so it takes effect when libcuda/libGL next
//!    initializes (usually this same run — we write before any GPU work — else the next host
//!    start). Opt out with `PUNKTFUNK_NV_PROFILE=0`. (Do NOT set `CUDA_DISABLE_PERF_BOOST` for the
//!    host — that's the other half of the driver knob: it stops the boost *to* P2; the profile
//!    lifts the cap *at* P2 so the process can reach P0.)
//!
//! 2. **GPU core-clock floor** (`PUNKTFUNK_PIN_CLOCKS=1`, opt-in; root-gated by the driver):
//!    `nvmlDeviceSetGpuLockedClocks(TDP, UNLIMITED)` floors the core clock at the TDP/base clock
//!    while leaving boost headroom — NVIDIA's own latency guidance is "raise the floor, don't pin
//!    the max" (locking above base just gets throttled; a max pin only burns idle watts). Non-root
//!    callers get `NVML_ERROR_NO_PERMISSION` — logged once with the privilege recipe, then the
//!    host runs unpinned. The pin is undone on drop (host exit); after a crash it persists until
//!    driver reload/reboot, which the reset-before-pin on the next start self-heals. Deliberately
//!    NOT default-on: it defeats idle downclocking for the whole box and is wrong on
//!    battery-powered hosts.
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

/// `nvmlDevice_t` — an opaque driver handle.
type NvmlDevice = *mut c_void;

const NVML_SUCCESS: c_int = 0;
const NVML_ERROR_NO_PERMISSION: c_int = 4;
/// `nvmlClockLimitId_t`: symbolic "TDP/base clock" / "unlimited" sentinels for
/// `nvmlDeviceSetGpuLockedClocks` (nvml.h; `(TDP, UNLIMITED)` = "lower bound is TDP but clock may
/// boost above this" — the floor-without-capping combination).
const NVML_CLOCK_LIMIT_ID_TDP: c_uint = 0xffff_ff01;
const NVML_CLOCK_LIMIT_ID_UNLIMITED: c_uint = 0xffff_ff02;

/// The NVML entry points we use, resolved from `libnvidia-ml.so.1` at runtime (same pattern as
/// `zerocopy::cuda` — no link-time NVIDIA dependency, absent library = clean no-op).
struct Nvml {
    _lib: libloading::Library,
    init: unsafe extern "C" fn() -> c_int,
    shutdown: unsafe extern "C" fn() -> c_int,
    device_count: unsafe extern "C" fn(*mut c_uint) -> c_int,
    device_by_index: unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int,
    set_locked_clocks: unsafe extern "C" fn(NvmlDevice, c_uint, c_uint) -> c_int,
    reset_locked_clocks: unsafe extern "C" fn(NvmlDevice) -> c_int,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Nvml {
    fn load() -> Option<Nvml> {
        // SAFETY: `Library::new` runs the trusted NVIDIA driver library's initializers
        // (`libnvidia-ml.so.1`), exactly as `zerocopy::cuda` does for `libcuda.so.1`. Each
        // `lib.get` resolves a documented NVML symbol to the matching `unsafe extern "C"`
        // signature transcribed from nvml.h (all by-value ints/pointers, no callbacks). The
        // `Library` is stored in the returned struct, so every resolved fn pointer outlives its
        // uses (`_lib` drops last).
        unsafe {
            let lib = libloading::Library::new("libnvidia-ml.so.1")
                .or_else(|_| libloading::Library::new("libnvidia-ml.so"))
                .ok()?;
            let init = *lib.get(b"nvmlInit_v2\0").ok()?;
            let shutdown = *lib.get(b"nvmlShutdown\0").ok()?;
            let device_count = *lib.get(b"nvmlDeviceGetCount_v2\0").ok()?;
            let device_by_index = *lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let set_locked_clocks = *lib.get(b"nvmlDeviceSetGpuLockedClocks\0").ok()?;
            let reset_locked_clocks = *lib.get(b"nvmlDeviceResetGpuLockedClocks\0").ok()?;
            let error_string = *lib.get(b"nvmlErrorString\0").ok()?;
            Some(Nvml {
                _lib: lib,
                init,
                shutdown,
                device_count,
                device_by_index,
                set_locked_clocks,
                reset_locked_clocks,
                error_string,
            })
        }
    }

    fn err_str(&self, r: c_int) -> String {
        // SAFETY: `nvmlErrorString` returns a pointer into NVML's static error-string table for
        // ANY input value (documented total function), valid for the process lifetime; we only
        // read it via `CStr` while the library is loaded (`self` borrows `_lib`).
        unsafe {
            let p = (self.error_string)(r);
            if p.is_null() {
                format!("NVML error {r}")
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

/// Whether an NVIDIA GPU is present (device nodes; mirrors `encode::nvidia_present` — cheap and
/// side-effect-free, deliberately no CUDA/NVML init on the probe).
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

fn flag_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// An NVML session holding locked-clock floors on one or more NVIDIA devices.
struct NvmlPin {
    nvml: Nvml,
    pinned: Vec<NvmlDevice>,
}

/// One amdgpu card whose `power_dpm_force_performance_level` we forced to `high`; `restore` is
/// the pre-pin value written back on drop.
struct AmdPin {
    path: std::path::PathBuf,
    restore: String,
}

/// Host-lifetime guard: holds the armed clock pins (NVML floor and/or amdgpu perf level) and
/// undoes them on drop.
pub struct ClockGuard {
    nvml: Option<NvmlPin>,
    amd: Vec<AmdPin>,
}

// SAFETY: `ClockGuard` holds opaque NVML device handles + resolved fn pointers from the loaded
// driver library (plus plain sysfs paths/strings). NVML is documented thread-safe, the handles are
// plain driver tokens with no thread affinity, and the guard is only ever *moved* (held in `main`,
// dropped once at exit) and used through `&mut`/ownership — never shared. Transfer across threads
// is therefore sound.
unsafe impl Send for ClockGuard {}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        if let Some(pin) = &self.nvml {
            // SAFETY: each handle in `pinned` came from `nvmlDeviceGetHandleByIndex_v2` on this
            // live NVML session (init'd in `pin_nvidia`, shut down only here, after the resets).
            // The calls take the handle by value and return an int status — no Rust memory is
            // borrowed.
            unsafe {
                for &dev in &pin.pinned {
                    let _ = (pin.nvml.reset_locked_clocks)(dev);
                }
                let _ = (pin.nvml.shutdown)();
            }
            if !pin.pinned.is_empty() {
                tracing::info!("NVIDIA clock floor released (locked clocks reset)");
            }
        }
        for pin in &self.amd {
            match std::fs::write(&pin.path, &pin.restore) {
                Ok(()) => tracing::info!(
                    card = %pin.path.display(),
                    restored = %pin.restore,
                    "amdgpu performance level restored"
                ),
                Err(e) => tracing::warn!(
                    card = %pin.path.display(),
                    error = %e,
                    "could not restore amdgpu performance level"
                ),
            }
        }
    }
}

/// Startup hook for the host subcommands (`serve` / `punktfunk1-host`): install the NVIDIA P2-cap
/// application profile and, when `PUNKTFUNK_PIN_CLOCKS` is set, arm the vendor clock pin (NVML
/// core-clock floor / amdgpu `high` performance level). Returns the guard keeping the pins for
/// the host lifetime. `None` when nothing was armed.
pub fn on_host_start() -> Option<ClockGuard> {
    if nvidia_present() {
        ensure_cuda_perf_profile();
    }
    if !flag_truthy("PUNKTFUNK_PIN_CLOCKS") {
        return None;
    }
    let nvml = if nvidia_present() { pin_nvidia() } else { None };
    let amd = pin_amdgpu();
    if nvml.is_none() && amd.is_empty() {
        return None;
    }
    Some(ClockGuard { nvml, amd })
}

/// Force every amdgpu card's DPM performance level to `high` for the session — the encode-latency
/// lever on AMD: the VCN's per-frame time tracks the clock sag (measured 8 → 4.4 ms/frame at
/// 1440p on the 780M once clocks stay hot). Remembers the prior level for restore-on-drop.
/// Root-gated by sysfs ownership; non-root warns once with the recipe, the host runs unpinned.
fn pin_amdgpu() -> Vec<AmdPin> {
    let mut pins = Vec::new();
    let mut denied = false;
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return pins;
    };
    for entry in cards.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        // Cards only (card0, card1, …) — not connectors (card0-DP-1) or render nodes.
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = entry.path().join("device");
        let is_amdgpu = std::fs::read_link(dev.join("driver"))
            .map(|t| t.to_string_lossy().ends_with("amdgpu"))
            .unwrap_or(false);
        if !is_amdgpu {
            continue;
        }
        let path = dev.join("power_dpm_force_performance_level");
        let Ok(prev) = std::fs::read_to_string(&path) else {
            continue;
        };
        let prev = prev.trim().to_string();
        if prev == "high" {
            continue; // already pinned (externally) — nothing to do or restore
        }
        match std::fs::write(&path, "high") {
            Ok(()) => {
                tracing::info!(
                    card = %name,
                    was = %prev,
                    "amdgpu performance level pinned to high (encode clock sag removed) — \
                     restored on host exit"
                );
                pins.push(AmdPin {
                    path,
                    restore: prev,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            Err(e) => tracing::debug!(card = %name, error = %e, "amdgpu perf-level write failed"),
        }
    }
    if denied {
        tracing::warn!(
            "PUNKTFUNK_PIN_CLOCKS: writing power_dpm_force_performance_level requires root — \
             grant it via a boot oneshot / udev rule chowning the attribute, or run the host as \
             a system service. The host keeps running unpinned"
        );
    }
    pins
}

/// Floor the core clock at TDP/base on every NVIDIA device (reset first, so a stale pin from a
/// crashed previous run is replaced rather than compounded).
fn pin_nvidia() -> Option<NvmlPin> {
    let nvml = match Nvml::load() {
        Some(n) => n,
        None => {
            tracing::warn!("PUNKTFUNK_PIN_CLOCKS: libnvidia-ml not loadable — clocks not pinned");
            return None;
        }
    };
    // SAFETY: all calls follow the documented NVML lifecycle on the successfully-loaded library:
    // `nvmlInit_v2` first (status-checked; on failure we return without touching anything else),
    // then count/handle queries writing through valid `&mut` out-pointers of the exact C types,
    // then set/reset taking those returned handles by value. `shutdown` is called on every path
    // that does not hand the session to a `ClockGuard` (whose Drop shuts it down).
    unsafe {
        let r = (nvml.init)();
        if r != NVML_SUCCESS {
            tracing::warn!(
                error = nvml.err_str(r),
                "PUNKTFUNK_PIN_CLOCKS: NVML init failed — clocks not pinned"
            );
            return None;
        }
        let mut count: c_uint = 0;
        if (nvml.device_count)(&mut count) != NVML_SUCCESS || count == 0 {
            let _ = (nvml.shutdown)();
            return None;
        }
        let mut pinned = Vec::new();
        let mut denied = false;
        for i in 0..count {
            let mut dev: NvmlDevice = std::ptr::null_mut();
            if (nvml.device_by_index)(i, &mut dev) != NVML_SUCCESS {
                continue;
            }
            let _ = (nvml.reset_locked_clocks)(dev);
            let r = (nvml.set_locked_clocks)(
                dev,
                NVML_CLOCK_LIMIT_ID_TDP,
                NVML_CLOCK_LIMIT_ID_UNLIMITED,
            );
            match r {
                NVML_SUCCESS => pinned.push(dev),
                NVML_ERROR_NO_PERMISSION => denied = true,
                _ => tracing::debug!(
                    device = i,
                    error = nvml.err_str(r),
                    "SetGpuLockedClocks failed"
                ),
            }
        }
        if denied {
            // The driver gates locked clocks to root — no GeForce exception. Give the operator
            // the two supported recipes instead of failing the host.
            tracing::warn!(
                "PUNKTFUNK_PIN_CLOCKS: the driver requires root for locked clocks \
                 (NVML_ERROR_NO_PERMISSION). Grant it via a boot oneshot (`nvidia-smi -lgc \
                 tdp,unlimited`) or sudoers (`<user> ALL=(ALL) NOPASSWD: /usr/bin/nvidia-smi`) — \
                 the host keeps running unpinned"
            );
        }
        if pinned.is_empty() {
            let _ = (nvml.shutdown)();
            return None;
        }
        tracing::info!(
            devices = pinned.len(),
            "NVIDIA core-clock floor armed (min=TDP/base, max=boost) — released on host exit"
        );
        Some(NvmlPin { nvml, pinned })
    }
}

/// Install the `CudaNoStablePerfLimit` application profile + a `punktfunk-host` procname rule in
/// `~/.nv/nvidia-application-profiles-rc.d/` (created if missing, never overwritten — the file is
/// the operator's once it exists). Lifts the driver's P2 memory-clock cap for the host process.
fn ensure_cuda_perf_profile() {
    if std::env::var("PUNKTFUNK_NV_PROFILE").as_deref() == Ok("0") {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home)
        .join(".nv")
        .join("nvidia-application-profiles-rc.d");
    let path = dir.join("50-punktfunk");
    if path.exists() {
        return;
    }
    // The exact shape NVIDIA published (open-gpu-kernel-modules#333) and ships for obs/Discord in
    // R595; the inline profile definition makes it work on pre-R595 drivers too.
    let profile = r#"{
    "profiles": [ { "name": "CudaNoStablePerfLimit", "settings": [ "0x166c5e", 0 ] } ],
    "rules": [
        { "pattern": { "feature": "procname", "matches": "punktfunk-host" }, "profile": "CudaNoStablePerfLimit" }
    ]
}
"#;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, profile)
    };
    match write() {
        Ok(()) => tracing::info!(
            path = %path.display(),
            "installed the CudaNoStablePerfLimit driver profile (lifts the P2 memory-clock cap \
             for NVENC/CUDA; read when the driver next initializes — PUNKTFUNK_NV_PROFILE=0 opts \
             out)"
        ),
        Err(e) => tracing::debug!(error = %e, "could not install the NVIDIA application profile"),
    }
}
