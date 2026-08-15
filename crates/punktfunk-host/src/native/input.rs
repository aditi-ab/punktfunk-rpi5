//! The native input plane (plan §W1 — carved out of the [`super`] module): the client→host input
//! thread and the per-pad virtual-gamepad router ([`Pads`]) that fans mixed controller kinds out to
//! the right injector backend (uinput / UHID on Linux, XUSB / UMDF on Windows), plus rumble
//! feedback. `serve_session` spawns [`input_thread`] and feeds it a channel of [`ClientInput`].

use super::*;

/// Per-pad accumulated state: punktfunk/1 gamepad events are incremental (one button or axis
/// per datagram, see `punktfunk_core::input::gamepad`), the virtual xpad applies full frames.
/// A snapshot-capable client replaces the whole state at once ([`PadState::set_snapshot`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PadState {
    buttons: u32,
    left_trigger: u8,
    right_trigger: u8,
    ls_x: i16,
    ls_y: i16,
    rs_x: i16,
    rs_y: i16,
}

impl PadState {
    /// Fold one wire event into the state. `false` = unknown axis id (event dropped).
    fn apply(&mut self, ev: &InputEvent) -> bool {
        if ev.kind == InputKind::GamepadButton {
            if ev.x != 0 {
                self.buttons |= ev.code;
            } else {
                self.buttons &= !ev.code;
            }
            return true;
        }
        use punktfunk_core::input::gamepad::*;
        let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let trigger = ev.x.clamp(0, 255) as u8;
        match ev.code {
            AXIS_LS_X => self.ls_x = stick,
            AXIS_LS_Y => self.ls_y = stick,
            AXIS_RS_X => self.rs_x = stick,
            AXIS_RS_Y => self.rs_y = stick,
            AXIS_LT => self.left_trigger = trigger,
            AXIS_RT => self.right_trigger = trigger,
            _ => return false,
        }
        true
    }

    /// Replace the whole state from one client snapshot (the [`InputKind::GamepadState`] form).
    fn set_snapshot(&mut self, s: &punktfunk_core::input::GamepadSnapshot) {
        self.buttons = s.buttons;
        self.left_trigger = s.left_trigger;
        self.right_trigger = s.right_trigger;
        self.ls_x = s.ls_x;
        self.ls_y = s.ls_y;
        self.rs_x = s.rs_x;
        self.rs_y = s.rs_y;
    }

    fn frame(&self, index: usize, active_mask: u16) -> punktfunk_core::input::GamepadFrame {
        punktfunk_core::input::GamepadFrame {
            index: index as i16,
            active_mask,
            buttons: self.buttons,
            left_trigger: self.left_trigger,
            right_trigger: self.right_trigger,
            ls_x: self.ls_x,
            ls_y: self.ls_y,
            rs_x: self.rs_x,
            rs_y: self.rs_y,
        }
    }
}

/// Highest pad index addressable on the wire (`flags` field / snapshot `pad`); the uinput
/// manager caps actual pad creation at its own MAX_PADS.
const MAX_WIRE_PADS: usize = punktfunk_core::input::MAX_PADS;

/// Per-pad virtual-gamepad router: each pad index is served by a backend of that pad's declared
/// kind ([`InputKind::GamepadArrival`](punktfunk_core::input::InputKind::GamepadArrival)), so ONE
/// session can MIX controller types — pad 0 a DualSense, pad 1 an Xbox pad. A pad the client never
/// declares uses `default` (the session kind resolved from the Hello — the pre-existing single-kind
/// behaviour).
///
/// Backends are created lazily per kind (an empty manager holds no device), and each owns only the
/// indices routed to it. A manager's `active_mask` unplug sweep stays correct across managers
/// because an index another manager owns is `None` in this one, so the sweep never touches it.
///
/// - Xbox 360 / One — uinput on Linux ([`GamepadManager`](crate::inject::gamepad::GamepadManager),
///   two identities), the XUSB companion driver (classic XInput) on Windows.
/// - DualSense / DualSense Edge / DualShock 4 — Linux UHID `hid-playstation`, or the Windows UMDF
///   minidriver (device-type 0/2/1).
/// - Steam Deck — Linux UHID `hid-steam` (or usbip/gadget), or the Windows UMDF minidriver
///   (device-type 3, Steam-Input-promoted).
///
/// [`resolve_pad_kind`] folds any kind a platform can't build into one it can, so this never
/// constructs a manager the build lacks.
struct Pads {
    /// Declared (and host-resolved) kind per pad index; `default` until a `GamepadArrival` lands.
    kinds: [GamepadPref; MAX_WIRE_PADS],
    /// The kind of the manager that currently OWNS a built device at each index (`None` = no
    /// device). A live device stays in its manager even if `kinds[idx]` later changes (the rare
    /// arrival-after-first-frame reorder), so a pad is never duplicated across managers and its
    /// removal always reaches the manager that actually holds it.
    owner: [Option<GamepadPref>; MAX_WIRE_PADS],
    xbox360: Option<crate::inject::gamepad::GamepadManager>,
    #[cfg(target_os = "linux")]
    xboxone: Option<crate::inject::gamepad::GamepadManager>,
    #[cfg(target_os = "linux")]
    dualsense: Option<crate::inject::dualsense::DualSenseManager>,
    #[cfg(target_os = "linux")]
    dualsense_edge: Option<crate::inject::dualsense::DualSenseEdgeManager>,
    #[cfg(target_os = "linux")]
    dualshock4: Option<crate::inject::dualshock4::DualShock4Manager>,
    #[cfg(target_os = "linux")]
    steamdeck: Option<crate::inject::steam_controller::SteamControllerManager>,
    #[cfg(target_os = "linux")]
    switchpro: Option<crate::inject::switch_pro::SwitchProManager>,
    #[cfg(target_os = "linux")]
    steamctrl: Option<crate::inject::steam_controller::SteamCtrlManager>,
    #[cfg(target_os = "linux")]
    steamctrl2: Option<crate::inject::steam_controller2::Triton2Manager>,
    #[cfg(target_os = "linux")]
    steamctrl2_puck: Option<crate::inject::steam_controller2::Triton2Manager>,
    #[cfg(target_os = "windows")]
    dualsense_win: Option<crate::inject::dualsense_windows::DualSenseWindowsManager>,
    /// The HID-visible Xbox pad ([`crate::inject::xbox_windows`]) — used INSTEAD of `xbox360`'s
    /// XUSB companion when [`super::gamepad::windows_xbox_hid`] says so. Never both at once: two
    /// devices for one wire pad is the "the game sees two controllers" bug.
    ///
    /// Three managers because the HID backend now has three IDENTITIES (Xbox Wireless `045E:0B13`,
    /// Xbox One S `045E:02FD`, Elite Series 2 `045E:0B22`) and a manager is bound to one at
    /// construction. They are otherwise the same backend — same codec, same report descriptor,
    /// same rumble plane — so the split is purely so a mixed session can present, say, a Series
    /// pad on slot 0 and an Elite on slot 1.
    #[cfg(target_os = "windows")]
    xbox_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    #[cfg(target_os = "windows")]
    xbox_one_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    #[cfg(target_os = "windows")]
    xbox_elite_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    #[cfg(target_os = "windows")]
    dualsense_edge_win: Option<crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager>,
    #[cfg(target_os = "windows")]
    dualshock4_win: Option<crate::inject::dualshock4_windows::DualShock4WindowsManager>,
    #[cfg(target_os = "windows")]
    steamdeck_win: Option<crate::inject::steam_deck_windows::SteamDeckWindowsManager>,
}

