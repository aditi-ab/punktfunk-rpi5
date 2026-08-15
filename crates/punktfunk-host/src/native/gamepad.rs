//! Virtual-gamepad backend resolution for the native session (plan §W1 — carved out of the
//! [`super`] module). Maps the client's [`GamepadPref`] (plus the host `PUNKTFUNK_GAMEPAD` env and
//! the live platform) to a backend this host can actually build, applying the runtime UHID /
//! Steam-conflict degrades. Pure selection ([`pick_gamepad`]) is separated from the env/logging
//! shell ([`resolve_gamepad`]) and the per-pad variant ([`resolve_pad_kind`]); the platform degrades
//! (`degrade_if_no_uhid`, `physical_steam_product_present`, `degrade_steam_on_conflict`) are
//! cfg-split linux/other and MUST be re-verified on Windows when touched (Linux clippy can't see the
//! non-linux copies).

use super::*;

/// The per-pad routing decision for one frame ([`Pads::handle`]): given `owner` (the manager
/// holding a live device at this index, if any), the client-`declared` kind, and whether this is a
/// create/update frame (`present`) vs a removal, return `(kind to route to, new owner)`.
///
/// A live device stays in its owning manager even if the declared kind later changes (so a pad is
/// never duplicated across managers); the declared kind takes effect only when no device exists
/// yet; a removal routes to the owner's manager (so it tears the right device down) and clears the
/// owner.
pub(super) fn route_decision(
    owner: Option<GamepadPref>,
    declared: GamepadPref,
    present: bool,
) -> (GamepadPref, Option<GamepadPref>) {
    match (owner, present) {
        (Some(k), true) => (k, Some(k)), // keep the existing device in its manager
        (Some(k), false) => (k, None),   // removal → owner's manager, then clear
        (None, true) => (declared, Some(declared)), // create in the declared kind's manager
        (None, false) => (declared, None), // removal with no device — a harmless no-op
    }
}

