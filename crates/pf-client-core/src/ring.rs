//! The in-stream quick-action ring's portable contract (design/touch-client-overlay.md §2–3):
//! what the presenter feeds it, what it asks back, and the session facts its slots draw from.
//! Plain data — the Skia console renders it on desktop; the Android GL host may share it.

/// What the presenter feeds the ring: the two-finger twist, or a key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RingInput {
    /// The twist is turning: `progress` 0…1 drives the unwind, `clockwise` is the hand's
    /// direction, `x`/`y` (window pixels) the centroid the ring is centred on.
    Turn {
        progress: f32,
        clockwise: bool,
        x: f32,
        y: f32,
    },
    /// The twist reached the commit angle: the ring stays open after the lift.
    Commit,
    /// Lifted short of commit, or wound back after one: the ring winds back in.
    Cancel,
    /// A key or chord: open at `x`/`y`, or close if open.
    Toggle { x: f32, y: f32 },
}

/// What the ring asks the session to do; the presenter drains these once per iteration.
/// Host actions do not appear here — the console's own command bus carries them.
#[derive(Clone, Debug, PartialEq)]
pub enum RingCommand {
    EndStream,
    DisconnectLinger,
    CycleStats,
    ToggleMic,
    CycleTouchMode,
    /// Toggle the platform's text input (Steam's on-screen keyboard under gamescope).
    Keyboard,
    RequestMode {
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    /// A custom chord as key NAMES (`ctrl`, `shift`, `escape`); see `overlay_actions::key_vk`.
    Shortcut(Vec<String>),
}

/// The ring's geometry at 100 % scale, in the client's design units (px on Skia, dp/pt on the
/// phones). Every desktop drawing of the ring — the Skia console in-stream and its editor,
/// the GTK shell's editor, the Windows client's editor — reads these, so the three cannot
/// drift apart (design tenet 8).
pub const RING_RADIUS: f32 = 120.0;
pub const SLOT_DIAMETER: f32 = 56.0;
pub const CENTRE_DIAMETER: f32 = 64.0;

/// Slot `k` sits at 12, 2, 4… o'clock: its angle in degrees with 0 at 3 o'clock, clockwise.
pub fn slot_angle_deg(k: usize) -> f32 {
    -90.0 + 60.0 * k as f32
}

/// Slot `k`'s centre relative to the ring's centre, `radius` out, y down.
pub fn slot_offset(k: usize, radius: f32) -> (f32, f32) {
    let (s, c) = slot_angle_deg(k).to_radians().sin_cos();
    (radius * c, radius * s)
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    /// Slot 0 is straight up, slot 3 straight down, and the six sit 60° apart on the circle.
    #[test]
    fn slots_sit_at_twelve_two_four_six_eight_and_ten() {
        let (x0, y0) = slot_offset(0, 100.0);
        assert!(
            x0.abs() < 1e-3 && (y0 + 100.0).abs() < 1e-3,
            "12 o'clock is up: {x0} {y0}"
        );
        let (x3, y3) = slot_offset(3, 100.0);
        assert!(
            x3.abs() < 1e-3 && (y3 - 100.0).abs() < 1e-3,
            "6 o'clock is down"
        );
        let (x1, y1) = slot_offset(1, 100.0);
        assert!(x1 > 0.0 && y1 < 0.0, "2 o'clock is up and right");
        for k in 0..6 {
            assert!((slot_angle_deg(k) - (-90.0 + 60.0 * k as f32)).abs() < 1e-6);
        }
    }
}

/// The session facts the ring's slots show and gate on, composed by the presenter per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RingFacts {
    /// The resolved profile's `overlay_actions` blob (empty = the platform default ring).
    pub overlay_actions: String,
    /// The live touch model's stored name (`trackpad` / `pointer` / `touch`).
    pub touch_mode: String,
    /// Whether the host injects touch contacts — the `touch` model is skipped without it.
    pub host_accepts_touch: bool,
    /// The stats tier's label.
    pub stats_tier: String,
    pub mic_available: bool,
    pub mic_muted: bool,
    /// The live mode, and the mode the Welcome carried (the Resolution row's "Native").
    pub mode: (u32, u32, u32),
    pub native_mode: (u32, u32, u32),
    /// The host, for its pre-fetched actions: address, management port, pinned
    /// fingerprint (hex) and display name.
    pub addr: String,
    pub mgmt_port: u16,
    pub fp_hex: String,
    pub host_name: String,
}