impl Pads {
    /// `default` is the session kind (see [`resolve_gamepad`]); every pad starts on it until the
    /// client declares its own kind.
    fn new(default: GamepadPref) -> Pads {
        let default = resolve_pad_kind(default);
        tracing::info!(
            default = default.as_str(),
            "gamepad backends: per-pad router (session default)"
        );
        Pads {
            kinds: [default; MAX_WIRE_PADS],
            owner: [None; MAX_WIRE_PADS],
            xbox360: None,
            #[cfg(target_os = "linux")]
            xboxone: None,
            #[cfg(target_os = "linux")]
            dualsense: None,
            #[cfg(target_os = "linux")]
            dualsense_edge: None,
            #[cfg(target_os = "linux")]
            dualshock4: None,
            #[cfg(target_os = "linux")]
            steamdeck: None,
            #[cfg(target_os = "linux")]
            switchpro: None,
            #[cfg(target_os = "linux")]
            steamctrl: None,
            #[cfg(target_os = "linux")]
            steamctrl2: None,
            #[cfg(target_os = "linux")]
            steamctrl2_puck: None,
            #[cfg(target_os = "windows")]
            dualsense_win: None,
            #[cfg(target_os = "windows")]
            xbox_hid: None,
            #[cfg(target_os = "windows")]
            xbox_one_hid: None,
            #[cfg(target_os = "windows")]
            xbox_elite_hid: None,
            #[cfg(target_os = "windows")]
            dualsense_edge_win: None,
            #[cfg(target_os = "windows")]
            dualshock4_win: None,
            #[cfg(target_os = "windows")]
            steamdeck_win: None,
        }
    }

    /// Record a pad's client-declared kind (resolved to a buildable backend). Takes effect on the
    /// pad's next frame; the arrival is sent before the pad's first input, so a device already
    /// built under the wrong kind is only the rare arrival-after-first-frame reorder — it then
    /// keeps the earlier kind until re-plug (no live device swap).
    fn set_kind(&mut self, idx: usize, kind: GamepadPref) {
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let resolved = resolve_pad_kind(kind);
        if self.kinds[idx] != resolved {
            tracing::info!(
                pad = idx,
                kind = resolved.as_str(),
                "gamepad kind declared (per-pad)"
            );
        }
        self.kinds[idx] = resolved;
    }