/// Resolve one client-declared per-pad kind to a backend this host can actually build (mixed
/// types): the platform map + the runtime UHID / Steam-conflict degrades that [`resolve_gamepad`]
/// applies to the session default, minus the Auto/env session logic (a per-pad declaration is
/// always a concrete kind).
pub(super) fn resolve_pad_kind(kind: GamepadPref) -> GamepadPref {
    let chosen = pick_gamepad(
        kind,
        None,
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    degrade_xbox_identity(degrade_steam_on_conflict(degrade_if_no_uhid(chosen)))
}

/// Pure selection of the session's virtual-gamepad backend: the client's explicit `pref` wins,
/// then the host's `PUNKTFUNK_GAMEPAD` env var (under a client `Auto`), then X-Box 360.
///
/// `linux`/`windows` flag the host platform. DualSense and DualShock 4 each have both a Linux (UHID
/// hid-playstation) and a Windows (UMDF minidriver) backend; on any other platform such a wish degrades
/// to X-Box 360 (never an error: a session without rich pads still streams).
///
/// The X-Box identities are now distinct on BOTH platforms: a uinput identity on Linux (360 /
/// One S), and a UMDF HID identity on Windows (360 → `045E:0B13`, One → `045E:02FD`, Elite →
/// `045E:0B22`). The Windows fold of One/Series into the 360 pad is gone with the reason for it —
/// it existed because the only Windows X-Box backend was the XUSB companion, which presents one
/// fixed 360 identity and cannot vary it. The Elite has no Linux identity (`PadIdentity` stops at
/// One S), so it folds there.
///
/// ⚠️ **This is compile-time only.** `PUNKTFUNK_XBOX_BACKEND=xusb` puts Windows back on the
/// companion at RUNTIME, which un-varies the identity again — that is [`degrade_xbox_identity`]'s
/// job, not this function's.
fn pick_gamepad(pref: GamepadPref, env: Option<&str>, linux: bool, windows: bool) -> GamepadPref {
    let want = match pref {
        GamepadPref::Auto => env
            .and_then(GamepadPref::from_name)
            .unwrap_or(GamepadPref::Auto),
        explicit => explicit,
    };
    match want {
        // DualSense / DualShock 4: Linux UHID hid-playstation, or the Windows UMDF minidriver backend.
        GamepadPref::DualSense if linux || windows => GamepadPref::DualSense,
        GamepadPref::DualShock4 if linux || windows => GamepadPref::DualShock4,
        // One/Series: a real, distinct uinput identity on Linux, and — since the HID X-Box backend
        // became the default — a distinct UMDF HID identity (`045E:02FD`) on Windows too.
        GamepadPref::XboxOne if linux || windows => GamepadPref::XboxOne,
        // Elite Series 2: Windows-only (UMDF device-type 6, `045E:0B22`). There is no Linux uinput
        // Elite identity to fold onto, so it takes the `_` arm and lands on the 360 pad there.
        GamepadPref::XboxElite if windows => GamepadPref::XboxElite,
        // Steam Deck / classic Steam Controller: Linux UHID hid-steam (Windows Steam devices
        // are the N4 spike).
        GamepadPref::SteamDeck if linux => GamepadPref::SteamDeck,
        GamepadPref::SteamController if linux => GamepadPref::SteamController,
        // Windows virtual Deck: the UMDF device-type-3 identity, Steam-Input-promoted via the
        // MI_02 hardware-id synthesis (gamepad-new-types N4) — native Deck glyphs + trackpads +
        // gyro + back grips, replacing the old fold to DualSense.
        GamepadPref::SteamDeck if windows => GamepadPref::SteamDeck,
        // DualSense Edge: Linux UHID hid-playstation / Windows UMDF (device-type 2) — the plain
        // DualSense plus native back/Fn buttons, so the wire paddles stop hitting the fold/drop
        // policy. Degrades to Xbox360 elsewhere like its siblings.
        GamepadPref::DualSenseEdge if linux || windows => GamepadPref::DualSenseEdge,
        // Switch Pro: Linux UHID hid-nintendo (≥ 5.16) — correct Nintendo glyphs + positional
        // layout + gyro + HD rumble. No Windows backend; folds to Xbox360 there.
        GamepadPref::SwitchPro if linux => GamepadPref::SwitchPro,
        // New Steam Controller (2026, `28DE:1302`): passed through as-is on Linux — the Triton
        // UHID backend mirrors the client's raw reports under the real identity and Steam on
        // the host drives it over hidraw (no kernel driver binds the PID; Steam Input is the
        // consumer). No Windows backend; folds to Xbox360 there.
        GamepadPref::SteamController2 if linux => GamepadPref::SteamController2,
        GamepadPref::SteamController2Puck if linux => GamepadPref::SteamController2Puck,
        _ => GamepadPref::Xbox360,
    }
}

/// Runtime degrade for the Linux UHID backends (DualSense / DualShock 4 / Steam Deck): if
/// `/dev/uhid` can't be opened for write *now*, fall back to the uinput X-Box 360 pad rather than a
/// dead controller (the UHID device-create would just fail). Cheap — opens + drops the char device,
/// no `UHID_CREATE2`, so no device is created. A no-op on non-Linux (those backends are UMDF/uinput).
#[cfg(target_os = "linux")]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    let needs_uhid = matches!(
        chosen,
        GamepadPref::DualSense
            | GamepadPref::DualSenseEdge
            | GamepadPref::DualShock4
            | GamepadPref::SteamDeck
            | GamepadPref::SteamController
            | GamepadPref::SteamController2
            | GamepadPref::SteamController2Puck
            | GamepadPref::SwitchPro
    );
    if needs_uhid
        && std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uhid")
            .is_err()
    {
        tracing::warn!(
            wanted = chosen.as_str(),
            "/dev/uhid not writable — falling back to the X-Box 360 pad"
        );
        return GamepadPref::Xbox360;
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Detection half of [`warn_if_ds_inhibit_storm`], split out for tests: `true` when a process
/// named `steamos-manager` is running (its full name fits `comm`'s 15-char limit exactly) AND
/// SELinux is enforcing (the `enforce` file reads `1`). Both halves are required for the storm:
/// a permissive box logs one denial per walk and moves on, and without steamos-manager there is
/// no ds_inhibit to trigger.
#[cfg(target_os = "linux")]
fn ds_inhibit_storm_risk(proc_root: &std::path::Path, enforce: &std::path::Path) -> bool {
    let enforcing = std::fs::read_to_string(enforce).is_ok_and(|v| v.trim() == "1");
    if !enforcing {
        return false;
    }
    std::fs::read_dir(proc_root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|c| c.trim() == "steamos-manager")
        })
}

