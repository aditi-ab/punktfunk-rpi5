//! The Windows sources and the pure models they run on. A model compiles on every target, so
//! its table runs on Linux CI; a source is `cfg(windows)`. The IDD models live in `idd/` and
//! are declared here, outside the Windows-only `idd_push` module.
//!
//! - `remote/`: the remote-encoder source the driver and the capture worker share.
//! - `idd/`: IDD-push, where the pf-vdisplay driver encodes and this side owns no pixels.
//! - `wgc.rs`: a monitor the host did not create, captured and encoded by a capture worker.
//! - `gpu_pref.rs`: the win32u GPU-preference hook a hybrid-GPU box needs before DXGI starts.
//! - `win32u_stub.rs`: the bytes that hook writes, and the one head it may write over.
//! - `cursor_state.rs`: the IDD cursor model: who draws the pointer, what the driver is told.
//! - `cursor_witness.rs`: did the pointer travel through a gap, for the recovery classifier.
//! - `cursor_raster.rs`: the GDI cursor poller's planes-to-RGBA rasterisers.
//! - `stall_model.rs`: holes in DWM delivery and their verdicts; `idd/stall.rs` reports them.
//! - `idd/depth_pin.rs`: which depth the display holds, and when a change re-opens the encoder.
//! - `idd/open_plan.rs`: the IDD open as a plan of steps, and its display half over the seam.
//! - `idd/tick.rs`: the IDD tick: the wrapper's order around the remote source's phases.
//! - `synthetic_nv12.rs`: a headless NV12 D3D11 source for the GPU encoder tests.

mod cursor_raster;
mod cursor_state;
mod cursor_witness;
#[path = "idd/depth_pin.rs"]
mod depth_pin;
#[cfg(target_os = "windows")]
pub mod gpu_pref;
#[cfg(target_os = "windows")]
#[path = "idd/mod.rs"]
pub(crate) mod idd_push;
#[path = "idd/open_plan.rs"]
mod open_plan;
pub(crate) mod remote;
mod stall_model;
#[cfg(target_os = "windows")]
pub mod synthetic_nv12;
#[path = "idd/tick.rs"]
mod tick;
#[cfg(target_os = "windows")]
pub(crate) mod wgc;
mod win32u_stub;
