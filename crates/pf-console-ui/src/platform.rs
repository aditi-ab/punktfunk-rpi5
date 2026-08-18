//! Which platform the shell fronts. One shell, two hosts (design
//! android-skia-console-port.md D3/D7): the screens are the same everywhere, but not every
//! settings row means something on every platform — a decoder picker is a desktop concept,
//! low-latency decode an Android one — and only Android has native sub-screens (its
//! Controllers and Licenses views) for the settings list to open. Everything platform-shaped
//! is decided by asking this enum, so the row tables stay one union and no screen carries a
//! `cfg`.

/// The host platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// The Vulkan session binary on Linux/Windows (Steam Deck included).
    Desktop,
    /// The Android client's GL host — phones, tablets, TV boxes.
    Android,
}

/// A native screen the platform owns and the settings list can open (D7): the shell
/// raises [`crate::model::ConsoleCmd::OpenPlatformScreen`] with one of these and suspends
/// its own input until the host says the screen closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformScreen {
    /// Android's connected-controllers view (USB grant, rumble/haptics tests, DS capture).
    Controllers,
    /// The open-source licences view.
    Licenses,
}

impl PlatformScreen {
    /// The stable id the host matches on (crosses JNI as a string).
    pub fn id(self) -> &'static str {
        match self {
            PlatformScreen::Controllers => "controllers",
            PlatformScreen::Licenses => "licenses",
        }
    }
}
