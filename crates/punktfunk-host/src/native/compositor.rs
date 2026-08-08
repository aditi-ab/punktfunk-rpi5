//! Compositor-preference resolution for the native handshake (plan §W1 — carved out of the
//! [`super`] module): map the client's [`CompositorPref`] to a concrete `crate::vdisplay::Compositor`
//! backend, honoring an explicit request when the named backend is live and otherwise auto-detecting
//! the active graphical session. The pure decision ([`pick_compositor`]) is separated from the I/O
//! shell ([`resolve_compositor`]) that runs the blocking session probes.

use super::*;

/// Pure selection: choose the backend to drive from the client's `pref`, the set `available`
/// right now, and the auto-`detected` default. A concrete preference wins only if it's available;
/// otherwise (and for `Auto`) fall back to the detected default. `None` only when nothing is
/// available *and* nothing was detected — the caller turns that into a handshake error.
fn pick_compositor(
    pref: CompositorPref,
    available: &[crate::vdisplay::Compositor],
    detected: Option<crate::vdisplay::Compositor>,
) -> Option<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    match Compositor::from_pref(pref) {
        Some(want) if available.contains(&want) => Some(want),
        // `CompositorPref::Wlroots` names the wlroots *family* (D2): sway/river ([`Wlroots`]) and
        // Hyprland are distinct backends but mutually-exclusive live sessions, so honor the request
        // with whichever family member is actually available — the detected one if it's a family
        // member, else the first available of the two.
        Some(Compositor::Wlroots) => match detected {
            Some(d @ (Compositor::Wlroots | Compositor::Hyprland)) => Some(d),
            _ => [Compositor::Wlroots, Compositor::Hyprland]
                .into_iter()
                .find(|c| available.contains(c))
                .or(detected),
        },
        _ => detected,
    }
}

/// Is this connect pinned at a compositor that is not actually running?
///
/// Pure (the I/O shell passes in the observed liveness) so the interaction is unit-tested, because
/// it is invisible from the outside: an operator pin puts its backend into
/// [`crate::vdisplay::available`] unconditionally AND skips `apply_session_env`'s
/// `XDG_CURRENT_DESKTOP` scrub, so [`pick_compositor`] hands back a compositor that may be a corpse
/// and its `None` (recover) arm can never fire. [`Compositor::Gamescope`] is exempt — it stands its
/// own session up, which is the whole reason a headless box pins it.
#[cfg(not(target_os = "windows"))]
fn pinned_at_a_dead_session(
    overridden: bool,
    chosen: crate::vdisplay::Compositor,
    live: crate::vdisplay::ActiveKind,
) -> bool {
    overridden && chosen.needs_live_session() && live == crate::vdisplay::ActiveKind::None
}

/// The handshake error for "no graphical session is live for this uid" — the state a compositor
/// crash leaves behind (gnome-shell SIGSEGV → GDM greeter, whose auto-login is once-per-boot, so the
/// box would otherwise need a walk-up or a reboot).
///
/// Fires the operator's recovery hook (debounced) on the way out when one is configured, so the
/// client's retry a few seconds later lands in a recovered desktop. `pinned` names the
/// `PUNKTFUNK_COMPOSITOR` value when the pin is what got us here, so the message can say which knob
/// to change rather than the generic advice to *set* the knob that caused it.
#[cfg(not(target_os = "windows"))]
fn no_live_session(pinned: Option<&str>) -> anyhow::Error {
    if crate::vdisplay::try_recover_session() {
        return anyhow::anyhow!(
            "no live graphical session for this uid — host session recovery launched \
             (PUNKTFUNK_RECOVER_SESSION_CMD); retry in a few seconds"
        );
    }
    match pinned {
        Some(pin) => anyhow::anyhow!(
            "PUNKTFUNK_COMPOSITOR={pin} pins this host to a backend that can only attach to an \
             already-running compositor, and no graphical session is live for this uid — start a \
             session, pin `gamescope` (it stands its own up), or set PUNKTFUNK_RECOVER_SESSION_CMD"
        ),
        None => anyhow::anyhow!(
            "no usable compositor (no live graphical session for this uid; set \
             PUNKTFUNK_COMPOSITOR or start a desktop/gaming session)"
        ),
    }
}