/// One-shot diagnostic for the Bazzite/SteamOS ds_inhibit audit storm: Valve's `steamos-manager`
/// reacts to every open/close of a `hid-playstation` hidraw — exactly what our virtual
/// DualSense / DualShock 4 is; it has no VID/PID or virtual/uhid filtering — by walking
/// `/proc/*/fd/` to see whether Steam holds the node. Under SELinux enforcing that walk is
/// denied (`steamos_manager_t` lacks `sys_ptrace`/`dac_*`; measured ~324 AVCs/sec on Bazzite),
/// and `setroubleshootd` amplifies the flood into a box-wide fork storm that starves the stream
/// (gamescope 0 fps, encode submit ~150 ms/frame). The audit lines read `comm="tokio-rt-worker"`
/// and look like us — they are steamos-manager's (check `scontext=`).
///
/// Warn-only, never degrade: a per-pad fold has no wire channel back to the client
/// (`Welcome::gamepad` only describes the session default), and it would strip the DS5 feature
/// set on exactly the platform where users want it. The real fix is the shipped SELinux drop-in;
/// this warning cannot see the policy store (root-only), so it fires even where that drop-in is
/// already installed — it names that, and puts the cause in OUR logs so the audit-log trap above
/// doesn't get someone blaming the encoder again.
#[cfg(target_os = "linux")]
fn warn_if_ds_inhibit_storm(chosen: GamepadPref) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    // Selection criterion is the bound driver (`sony`/`playstation`) plus the touchpad's mouse
    // node — i.e. the hid-playstation backends. (The kernel registers the touchpad from its own
    // hardcoded DS5/DS4 handling, so no descriptor shaping can duck the selection.)
    let playstation = matches!(
        chosen,
        GamepadPref::DualSense | GamepadPref::DualSenseEdge | GamepadPref::DualShock4
    );
    if !playstation
        || !ds_inhibit_storm_risk(
            std::path::Path::new("/proc"),
            std::path::Path::new("/sys/fs/selinux/enforce"),
        )
        || !ONCE.swap(false, Ordering::Relaxed)
    {
        return;
    }
    tracing::warn!(
        gamepad = chosen.as_str(),
        "steamos-manager is running and SELinux is enforcing — its ds_inhibit scans /proc on \
         every open/close of this pad's hidraw, the scan is denied at hundreds of AVCs/sec, and \
         setroubleshootd can amplify that into a box-wide stall that starves the stream. Install \
         the shipped SELinux drop-in (`sudo punktfunk-sysext reapply`, or `sudo semodule -i \
         /usr/share/punktfunk/selinux/punktfunk-ds-inhibit.cil`) — harmless if already installed. \
         Masking setroubleshootd (`sudo systemctl mask --now setroubleshootd`) hardens the box \
         against any audit flood. Details: packaging/bazzite/README.md."
    );
}

#[cfg(not(target_os = "linux"))]
fn warn_if_ds_inhibit_storm(_chosen: GamepadPref) {}

