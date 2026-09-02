//! ScreenCast cursor mode against `AvailableCursorModes`.
//!
//! `SelectSources` with a bit the portal did not advertise is
//! `INVALID_ARGUMENT` from xdg-desktop-portal, not the backend. The request is
//! always a subset of the advertised bits.
//!
//! Want is `Metadata` when the session has a cursor channel (`set_hw_cursor`),
//! else `Embedded`. `PUNKTFUNK_PORTAL_CURSOR_MODE` pins a preference; the
//! ladder still runs, so a pin cannot request an unadvertised mode.
//!
//! `pf-capture` keeps the same ladder (`portal::choose_cursor_mode`). This crate
//! must not depend on capture; keep the two copies in step.
//!
//! Compiled on every platform so the tests run in CI without a compositor.

/// Portal wire bits. Re-exported as [`crate::PortalCursorMode`].
///
/// The host reads [`VirtualDisplay::last_portal_cursor_mode`] to know whether
/// `SPA_META_Cursor` can arrive.
///
/// [`VirtualDisplay::last_portal_cursor_mode`]: crate::VirtualDisplay::last_portal_cursor_mode
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hidden = 1,
    /// Pointer burnt into the frames; no `SPA_META_Cursor`.
    Embedded = 2,
    /// `SPA_META_Cursor` beside the frames; the compositor keeps its hardware plane.
    Metadata = 4,
}

impl Mode {
    pub(crate) const fn bit(self) -> u32 {
        self as u32
    }

    /// Spelling for logs and `PUNKTFUNK_PORTAL_CURSOR_MODE`.
    pub const fn name(self) -> &'static str {
        match self {
            Mode::Hidden => "hidden",
            Mode::Embedded => "embedded",
            Mode::Metadata => "metadata",
        }
    }

    /// Under `Embedded` a missing overlay is not "pointer off the recorded view".
    pub const fn delivers_metadata(self) -> bool {
        matches!(self, Mode::Metadata)
    }

    const fn fallbacks(self) -> [Mode; 2] {
        match self {
            // Embedded still shows a pointer (burnt in). Hidden last: no pointer,
            // and no `SPA_META_Cursor` for a cursor-forward client to draw.
            Mode::Metadata => [Mode::Embedded, Mode::Hidden],
            // CPU capture composites `SPA_META_Cursor` inline, so Metadata still shows a pointer.
            Mode::Embedded => [Mode::Metadata, Mode::Hidden],
            // Either remaining mode shows a pointer. Prefer Embedded: this path is
            // not set up to draw `SPA_META_Cursor`.
            Mode::Hidden => [Mode::Embedded, Mode::Metadata],
        }
    }
}

/// Requested mode, plus the original want when that is a downgrade (the caller logs the gap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    /// `SelectSources` mode. In `advertised`, unless the backend advertised nothing we know.
    pub(crate) mode: Mode,
    pub(crate) wanted: Option<Mode>,
}

/// Unknown bits fall to `Hidden` and set `wanted`.
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
    // Advertised nothing this build knows (0, or bits from a newer spec). Hidden
    // draws no pointer. The caller warns.
    Choice {
        mode: Mode::Hidden,
        wanted: Some(want),
    }
}

/// Parsed `PUNKTFUNK_PORTAL_CURSOR_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pin {
    /// Unset or `auto`. The session's own negotiation decides.
    Auto,
    /// Prefer this mode. The ladder still runs; a pin cannot request an unadvertised mode.
    Mode(Mode),
    /// Unknown spelling. Treated as `Auto`; the caller logs it rather than swallowing a typo.
    Unrecognised,
}

pub(crate) fn parse_pin(raw: &str) -> Pin {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Pin::Auto,
        "hidden" | "none" => Pin::Mode(Mode::Hidden),
        "embedded" | "composited" => Pin::Mode(Mode::Embedded),
        "metadata" | "meta" => Pin::Mode(Mode::Metadata),
        _ => Pin::Unrecognised,
    }
}

/// `Metadata` when the cursor channel was negotiated (`set_hw_cursor`: the client
/// draws, so the compositor must not burn the pointer in), else `Embedded`.
/// `PUNKTFUNK_PORTAL_CURSOR_MODE` overrides both. `backend` is the log line only.
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
    pub(crate) fn to_ashpd(self) -> ashpd::desktop::screencast::CursorMode {
        use ashpd::desktop::screencast::CursorMode;
        match self {
            Mode::Hidden => CursorMode::Hidden,
            Mode::Embedded => CursorMode::Embedded,
            Mode::Metadata => CursorMode::Metadata,
        }
    }
}