/// Resolve the client's compositor preference to a concrete backend (the I/O shell around
/// [`pick_compositor`]): enumerate what's available, auto-detect the default, pick, and log
/// whether the explicit request was honored or fell back. Runs blocking probes — call off the
/// async reactor (`spawn_blocking`).
pub(super) fn resolve_compositor(
    pref: CompositorPref,
    dedicated_launch: bool,
) -> Result<(
    crate::vdisplay::Compositor,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    use crate::vdisplay::Compositor;
    // Windows has a single virtual-display backend (pf-vdisplay); vdisplay::open ignores the compositor
    // arg there, so short-circuit the Linux session-detection state machine with a placeholder.
    #[cfg(target_os = "windows")]
    {
        let _ = (pref, dedicated_launch);
        Ok((Compositor::Kwin, None))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // A client is (re)connecting → cancel any pending TV-session restore so the box stays in the
        // streamed session (covers the keep-alive REUSE reconnect, which skips create_managed_session's
        // own cancel — review #3). No-op when nothing is pending.
        crate::vdisplay::cancel_pending_tv_restore();
        // Explicit operator override (legacy / CI / forcing a backend for a test) wins and is assumed
        // to come with a hand-set env — don't retarget the process env in that case.
        let overridden = pf_host_config::config().compositor.is_some();
        // Liveness is read on BOTH paths. The auto path retargets the process env at the live
        // session (below); the PINNED path needs it too, because a pin names a BACKEND, not a
        // running session — and a pin whose compositor has died used to be indistinguishable from a
        // healthy one here (it skips `apply_session_env`'s `XDG_CURRENT_DESKTOP` scrub and lands
        // itself in `available()`, so `pick_compositor` could never return `None`). That combination
        // marched every client through 8 doomed `create` retries and left the operator's
        // `PUNKTFUNK_RECOVER_SESSION_CMD` unreachable — see the `needs_live_session` gate below.
        let active = crate::vdisplay::detect_active_session();
        let detected = if overridden {
            crate::vdisplay::detect().ok()
        } else {
            // Auto: detect the LIVE session (Gaming vs Desktop) and retarget the process env at it so
            // every backend (video capture + input) this connect opens against the active session —
            // this is the state machine that lets one host follow a Bazzite box across Gaming↔Desktop.
            //
            // A4: if the compositor instance changed since the last connect (an idle-time Game↔Desktop
            // switch), bump the epoch + invalidate the old backend's kept displays so this connect never
            // reuses a node id from the dead instance.
            crate::vdisplay::observe_session_instance(&active);
            crate::vdisplay::apply_session_env(&active);
            tracing::info!(
                active = ?active.kind,
                wayland = active.env.wayland_display.as_deref().unwrap_or("-"),
                "detected active graphical session"
            );
            crate::vdisplay::compositor_for_kind(active.kind)
        };
        // Dedicated game session (design/gamemode-and-dedicated-sessions.md B0): a launching session
        // under `game_session=dedicated` (gamescope confirmed available) forces its OWN headless
        // gamescope spawn at the client's mode, overriding the detected desktop/game-mode backend. The
        // env was already retargeted above (for XDG_RUNTIME_DIR / the PipeWire daemon); we just pin the
        // backend + input to the spawn sub-mode. An explicit operator compositor pin still outranks
        // it — but says so out loud (below), because a silent veto is indistinguishable from the
        // feature being broken.
        if dedicated_launch {
            if overridden {
                // The pin still wins (it is the operator's explicit, hand-configured knob), but it
                // must NEVER win silently: the console goes on displaying `game_session=dedicated`
                // while every launch lands in the pinned session instead, and nothing in the log
                // connects the two. That cost a full triage on a box whose `PUNKTFUNK_COMPOSITOR`
                // was a forgotten validation leftover — the setting had never once taken effect and
                // the only evidence was the ABSENCE of the info! line below.
                tracing::warn!(
                    pin = pf_host_config::config()
                        .compositor
                        .as_deref()
                        .unwrap_or("-"),
                    "game_session=dedicated asked for this launch's OWN headless gamescope, but \
                     PUNKTFUNK_COMPOSITOR pins this host to a backend — the operator pin wins and \
                     the game launches into the pinned session instead. Unset PUNKTFUNK_COMPOSITOR \
                     to get dedicated game sessions."
                );
            } else {
                let route = crate::vdisplay::apply_input_env(Compositor::Gamescope, true);
                tracing::info!(
                    ?route,
                    "dedicated game session — routing to a headless gamescope spawn at the client \
                     mode"
                );
                return Ok((Compositor::Gamescope, route));
            }
        }
        let available = crate::vdisplay::available();
        let chosen = match pick_compositor(pref, &available, detected) {
            Some(c) => c,
            // No live session, but the MANAGED gamescope infra exists (SteamOS's
            // `gamescope-session`, Bazzite's `gamescope-session-plus`): route to the gamescope
            // backend anyway — its managed path stands the session up from nothing at the
            // client's mode (drop-in takeover / session relaunch), so a dead gaming session
            // self-heals on the next connect instead of bouncing every client until someone
            // restarts it by hand. (The trap that motivated this: a headless SteamOS box whose
            // gamescope died — every connect failed "no usable compositor" even though the
            // takeover could rebuild it.) Not under an operator pin: an explicit
            // `PUNKTFUNK_COMPOSITOR` keeps its exact, hand-configured meaning.
            None if !overridden && crate::vdisplay::managed_session_available() => {
                tracing::info!(
                    "no live graphical session — managed gamescope infra present; routing to \
                     the managed takeover to revive the session"
                );
                Compositor::Gamescope
            }
            None => return Err(no_live_session(None)),
        };
        // Same dead-session exit, reached the other way: a pin puts its backend in `available()`
        // unconditionally, so `pick_compositor` above can hand back a compositor that is not
        // actually running and the `None` arm never fires. Check the backend's own requirement
        // against observed liveness instead of trusting the pin. Gamescope is exempt — it stands
        // its own session up, which is the whole point of pinning it on a headless box.
        if pinned_at_a_dead_session(overridden, chosen, active.kind) {
            return Err(no_live_session(
                pf_host_config::config().compositor.as_deref(),
            ));
        }
        // Point input at the same backend and resolve the gamescope sub-mode (managed where the
        // session infra exists, attach to a foreign gamescope, else per-session bare spawn). The
        // route travels back to the caller as a VALUE and is carried on the backend instance — an
        // operator pin skips the input retarget but still needs a route resolved, or `create` would
        // fall through to a bare spawn on a box that was pinned to the managed session.
        let route = if !overridden {
            crate::vdisplay::apply_input_env(chosen, false)
        } else {
            // An operator pin deliberately leaves PUNKTFUNK_INPUT_BACKEND alone, but still needs a
            // route resolved — otherwise `create` falls through to a bare spawn on a box pinned to
            // the managed session.
            crate::vdisplay::resolve_gamescope_route(chosen, false)
        };
        let avail_ids: Vec<&str> = available.iter().map(|c| c.id()).collect();
        match Compositor::from_pref(pref) {
            Some(want) if want == chosen => {
                tracing::info!(
                    compositor = chosen.id(),
                    "honoring client compositor request"
                )
            }
            Some(want) => tracing::warn!(
                requested = want.id(),
                chosen = chosen.id(),
                available = ?avail_ids,
                "client-requested compositor unavailable — falling back to auto-detect"
            ),
            None => tracing::info!(
                compositor = chosen.id(),
                "auto-detected compositor (client: auto)"
            ),
        }
        Ok((chosen, route))
    }
}

