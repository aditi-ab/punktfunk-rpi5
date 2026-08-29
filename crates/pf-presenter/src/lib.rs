//! The Vulkan session presenter (punktfunk-planning `linux-client-rearchitecture.md`,
//! Phase 1): an SDL3 window + ash swapchain that presents the shared session pump's
//! decoded frames, captures input on the `ui_stream` state-machine contract, and reports
//! the unified stats window on stdout. No UI toolkit anywhere in the dependency tree.
//!
//! Three frame paths: software (`CpuPlanarFrame` — I420 planes staged into three R8
//! images and converted by the same CICP-driven CSC pass as the hardware lanes; before M8
//! this lane arrived as swscale RGBA and skipped the pass entirely), Vulkan Video (the
//! decoder's VkImage on THIS device — plane views + the CICP-driven CSC pass), and on
//! Linux additionally VAAPI hardware (NV12 dmabuf imported per-plane — `dmabuf.rs`),
//! all composited by a letterboxed blit. Devices without the import extensions, and any
//! import/present failure streak, demote the decoder to software via the session pump's
//! `force_software` contract, same as the GTK presenter.
//!
//! Builds on Linux AND Windows; `dmabuf` is Linux-only (DRM-PRIME does not exist on
//! Windows) and `d3d11` is its Windows counterpart (D3D11VA shared-texture import) —
//! the decode chain there is Vulkan → D3D11VA → software.

// Unsafe-proof program: every `unsafe {}` in this crate carries a `// SAFETY:` proof.

// THE VULKAN CONTRACT, stated once - most `// SAFETY:` proofs in this crate are an instance of it.
//
// Nearly every `unsafe` here is an `ash` call, which is `unsafe` because Vulkan is a C API, not
// because each call carries its own bespoke obligation. Three shapes recur, and only one of them
// has a real precondition worth restating per site:
//
//  * CREATE / ALLOCATE - `create_*`, `allocate_*`. The device is live (this type owns it), and the
//    `vk::*CreateInfo` builders are locals that outlive the synchronous call. The handle returned is
//    owned by the value being constructed and destroyed in its `Drop`.
//  * RECORD - `cmd_*`, `begin/end_command_buffer`, `update_descriptor_sets`. Recorded into a command
//    buffer this code owns and has begun, referencing handles it also owns. Nothing executes until
//    submit, so a recording error is not yet a memory error.
//  * DESTROY - `destroy_*`, `free_*`, `unmap_memory`. THIS is the one with a real obligation: the
//    GPU must not still be using the object. That is established on the path, not by the call - a
//    fence wait, a `queue_wait_idle`, or the swapchain having been retired - and the per-site proofs
//    say so, because getting it wrong is a use-after-free the type system cannot catch.
//
// A block doing something OUTSIDE these three shapes gets a real, specific proof; if you add one and
// find yourself writing "as above", it probably belongs in one of them.

#[cfg(any(target_os = "linux", windows))]
pub mod csc;
#[cfg(any(target_os = "linux", windows))]
pub mod cursor;
#[cfg(windows)]
pub mod d3d11;
#[cfg(target_os = "linux")]
pub mod dmabuf;
#[cfg(any(target_os = "linux", windows))]
pub mod input;
#[cfg(any(target_os = "linux", windows))]
pub mod keymap_sdl;
#[cfg(any(target_os = "linux", windows))]
pub mod overlay;
#[cfg(any(target_os = "linux", windows))]
mod present_pace;
#[cfg(any(target_os = "linux", windows))]
mod run;
// Pure gesture logic with no SDL or Vulkan dependency: built (and tested) on every platform.
pub mod touch;
#[cfg(any(target_os = "linux", windows))]
pub mod vk;
#[cfg(windows)]
mod win32;

#[cfg(any(target_os = "linux", windows))]
pub use run::{run_browse, run_session, ActionOutcome, Outcome, SessionOpts};
