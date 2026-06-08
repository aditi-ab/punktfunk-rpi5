//! Virtual display orchestration (plan §6) — the project's differentiator.
//!
//! A [`VirtualDisplay`] creates a client-sized output on demand, to be captured and
//! streamed, then torn down on disconnect. Two deployment models exist (Model A: attach
//! to the running session; Model B: dedicated headless session); both sit behind this
//! trait so compositors are pluggable and a stuck one never blocks the project.
//!
//! Backends are `#[cfg(target_os = "linux")]` and currently stubs (see the per-backend
//! modules). The MVP target is KWin; a wlroots spike validates the pipeline first.

use anyhow::Result;
pub use lumen_core::Mode;

/// Opaque handle to a created virtual output, returned by [`VirtualDisplay::create`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputHandle(pub u64);

/// Pluggable virtual-output creation, per compositor.
pub trait VirtualDisplay: Send {
    /// Human-readable backend name (e.g. `"kwin"`, `"wlroots"`, `"mutter"`).
    fn name(&self) -> &'static str;
    /// Create a virtual output of the given mode.
    fn create(&mut self, mode: Mode) -> Result<OutputHandle>;
    /// Destroy a previously created output.
    fn destroy(&mut self, handle: OutputHandle) -> Result<()>;
}

/// Compositors lumen knows how to drive (plan §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    /// KWin / Plasma 6 — MVP target (matches the CachyOS/KDE daily driver).
    Kwin,
    /// wlroots (Sway/Hyprland) — fastest to prototype the pipeline.
    Wlroots,
    /// Mutter / GNOME — headless backend + Mutter DBus.
    Mutter,
}

/// Detect or select a backend and return its driver.
pub fn open(compositor: Compositor) -> Result<Box<dyn VirtualDisplay>> {
    #[cfg(target_os = "linux")]
    {
        match compositor {
            Compositor::Kwin => Ok(Box::new(linux::kwin::KwinDisplay::new()?)),
            Compositor::Wlroots => Ok(Box::new(linux::wlroots::WlrootsDisplay::new()?)),
            Compositor::Mutter => Ok(Box::new(linux::mutter::MutterDisplay::new()?)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = compositor;
        anyhow::bail!("virtual displays require Linux (Wayland compositor)")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    //! Linux backends. TODO(M2): drive KWin via DBus (study KRdp's source for the
    //! virtual-output path); wlroots via `create_output` on the headless backend;
    //! Mutter via `org.gnome.Mutter.*`.
    macro_rules! stub_backend {
        ($modname:ident, $ty:ident, $name:literal) => {
            pub mod $modname {
                use super::super::{Mode, OutputHandle, VirtualDisplay};
                use anyhow::Result;

                pub struct $ty;
                impl $ty {
                    pub fn new() -> Result<Self> {
                        Ok($ty)
                    }
                }
                impl VirtualDisplay for $ty {
                    fn name(&self) -> &'static str {
                        $name
                    }
                    fn create(&mut self, _mode: Mode) -> Result<OutputHandle> {
                        anyhow::bail!(concat!(
                            $name,
                            " virtual-output creation not yet implemented"
                        ))
                    }
                    fn destroy(&mut self, _handle: OutputHandle) -> Result<()> {
                        anyhow::bail!(concat!(
                            $name,
                            " virtual-output destroy not yet implemented"
                        ))
                    }
                }
            }
        };
    }
    stub_backend!(kwin, KwinDisplay, "kwin");
    stub_backend!(wlroots, WlrootsDisplay, "wlroots");
    stub_backend!(mutter, MutterDisplay, "mutter");
}
