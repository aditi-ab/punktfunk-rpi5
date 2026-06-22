//! The monitor + mode model and control-plane state. Replaces virtual-display-rs's `ipc.rs`
//! (named-pipe IPC + serde `driver_ipc` types). Monitors are created on demand by the SudoVDA IOCTL
//! control plane (`control.rs`); each carries the GUID the host keys it by plus the OS target id +
//! render-adapter LUID captured at arrival (the ADD reply).

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Mutex, OnceLock};

use wdf_umdf_sys::{IDDCX_ADAPTER__, IDDCX_MONITOR__};

pub type Dimen = u32;
pub type RefreshRate = u32;

/// One resolution with the refresh rates it supports.
#[derive(Clone, PartialEq, Eq)]
pub struct Mode {
    pub width: Dimen,
    pub height: Dimen,
    pub refresh_rates: Vec<RefreshRate>,
}

/// A monitor's identity (the EDID serial) + advertised modes.
#[derive(Clone)]
pub struct MonitorData {
    pub id: u32,
    pub modes: Vec<Mode>,
}

/// A live (or pending) monitor.
pub struct MonitorObject {
    pub object: Option<NonNull<IDDCX_MONITOR__>>,
    pub data: MonitorData,
    /// The full GUID the host keys this monitor by (ADD dedup / REMOVE).
    pub guid: u128,
    /// OS target id + render-adapter LUID, captured from `IDARG_OUT_MONITORARRIVAL` (the ADD reply).
    pub target_id: u32,
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
}
// SAFETY: the raw IddCx object ptr is framework-managed; access is serialized by MONITOR_MODES.
unsafe impl Send for MonitorObject {}
unsafe impl Sync for MonitorObject {}

/// The IddCx adapter object, stashed for the control plane (SET_RENDER_ADAPTER).
pub struct AdapterObject(pub NonNull<IDDCX_ADAPTER__>);
// SAFETY: raw ptr managed by the framework.
unsafe impl Send for AdapterObject {}
unsafe impl Sync for AdapterObject {}

pub static ADAPTER: OnceLock<AdapterObject> = OnceLock::new();
pub static MONITOR_MODES: Mutex<Vec<MonitorObject>> = Mutex::new(Vec::new());

/// Monitor id / EDID-serial counter (unique per created monitor).
pub static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Watchdog (seconds). The host reads the timeout via GET_WATCHDOG and PINGs to keep alive.
pub static WATCHDOG_TIMEOUT: AtomicU32 = AtomicU32::new(3);
pub static WATCHDOG_COUNTDOWN: AtomicU32 = AtomicU32::new(3);
/// The preferred render adapter LUID set via SET_RENDER_ADAPTER, packed `(high<<32)|low`. 0 = none.
pub static PREFERRED_RENDER_ADAPTER: AtomicU64 = AtomicU64::new(0);

/// Protocol version reported by GET_VERSION: {major, minor, incremental, testbuild} — matches SudoVDA.
pub const PROTOCOL_VERSION: [u8; 4] = [0, 2, 1, 1];

/// A single (width, height, refresh) tuple — modes flattened across their refresh rates.
#[derive(Copy, Clone)]
pub struct ModeItem {
    pub width: Dimen,
    pub height: Dimen,
    pub refresh_rate: RefreshRate,
}

pub trait FlattenModes {
    fn flatten(&self) -> impl Iterator<Item = ModeItem>;
}

impl FlattenModes for Vec<Mode> {
    fn flatten(&self) -> impl Iterator<Item = ModeItem> {
        self.iter().flat_map(|m| {
            m.refresh_rates.iter().map(|&rr| ModeItem {
                width: m.width,
                height: m.height,
                refresh_rate: rr,
            })
        })
    }
}

/// Fallback modes appended after the client's requested mode, so a topology change still has options.
pub fn default_modes() -> Vec<Mode> {
    vec![
        Mode {
            width: 1920,
            height: 1080,
            refresh_rates: vec![60, 120],
        },
        Mode {
            width: 1280,
            height: 720,
            refresh_rates: vec![60],
        },
    ]
}
