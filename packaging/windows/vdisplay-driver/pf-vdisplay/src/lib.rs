//! pf-vdisplay — punktfunk Windows virtual display (IddCx), in Rust.
//!
//! P1: a UMDF2 IddCx virtual display. Adapted from MolotovCherry/virtual-display-rs (MIT) — its
//! named-pipe IPC + serde mode config is replaced by an in-tree `monitor` model (and, next, the
//! SudoVDA-compatible IOCTL control plane our host already speaks). Logging goes to
//! `OutputDebugString` (no `log`-eventlog/`tokio`). See `docs/windows-virtual-display-rust-port.md`.
#![allow(non_snake_case)]

mod callbacks;
mod context;
mod control;
mod direct_3d_device;
mod edid;
mod entry;
mod helpers;
mod logger;
mod monitor;
mod panic;
mod swap_chain_processor;

use wdf_umdf_sys::{NTSTATUS, PUNICODE_STRING, PVOID};

// The framework entry point. UMDF's reflector calls this; the `+whole-archive` stub forwards to the
// `DriverEntry` symbol exported from `entry.rs`.
#[link(name = "WdfDriverStubUm", kind = "static", modifiers = "+whole-archive")]
extern "C" {
    pub fn FxDriverEntryUm(
        LoaderInterface: PVOID,
        Context: PVOID,
        DriverObject: PVOID,
        RegistryPath: PUNICODE_STRING,
    ) -> NTSTATUS;
}
