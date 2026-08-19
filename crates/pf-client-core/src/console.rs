//! The console shell's platform-facing data types: what it RAISES out of its input handling
//! ([`OverlayAction`]), the pointer vocabulary a host feeds it ([`PointerInput`]) and the
//! session-lifecycle notifications a host feeds back ([`SessionPhase`]). Plain data with no
//! Vulkan/SDL in sight, kept in this portable crate because two hosts drive the same shell:
//! the Vulkan session binary (`pf-presenter`, which re-exports these under `overlay::`) and the
//! Android client's GL host. `pf-console-ui` sits ABOVE `pf-presenter` in the dependency
//! order, so the types cannot live there without a cycle.

/// An action the overlay raises out of its input handling (browse mode). Only actions
/// the RUN LOOP must act on live here — starting/canceling sessions and quitting; data
/// work (pairing, discovery, library fetches…) rides the console command bus instead.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OverlayAction {
    /// Start a session on this host. `launch` carries a library title id on the Hello
    /// (`None` streams the desktop); `title` is display-only (window title).
    Launch {
        addr: String,
        port: u16,
        fp_hex: String,
        launch: Option<String>,
        title: String,
        /// One-off settings-profile override for THIS launch (a profile id — a pinned
        /// card's connect). `None` resolves the host's default binding as before; the
        /// binary feeds it to `trust::effective_settings`, so a dangling id quietly
        /// falls back to the defaults and never blocks the connect.
        profile: Option<String>,
        /// The no-PIN delegated-approval path: pin the host's advertised fingerprint and
        /// open a connect the host PARKS until the operator approves this device in its
        /// console (a long connect budget), then persist it as paired. `false` = an
        /// ordinary connect to an already-paired host.
        request_access: bool,
    },
    /// Abort an in-flight connect (B while Connecting) — the console keeps browsing.
    /// The run loop stops the pump; a dial that already won the race is quit-closed.
    CancelConnect,
    /// Quit the launcher (B at the root) — ends the process, Gaming Mode returns.
    Quit,
    /// Put this text on the system clipboard (the host menu's "Copy link"). An action
    /// rather than a console command because the clipboard belongs to SDL, which lives on
    /// the run loop's thread and nowhere else.
    CopyText(String),
}

/// Which button a [`PointerInput`] press/release carries. A touchscreen contact always
/// arrives as `Primary` — there is no second finger-button, and the console's back
/// affordance is on glass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    /// The right button — the console reads it as Back, the pointer's B.
    Secondary,
}

/// Pointer or touch input offered to the overlay, in SWAPCHAIN PIXELS.
///
/// Pixels, not window coordinates, because that is the space the overlay renders in: a
/// screen hit-tests the very rects it drew last frame instead of re-deriving a layout
/// through the display scale. The run loop owns the conversion — it is the side that
/// holds the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerInput {
    Move {
        x: f32,
        y: f32,
    },
    Down {
        x: f32,
        y: f32,
        button: PointerButton,
        /// A finger (or stylus) on the glass, as opposed to a mouse button. The console
        /// defers a touch press until the lift so a swipe can scroll instead of acting on
        /// whatever the finger first lands on; a mouse press keeps acting immediately.
        /// Only `Down` carries it — the console tracks the gesture it opened, so the
        /// matching `Up`/`Move`/`Cancel` need no flag of their own.
        touch: bool,
    },
    Up {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    /// One wheel/trackpad scroll step at `x`/`y`; `dy` > 0 scrolls away from the user.
    Wheel {
        x: f32,
        y: f32,
        dy: f32,
    },
    /// The gesture was abandoned (the pointer left the window, the touch was canceled) —
    /// any armed press is dropped without acting.
    Cancel,
}

/// Session lifecycle notifications into the overlay (browse mode drives its scenes off
/// these; the OSD/HUD ignore them).
pub enum SessionPhase<'a> {
    /// A launch action was accepted — the connect is in flight.
    Connecting,
    /// Connected; frames are coming.
    Streaming,
    /// The connect failed (browse mode returns to the library with this message).
    Failed(&'a str),
    /// The session ran and ended (`Some` = abnormal reason for the status strip).
    Ended(Option<&'a str>),
    /// The session ended and the client is DIALING AGAIN by itself — today only because
    /// the negotiated codec ran out of decode rungs (M8's software-HEVC drop) and the
    /// retry advertises a codec this device can actually finish.
    ///
    /// Distinct from [`Self::Ended`] and [`Self::Failed`] because the user's next action
    /// is different: nothing. "Session ended — HEVC decoding failed" invites a manual
    /// reconnect that is already in flight, and "Couldn't connect" is simply false — the
    /// connect worked, the decode did not.
    Reconnecting(&'a str),
}
