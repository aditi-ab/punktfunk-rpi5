//! Virtual display orchestration (plan §6) — the project's differentiator.
//!
//! A [`VirtualDisplay`] creates a *client-sized* output on demand, rendered natively and
//! headless (no scaling), to be captured and streamed, then torn down on disconnect. There is
//! no cross-compositor Wayland protocol for this, so each compositor has its own backend behind
//! this trait:
//!
//! * **KWin** — privileged `zkde_screencast_unstable_v1::stream_virtual_output` ([`kwin`]).
//! * **wlroots/Sway** — `swaymsg create_output` + `output mode --custom` (TODO).
//! * **Mutter/GNOME** — D-Bus `RemoteDesktop` + `ScreenCast.RecordVirtual` (TODO).
//!
//! [`VirtualDisplay::create`] returns a [`VirtualOutput`]: the PipeWire node to capture plus an
//! owned keepalive whose `Drop` releases the output (RAII — no explicit `destroy`). Capture
//! consumes the node via [`crate::capture::capture_virtual_output`].

use anyhow::Result;
pub use lumen_core::Mode;
use std::os::fd::OwnedFd;

/// A created virtual output: a PipeWire source to capture, plus an owned keepalive whose drop
/// tears the output down (releases the compositor-side resource).
///
/// Allowed dead on non-Linux: the backends that construct it are all `cfg(target_os = "linux")`.
#[allow(dead_code)]
pub struct VirtualOutput {
    /// PipeWire node id of the output's screencast stream.
    pub node_id: u32,
    /// Portal/remote PipeWire fd when the node lives on a sandboxed remote (e.g. Mutter's
    /// RemoteDesktop+ScreenCast). `None` means the node is on the user's default PipeWire daemon
    /// (KWin `zkde_screencast`), captured by connecting to that daemon directly.
    pub remote_fd: Option<OwnedFd>,
    /// Keeps the output — and whatever connection/thread backs it — alive; dropped on teardown.
    pub keepalive: Box<dyn Send>,
}

/// Pluggable virtual-output creation, per compositor.
pub trait VirtualDisplay: Send {
    /// Human-readable backend name (e.g. `"kwin"`, `"wlroots"`, `"mutter"`).
    fn name(&self) -> &'static str;
    /// Create a virtual output of the given mode. Teardown is RAII: drop the returned
    /// [`VirtualOutput`]'s `keepalive`.
    fn create(&mut self, mode: Mode) -> Result<VirtualOutput>;
}

/// Compositors lumen knows how to drive (plan §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    /// KWin / Plasma 6 — `zkde_screencast` virtual output.
    Kwin,
    /// wlroots (Sway/Hyprland) — headless `create_output`.
    Wlroots,
    /// Mutter / GNOME — headless backend + Mutter DBus `RecordVirtual`.
    Mutter,
}

/// Detect the compositor to drive: `LUMEN_COMPOSITOR` override, else `XDG_CURRENT_DESKTOP`.
pub fn detect() -> Result<Compositor> {
    if let Ok(v) = std::env::var("LUMEN_COMPOSITOR") {
        return match v.trim().to_ascii_lowercase().as_str() {
            "kwin" | "kde" | "plasma" => Ok(Compositor::Kwin),
            "wlroots" | "sway" | "hyprland" | "wlr" => Ok(Compositor::Wlroots),
            "mutter" | "gnome" => Ok(Compositor::Mutter),
            other => anyhow::bail!("unknown LUMEN_COMPOSITOR '{other}' (kwin|wlroots|mutter)"),
        };
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    if desktop.contains("KDE") {
        Ok(Compositor::Kwin)
    } else if desktop.contains("GNOME") {
        Ok(Compositor::Mutter)
    } else if desktop.contains("SWAY")
        || desktop.contains("WLROOTS")
        || desktop.contains("HYPRLAND")
    {
        Ok(Compositor::Wlroots)
    } else {
        anyhow::bail!(
            "could not detect compositor from XDG_CURRENT_DESKTOP='{desktop}'; set LUMEN_COMPOSITOR"
        )
    }
}

/// Open the virtual-display driver for `compositor`.
pub fn open(compositor: Compositor) -> Result<Box<dyn VirtualDisplay>> {
    #[cfg(target_os = "linux")]
    {
        match compositor {
            Compositor::Kwin => Ok(Box::new(kwin::KwinDisplay::new()?)),
            Compositor::Wlroots => {
                anyhow::bail!("wlroots virtual-output backend not yet implemented")
            }
            Compositor::Mutter => {
                anyhow::bail!("mutter virtual-output backend not yet implemented")
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux (Wayland compositor)")
    }
}

#[cfg(target_os = "linux")]
mod kwin;
