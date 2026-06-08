//! Input injection (plan §4): turn client [`lumen_core::input::InputEvent`]s into host
//! input. Wayland-native via libei (`reis`) first; uinput as the universal fallback.

use anyhow::Result;
use lumen_core::input::InputEvent;

/// Injects input events into the host session.
pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Preferred injection backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// libei via `reis` — Wayland-native (RemoteDesktop portal).
    Libei,
    /// `/dev/uinput` — universal fallback, always available.
    Uinput,
}

pub fn open(_backend: Backend) -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        anyhow::bail!("libei/uinput injection not yet implemented (M2)")
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("input injection requires Linux (libei/uinput)")
    }
}
