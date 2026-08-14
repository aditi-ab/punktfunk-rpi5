//! Which ScreenCast cursor mode to ASK the portal for — negotiated against what the backend
//! advertises, rather than asserted.
//!
//! The portal spec is unforgiving here: `SelectSources` with a cursor mode that is absent from
//! `AvailableCursorModes` does not quietly degrade — **xdg-desktop-portal itself rejects the call**
//! (`"Unavailable cursor mode %x"`, an `INVALID_ARGUMENT` from the FRONTEND, which validates the
//! request against the backend's advertised bitfield before the backend ever sees it). Both
//! wlr-family backends used to hardcode `Metadata` whenever the session had negotiated the cursor
//! channel, so every cursor-forward session died at `select_sources` — `unavailable cursor mode 4`
//! (4 being `Metadata`'s bit) and a client left on a black screen behind "pipeline build failed".
//! Field report 2026-08-14.
//!
//! ⚠️ This is NOT a stale-portal problem, and not Hyprland-specific. MEASURED on .21 2026-08-14 on
//! fully current packages — Hyprland **0.56.2**, xdg-desktop-portal-hyprland **1.4.1**,
//! xdg-desktop-portal **1.22.1** — with a live session and xdph attached (`[screencopy] init
//! successful`): `AvailableCursorModes` reads **3** (`Hidden|Embedded`) on both the backend impl
//! interface and the frontend. **Metadata is simply not offered by xdph today.** xdpw is the same
//! story from the other end: its `screencast.c` refuses `METADATA` outright. So the hardcode broke
//! every cursor-forward session on the entire wlr family, on current software — not only on old
//! installs. (xdph 1.4.1 would itself fall back — its binary carries
//! `"[screencopy] unsupported cursor_mode {}, fallback to {}"` — but it never gets the chance,
//! because the frontend fails the call first.)
//!
//! `pf-capture`'s own portal path has always negotiated (`portal::choose_cursor_mode`) — this is
//! that ladder, restated in the crate that owns the virtual-display backends. pf-vdisplay must not
//! depend on pf-capture (see this crate's Cargo.toml: "never on capture/inject or the
//! orchestrator"), so the two copies are deliberate; keep the ladders in step.
//!
//! Declared unconditionally although only the Linux backends call it: the ladder is pure integer
//! work, and its tests are the whole point of the module — this is a decision that leaves no trace
//! anyone can check without a compositor in front of them — so they run on every platform's CI
//! rather than on the one leg that compiles `mod hyprland`.

/// A ScreenCast cursor mode, valued as the portal's own wire bits — which is what a backend prints
/// when it rejects one, so `Metadata`'s `4` is literally the number in the field report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// No pointer in the cast at all.
    Hidden = 1,
    /// The compositor paints the pointer into the frames it hands us.
    Embedded = 2,
    /// The pointer rides `SPA_META_Cursor` metadata beside the frames: the compositor keeps its
    /// cheap hardware cursor plane, and the consumer either composites the shape itself or
    /// forwards it to a client that draws its own.
    Metadata = 4,
}

impl Mode {
    /// The portal's bit for this mode.
    pub(crate) const fn bit(self) -> u32 {
        self as u32
    }

    /// The spelling used in logs and in `PUNKTFUNK_PORTAL_CURSOR_MODE`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Mode::Hidden => "hidden",
            Mode::Embedded => "embedded",
            Mode::Metadata => "metadata",
        }
    }

    /// What to ask for instead, best first, when this mode is not advertised.
    const fn fallbacks(self) -> [Mode; 2] {
        match self {
            // The session wanted out-of-band shapes and cannot have them. `Embedded` still puts a
            // pointer on the client's screen (the compositor's, burnt in) — and because no
            // `SPA_META_Cursor` then arrives, the host feeds the cursor channel nothing and a
            // cursor-forward client draws nothing of its own, so this is one pointer, not two.
            // `Hidden` is last: it streams a desktop nobody can point at.
            Mode::Metadata => [Mode::Embedded, Mode::Hidden],
            // Embedded wanted but not offered. Metadata still beats Hidden: the CPU capture path
            // composites `SPA_META_Cursor` inline, so part of the matrix keeps a pointer.
            Mode::Embedded => [Mode::Metadata, Mode::Hidden],
            // A deliberate request for no pointer that the backend will not honour. Either
            // remaining mode shows one; prefer the cheap burnt-in pointer over metadata nothing on
            // this path is set up to draw.
            Mode::Hidden => [Mode::Embedded, Mode::Metadata],
        }
    }
}

