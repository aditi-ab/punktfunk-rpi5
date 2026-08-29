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