/// Returns our [`Mode`], not ashpd's: the caller converts with [`Mode::to_ashpd`]
/// and carries the value out of the portal thread for the session lifetime.
#[cfg(target_os = "linux")]
pub(crate) async fn negotiate(
    proxy: &ashpd::desktop::screencast::Screencast,
    hw_cursor: bool,
    backend: &str,
) -> Mode {
    let want = want(hw_cursor, backend);
    let advertised = match proxy.available_cursor_modes().await {
        Ok(avail) => avail.bits(),
        Err(e) => {
            // ScreenCast v2 property. A portal that cannot publish it is too old for Metadata.
            tracing::warn!(
                backend,
                error = %e,
                "ScreenCast: AvailableCursorModes query failed — requesting Embedded cursor"
            );
            return Mode::Embedded;
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
        Some(wanted) => tracing::warn!(
            backend,
            advertised = format_args!("{advertised:#05b}"),
            wanted = wanted.name(),
            mode = choice.mode.name(),
            "ScreenCast: requested cursor mode is not advertised by this portal — downgrading \
             (requesting it anyway would close the session)"
        ),
    }
    choice.mode
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Portal wire bits. A rejecting backend prints this number.
    #[test]
    fn mode_bits_are_the_portal_wire_values() {
        assert_eq!(Mode::Hidden.bit(), 1);
        assert_eq!(Mode::Embedded.bit(), 2);
        assert_eq!(Mode::Metadata.bit(), 4);
    }

    /// ashpd `CursorMode` bits follow enumflags2 declaration order; a reorder
    /// silently repoints every mode. Pin them where ashpd is compiled.
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

    /// xdph advertises `3` (`Hidden|Embedded`). Metadata (`4`) is
    /// `INVALID_ARGUMENT` from xdg-desktop-portal, not the backend.
    #[test]
    fn metadata_wanted_but_unadvertised_downgrades_to_embedded() {
        assert_eq!(Mode::Hidden.bit() | Mode::Embedded.bit(), 3);
        let c = pick(3, Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// Under Embedded no `SPA_META_Cursor` arrives. A missing overlay is not "pointer off view".
    #[test]
    fn only_metadata_can_deliver_a_cursor_overlay() {
        assert!(Mode::Metadata.delivers_metadata());
        assert!(!Mode::Embedded.delivers_metadata());
        assert!(!Mode::Hidden.delivers_metadata());
        // The negotiated mode governs, not the wanted one.
        assert!(!pick(3, Mode::Metadata).mode.delivers_metadata());
    }

    #[test]
    fn embedded_wanted_and_advertised_is_untouched() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Embedded);
        assert_eq!(c.wanted, None);
    }

    #[test]
    fn metadata_is_used_where_advertised() {
        let all = Mode::Hidden.bit() | Mode::Embedded.bit() | Mode::Metadata.bit();
        let c = pick(all, Mode::Metadata);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, None);
    }

    /// CPU capture composites `SPA_META_Cursor`, so Metadata still shows a pointer.
    #[test]
    fn embedded_unadvertised_falls_to_metadata_not_hidden() {
        let c = pick(Mode::Hidden.bit() | Mode::Metadata.bit(), Mode::Embedded);
        assert_eq!(c.mode, Mode::Metadata);
        assert_eq!(c.wanted, Some(Mode::Embedded));
    }

    /// Hidden-only backend: requesting Metadata would close the session.
    #[test]
    fn hidden_only_backend_yields_hidden() {
        let c = pick(Mode::Hidden.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Hidden);
        assert_eq!(c.wanted, Some(Mode::Metadata));
    }

    /// Unknown bits: still a legal `Mode`, flagged as a downgrade so the caller warns.
    #[test]
    fn unknown_advertisement_guesses_hidden_and_reports_a_downgrade() {
        for advertised in [0, 0b1000_0000] {
            let c = pick(advertised, Mode::Metadata);
            assert_eq!(c.mode, Mode::Hidden);
            assert_eq!(c.wanted, Some(Mode::Metadata));
        }
    }

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

    /// A pin is a preference: unadvertised Metadata still becomes Embedded.
    #[test]
    fn a_pin_still_runs_the_ladder() {
        let c = pick(Mode::Hidden.bit() | Mode::Embedded.bit(), Mode::Metadata);
        assert_eq!(c.mode, Mode::Embedded);
    }
}