/// The outcome of the ladder: what to request, and what the session actually wanted if those
/// differ (the caller logs the gap — a silently downgraded cursor is how this class of bug hides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    /// The mode to put in `SelectSources`. Advertised, unless the backend advertised nothing.
    pub(crate) mode: Mode,
    /// Set only when `mode` is a downgrade: the mode the session asked for and could not have.
    pub(crate) wanted: Option<Mode>,
}

/// Pick the cursor mode to request, given the backend's `AvailableCursorModes` bitfield.
///
/// Never returns a mode outside `advertised` unless `advertised` names none we know — see the tail
/// comment, which is the one case with no right answer.
pub(crate) fn pick(advertised: u32, want: Mode) -> Choice {
    if advertised & want.bit() != 0 {
        return Choice {
            mode: want,
            wanted: None,
        };
    }
    for alt in want.fallbacks() {
        if advertised & alt.bit() != 0 {
            return Choice {
                mode: alt,
                wanted: Some(want),
            };
        }
    }
    // The backend advertised no mode this build knows — 0, or only bits from a spec revision newer
    // than us. Every request is then a coin flip against a session-closing rejection; `Hidden` is
    // both the most universally implemented and the only one that cannot end up drawing two
    // pointers. The caller warns: whatever this backend is doing, we are guessing.
    Choice {
        mode: Mode::Hidden,
        wanted: Some(want),
    }
}

/// A parsed `PUNKTFUNK_PORTAL_CURSOR_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pin {
    /// Unset or `auto` — the session's own negotiation decides.
    Auto,
    /// Prefer this mode instead of what the session negotiated. Still runs the ladder, so a pin
    /// can never re-create the session-killing request this module exists to prevent.
    Mode(Mode),
    /// Set to something we do not recognise. Treated as `Auto`, but the caller says so out loud —
    /// a typo'd escape hatch that silently does nothing is worse than no escape hatch.
    Unrecognised,
}

/// Parse the `PUNKTFUNK_PORTAL_CURSOR_MODE` value.
pub(crate) fn parse_pin(raw: &str) -> Pin {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Pin::Auto,
        "hidden" | "none" => Pin::Mode(Mode::Hidden),
        "embedded" | "composited" => Pin::Mode(Mode::Embedded),
        "metadata" | "meta" => Pin::Mode(Mode::Metadata),
        _ => Pin::Unrecognised,
    }
}

/// The mode this session wants before the backend gets a say: `Metadata` when the cursor channel
/// was negotiated (`set_hw_cursor` — the client draws the pointer, so the compositor must not burn
/// it in), `Embedded` otherwise. `PUNKTFUNK_PORTAL_CURSOR_MODE` overrides both.
///
/// `backend` names the portal implementation for the log line only (`xdph`, `xdpw`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn want(hw_cursor: bool, backend: &str) -> Mode {
    let negotiated = if hw_cursor {
        Mode::Metadata
    } else {
        Mode::Embedded
    };
    let raw = match pf_host_config::config().portal_cursor_mode.as_deref() {
        Some(raw) => raw,
        None => return negotiated,
    };
    match parse_pin(raw) {
        Pin::Auto => negotiated,
        Pin::Mode(pinned) => {
            tracing::info!(
                backend,
                pinned = pinned.name(),
                negotiated = negotiated.name(),
                "ScreenCast: cursor mode pinned by PUNKTFUNK_PORTAL_CURSOR_MODE"
            );
            pinned
        }
        Pin::Unrecognised => {
            tracing::warn!(
                backend,
                value = raw,
                negotiated = negotiated.name(),
                "ScreenCast: unrecognised PUNKTFUNK_PORTAL_CURSOR_MODE (want auto|hidden|embedded|\
                 metadata) — ignoring"
            );
            negotiated
        }
    }
}