#[cfg(test)]
mod tests {
    use super::pick_compositor;
    use punktfunk_core::config::CompositorPref;

    /// A pin at a compositor that ISN'T RUNNING must take the recovery exit rather than march the
    /// client into a bring-up that can only fail.
    ///
    /// The regression this pins down: `PUNKTFUNK_COMPOSITOR=mutter` on a box whose gnome-shell had
    /// segfaulted. The pin put Mutter in `available()` and suppressed the `XDG_CURRENT_DESKTOP`
    /// scrub, so every connect "resolved" happily and then spent 8 retries on
    /// `RemoteDesktop.CreateSession: ServiceUnknown` — while the operator's
    /// `PUNKTFUNK_RECOVER_SESSION_CMD` sat unreachable behind a `None` arm that could never fire.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_pin_at_a_dead_session_recovers_instead_of_retrying() {
        use super::pinned_at_a_dead_session as dead;
        use crate::vdisplay::{ActiveKind, Compositor::*};
        // The bug: pinned to a desktop backend with nothing live for this uid.
        assert!(dead(true, Mutter, ActiveKind::None));
        assert!(dead(true, Kwin, ActiveKind::None));
        assert!(dead(true, Wlroots, ActiveKind::None));
        assert!(dead(true, Hyprland, ActiveKind::None));
        // Pinned but the session IS up — the ordinary case, must not bail.
        assert!(!dead(true, Mutter, ActiveKind::DesktopGnome));
        // Gamescope stands its own session up from nothing: pinning it on a headless box is a
        // SUPPORTED setup, not a dead session. (This is the .21 no-login workaround — never break it.)
        assert!(!dead(true, Gamescope, ActiveKind::None));
        // Unpinned is untouched: the auto path already reaches `pick_compositor`'s `None` arm via
        // `compositor_for_kind(ActiveKind::None)`, and it owns the managed-takeover case.
        assert!(!dead(false, Mutter, ActiveKind::None));
    }

    /// gamescope is the ONLY backend that can serve a connect with no session already running.
    #[test]
    fn only_gamescope_survives_a_dead_session() {
        use crate::vdisplay::Compositor::*;
        assert!(!Gamescope.needs_live_session());
        for c in [Mutter, Kwin, Wlroots, Hyprland] {
            assert!(c.needs_live_session(), "{c:?} needs a live compositor");
        }
    }

    #[test]
    fn compositor_resolution_precedence() {
        use crate::vdisplay::Compositor::*;
        // A concrete, available preference is honored.
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Kwin, Gamescope], Some(Kwin)),
            Some(Gamescope)
        );
        // A concrete but UNavailable preference falls back to the detected default.
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        // Auto always uses the detected default.
        assert_eq!(
            pick_compositor(CompositorPref::Auto, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        // Unavailable preference + nothing detected → None (caller errors the handshake).
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Gamescope], None),
            None
        );
        // Available preference still wins even when nothing was auto-detected.
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Gamescope], None),
            Some(Gamescope)
        );
        // Wlroots family (D2): the shared `Wlroots` pref resolves to whichever of sway/river
        // (Wlroots) and Hyprland is the live session.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], Some(Hyprland)),
            Some(Hyprland)
        );
        // …and to Wlroots-proper on a sway/river host.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Wlroots], Some(Wlroots)),
            Some(Wlroots)
        );
        // Family fallback even if detection came back empty but a member is available.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], None),
            Some(Hyprland)
        );
    }
}
