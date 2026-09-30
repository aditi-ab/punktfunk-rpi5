//! V4L2 video decode for the Linux client, the half that needs no device.
//!
//! [`uapi`] hand-declares the kernel structs, ioctl numbers, and fourccs the
//! decode rung uses; sizes are pinned by compile-time assertions and the
//! request codes by a test against the kernel's published values. The layouts
//! are the 64-bit little-endian ABI, which is every target the rung builds for.
//!
//! [`stateful`] is the memory-to-memory decoder flow — formats, queues, the
//! source-change renegotiation — written against the [`stateful::Device`]
//! trait. The ioctl implementation of that trait lives in `pf-client-core`;
//! `testing::FakeDecoder` (feature `testing`) is the only decoder most machines have.
//!
//! No `unsafe` here: nothing in this crate opens, maps, or calls a device.

#![forbid(unsafe_code)]

pub mod stateful;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod uapi;