    fn handle(&mut self, ev: &punktfunk_core::input::GamepadEvent) {
        use punktfunk_core::input::GamepadEvent;
        // Present = a create/update frame (the pad's mask bit is set); a cleared bit is the
        // removal frame emitted by the native detach path (`GamepadRemove`).
        let (idx, present) = match ev {
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                (idx, f.active_mask & (1 << idx) != 0)
            }
            GamepadEvent::Arrival { index, .. } => (*index as usize, true),
        };
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let (kind, new_owner) = route_decision(self.owner[idx], self.kinds[idx], present);
        self.owner[idx] = new_owner;
        self.route_handle(kind, ev);
    }

    /// Dispatch a decoded event to the manager for `kind`, creating it lazily.
    fn route_handle(&mut self, kind: GamepadPref, ev: &punktfunk_core::input::GamepadEvent) {
        match kind {
            #[cfg(target_os = "linux")]
            GamepadPref::DualSense => self
                .dualsense
                .get_or_insert_with(crate::inject::dualsense::DualSenseManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::DualSenseEdge => self
                .dualsense_edge
                .get_or_insert_with(crate::inject::dualsense::DualSenseEdgeManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::DualShock4 => self
                .dualshock4
                .get_or_insert_with(crate::inject::dualshock4::DualShock4Manager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamDeck => self
                .steamdeck
                .get_or_insert_with(crate::inject::steam_controller::SteamControllerManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SwitchPro => self
                .switchpro
                .get_or_insert_with(crate::inject::switch_pro::SwitchProManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController => self
                .steamctrl
                .get_or_insert_with(crate::inject::steam_controller::SteamCtrlManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController2 => self
                .steamctrl2
                .get_or_insert_with(crate::inject::steam_controller2::Triton2Manager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController2Puck => self
                .steamctrl2_puck
                .get_or_insert_with(|| {
                    crate::inject::steam_controller2::Triton2Manager::with_backend(
                        crate::inject::steam_controller2::TritonProto::puck(),
                    )
                })
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::XboxOne => self
                .xboxone
                .get_or_insert_with(|| {
                    crate::inject::gamepad::GamepadManager::with_identity(
                        crate::inject::gamepad::PadIdentity::xbox_one(),
                    )
                })
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualSense => self
                .dualsense_win
                .get_or_insert_with(crate::inject::dualsense_windows::DualSenseWindowsManager::new)
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualSenseEdge => self
                .dualsense_edge_win
                .get_or_insert_with(
                    crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new,
                )
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualShock4 => self
                .dualshock4_win
                .get_or_insert_with(
                    crate::inject::dualshock4_windows::DualShock4WindowsManager::new,
                )
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::SteamDeck => self
                .steamdeck_win
                .get_or_insert_with(crate::inject::steam_deck_windows::SteamDeckWindowsManager::new)
                .handle(ev),
            // The Xbox pads, as real HID devices rather than the XUSB companion. This is now the
            // DEFAULT (see `windows_xbox_hid`; `PUNKTFUNK_XBOX_BACKEND=xusb` reverts it). It is no
            // longer a trade: with the `xinputhid` bus filter the INF attaches, the HID pad keeps
            // classic XInput AND gains everything XUSB never had — Steam, SDL, RawInput,
            // DirectInput, `joy.cpl`, WGI — plus rumble, which the XUSB path could not source.
            //
            // Three arms, one per identity. The `windows_xbox_hid()` guard stays on each: with the
            // escape hatch set, `degrade_xbox_identity` has already folded One/Elite to Xbox360, so
            // only Xbox360 can reach here and it must fall through to the XUSB companion below.
            #[cfg(target_os = "windows")]
            GamepadPref::Xbox360 if super::gamepad::windows_xbox_hid() => self
                .xbox_hid
                .get_or_insert_with(crate::inject::xbox_windows::XboxWindowsManager::new)
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::XboxOne if super::gamepad::windows_xbox_hid() => self
                .xbox_one_hid
                .get_or_insert_with(|| {
                    crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                        crate::inject::xbox_windows::XboxWinProto::one_s(),
                    )
                })
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::XboxElite if super::gamepad::windows_xbox_hid() => self
                .xbox_elite_hid
                .get_or_insert_with(|| {
                    crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                        crate::inject::xbox_windows::XboxWinProto::elite(),
                    )
                })
                .handle(ev),
            _ => self
                .xbox360
                .get_or_insert_with(crate::inject::gamepad::GamepadManager::new)
                .handle(ev),
        }
    }

    /// Apply a rich client→host event (touchpad / motion) to the pad's kind manager, if it exists
    /// (rich before the first frame = no device yet = a no-op anyway). The X-Box pads have no rich
    /// plane, so those indices ignore it.
    fn apply_rich(&mut self, rich: punktfunk_core::quic::RichInput) {
        use punktfunk_core::quic::RichInput;
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. }
            | RichInput::HidReport { pad, .. } => pad as usize,
        };
        // Route to the manager that actually owns the device (falling back to the declared kind
        // before the first frame builds it), so a pad's touchpad/motion never lands on the wrong
        // backend after a kind change.
        let kind = self
            .owner
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| self.kinds.get(idx).copied())
            .unwrap_or(GamepadPref::Xbox360);
        match kind {
            #[cfg(target_os = "linux")]
            GamepadPref::DualSense => {
                if let Some(m) = &mut self.dualsense {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::DualSenseEdge => {
                if let Some(m) = &mut self.dualsense_edge {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::DualShock4 => {
                if let Some(m) = &mut self.dualshock4 {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamDeck => {
                if let Some(m) = &mut self.steamdeck {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SwitchPro => {
                if let Some(m) = &mut self.switchpro {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController => {
                if let Some(m) = &mut self.steamctrl {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController2 => {
                if let Some(m) = &mut self.steamctrl2 {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController2Puck => {
                if let Some(m) = &mut self.steamctrl2_puck {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualSense => {
                if let Some(m) = &mut self.dualsense_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualSenseEdge => {
                if let Some(m) = &mut self.dualsense_edge_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualShock4 => {
                if let Some(m) = &mut self.dualshock4_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::SteamDeck => {
                if let Some(m) = &mut self.steamdeck_win {
                    m.apply_rich(rich)
                }
            }
            _ => {}
        }
    }

    /// Triton's USB output endpoint is polled at 1 kHz. Service its raw haptic writes on the same
    /// cadence so PC-generated trackpad pulses do not sit for up to 4 ms and then arrive at the
    /// client in bursts. Other backends keep the lower-frequency poll to avoid idle churn.
    fn feedback_poll_interval(&self) -> std::time::Duration {
        #[cfg(target_os = "linux")]
        if self.steamctrl2.is_some() || self.steamctrl2_puck.is_some() {
            return std::time::Duration::from_millis(1);
        }
        std::time::Duration::from_millis(4)
    }

    /// Service feedback for every instantiated backend each cycle. `rumble` carries motor
    /// force-feedback on the universal plane (every backend, tagged with its own pad index) as
    /// `(pad, low, high, left_trigger, right_trigger)`; `hidout` carries rich feedback (lightbar /
    /// player LEDs / adaptive triggers) for the UHID/UMDF pads. The `&mut` closure re-borrows
    /// satisfy `FnMut` for each backend.
    ///
    /// Only the Windows HID Xbox backends (`xbox_hid` and its two identity siblings) can ever
    /// report non-zero trigger levels — no
    /// other backend's source packet has a field for them (see `PadFeedback::rumble`), so they pass
    /// zeros and the v3 datagram they produce is a v2 datagram with a zero tail.
    fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16, u16, u16),
        mut hidout: impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        if let Some(m) = &mut self.xbox360 {
            m.pump_rumble(&mut rumble); // the X-Box pad has no rich-feedback plane
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(m) = &mut self.xboxone {
                m.pump_rumble(&mut rumble);
            }
            if let Some(m) = &mut self.dualsense {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualsense_edge {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualshock4 {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamdeck {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.switchpro {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamctrl {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamctrl2 {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamctrl2_puck {
                m.pump(&mut rumble, &mut hidout);
            }
        }
        #[cfg(target_os = "windows")]
        {
            // All three HID Xbox identities. Rumble only — an Xbox pad has no rich-feedback plane
            // (no lightbar / adaptive triggers), same as its XUSB sibling above. Missing one of
            // these is silent: the pad works and simply never rumbles.
            for m in [
                &mut self.xbox_hid,
                &mut self.xbox_one_hid,
                &mut self.xbox_elite_hid,
            ]
            .into_iter()
            .flatten()
            {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualsense_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualsense_edge_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualshock4_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamdeck_win {
                m.pump(&mut rumble, &mut hidout);
            }
        }
    }

    /// Keep every instantiated virtual UHID/UMDF pad alive during input silence (re-emit its HID
    /// report so the kernel driver / SDL don't drop a held-steady pad). The X-Box pads need no
    /// heartbeat (evdev holds last-known state). Per-pad gap timers inside each manager govern the
    /// actual emit cadence, not this per-tick call.
    fn heartbeat(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let gap = std::time::Duration::from_millis(8);
            if let Some(m) = &mut self.dualsense {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualsense_edge {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualshock4 {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamdeck {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.switchpro {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamctrl {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamctrl2 {
                m.heartbeat(gap);
            }
        }
        #[cfg(target_os = "windows")]
        {
            let gap = std::time::Duration::from_millis(8);
            if let Some(m) = &mut self.dualsense_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualsense_edge_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualshock4_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamdeck_win {
                m.heartbeat(gap);
            }
        }
    }
}

/// Per-pad 0xD1 streamers (`super::pad_audio`), keyed by pad index like every per-pad table
/// here (bounded by [`MAX_WIRE_PADS`]; only slots 0..4 can ever have a provisioned endpoint —
/// `spawn` refuses the rest). Spawned when a negotiated session's DualSense-family arrival
/// declares renderer bits, reaped on remove / re-declare / session teardown.
struct PadAudioSlots {
    /// `(kinds, handle)` per running pad — `kinds` is the arrival's audio-caps mask, kept so
    /// an identical re-arrival (they are re-sent against datagram loss) is a no-op.
    slots: [Option<(u8, pad_audio::PadAudioHandle)>; MAX_WIRE_PADS],
    /// Kind-change restarts spent per pad this session (R3). The trigger is a client-sent
    /// arrival, so without a ceiling the client decides how many WASAPI captures the host opens.
    restarts: [u8; MAX_WIRE_PADS],
}

/// R3: how many times one pad may change its declared audio kinds before the host stops
/// obliging. A real controller declares once at open and never again; the re-sent arrivals are
/// identical and take the no-op path above, so this is only reached by a client that keeps
/// changing its mind.
const MAX_PAD_AUDIO_RESTARTS: u8 = 8;

impl PadAudioSlots {
    fn new() -> PadAudioSlots {
        PadAudioSlots {
            slots: std::array::from_fn(|_| None),
            restarts: [0; MAX_WIRE_PADS],
        }
    }

    /// Idempotent spawn: same kinds → keep the running streamer; changed kinds → restart with
    /// the new mask; not running → spawn (a slot without an endpoint stays empty — bounded
    /// retries, since arrivals are only re-sent a few times per slot open). `edge` picks the
    /// DualSense Edge identity for the Linux sink (ignored on Windows — endpoints are
    /// pre-stamped).
    fn ensure(&mut self, conn: &quinn::Connection, pad: u8, kinds: u8, edge: bool) {
        let idx = pad as usize;
        if idx >= MAX_WIRE_PADS {
            return;
        }
        if let Some((have, _)) = &self.slots[idx] {
            if *have == kinds {
                return; // identical re-arrival — keep the running streamer
            }
            // R3: the restart trigger is a CLIENT-sent arrival, so the count is client-driven.
            // Nothing bounded it: a client alternating its declared kinds could make the host
            // tear down and re-spawn a WASAPI loopback capture indefinitely, each cycle paying a
            // thread spawn and an endpoint activation. Cheap to bound, and a pad that has already
            // changed its mind this many times in one session is not doing anything legitimate.
            if self.restarts[idx] >= MAX_PAD_AUDIO_RESTARTS {
                tracing::warn!(
                    pad = idx,
                    "pad-audio kinds changed again after {MAX_PAD_AUDIO_RESTARTS} restarts — \
                     ignoring; the streamer keeps its current kinds for this session"
                );
                return;
            }
            self.restarts[idx] += 1;
            tracing::info!(
                pad = idx,
                restarts = self.restarts[idx],
                "pad-audio kinds changed — restarting the streamer"
            );
            self.stop(idx);
        }
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(h) = pad_audio::spawn(conn.clone(), pad, kinds, edge, stop) {
            self.slots[idx] = Some((kinds, h));
        }
    }

    /// Stop + reap one pad's streamer. The join rides a detached reaper thread: a quiet pad's
    /// capturer can sit out its ~5 s recv timeout, and this thread must keep its ≤4 ms
    /// feedback cadence (games block on GET_REPORT handshakes) — the reaper still joins, just
    /// not here. A failed reaper spawn falls back to the handle's own drop (signal + join).
    fn stop(&mut self, idx: usize) {
        if let Some((_, h)) = self.slots.get_mut(idx).and_then(|s| s.take()) {
            h.signal();
            let _ = std::thread::Builder::new()
                .name("punktfunk1-padreap".into())
                .spawn(move || h.stop());
        }
    }

    /// Session teardown: flag every streamer FIRST so they wind down concurrently, then join —
    /// the worst case is ONE quiet-endpoint recv timeout (~5 s), well inside the session's
    /// 10 s side-thread join grace, not one per pad.
    fn stop_all(&mut self) {
        for s in self.slots.iter().flatten() {
            s.1.signal();
        }
        for s in &mut self.slots {
            if let Some((_, h)) = s.take() {
                h.stop();
            }
        }
    }
}

/// One client→host input item, both planes on ONE channel so the input thread wakes the
/// moment either arrives (a second rich channel drained after the 4 ms recv timeout cost
/// every pure-gyro motion sample up to 4 ms of quantization).
pub(super) enum ClientInput {
    /// The 0xC8 plane: pointer / keyboard / gamepad button+axis.
    Event(InputEvent),
    /// The 0xCC plane: touchpad contacts + motion samples.
    Rich(punktfunk_core::quic::RichInput),
    /// The 0xCC/0x05 stylus plane: state-full pen sample batches, diffed into a per-session
    /// virtual tablet (design/pen-tablet-input.md).
    Pen(punktfunk_core::quic::PenBatch),
}

/// The per-session stylus lane: the core [`PenTracker`](punktfunk_core::quic::PenTracker)
/// diffs state-full batches into transitions, applied to a lazily-created uinput tablet
/// ([`crate::inject::pen::VirtualPen`]) — a session that never draws never creates a device,
/// and the device dies with the session (kernel removes the tablet, apps see the pen unplug).
struct PenSession {
    tracker: punktfunk_core::quic::PenTracker,
    dev: Option<crate::inject::pen::VirtualPen>,
    /// Creation failed once — don't retry per batch (240 Hz of ioctl churn + log spam); the
    /// tracker still consumes batches so its state stays coherent.
    create_failed: bool,
    last_rx: std::time::Instant,
    /// Reused transition buffer (a batch yields at most a few).
    out: Vec<punktfunk_core::quic::PenTransition>,
}

impl PenSession {
    fn new() -> PenSession {
        PenSession {
            tracker: punktfunk_core::quic::PenTracker::default(),
            dev: None,
            create_failed: false,
            last_rx: std::time::Instant::now(),
            out: Vec::new(),
        }
    }

    fn apply(&mut self, batch: &punktfunk_core::quic::PenBatch) {
        self.last_rx = std::time::Instant::now();
        if self.dev.is_none() && !self.create_failed {
            match crate::inject::pen::VirtualPen::create() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    // Shouldn't happen when the Welcome advertised HOST_CAP_PEN off the same
                    // uinput probe — but permissions can change between then and first ink.
                    self.create_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "pen: virtual tablet creation failed — dropping pen input this session"
                    );
                }
            }
        }
        self.out.clear();
        self.tracker.apply(batch, &mut self.out);
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }

    /// The dead-client failsafe ([`PEN_TOUCH_TIMEOUT_MS`](punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS)):
    /// clients repeat the last sample (≤100 ms) while the pen is in range — a stationary
    /// touching pen included — so silence past the timeout means the client is gone, and the
    /// host must not leave the stroke inked-down. Called every input-loop wake; the loop caps
    /// its recv timeout at 100 ms while the pen is active so this actually gets to run.
    fn check_timeout(&mut self) {
        if self.tracker.is_active()
            && self.last_rx.elapsed().as_millis()
                >= punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS as u128
        {
            tracing::debug!("pen: sample stream went silent — force-releasing the stroke");
            self.release_all();
        }
    }

    /// Lift buttons/tip/proximity in order (a no-op when idle). Session end + the timeout.
    fn release_all(&mut self) {
        self.out.clear();
        self.tracker.force_release(&mut self.out);
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }

    fn active(&self) -> bool {
        self.tracker.is_active()
    }
}

/// Default TTL stamped on a non-zero rumble envelope (0xCA v2): how long the client renders the
/// level before silencing unless the host renews it. Tolerates 2–3 lost renewals (same loss
/// margin the old flat 500 ms refresh gave) while capping a host-abandoned rumble at this on every
/// client — versus the per-platform client heuristics it replaces (SDL 1.5 s, Apple 1.6 s, Android
/// up to the QUIC idle-timeout). Overridable via `PUNKTFUNK_RUMBLE_TTL_MS` (floored at
/// [`RUMBLE_TTL_FLOOR_MS`] so expiry jitter stays below the clients' tick granularity).
const RUMBLE_TTL_MS: u16 = 400;
/// Floor for the `PUNKTFUNK_RUMBLE_TTL_MS` hatch — below this the ~50 ms client ticks make expiry
/// audible (see `rumble-envelope-plan.md` §5).
const RUMBLE_TTL_FLOOR_MS: u16 = 150;
/// Ceiling for the `PUNKTFUNK_RUMBLE_TTL_MS` hatch. A lease longer than a few seconds defeats the
/// design's "an abandoned rumble stops promptly" goal, and keeping it well under `u16::MAX` means
/// the wire never emits a TTL a narrower client-side slot could mistake for a sentinel.
const RUMBLE_TTL_CEIL_MS: u16 = 5_000;
/// Floor for the derived renewal interval (renew = ttl × 3/10) so an aggressive TTL hatch can't
/// spin the renewal loop faster than this.
const RUMBLE_RENEW_FLOOR_MS: u64 = 60;
/// How many times a transition-to-zero (a stop) is re-sent on the renewal ticks after the
/// immediate stop datagram, before the pad goes quiet. Covers stop-datagram loss for legacy
/// clients (a v2 client also self-silences at TTL); even a fully lost burst heals via the client's
/// own expiry. `3` total zero sends = the immediate one + this many renewal re-sends.
const RUMBLE_STOP_BURST: u8 = 2;

/// Clear a removed pad's rumble bookkeeping — the level, the "we have seen a level" flag, and any
/// stop re-sends still owed. Together these end the pad's lease, so a re-plug on the same wire
/// index inherits nothing that could buzz the new device.
///
/// The per-pad rumble **sequence is deliberately not a parameter**: it must stay monotonic for the
/// life of the connection because the client gates on it with a wrapping half-space compare and
/// never resets its side (`punktfunk-core/src/client/pump/datagram_task.rs`). Resetting it here is
/// the bug pinned by [`tests::rumble_seq_survives_a_removal_so_the_client_gate_accepts`].
fn clear_pad_feedback(state: &mut RumbleLevels, seen: &mut bool, stop_burst: &mut u8) {
    *state = (0, 0, 0, 0);
    *seen = false;
    *stop_burst = 0;
}

/// One pad's four motor levels as the 0xCA plane orders them:
/// `(low, high, left_trigger, right_trigger)`, all `0..=0xFFFF`. Kept as one value rather than four
/// parallel arrays because they are a single statement of the pad's feedback state at one instant —
/// the same reason they share one `seq` and one TTL on the wire.
type RumbleLevels = (u16, u16, u16, u16);

/// Is this pad's feedback fully silent? **All four** motors, and that is the whole point of it
/// being a named predicate rather than an inline comparison repeated at each site.
///
/// Every "is this pad quiet?" decision in the rumble path routes through here: whether to log the
/// silent→active transition, whether to arm the post-stop burst, and — the one that decides
/// whether the feature works at all — whether the envelope gets a live TTL or the `0` that means
/// *stop*. Written as a two-field test, a trigger-only rumble (the normal shape of
/// impulse-trigger content: racing titles drive the triggers continuously while the handles stay
/// near-silent) is stamped `ttl = 0`, the client reads an already-expired lease and silences on
/// arrival, and nothing anywhere logs an error. See
/// [`tests::a_trigger_only_rumble_gets_a_live_ttl`].
fn rumble_silent(lv: RumbleLevels) -> bool {
    lv == (0, 0, 0, 0)
}

/// Send one rumble datagram on the universal 0xCA plane. `envelope_on` picks the self-terminating
/// v3 form (`[level][seq][ttl_ms][trigger levels]`, the default) or the legacy v1 level datagram
/// (the `PUNKTFUNK_RUMBLE_ENVELOPE=0` bisect hatch). Best-effort like every side-plane datagram.
///
/// v3 goes out **unconditionally** while the envelope is on — not "only when a trigger level is
/// non-zero". A wire form that depends on history is how you get a bug that reproduces only after a
/// specific sequence of events, and the four extra bytes cost nothing: a client that predates v3
/// reads the 10-byte prefix and ignores them.
///
/// ⚠️ The bisect hatch drops to v1, which takes trigger rumble down with it (v1 has no tail at
/// all). That is correct for a hatch whose job is to reproduce the pre-envelope wire, but it means
/// "trigger rumble stopped working" is an expected symptom of setting it — do not bisect a trigger
/// bug into this hatch and conclude the hatch fixed it.
fn send_rumble(
    conn: &quinn::Connection,
    envelope_on: bool,
    pad: u16,
    lv: RumbleLevels,
    seq: u8,
    ttl_ms: u16,
) {
    let (low, high, lt, rt) = lv;
    let d: Vec<u8> = if envelope_on {
        punktfunk_core::quic::encode_rumble_datagram_v3(pad, low, high, seq, ttl_ms, lt, rt)
            .to_vec()
    } else {
        punktfunk_core::quic::encode_rumble_datagram(pad, low, high).to_vec()
    };
    let _ = conn.send_datagram(d.into());
}

/// The per-session input thread: route pointer/keyboard events to the host-lifetime injector
/// service (`inj_tx`) and gamepad events to this session's [`Pads`] router (`gamepad` — the
/// resolved Hello preference is the per-pad default; clients declare each pad's kind so a session
/// can mix uinput X-Box pads and virtual DualSense pads), with rich
/// client→host input (touchpad / motion, [`ClientInput::Rich`]) applied on arrival and
/// feedback pumped between events — rumble on the universal datagram plane, DualSense
/// LED/trigger feedback on the HID-output plane. The gamepads are created and torn down with
/// the session; the pointer/keyboard injector (and its portal grant) lives in the service,
/// across sessions.
///
/// Rumble is emitted as self-terminating 0xCA v3 envelopes
/// (`[level][seq][ttl_ms][trigger levels]`): the host owns the timeline, renewing an active level
/// every ~`RUMBLE_TTL_MS × 3/10` ms and letting an abandoned one expire client-side, so "stuck
/// rumble" is inexpressible on the wire (see `punktfunk-planning/design/rumble-envelope-plan.md`
/// and `design/trigger-rumble-plane.md`). The four motors share one `seq` and one TTL, so the
/// trigger pair inherits the whole envelope apparatus unchanged.
/// `PUNKTFUNK_RUMBLE_ENVELOPE=0` reverts to legacy v1 level datagrams + the flat 500 ms refresh
/// (bisect hatch — which drops trigger rumble with it, see [`send_rumble`]).
pub(super) fn input_thread(
    rx: std::sync::mpsc::Receiver<ClientInput>,
    conn: quinn::Connection,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    gamepad: GamepadPref,
    pad_audio_on: bool,
    // The session's LIVE grant mask (per-client access §5.4). The datagram dispatch already
    // drops non-granted traffic before it reaches this channel; the guards below are the
    // deny-at-SETUP layer — without `GRANT_GAMEPAD` no arm that could create a virtual pad (or
    // spawn a pad-audio streamer) ever runs, so a View-only session holds no uinput/ViGEm node
    // a dispatch-filter bug could drive. One relaxed load per item.
    grants: Arc<AtomicU32>,
) {
    let mut pads = Pads::new(gamepad);
    // Per-pad 0xD1 audio streamers, live only when the Welcome granted the cap (`pad_audio_on`
    // — read back off the negotiated host_caps). Spawned on DualSense-family arrivals that
    // declare renderer bits, reaped on remove/teardown below.
    let mut pad_streams = PadAudioSlots::new();
    // Motion-cadence observability, PER PAD and always on — see `motion_cadence`. Summarized at
    // `info` when this session ends, which is when a field report is being written.
    let mut motion_cadence = super::motion_cadence::MotionCadence::new();
    let mut pad_state = [PadState::default(); MAX_WIRE_PADS];
    let mut pad_mask = 0u16;
    // Last applied snapshot seq per pad (`None` until the first one): the reorder gate for
    // `InputKind::GamepadState` — a late datagram with an older seq must not roll held state back.
    let mut pad_seq: [Option<u8>; MAX_WIRE_PADS] = [None; MAX_WIRE_PADS];
    // Rumble self-terminating envelopes (0xCA v3). Each non-zero level is authorized for
    // `rumble_ttl_ms`; the host renews an active pad every `rumble_renew` and lets an abandoned
    // one expire on the client, so a dropped transition heals on the next renewal and a stop that
    // is lost heals via the stop burst (or the client's own TTL expiry). `rumble_seq` is the
    // per-pad wrapping reorder counter (bumped on changes AND renewals) the client gates on;
    // `rumble_stop_burst` counts the post-stop zero re-sends still owed. `PUNKTFUNK_RUMBLE_ENVELOPE=0`
    // reverts to legacy v1 datagrams re-sent flat every 500 ms.
    //
    // `rumble_state` holds ALL FOUR levels (see `RumbleLevels`), and every "is this pad silent?"
    // test below is an all-four-zero test for one specific reason: a trigger-only rumble — the
    // normal shape of impulse-trigger content, since racing titles drive the triggers continuously
    // against near-silent handles — would otherwise be stamped `ttl = 0`, which the client reads as
    // an instantly-expired lease. That is trigger rumble that never plays, with no error anywhere.
    let mut rumble_state = [(0u16, 0u16, 0u16, 0u16); MAX_WIRE_PADS];
    let mut rumble_seen = [false; MAX_WIRE_PADS];
    let mut rumble_seq = [0u8; MAX_WIRE_PADS];
    let mut rumble_stop_burst = [0u8; MAX_WIRE_PADS];
    let mut last_refresh = std::time::Instant::now();
    let rumble_envelope_on = std::env::var("PUNKTFUNK_RUMBLE_ENVELOPE").as_deref() != Ok("0");
    let rumble_ttl_ms: u16 = std::env::var("PUNKTFUNK_RUMBLE_TTL_MS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .map(|v| v.clamp(RUMBLE_TTL_FLOOR_MS, RUMBLE_TTL_CEIL_MS))
        .unwrap_or(RUMBLE_TTL_MS);
    // Renew at 30 % of the TTL (≈120 ms for the 400 ms default) so 2–3 renewals cover the lease;
    // in legacy mode the periodic block instead runs the old flat 500 ms full-state refresh.
    let rumble_refresh_interval = if rumble_envelope_on {
        std::time::Duration::from_millis((rumble_ttl_ms as u64 * 3 / 10).max(RUMBLE_RENEW_FLOOR_MS))
    } else {
        std::time::Duration::from_millis(500)
    };
    // Pointer buttons / keys the client currently holds down. The injector is host-lifetime, so a
    // press left dangling by an abrupt client disconnect stays latched in the compositor across the
    // reconnect (Mutter keeps the implicit pointer grab of the still-pressed button — a stuck
    // left-button-down then turns every later click into a drag: windows move, but clicking buttons
    // and text inputs does nothing). We synthesize the matching up-events when this session ends —
    // see the release loop after the `break`.
    // Sets (not Vecs) so the presence test is O(1), not O(n) per event, and bounded by `MAX_HELD`
    // so a client flooding distinct never-released codes can't grow the tracking state or spike the
    // input thread (security-review 2026-06-28 S3). A real keyboard+mouse holds far fewer at once;
    // codes past the cap simply aren't tracked for end-of-session release (worst case: one unreleased
    // key on a pathological disconnect, which the injector's own state still bounds).
    const MAX_HELD: usize = 256;
    let mut held_buttons: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut held_keys: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut pen = PenSession::new();
    loop {
        // While a pen is in range/touching, wake at least every 100 ms so the stroke failsafe
        // (PenSession::check_timeout) has real granularity against its 200 ms deadline.
        let poll = if pen.active() {
            pads.feedback_poll_interval()
                .min(std::time::Duration::from_millis(100))
        } else {
            pads.feedback_poll_interval()
        };
        match rx.recv_timeout(poll) {
            // Rich input (touchpad / motion) is applied the moment it arrives; the single channel
            // wakes for gyro samples instead of making them wait out the feedback poll interval.
            // Guarded on the pad grant like every gamepad arm below — see the `grants` parameter.
            Ok(ClientInput::Rich(rich))
                if grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_GAMEPAD != 0 =>
            {
                // Per-pad inter-arrival, unconditionally: one subtraction and one array increment,
                // cheap enough that a session no longer has to be re-run with debug logging on to
                // answer "is the gyro feed even arriving evenly". The old instrument grew and
                // sorted a Vec, which is why it had to be gated — and it shared ONE accumulator
                // across pads, so two motion pads measured each other.
                if let punktfunk_core::quic::RichInput::Motion { pad, .. } = rich {
                    motion_cadence.record(pad, std::time::Instant::now());
                }
                pads.apply_rich(rich);
            }
            // Stylus batches apply on arrival like rich input — the tracker synthesizes the
            // transitions, the lazily-created virtual tablet renders them. The pen plane is
            // pointer-class (core `classify`), and the guard is also its deny-at-setup: a
            // session that never passes it never creates the virtual tablet.
            Ok(ClientInput::Pen(batch))
                if grants.load(Ordering::Relaxed) & punktfunk_core::quic::GRANT_POINTER != 0 =>
            {
                pen.apply(&batch)
            }
            // Per-event grant test, the same one the datagram dispatch already ran (one relaxed
            // load + the exhaustive `classify`): kept here too so the resource-creating arms
            // below (virtual pads, pad-audio streamers) are unreachable without their grant even
            // if an upstream filter regresses.
            Ok(ClientInput::Event(ev))
                if grants.load(Ordering::Relaxed)
                    & punktfunk_core::quic::classify(ev.kind).bit()
                    != 0 =>
            {
                match ev.kind {
                    InputKind::GamepadButton | InputKind::GamepadAxis => {
                        // A bad index / unknown axis just doesn't update a pad — fall through (no
                        // `continue`) so the rich-input drain + feedback pump below still run every
                        // iteration (the DualSense GET_REPORT handshake must be serviced promptly).
                        let idx = ev.flags as usize;
                        if idx < MAX_WIRE_PADS && pad_state[idx].apply(&ev) {
                            pad_mask |= 1 << idx;
                            let frame = pad_state[idx].frame(idx, pad_mask);
                            pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                        }
                    }
                    InputKind::GamepadState => {
                        // Idempotent full-state snapshot from a capable client (see
                        // `GamepadSnapshot`): applied only when its seq supersedes the last one, so
                        // a datagram the network reordered can't roll held state backwards. The
                        // client refreshes touched pads every ~100 ms, so an unchanged refresh is
                        // the common case — skip the frame emit then (an XInput packet-number bump
                        // for identical state is pure churn), but always advance the gate.
                        use punktfunk_core::input::GamepadSnapshot;
                        if let Some(snap) = GamepadSnapshot::from_event(&ev) {
                            let idx = snap.pad as usize;
                            if idx < MAX_WIRE_PADS
                                && GamepadSnapshot::seq_newer(snap.seq, pad_seq[idx])
                            {
                                pad_seq[idx] = Some(snap.seq);
                                let before = pad_state[idx];
                                pad_state[idx].set_snapshot(&snap);
                                let first = pad_mask & (1 << idx) == 0;
                                if first || pad_state[idx] != before {
                                    pad_mask |= 1 << idx;
                                    let frame = pad_state[idx].frame(idx, pad_mask);
                                    pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                                }
                            }
                        }
                    }
                    InputKind::GamepadRemove => {
                        // Mid-session hot-unplug from a snapshot-capable client (the native plane's
                        // `activeGamepadMask` equivalent). Seq-gated in the SAME per-pad sequence
                        // space as snapshots, so a snapshot the network reordered past this removal
                        // is dropped (older seq) and can't resurrect the pad — while a later re-plug
                        // on the same index arrives with a still-newer seq and is accepted. Clearing
                        // the `active_mask` bit and re-emitting the frame fires every backend's
                        // unplug sweep (`inject/*/gamepad.rs`), tearing down just this pad's device.
                        let (pad, seq) = punktfunk_core::input::decode_gamepad_remove(ev.flags);
                        let idx = pad as usize;
                        if idx < MAX_WIRE_PADS
                            && punktfunk_core::input::GamepadSnapshot::seq_newer(seq, pad_seq[idx])
                        {
                            pad_seq[idx] = Some(seq);
                            if pad_mask & (1 << idx) != 0 {
                                pad_mask &= !(1 << idx);
                                pad_state[idx] = PadState::default();
                                let frame = pad_state[idx].frame(idx, pad_mask);
                                pads.handle(&punktfunk_core::input::GamepadEvent::State(frame));
                                tracing::info!(pad = idx, "gamepad unplugged (native detach)");
                            }
                            // Fresh feedback bookkeeping so a later re-plug on this index inherits no
                            // stale rumble lease (a lease still ticking would buzz the new pad).
                            //
                            // `rumble_seq` deliberately SURVIVES — do not reset it here. The client's
                            // rumble reorder gate (`client/pump/datagram_task.rs`) is per-CONNECTION
                            // and has no reset path, so restarting this counter strands every later
                            // envelope for the re-plugged pad until the host climbs back past the
                            // value the client already stored (up to 128 sends ≈ 15 s of continuous
                            // rumble, or dozens of separate rumble events). The three clears below are
                            // what actually kill a stale lease; the sibling `pad_seq` gate keeps its
                            // value across a removal for exactly the same reason (see the comment at
                            // the top of this arm).
                            clear_pad_feedback(
                                &mut rumble_state[idx],
                                &mut rumble_seen[idx],
                                &mut rumble_stop_burst[idx],
                            );
                            // The unplugged pad's 0xD1 streamer goes with it (seq-gated like the
                            // rest of this arm, so a reordered stale removal can't kill the
                            // stream of a re-plugged pad). A re-plug re-arrives and re-spawns.
                            pad_streams.stop(idx);
                        }
                    }
                    InputKind::GamepadArrival => {
                        // Per-pad controller kind declaration (mixed types): route this pad's future
                        // frames to a backend of the declared kind. `code` = the GamepadPref wire
                        // byte, `flags` = pad index in the LOW BYTE — bits 8/9 carry the pad's
                        // audio-render caps (haptics/speaker) from a pad-audio-capable client, so
                        // the index MUST come from `decode_gamepad_arrival`, never the whole word.
                        // Applied before the pad's first frame (the client sends it on slot open),
                        // so the device is built as the right type from the start. The audio caps
                        // are surfaced here for the 0xD1 capture path (which emits pad audio only
                        // toward pads that declared a renderer).
                        let (pad, audio_caps) =
                            punktfunk_core::input::decode_gamepad_arrival(ev.flags);
                        let idx = pad as usize;
                        let kind = GamepadPref::from_u8(ev.code as u8);
                        if audio_caps != 0 {
                            tracing::debug!(
                                pad = idx,
                                haptics = audio_caps & 0x01 != 0,
                                speaker = audio_caps & 0x02 != 0,
                                "pad-audio render caps declared (arrival flags bits 8/9)"
                            );
                        }
                        pads.set_kind(idx, kind);
                        // Pad audio (0xD1): stream toward DualSense-family pads that declared a
                        // renderer, only on a session that negotiated the cap. Idempotent across
                        // the arrival re-sends (same kinds keeps the running streamer); a
                        // re-declare without bits — or as a kind with no pad audio — stops it.
                        if pad_audio_on {
                            let want = if matches!(
                                kind,
                                GamepadPref::DualSense | GamepadPref::DualSenseEdge
                            ) {
                                audio_caps
                            } else {
                                0
                            };
                            if want != 0 {
                                pad_streams.ensure(
                                    &conn,
                                    pad,
                                    want,
                                    matches!(kind, GamepadPref::DualSenseEdge),
                                );
                            } else {
                                pad_streams.stop(idx);
                            }
                        }
                    }
                    _ => {
                        // Track press/release so a mid-press disconnect can be undone below.
                        match ev.kind {
                            InputKind::MouseButtonDown if held_buttons.len() < MAX_HELD => {
                                held_buttons.insert(ev.code);
                            }
                            InputKind::MouseButtonUp => {
                                held_buttons.remove(&ev.code);
                            }
                            InputKind::KeyDown if held_keys.len() < MAX_HELD => {
                                held_keys.insert(ev.code);
                            }
                            InputKind::KeyUp => {
                                held_keys.remove(&ev.code);
                            }
                            _ => {}
                        }
                        // Pointer/keyboard → the host-lifetime injector service (one persistent
                        // portal session for every punktfunk/1 session). A send error only means the
                        // service thread is gone (host shutting down) — dropping the event is fine,
                        // input is lossy by design.
                        let _ = inj_tx.send(ev);
                    }
                }
            }
            // An item whose grant guard above didn't hold: dropped. Normally unreachable — the
            // datagram dispatch filters (and counts, and logs) non-granted traffic before it is
            // offered to this channel — so no second counter here; this arm only exists to make
            // the guarded arms above sound.
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Pen stroke failsafe: a silent sample stream past the deadline force-releases (see
        // PenSession::check_timeout — clients heartbeat while the pen is down).
        pen.check_timeout();
        // Service feedback every iteration (≤1 ms for Triton, ≤4 ms otherwise; games block on
        // EVIOCSFF, and HID handshakes must be answered promptly). Rumble → the universal 0xCA
        // plane; rich/raw HID feedback → 0xCD.
        pads.pump(
            |pad, low, high, lt, rt| {
                let lv: RumbleLevels = (low, high, lt, rt);
                let silent = rumble_silent(lv);
                let idx = pad as usize;
                if idx < MAX_WIRE_PADS {
                    let prev = rumble_state[idx];
                    // Log the silent→active transition (once per buzz) so a live test can tell
                    // "host never gets rumble from the game" apart from "client doesn't render it".
                    // It carries `lt`/`rt` because it is the attribution line for exactly the
                    // trigger case too — without them a "triggers never buzzed" report cannot be
                    // split into "the host never saw them" and "the client never rendered them".
                    if rumble_silent(prev) && !silent {
                        tracing::debug!(
                            pad,
                            low,
                            high,
                            lt,
                            rt,
                            "rumble: forwarding to client (0xCA)"
                        );
                    }
                    rumble_state[idx] = lv;
                    rumble_seen[idx] = true;
                    // Bump the reorder counter on every change, then arm the stop burst on a
                    // transition to zero (so a lost stop still reaches a legacy client) and clear
                    // it when the game re-asserts a non-zero level.
                    rumble_seq[idx] = rumble_seq[idx].wrapping_add(1);
                    if silent {
                        rumble_stop_burst[idx] = if !rumble_silent(prev) {
                            RUMBLE_STOP_BURST
                        } else {
                            0
                        };
                    } else {
                        rumble_stop_burst[idx] = 0;
                    }
                    // A pad with ANY of its four motors asserted gets a live lease. Testing only
                    // `(low, high)` here would stamp a trigger-only rumble `ttl = 0` — an
                    // already-expired lease the client silences on arrival.
                    let ttl = if silent { 0 } else { rumble_ttl_ms };
                    send_rumble(&conn, rumble_envelope_on, pad, lv, rumble_seq[idx], ttl);
                } else {
                    // Out-of-range pad (a backend never produces these) — forward without gating.
                    send_rumble(&conn, rumble_envelope_on, pad, lv, 0, rumble_ttl_ms);
                }
            },
            |h| {
                let _ = conn.send_datagram(h.encode().into());
            },
        );
        // Keep the virtual DualSense from going silent during steady input (no-op for X-Box): a
        // held-steady pad sends no wire events, so without a periodic re-emit the kernel/SDL drop
        // it as unplugged. The 8 ms gap inside heartbeat() governs the rate, not this ≤4 ms tick.
        pads.heartbeat();
        if last_refresh.elapsed() >= rumble_refresh_interval {
            last_refresh = std::time::Instant::now();
            if rumble_envelope_on {
                // Renewal: refresh an active pad's lease (bump seq, fresh TTL), and drain each
                // pad's post-stop zero burst, then let it go quiet — no perpetual zero refreshes.
                for i in 0..MAX_WIRE_PADS {
                    if !rumble_seen[i] {
                        continue;
                    }
                    let lv = rumble_state[i];
                    if !rumble_silent(lv) {
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(&conn, true, i as u16, lv, rumble_seq[i], rumble_ttl_ms);
                    } else if rumble_stop_burst[i] > 0 {
                        rumble_stop_burst[i] -= 1;
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(&conn, true, i as u16, (0, 0, 0, 0), rumble_seq[i], 0);
                    }
                }
            } else {
                // Legacy: re-send the current level of every seen pad every 500 ms (v1). The
                // trigger levels are dropped here by construction — v1 has no tail (see
                // `send_rumble`).
                for (i, &(low, high, _, _)) in rumble_state.iter().enumerate() {
                    if rumble_seen[i] {
                        let d = punktfunk_core::quic::encode_rumble_datagram(i as u16, low, high);
                        let _ = conn.send_datagram(d.to_vec().into());
                    }
                }
            }
        }
    }
    // Pen: lift anything still inked (buttons → tip → proximity), then the device dies with
    // this thread (VirtualPen::drop → UI_DEV_DESTROY; apps see the tablet unplug).
    pen.release_all();
    // Session ended (client gone). Release anything still held through the host-lifetime injector —
    // its EIS connection (and any implicit grab Mutter holds for our pressed button) outlives this
    // session, so without this a button pressed at disconnect stays latched and breaks clicks for
    // the next session. Mirror of the injector's own release_all, but keyed off the session, which
    // is where a client actually vanishes mid-press.
    if !held_buttons.is_empty() || !held_keys.is_empty() {
        tracing::debug!(
            buttons = held_buttons.len(),
            keys = held_keys.len(),
            "input: releasing held buttons/keys at session end"
        );
    }
    for code in held_buttons {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::MouseButtonUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    for code in held_keys {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::KeyUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    // Reap the per-pad 0xD1 streamers with the session (after the instant release sends above
    // — this can block on a quiet pad's capturer timeout, see PadAudioSlots::stop_all).
    pad_streams.stop_all();
    // One line per pad that carried motion. At `info` deliberately: the question it answers
    // ("was the gyro feed even arriving evenly?") is asked from a field log after the fact, and a
    // measurement that needs the session re-run with debug logging is a measurement nobody gets.
    motion_cadence.log_summary();
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::{InputEvent, InputKind};

    #[test]
    fn pad_snapshot_replaces_state_and_seq_gates() {
        use punktfunk_core::input::{gamepad, GamepadSnapshot};
        let mut state = PadState::default();
        let mut last_seq: Option<u8> = None;

        // Legacy accumulation first (an older client), then a snapshot replaces it wholesale.
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 200,
            y: 0,
            flags: 0,
        };
        assert!(state.apply(&axis));
        assert_eq!(state.left_trigger, 200);

        let snap = GamepadSnapshot {
            pad: 0,
            seq: 1,
            buttons: gamepad::BTN_A,
            left_trigger: 255,
            right_trigger: 0,
            ls_x: 100,
            ls_y: -100,
            rs_x: 0,
            rs_y: 0,
        };
        assert!(GamepadSnapshot::seq_newer(snap.seq, last_seq));
        last_seq = Some(snap.seq);
        state.set_snapshot(&snap);
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.buttons, gamepad::BTN_A);
        assert_eq!((state.ls_x, state.ls_y), (100, -100));

        // A reordered (stale) snapshot must not roll the trigger back.
        let stale = GamepadSnapshot {
            seq: 0,
            left_trigger: 10,
            ..snap
        };
        assert!(!GamepadSnapshot::seq_newer(stale.seq, last_seq));

        // The unchanged-refresh case the input thread skips the frame emit for: identical
        // payload with a newer seq compares equal after apply.
        let refresh = GamepadSnapshot { seq: 2, ..snap };
        assert!(GamepadSnapshot::seq_newer(refresh.seq, last_seq));
        let before = state;
        state.set_snapshot(&refresh);
        assert_eq!(state, before);

        // The snapshot survives the wire roundtrip into the same PadState shape.
        let dec =
            GamepadSnapshot::from_event(&InputEvent::decode(&snap.to_event().encode()).unwrap())
                .unwrap();
        assert_eq!(dec, snap);
    }

    fn gp(kind: InputKind, code: u32, x: i32, pad: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: pad,
        }
    }

    /// A pad re-plug must not strand the client's rumble reorder gate.
    ///
    /// The client's `rumble_last_seq` lives for the whole QUIC connection and has no reset path
    /// (`punktfunk-core/src/client/pump/datagram_task.rs`), so this host's per-pad rumble counter
    /// has to stay monotonic across a `GamepadRemove`. Regression: the removal arm used to do
    /// `rumble_seq[idx] = 0`, which made every envelope after a re-plug fail `seq_newer` until the
    /// counter climbed back past the value the client had already stored — up to 128 sends.
    ///
    /// Drives the real wire encoder and the real gate, so it fails if either side's rule moves.
    #[test]
    fn rumble_seq_survives_a_removal_so_the_client_gate_accepts() {
        use punktfunk_core::input::GamepadSnapshot;
        use punktfunk_core::quic::{decode_rumble_envelope, encode_rumble_datagram_v2};

        // The client half: one per-pad slot, per connection, never reset.
        let deliver = |seq: u8, gate: &mut Option<u8>| {
            let d = encode_rumble_datagram_v2(0, 0x4000, 0x8000, seq, 400);
            let env = decode_rumble_envelope(&d)
                .expect("v2 envelope decodes")
                .envelope
                .expect("v2 tail present");
            if GamepadSnapshot::seq_newer(env.seq, *gate) {
                *gate = Some(env.seq);
                true
            } else {
                false
            }
        };

        // The host half: one wrapping counter, bumped on every change and every renewal.
        let mut gate: Option<u8> = None;
        let mut seq = 0u8;

        // A long rumble before the unplug pushes the client's stored seq well past zero.
        for _ in 0..100 {
            seq = seq.wrapping_add(1);
            assert!(deliver(seq, &mut gate));
        }
        assert_eq!(gate, Some(100));

        // The pad is unplugged mid-buzz: the lease is cleared, the counter is not.
        let (mut state, mut seen, mut burst) =
            ((0x1234, 0x5678, 0x9ABC, 0xDEF0), true, RUMBLE_STOP_BURST);
        clear_pad_feedback(&mut state, &mut seen, &mut burst);
        assert_eq!(
            (state, seen, burst),
            ((0, 0, 0, 0), false, 0),
            "lease not cleared"
        );

        // It returns on the same wire index and the game rumbles again: the very first envelope
        // has to reach the actuator.
        seq = seq.wrapping_add(1);
        assert!(
            deliver(seq, &mut gate),
            "first envelope after a re-plug was dropped by the client's reorder gate"
        );

        // Non-vacuity: the pre-fix behaviour (counter restarted at 0) really is rejected, and
        // stays rejected for the whole forward window — this is the bug, reproduced.
        let mut stranded = Some(100u8);
        assert!(
            (1..=100).all(|s| !deliver(s, &mut stranded)),
            "test is vacuous — a restarted counter should have been gated out"
        );
    }

    /// Incremental wire events accumulate into the full pad frame the virtual xpad applies.
    #[test]
    fn gamepad_accumulator() {
        use punktfunk_core::input::gamepad::*;
        let mut s = PadState::default();
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_LB, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LS_X, -32768, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_RT, 255, 0)));
        let f = s.frame(2, 0b0100);
        assert_eq!(f.buttons, BTN_A | BTN_LB);
        assert_eq!((f.ls_x, f.right_trigger), (-32768, 255));
        assert_eq!((f.index, f.active_mask), (2, 0b0100));

        // Release folds out; axis values clamp; unknown axis ids are rejected.
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 0, 0)));
        assert_eq!(s.frame(0, 1).buttons, BTN_LB);
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LT, 9_999, 0)));
        assert_eq!(s.left_trigger, 255);
        assert!(!s.apply(&gp(InputKind::GamepadAxis, 42, 1, 0)));
    }

    /// The single most likely way to ship trigger rumble broken (design/trigger-rumble-plane.md
    /// §5): a rumble that drives ONLY the impulse triggers must still get a live lease.
    ///
    /// The pre-existing silence test was `(low, high) == (0, 0)`, and a trigger-only level passes
    /// it. Stamped `ttl = 0`, the envelope reaches the client as an already-expired lease, which
    /// it silences on arrival — trigger rumble that never plays, with no error on either side.
    /// Drives the real predicate and the real encoder/decoder pair, so it fails if either moves.
    #[test]
    fn a_trigger_only_rumble_gets_a_live_ttl() {
        use punktfunk_core::quic::{decode_rumble_envelope, encode_rumble_datagram_v3};

        // What a racing title's impulse-trigger stream looks like: handles at rest throughout.
        let trigger_only: RumbleLevels = (0, 0, 0x8000, 0);
        assert!(
            !rumble_silent(trigger_only),
            "a trigger-only level was read as silence — the ttl=0 trap"
        );
        let ttl = if rumble_silent(trigger_only) {
            0
        } else {
            RUMBLE_TTL_MS
        };
        let d = encode_rumble_datagram_v3(0, 0, 0, 1, ttl, trigger_only.2, trigger_only.3);
        let u = decode_rumble_envelope(&d).expect("v3 envelope decodes");
        assert_eq!(
            u.envelope.expect("v3 carries the v2 tail").ttl_ms,
            RUMBLE_TTL_MS,
            "trigger-only rumble was stamped with a dead lease"
        );
        assert_eq!((u.left_trigger, u.right_trigger), (0x8000, 0));
        assert_eq!((u.low, u.high), (0, 0), "handles stay at rest");

        // The reserved stop is still expressible, and is still the ONLY thing that gets ttl = 0.
        assert!(rumble_silent((0, 0, 0, 0)));
        for lv in [
            (1, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 0, 1, 0),
            (0, 0, 0, 1),
            (0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF),
        ] {
            assert!(!rumble_silent(lv), "{lv:?} must not read as a stop");
        }
    }
}
