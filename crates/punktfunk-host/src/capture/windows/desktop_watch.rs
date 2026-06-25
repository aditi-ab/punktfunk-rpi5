//! Input-desktop watcher (Windows) — the authoritative "normal vs secure desktop" signal for the
//! two-process secure-desktop design (docs/windows-secure-desktop.md).
//!
//! Windows switches the *input desktop* to "Winlogon" (the secure desktop) for UAC elevation, the
//! lock screen and the login screen, and back to "Default" for the normal session. WGC captures only
//! the normal desktop; DDA-as-SYSTEM captures the secure one. A dedicated thread polls the input
//! desktop's NAME (WTS session notifications miss UAC entirely, so the name is the reliable signal)
//! and publishes it as an atomic the capture mux + input path read.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, UOI_NAME,
};

/// The normal interactive desktop ("Default") — WGC capture applies.
pub const DESKTOP_NORMAL: u8 = 0;
/// The secure desktop ("Winlogon": UAC / lock / login) — DDA-as-SYSTEM capture applies.
pub const DESKTOP_SECURE: u8 = 1;

/// Polls the input-desktop name on its own thread and publishes [`DESKTOP_NORMAL`]/[`DESKTOP_SECURE`].
pub struct DesktopWatcher {
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
}

impl DesktopWatcher {
    pub fn start() -> Self {
        // Compute the CURRENT desktop synchronously before returning, so the first reader (the capture
        // mux) sees the real state immediately. Otherwise a session that begins already on the secure
        // desktop (e.g. a reconnect to a locked box) would read DESKTOP_NORMAL for the first poll
        // interval and relay one stale normal-desktop frame — the "flash of the login screen" bug.
        let initial = if unsafe { is_secure_desktop() } {
            DESKTOP_SECURE
        } else {
            DESKTOP_NORMAL
        };
        let state = Arc::new(AtomicU8::new(initial));
        let stop = Arc::new(AtomicBool::new(false));
        let s = state.clone();
        let st = stop.clone();
        let _ = std::thread::Builder::new()
            .name("desktop-watch".into())
            .spawn(move || {
                // Debounce: only publish a change after the raw reading has been stable for several
                // polls. The input desktop flaps Default↔Winlogon transiently during a lock/UAC
                // transition; publishing every flap makes the capture mux thrash (rebuild storms).
                const STABLE_POLLS: u32 = 4; // ~80ms
                let mut published = initial;
                let mut candidate = initial;
                let mut stable = 0u32;
                while !st.load(Ordering::Relaxed) {
                    let v = if unsafe { is_secure_desktop() } {
                        DESKTOP_SECURE
                    } else {
                        DESKTOP_NORMAL
                    };
                    if v == candidate {
                        stable = stable.saturating_add(1);
                    } else {
                        candidate = v;
                        stable = 1;
                    }
                    if stable >= STABLE_POLLS && candidate != published {
                        s.store(candidate, Ordering::Release);
                        published = candidate;
                        tracing::info!(
                            desktop = if candidate == DESKTOP_SECURE {
                                "Winlogon(secure)"
                            } else {
                                "Default"
                            },
                            "input desktop changed (debounced)"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
        DesktopWatcher { state, stop }
    }

    /// The shared atomic ([`DESKTOP_NORMAL`]/[`DESKTOP_SECURE`]) for the capture mux to read.
    pub fn state(&self) -> Arc<AtomicU8> {
        self.state.clone()
    }

    /// True when the secure (Winlogon) desktop is the input desktop right now.
    pub fn is_secure(&self) -> bool {
        self.state.load(Ordering::Acquire) == DESKTOP_SECURE
    }
}

impl Drop for DesktopWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// True if the current input desktop is "Winlogon" (the secure desktop). Best-effort: if the desktop
/// can't be opened or named, report not-secure (the safe default — keep WGC/normal capture).
pub(crate) unsafe fn is_secure_desktop() -> bool {
    let desk = match OpenInputDesktop(
        DESKTOP_CONTROL_FLAGS(0),
        false,
        DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS),
    ) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    let ok = GetUserObjectInformationW(
        HANDLE(desk.0),
        UOI_NAME,
        Some(buf.as_mut_ptr() as *mut _),
        (buf.len() * 2) as u32,
        Some(&mut needed),
    )
    .is_ok();
    let _ = CloseDesktop(desk);
    if !ok {
        return false;
    }
    let name = String::from_utf16_lossy(&buf);
    name.trim_end_matches('\u{0}')
        .eq_ignore_ascii_case("Winlogon")
}

/// `DESKTOP_READOBJECTS` access right (the windows crate exposes it as a typed flag; we need the raw
/// bit for `OpenInputDesktop`'s access mask).
const DESKTOP_READOBJECTS: u32 = 0x0001;