/// The Valve product id (`28DE:xxxx`) a virtual Steam backend enumerates as, or `None` for a
/// non-Steam backend. This is the identity the conflict gate compares against the *physical* Valve
/// devices attached to the host: only a genuine duplicate (same VID **and** PID) confuses Steam
/// Input, so the gate keys on the PID, not on the `28DE` vendor alone.
#[cfg(target_os = "linux")]
fn steam_backend_product(pref: GamepadPref) -> Option<u16> {
    match pref {
        GamepadPref::SteamDeck => Some(0x1205), // built-in Deck controller identity
        GamepadPref::SteamController => Some(0x1102), // classic wired Steam Controller
        GamepadPref::SteamController2 => Some(0x1302), // Steam Controller 2 (Triton), wired
        GamepadPref::SteamController2Puck => Some(0x1304), // Steam Controller 2 via its Puck dongle
        _ => None,
    }
}

/// True if a **physical** Valve device with this exact product id (`28DE:{product}`) is already
/// attached. The host's own Steam Input is then managing that identity, and presenting a *second,
/// identical* virtual one makes Steam juggle two copies of the same Deck — confirmed conflict-prone
/// on a Deck-as-host (the physical `28DE:1205` + Steam's `28DE:11FF` XInput output pad are both
/// live).
///
/// Crucially this keys on the PID, not just the `28DE` vendor: a *different* Valve product is not a
/// conflict. A physical Steam Controller 2 (`28DE:1302`) must NOT block a virtual Steam Deck
/// (`28DE:1205`) — Steam Input drives distinct Steam controllers side by side, so the wanted pad
/// has to pass through unharmed (this over-broad vendor match degraded such sessions to DualSense).
///
/// HID device dirs are named `BUS:VID:PID.INST` (uppercase); a UHID virtual device resolves through
/// `/devices/virtual/…`, a real one does not. Punktfunk's OWN virtual pads must never count: the
/// usbip/gadget transports present a real USB device (vhci resolves through `vhci_hcd`, NOT
/// `/devices/virtual/`), so a just-ended session's pad still detaching — or a concurrent session's
/// live one — read as "physical" and degraded every back-to-back Deck session to DualSense
/// (observed live on Bazzite 2026-07-04). Ours are recognizable by the `FVPF…` serial
/// ([`steam_proto::deck_serial`]) in `HID_UNIQ`, with the vhci path as belt and braces.
#[cfg(target_os = "linux")]
fn physical_steam_product_present(product: u16) -> bool {
    let needle = format!(":28DE:{product:04X}");
    let Ok(entries) = std::fs::read_dir("/sys/bus/hid/devices") else {
        return false;
    };
    entries.flatten().any(|e| {
        if !e.file_name().to_string_lossy().contains(&needle) {
            return false;
        }
        if std::fs::read_to_string(e.path().join("uevent"))
            .is_ok_and(|u| u.lines().any(|l| l.starts_with("HID_UNIQ=FVPF")))
        {
            return false; // one of our own virtual pads
        }
        match std::fs::read_link(e.path()) {
            Ok(target) => {
                let t = target.to_string_lossy();
                !t.contains("/virtual/") && !t.contains("vhci_hcd")
            }
            Err(_) => true,
        }
    })
}

