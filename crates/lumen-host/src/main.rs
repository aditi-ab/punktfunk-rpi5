//! `lumen-host` — the Linux streaming host (plan §2, §6, §7).
//!
//! Creates a client-sized virtual display, captures it via PipeWire, encodes with
//! VAAPI/NVENC, and hands encoded access units to `lumen_core` for FEC + packetization +
//! pacing + send. Input flows back via libei/uinput. The platform backends are
//! `#[cfg(target_os = "linux")]`; the crate compiles everywhere so the workspace builds
//! on non-Linux dev machines — it just can't run the pipeline there.
//!
//! Status: scaffold. M0 wires capture→encode→file; M2 wires the full P1 host that a
//! stock Moonlight client connects to.

// Scaffold: trait methods and config paths are defined ahead of their backends.
#![allow(dead_code)]

mod capture;
mod encode;
mod inject;
mod pipeline;
mod vdisplay;
mod web;

use vdisplay::{Compositor, Mode};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!(
        "lumen-host scaffold (lumen_core ABI v{})",
        lumen_core::ABI_VERSION
    );

    // The intended startup sequence (each step is a separate, pluggable subsystem):
    //   1. negotiate mode + codec + FEC scheme over the control plane (web::WebConfig)
    //   2. vdisplay::open(compositor).create(mode)  -> client-sized virtual output
    //   3. capture::open_pipewire(node) ; encode::open(codec, bitrate)
    //   4. build a lumen_core::Session (host role) over a UDP transport to the client
    //   5. loop pipeline::pump_once(..)  until disconnect, then destroy the output
    let target_mode = Mode {
        width: 2560,
        height: 1440,
        refresh_hz: 240,
    };
    let compositor = Compositor::Kwin; // MVP target

    if cfg!(target_os = "linux") {
        tracing::info!(
            ?compositor,
            ?target_mode,
            "would create a virtual output and start streaming (backends pending M0/M2)"
        );
    } else {
        tracing::warn!(
            "this is a Linux host; on {} only the shared lumen_core builds and is testable",
            std::env::consts::OS
        );
    }
}
