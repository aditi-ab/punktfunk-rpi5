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

/// Resolve the client's compositor preference to a concrete backend (the I/O shell around
/// [`pick_compositor`]): enumerate what's available, auto-detect the default, pick, and log
/// whether the explicit request was honored or fell back. Runs blocking probes — call off the
/// async reactor (`spawn_blocking`).
pub(super) fn resolve_compositor(
    pref: CompositorPref,
    dedicated_launch: bool,
) -> Result<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    // Windows has a single virtual-display backend (pf-vdisplay); vdisplay::open ignores the compositor
    // arg there, so short-circuit the Linux session-detection state machine with a placeholder.
    #[cfg(target_os = "windows")]
    {
        let _ = (pref, dedicated_launch);
        Ok(Compositor::Kwin)
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
        let detected = if overridden {
            crate::vdisplay::detect().ok()
        } else {
            // Auto: detect the LIVE session (Gaming vs Desktop) and retarget the process env at it so
            // every backend (video capture + input) this connect opens against the active session —
            // this is the state machine that lets one host follow a Bazzite box across Gaming↔Desktop.
            let active = crate::vdisplay::detect_active_session();
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
        // backend + input to the spawn sub-mode. Skipped under an explicit operator compositor pin.
        if dedicated_launch && !overridden {
            crate::vdisplay::apply_input_env(Compositor::Gamescope, true);
            tracing::info!(
                "dedicated game session — routing to a headless gamescope spawn at the client mode"
            );
            return Ok(Compositor::Gamescope);
        }
        let available = crate::vdisplay::available();
        let chosen = match pick_compositor(pref, &available, detected) {
            Some(c) => c,
            None => {
                // No live session — the state a compositor crash leaves behind (gnome-shell
                // SIGSEGV → GDM greeter, whose auto-login is once-per-boot). If the operator
                // configured a recovery hook, fire it (debounced) and tell the client to retry:
                // its next knock lands in the recovered desktop.
                if crate::vdisplay::try_recover_session() {
                    anyhow::bail!(
                        "no live graphical session for this uid — host session recovery launched \
                         (PUNKTFUNK_RECOVER_SESSION_CMD); retry in a few seconds"
                    );
                }
                anyhow::bail!(
                    "no usable compositor (no live graphical session for this uid; set \
                     PUNKTFUNK_COMPOSITOR or start a desktop/gaming session)"
                );
            }
        };
        if !overridden {
            // Point input at the same backend and resolve the gamescope sub-mode (managed where the
            // session infra exists, attach to a foreign gamescope, else per-session bare spawn).
            crate::vdisplay::apply_input_env(chosen, false);
        }
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
        Ok(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::pick_compositor;
    use punktfunk_core::config::CompositorPref;

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