/// Gate a virtual Steam pad off when a physical Steam controller *of the same identity* is attached
/// (§ conflict). Degrade to DualSense (then the uhid ladder), which Steam treats as an ordinary,
/// distinct pad. Override with `PUNKTFUNK_STEAM_FORCE=1` when the host has no competing Steam Input
/// (e.g. a remote-only box).
///
/// Only a same-PID duplicate degrades: a physical Steam Controller 2 no longer blocks a virtual
/// Steam Deck (and vice versa), so a Steam Machine with an SC2 plugged in streams the pad the client
/// actually asked for.
#[cfg(target_os = "linux")]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    let Some(product) = steam_backend_product(chosen) else {
        return chosen; // not a virtual Steam (28DE) pad — nothing to gate
    };
    let forced = std::env::var("PUNKTFUNK_STEAM_FORCE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !forced && physical_steam_product_present(product) {
        let conflict = format!("28DE:{product:04X}");
        tracing::warn!(
            wanted = chosen.as_str(),
            conflict = conflict.as_str(),
            "a physical Steam controller of the same identity is attached — the host's Steam Input \
             would manage two identical 28DE devices; falling back to DualSense (set \
             PUNKTFUNK_STEAM_FORCE=1 to override)"
        );
        return degrade_if_no_uhid(GamepadPref::DualSense);
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Runtime degrade for the two non-default Windows X-Box identities (One S / Elite Series 2): with
/// `PUNKTFUNK_XBOX_BACKEND=xusb` the session runs the XUSB companion, which presents ONE fixed
/// X-Box 360 identity and has no way to vary VID/PID — so the pad a player gets is a 360 pad no
/// matter what was asked for. Fold here so the `Welcome` echo says so.
///
/// This is a runtime check and [`pick_gamepad`] is a compile-time one, which is exactly the split
/// [`degrade_if_no_uhid`] already draws. Without it, asking for an Elite under the escape hatch
/// resolves to `xboxelite`, echoes `xboxelite`, and builds a 360 pad — the class of silent lie
/// `pad_motion_reaches` and the fold-logging in [`resolve_gamepad`] exist to prevent.
///
/// A no-op on every non-Windows host: `XboxElite` never survives `pick_gamepad` there, and
/// `XboxOne` is a genuine uinput identity on Linux.
#[cfg(target_os = "windows")]
fn degrade_xbox_identity(chosen: GamepadPref) -> GamepadPref {
    if matches!(chosen, GamepadPref::XboxOne | GamepadPref::XboxElite) && !windows_xbox_hid() {
        tracing::warn!(
            wanted = chosen.as_str(),
            "PUNKTFUNK_XBOX_BACKEND=xusb selects the XUSB companion, which has one fixed X-Box 360 \
             identity — falling back to the 360 pad"
        );
        return GamepadPref::Xbox360;
    }
    chosen
}

#[cfg(not(target_os = "windows"))]
fn degrade_xbox_identity(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Whether an Xbox-family pad should be built as a real **HID** device
/// ([`crate::inject::xbox_windows`]) instead of the **XUSB** companion
/// ([`crate::inject::gamepad`]). Windows only. **HID is the default**; set
/// `PUNKTFUNK_XBOX_BACKEND=xusb` to go back to the companion.
///
/// **Why HID is now the default.** The XUSB companion registers only `GUID_DEVINTERFACE_XUSB` and
/// exposes no HID collection, so Steam's hidapi enumeration, DirectInput, `joy.cpl` and
/// WGI/GameInput cannot see it at all — only classic `XInputGetState` via xinput1_4's interface walk
/// does. That is what left a reporter with a dead controller for two weeks (2026-08-09) until they
/// switched the client to DualSense, a real HID pad.
///
/// This was an opt-in knob for exactly one reason: the HID pad could not reach classic XInput, so
/// defaulting to it would have traded a known-working path for an unproven one. **That objection is
/// gone.** `pf_gamepad.inx`'s `pfGamepadXbox` section now attaches the `xinputhid` bus filter
/// (`UpperFilters` + `DevicePropertyFlags=1`), and with it the HID pad is promoted exactly like real
/// hardware: measured on `.173` 2026-08-09 it gains the `IG_00` token and an XUSB interface, classic
/// XInput reads it live (full stick range and buttons), `XInputSetState` rumble round-trips, and it
/// keeps everything the XUSB companion never had — Steam, SDL, RawInput, DirectInput, `joy.cpl`.
/// ⇒ the HID backend is now a **superset** of the XUSB one, which is the condition the old comment
/// set for flipping.
///
/// ⚠️ `xusb` stays as an escape hatch because the promotion depends on Microsoft's inbox
/// `xinputhid.inf` and its hardware-id allow-list. If a Windows servicing update changes that, or a
/// box has a third-party filter on the stack, one env var restores the previous behaviour without a
/// reinstall.
///
/// The two backends are mutually exclusive per pad by construction (one match arm or the other) —
/// presenting both would hand a game two controllers for one pair of hands.
#[cfg(target_os = "windows")]
pub(super) fn windows_xbox_hid() -> bool {
    match std::env::var("PUNKTFUNK_XBOX_BACKEND") {
        Ok(v) if v.trim().eq_ignore_ascii_case("xusb") => false,
        // Anything else — unset, empty, "hid", or a typo — takes the default. A misspelled opt-out
        // silently landing on the OLD path is the worse failure: it is invisible, and it is the
        // path with no HID collection.
        _ => true,
    }
}

/// Resolve the client's gamepad-backend preference (the env/logging shell around
/// [`pick_gamepad`]). Always concrete — the `Welcome` reports what the session will drive.
pub(super) fn resolve_gamepad(pref: GamepadPref) -> GamepadPref {
    let env = pf_host_config::config().gamepad.clone();
    let chosen = pick_gamepad(
        pref,
        env.as_deref(),
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    // Runtime degrade (separate from the compile-time platform check above): the Linux UHID
    // backends need `/dev/uhid` usable *now*, else creating the device just fails and the controller
    // goes dead — fall back to the always-available uinput X-Box 360 pad instead.
    let chosen = degrade_if_no_uhid(chosen);
    // Conflict gate: don't present a virtual Steam (28DE) pad when the host already has a physical
    // Steam controller — its own Steam Input would then manage two Decks (confirmed conflict-prone on
    // a Deck-as-host). `PUNKTFUNK_STEAM_FORCE=1` overrides.
    let chosen = degrade_steam_on_conflict(chosen);
    // The XUSB escape hatch can only present a 360 identity, so the One S / Elite wishes fold when
    // `PUNKTFUNK_XBOX_BACKEND=xusb` is set.
    let chosen = degrade_xbox_identity(chosen);
    // Bazzite/SteamOS heads-up, warn-only (see the fn for why never a degrade): a
    // hid-playstation pad on a box running steamos-manager under SELinux enforcing risks the
    // ds_inhibit audit storm.
    warn_if_ds_inhibit_storm(chosen);
    match pref {
        GamepadPref::Auto => {
            // The operator's env knob deserves a diagnostic when it didn't drive the
            // choice — a typo, or a DualSense wish on a non-UHID host, would otherwise
            // degrade silently.
            if let Some(env) = env.as_deref() {
                if GamepadPref::from_name(env) != Some(chosen) {
                    tracing::warn!(
                        env,
                        chosen = chosen.as_str(),
                        "PUNKTFUNK_GAMEPAD unrecognized or unavailable — falling back"
                    );
                }
            }
            tracing::info!(gamepad = chosen.as_str(), "gamepad backend (client: auto)")
        }
        want if want == chosen => {
            tracing::info!(gamepad = chosen.as_str(), "honoring client gamepad request")
        }
        want => tracing::warn!(
            requested = want.as_str(),
            chosen = chosen.as_str(),
            "client-requested gamepad backend unavailable — falling back"
        ),
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::{pick_gamepad, route_decision};
    use punktfunk_core::config::GamepadPref;

    #[test]
    fn per_pad_route_decision() {
        use GamepadPref::{DualSense, Xbox360};
        // First frame with no device: create in the declared kind's manager, record ownership.
        assert_eq!(
            route_decision(None, DualSense, true),
            (DualSense, Some(DualSense))
        );
        // Subsequent frame: stays in the owning manager even if the declared kind now differs
        // (the arrival-after-first-frame reorder) — never a second device in another manager.
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, true),
            (DualSense, Some(DualSense))
        );
        // Removal (cleared bit): routes to the owner so the RIGHT device is torn down, then clears.
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, false),
            (DualSense, None)
        );
        // Removal with no device is a harmless no-op route (owner stays cleared).
        assert_eq!(route_decision(None, Xbox360, false), (Xbox360, None));
        // A fresh device after a re-plug picks up the newly-declared kind (owner was cleared).
        assert_eq!(
            route_decision(None, Xbox360, true),
            (Xbox360, Some(Xbox360))
        );
    }

    #[test]
    fn gamepad_resolution_precedence() {
        use GamepadPref::*;
        // Trailing args are (linux, windows).
        // An explicit client choice wins over the env var.
        assert_eq!(
            pick_gamepad(DualSense, Some("xbox360"), true, false),
            DualSense
        );
        assert_eq!(
            pick_gamepad(Xbox360, Some("dualsense"), true, false),
            Xbox360
        );
        // Client Auto defers to the env var.
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), true, false),
            DualSense
        );
        assert_eq!(pick_gamepad(Auto, Some("xbox360"), true, false), Xbox360);
        // Auto + no env (or an unparseable one) → X-Box 360.
        assert_eq!(pick_gamepad(Auto, None, true, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("bogus"), true, false), Xbox360);
        // DualSense: honored on Linux (UHID) AND Windows (UMDF minidriver); degrades elsewhere.
        assert_eq!(pick_gamepad(DualSense, None, false, true), DualSense);
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), false, true),
            DualSense
        );
        assert_eq!(pick_gamepad(DualSense, None, false, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("dualsense"), false, false), Xbox360);
        // DualShock 4: honored on Linux (UHID) AND Windows (UMDF minidriver); degrades elsewhere.
        assert_eq!(pick_gamepad(DualShock4, None, true, false), DualShock4);
        assert_eq!(pick_gamepad(Auto, Some("ps4"), true, false), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, true), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, false), Xbox360);
        // X-Box One: a distinct uinput identity on Linux AND a distinct UMDF HID identity
        // (`045E:02FD`) on Windows. The old Windows fold to Xbox360 is deliberately gone — it
        // existed only because the XUSB companion has one fixed 360 identity, and the HID backend
        // is the default now. `degrade_xbox_identity` puts the fold back when the escape hatch
        // `PUNKTFUNK_XBOX_BACKEND=xusb` is set; that is a runtime check this pure one can't make.
        assert_eq!(pick_gamepad(XboxOne, None, true, false), XboxOne);
        assert_eq!(pick_gamepad(Auto, Some("series"), true, false), XboxOne);
        assert_eq!(pick_gamepad(XboxOne, None, false, true), XboxOne);
        assert_eq!(pick_gamepad(XboxOne, None, false, false), Xbox360);
        // X-Box Elite Series 2: Windows-only (UMDF device-type 6). No Linux uinput Elite identity
        // exists, so it folds to the 360 pad there rather than pretending.
        assert_eq!(pick_gamepad(XboxElite, None, false, true), XboxElite);
        assert_eq!(pick_gamepad(Auto, Some("elite"), false, true), XboxElite);
        assert_eq!(pick_gamepad(XboxElite, None, true, false), Xbox360);
        assert_eq!(pick_gamepad(XboxElite, None, false, false), Xbox360);

        // Steam Deck: native on Linux (UHID/usbip/gadget) AND Windows (UMDF device-type 3,
        // Steam-Input-promoted via MI_02 — gamepad-new-types N4); Xbox360 elsewhere.
        assert_eq!(pick_gamepad(SteamDeck, None, true, false), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, true), SteamDeck);
        assert_eq!(pick_gamepad(Auto, Some("deck"), false, true), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, false), Xbox360);
        // Classic Steam Controller: native on Linux (UHID hid-steam); Xbox360 elsewhere.
        assert_eq!(
            pick_gamepad(SteamController, None, true, false),
            SteamController
        );
        assert_eq!(
            pick_gamepad(Auto, Some("steamcontroller"), true, false),
            SteamController
        );
        assert_eq!(pick_gamepad(SteamController, None, false, true), Xbox360);

        // DualSense Edge: native on Linux (UHID) AND Windows (UMDF device-type 2); Xbox360
        // elsewhere.
        assert_eq!(
            pick_gamepad(DualSenseEdge, None, true, false),
            DualSenseEdge
        );
        assert_eq!(
            pick_gamepad(DualSenseEdge, None, false, true),
            DualSenseEdge
        );
        assert_eq!(pick_gamepad(Auto, Some("edge"), true, false), DualSenseEdge);
        assert_eq!(pick_gamepad(DualSenseEdge, None, false, false), Xbox360);
        // Switch Pro: native on Linux (UHID hid-nintendo); Xbox360 on Windows and elsewhere.
        assert_eq!(pick_gamepad(SwitchPro, None, true, false), SwitchPro);
        assert_eq!(
            pick_gamepad(Auto, Some("switchpro"), true, false),
            SwitchPro
        );
        assert_eq!(pick_gamepad(Auto, Some("switch"), true, false), SwitchPro);
        assert_eq!(pick_gamepad(SwitchPro, None, false, true), Xbox360);
        assert_eq!(pick_gamepad(SwitchPro, None, false, false), Xbox360);
        // New Steam Controller (as-is Triton passthrough): native on Linux (UHID, Steam-driven);
        // Xbox360 on Windows and elsewhere.
        assert_eq!(
            pick_gamepad(SteamController2, None, true, false),
            SteamController2
        );
        assert_eq!(
            pick_gamepad(Auto, Some("sc2"), true, false),
            SteamController2
        );
        assert_eq!(
            pick_gamepad(Auto, Some("ibex"), true, false),
            SteamController2
        );
        assert_eq!(pick_gamepad(SteamController2, None, false, true), Xbox360);
        assert_eq!(pick_gamepad(SteamController2, None, false, false), Xbox360);
        assert_eq!(
            pick_gamepad(SteamController2Puck, None, true, false),
            SteamController2Puck
        );
        assert_eq!(
            pick_gamepad(Auto, Some("sc2puck"), true, false),
            SteamController2Puck
        );
        assert_eq!(
            pick_gamepad(SteamController2Puck, None, false, true),
            Xbox360
        );
    }

    // The conflict gate keys on the exact Valve PID, so a physical Steam Controller 2 (`28DE:1302`)
    // never blocks a virtual Steam Deck (`28DE:1205`) — only a same-identity duplicate degrades.
    #[cfg(target_os = "linux")]
    #[test]
    fn steam_backend_product_ids() {
        use super::steam_backend_product;
        use GamepadPref::*;
        assert_eq!(steam_backend_product(SteamDeck), Some(0x1205));
        assert_eq!(steam_backend_product(SteamController), Some(0x1102));
        assert_eq!(steam_backend_product(SteamController2), Some(0x1302));
        assert_eq!(steam_backend_product(SteamController2Puck), Some(0x1304));
        // Non-Steam backends carry no Valve identity, so the gate never fires for them.
        assert_eq!(steam_backend_product(DualSense), None);
        assert_eq!(steam_backend_product(Xbox360), None);
        assert_eq!(steam_backend_product(SwitchPro), None);
    }

    // The ds_inhibit-storm detection needs BOTH halves: steamos-manager running AND SELinux
    // enforcing. A permissive box logs one denial per walk without storming, and without
    // steamos-manager there is no ds_inhibit — either alone must stay silent.
    #[cfg(target_os = "linux")]
    #[test]
    fn ds_inhibit_storm_risk_needs_both_halves() {
        use super::ds_inhibit_storm_risk;
        let dir = tempfile::tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let enforce = dir.path().join("enforce");
        std::fs::create_dir_all(proc_root.join("123")).unwrap();

        // Enforcing + steamos-manager present → risk.
        std::fs::write(proc_root.join("123/comm"), "steamos-manager\n").unwrap();
        std::fs::write(&enforce, "1\n").unwrap();
        assert!(ds_inhibit_storm_risk(&proc_root, &enforce));

        // Permissive (or SELinux absent — the enforce file unreadable) → no risk.
        std::fs::write(&enforce, "0\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
        assert!(!ds_inhibit_storm_risk(
            &proc_root,
            &dir.path().join("missing")
        ));

        // Enforcing but no steamos-manager (a comm that merely CONTAINS the name must not
        // match — the scan compares the whole trimmed comm) → no risk.
        std::fs::write(&enforce, "1\n").unwrap();
        std::fs::write(proc_root.join("123/comm"), "not-steamos\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
        std::fs::write(proc_root.join("123/comm"), "steamos-managerX\n").unwrap();
        assert!(!ds_inhibit_storm_risk(&proc_root, &enforce));
    }
}