#[cfg(target_os = "linux")]
impl Mode {
    fn to_ashpd(self) -> ashpd::desktop::screencast::CursorMode {
        use ashpd::desktop::screencast::CursorMode;
        match self {
            Mode::Hidden => CursorMode::Hidden,
            Mode::Embedded => CursorMode::Embedded,
            Mode::Metadata => CursorMode::Metadata,
        }
    }
}

/// Ask the portal what it supports, run the ladder, and hand back the mode to put in
/// `SelectSources`. Infallible by construction: a backend we cannot interrogate gets `Embedded`,
/// the mode that predates the property and that every implementation has always had.
#[cfg(target_os = "linux")]
pub(crate) async fn negotiate(
    proxy: &ashpd::desktop::screencast::Screencast,
    hw_cursor: bool,
    backend: &str,
) -> ashpd::desktop::screencast::CursorMode {
    let want = want(hw_cursor, backend);
    let advertised = match proxy.available_cursor_modes().await {
        Ok(avail) => avail.bits(),
        Err(e) => {
            // `AvailableCursorModes` is a versioned property (ScreenCast v2); a portal too old to
            // publish it is also too old to have metadata, and `Embedded` is what this backend
            // requested for its whole life before the cursor channel existed.
            tracing::warn!(
                backend,
                error = %e,
                "ScreenCast: AvailableCursorModes query failed — requesting Embedded cursor"
            );
            return Mode::Embedded.to_ashpd();
        }
    };
    let choice = pick(advertised, want);
    match choice.wanted {
        None => tracing::info!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            mode = choice.mode.name(),
            "ScreenCast: cursor mode negotiated"
        ),
        // The downgrade path — and the one that used to be a dead session. Loud, because a stream
        // whose pointer quietly changed hands is exactly what nobody thinks to check.
        Some(wanted) => tracing::warn!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            wanted = wanted.name(),
            mode = choice.mode.name(),
            "ScreenCast: requested cursor mode is not advertised by this portal — downgrading \
             (requesting it anyway would close the session)"
        ),
    }
    choice.mode.to_ashpd()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portal's wire values. These are ABI — a backend rejecting our request prints the
    /// number, and `4` is the one in the field report that started this module.
    #[test]
    fn mode_bits_are_the_portal_wire_values() {
        assert_eq!(Mode::Hidden.bit(), 1);
        assert_eq!(Mode::Embedded.bit(), 2);
        assert_eq!(Mode::Metadata.bit(), 4);
    }

    /// Our `Mode` is a restatement of ashpd's `CursorMode`, whose bits enumflags2 assigns from
    /// declaration order — so a reordering upstream would silently repoint every mode. Pin it
    /// where ashpd is actually compiled.
    #[cfg(target_os = "linux")]
    #[test]
    fn mode_bits_match_ashpd() {
        use ashpd::desktop::screencast::CursorMode;
        use ashpd::enumflags2::BitFlags;
        for m in [Mode::Hidden, Mode::Embedded, Mode::Metadata] {
            assert_eq!(
                BitFlags::from_flag(m.to_ashpd()).bits(),
                m.bit(),
                "{} drifted from ashpd",
                m.name()
            );
        }
        assert_eq!(BitFlags::from_flag(CursorMode::Metadata).bits(), 4);
    }

    /// THE REGRESSION, with the real number: `3` is what xdph actually advertises — measured on
    /// .21 2026-08-14 against a live Hyprland 0.56.2 + xdph 1.4.1, both current. A cursor-forward
    /// session wants metadata; asking for it made xdg-desktop-portal fail the call, and the client
    /// got a black screen behind "pipeline build failed" / "unavailable cursor mode 4".
    #[test]
    fn metadata_wanted_but_unadvertised_downgrades_to_embedded() {
        // Exactly the bitfield the portal reported on glass.
        assert_eq!(Mode::Hidden.bit() | Mode::Embedded.bit(), 3);
        let c = pick(3, Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// The same portal, a session with no cursor channel: already asking for what exists, so the
    /// fix must not perturb it.
    #[test]
    fn embedded_wanted_and_advertised_is_untouched() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, None);
    }

    /// A portal that does support metadata (KWin, Mutter, xdph ≥ #366) still gets it — the point
    /// is to stop asserting, not to stop using it.
    #[test]
    fn metadata_is_used_where_advertised() {
        let all = Mode::Hidden.bit() | Mode::Embedded.bit() | Mode::Metadata.bit();
        let c = pick(all, Mode::Metadata);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, None);
    }

    /// Embedded wanted, only metadata offered: the CPU capture path composites it, so a pointer
    /// survives. (Mirrors `pf-capture`'s ladder.)
    #[test]
    fn embedded_unadvertised_falls_to_metadata_not_hidden() {
        let c = pick(Mode::Hidden.bit() | Mode::Metadata.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, Some(Mode::Embedded));
    }

    /// A backend offering only `Hidden`: a cursorless stream beats a closed session.
    #[test]
    fn hidden_only_backend_yields_hidden() {
        let c = pick(Mode::Hidden.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Hidden);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// Advertises nothing we know — no right answer, but it must still be a legal enum and flagged
    /// as a downgrade so the warn fires.
    #[test]
    fn unknown_advertisement_guesses_hidden_and_reports_a_downgrade() {
        for advertised in [0, 0b1000_0000] {
            let c = pick(advertised, Mode::Metadata);
            assert_eq!(c.mode, Mode::Hidden);
            assert_eq!(c.wanted, Some(Mode::Metadata));
        }
    }

    /// Whatever the ladder returns must be a mode the backend named — the invariant the old
    /// hardcode broke. Exhaustive over every advertisement × every want.
    #[test]
    fn never_requests_an_unadvertised_mode() {
        let modes = [Mode::Hidden, Mode::Embedded, Mode::Metadata];
        for advertised in 1u32..=0b111 {
            for want in modes {
                let c = pick(advertised, want);
                assert!(
                    advertised & c.mode.bit() != 0,
                    "picked {} from advertised {advertised:#05b} (want {})",
                    c.mode.name(),
                    want.name()
                );
                // A downgrade is reported exactly when one happened.
                assert_eq!(c.wanted.is_some(), c.mode != want);
            }
        }
    }

    #[test]
    fn pin_parses_the_spellings_we_document() {
        assert_eq!(parse_pin(""), Pin::Auto);
        assert_eq!(parse_pin("auto"), Pin::Auto);
        assert_eq!(parse_pin(" AUTO "), Pin::Auto);
        assert_eq!(parse_pin("embedded"), Pin::Mode(Mode::Embedded));
        assert_eq!(parse_pin("Embedded"), Pin::Mode(Mode::Embedded));
        assert_eq!(parse_pin("metadata"), Pin::Mode(Mode::Metadata));
        assert_eq!(parse_pin("hidden"), Pin::Mode(Mode::Hidden));
        assert_eq!(parse_pin("2"), Pin::Unrecognised);
        assert_eq!(parse_pin("yes"), Pin::Unrecognised);
    }

    /// The hatch pins a PREFERENCE, not the request: pinning metadata at a portal without it must
    /// still come out embedded rather than re-closing the session.
    #[test]
    fn a_pin_still_runs_the_ladder() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
    }
}
