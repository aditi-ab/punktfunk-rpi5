// The gamepad-driven settings screen (iOS/iPadOS/macOS/tvOS): the couch-relevant subset of SettingsView,
// restyled as a console settings page and fully navigable with a controller — up/down moves the
// focus bar, left/right steps the focused value, A cycles/toggles it, B closes. Shown from the
// gamepad home launcher (X); the touch SettingsView remains the full-fidelity editor (custom
// resolutions, the log bitrate slider, debug tools), and both write the same DefaultsKey storage,
// so values round-trip freely between the two.
//
// Rows are rebuilt from live @AppStorage on every render; the focus list dispatches adjust/
// activate back here BY ROW ID (see `adjust`/`activate`), so a stored input callback can never act
// on stale captured state. Left/right CLAMPS at a choice list's ends (the dull boundary thud tells
// the thumb it's the last option); A always cycles forward, wrapping, so every option is reachable
// with one button. Toggles read left = off, right = on — refusing a no-op with the same thud.
//
// The trailing Profiles section (design/client-settings-profiles.md §5.2a/§5.4) is the pin manager
// for this controller-first surface: a row per catalog profile opens the pin-to-hosts picker — an
// in-place swap of the row list (B peels back, the "one layer" rule GamepadAddHostView set) with
// one toggle row per saved host, writing `StoredHost.pinnedProfileIDs` via HostStore.setPinned.
// Pins are presentation only: never the host's default binding, never the profile itself —
// profiles are created and edited in the standard interface (and can't be on tvOS, whose
// per-device catalog the detail strings are honest about).

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController
#if os(iOS)
import CoreHaptics
#endif

struct GamepadSettingsView: View {
    @Environment(\.dismiss) private var dismiss
    /// The saved-host store — the pin picker writes `setPinned` through it and the profile rows
    /// count pins from its live hosts. Threaded in from GamepadHomeView like the home screen
    /// itself (ContentView owns the instance).
    @ObservedObject var store: HostStore
    @AppStorage(DefaultsKey.streamWidth) private var width = 1920
    @AppStorage(DefaultsKey.streamHeight) private var height = 1080
    @AppStorage(DefaultsKey.streamHz) private var hz = 60
    @AppStorage(DefaultsKey.compositor) private var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) private var gamepadType = 0
    @AppStorage(DefaultsKey.gamepadForwarding) private var gamepadForwarding = true
    @AppStorage(DefaultsKey.systemButtons) private var systemButtons = "auto"
    @AppStorage(DefaultsKey.guideGesture) private var guideGesture = "auto"
    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = 0
    @AppStorage(DefaultsKey.audioChannels) private var audioChannels = 2
    @AppStorage(DefaultsKey.hdrEnabled) private var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) private var enable444 = false
    @AppStorage(DefaultsKey.codec) private var codec = "auto"
    @AppStorage(DefaultsKey.micEnabled) private var micEnabled = true
    @AppStorage(DefaultsKey.echoCancel) private var echoCancel = true
    // The overlay tier's raw string (rows tag by rawValue); the absent-key default runs the
    // legacy-hudEnabled migration (same pattern as ContentView/SettingsView).
    @AppStorage(DefaultsKey.statsVerbosity) private var statsVerbosityRaw
        = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = true
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    @AppStorage(DefaultsKey.autoWake) private var autoWakeEnabled = true
    @AppStorage(DefaultsKey.presentPriority) private var presentPriority =
        SettingsOptions.presentPriorityDefault
    @AppStorage(DefaultsKey.smoothBuffer) private var smoothBuffer = 0
    #if os(macOS)
    @AppStorage(DefaultsKey.windowedSafePresent) private var windowedSafePresent = true
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.rumbleOnDevice) private var rumbleOnDevice = false
    #endif
    @ObservedObject private var gamepads = GamepadManager.shared
    /// The profile catalog (ProfileStore.shared, like every other surface that reads it) — the
    /// Profiles rows re-derive from it each render, so a rename/delete made in the standard
    /// interface shows up live.
    @ObservedObject private var profiles = ProfileStore.shared

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so more rows fit.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized generously
    #endif
    @State private var focusID: String?
    /// The pin-to-hosts picker's profile — non-nil swaps the row list for one toggle row per
    /// saved host (§5.2a); B (Menu on tvOS) peels back to the settings rows.
    @State private var pinTarget: StreamProfile?
    /// The direction of the last value step (+1 right/forward, -1 left) — picks which edge the
    /// changed value slides in from, so the animation follows the user's motion.
    @State private var lastAdjustDelta = 1

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onAdjust: { row, delta in adjust(id: row.id, by: delta) },
            onActivate: { activate(id: $0.id) },
            onBack: { back() }
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: GamepadFormMetrics.rowMaxWidth)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            Text(title)
                .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                .foregroundStyle(.white)
                .padding(.top, gamepadTitleTopPadding(compact: compact))
                .padding(.bottom, compact ? 4 : 8)
                .frame(maxWidth: .infinity)
                .overlay(alignment: .trailing) { closeButton.padding(.trailing, 20) }
                .background { GamepadTrayScrim(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text(focusedDetail)
                    .font(.geist(GamepadFormMetrics.detailFont, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.55))
                    .lineLimit(2, reservesSpace: true)
                    .animation(.smooth(duration: 0.2), value: focusID)
                GamepadHintBar(hints: hints)
            }
            // Equal distance from the left and bottom edges for the legend pill (see GamepadHomeView).
            .padding(.leading, compact ? 12 : 18)
            .padding(.trailing, 22)
            .padding(.bottom, compact ? 12 : 18)
            .padding(.top, compact ? 6 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayScrim(edge: .bottom) }
        }
        // No aurora here — the settings read as clean Liquid Glass over a quiet dark base, so the
        // glass rows are the only material on the screen.
        .background { GamepadFormBackground() }
        .onAppear {
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        .onDisappear { gamepads.stopDiscovery() }
    }

    /// Touch/click fallback for closing — the controller path is B, a hardware keyboard's Esc
    /// rides the cancel action.
    private var closeButton: some View {
        Button { dismiss() } label: {
            Image(systemName: "xmark")
                .font(.system(size: GamepadFormMetrics.closeFont, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: GamepadFormMetrics.closeSide, height: GamepadFormMetrics.closeSide)
                .glassBackground(Circle(), interactive: true)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        #if !os(tvOS)
        .keyboardShortcut(.cancelAction) // unavailable on tvOS (Menu is the cancel there)
        #endif
        .accessibilityLabel("Close settings")
    }

    /// "Settings", or "Pin “Work”" while the pin picker is up — the title is what says which
    /// layer the row list currently is.
    private var title: String {
        pinTarget.map { "Pin “\($0.name)”" } ?? "Settings"
    }

    /// The legend follows the layer: value-editing hints on the settings rows, pin/unpin on the
    /// picker — where B reads "Back" (it peels to the settings rows, GamepadAddHostView's "one
    /// layer" rule), and a hostless picker has nothing to pin, so only Back remains.
    private var hints: [GamepadHint] {
        guard pinTarget != nil else {
            // A dimmed row takes neither, so offering them would be the same lie the row itself
            // used to tell — only Done remains, and the detail line says what to turn on first.
            guard rows.first(where: { $0.id == focusID })?.enabled ?? true else {
                return [.init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done")]
            }
            return [
                .init(glyph: "arrow.left.and.right", text: "Adjust"),
                .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Change"),
                .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done"),
            ]
        }
        guard !store.hosts.isEmpty else {
            return [.init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back")]
        }
        return [
            .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Pin / Unpin"),
            .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back"),
        ]
    }

    /// B peels one layer: the pin picker back to the settings rows — focus returning to the
    /// profile row it came from — then the screen itself.
    private func back() {
        if let profile = pinTarget {
            pinTarget = nil
            focusID = "profile-\(profile.id)"
        } else {
            dismiss()
        }
    }

    // MARK: - Row rendering

    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = GamepadFormMetrics.self
        return VStack(alignment: .leading, spacing: 6) {
            if let header = row.header {
                Text(header)
                    .font(.geist(m.headerFont, .semibold, relativeTo: .caption))
                    .tracking(1.4)
                    .foregroundStyle(.white.opacity(0.45))
                    .padding(.leading, m.rowHPad)
                    .padding(.top, 14)
            }
            HStack(spacing: 14) {
                Image(systemName: row.icon)
                    .font(.system(size: m.iconFont))
                    .foregroundStyle(focused ? Color.brand : .white.opacity(0.55))
                    .frame(width: m.iconWidth)
                Text(row.label)
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(.white)
                    .lineLimit(1)
                Spacer(minLength: 12)
                HStack(spacing: 9) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: m.chevronFont, weight: .semibold))
                        .foregroundStyle(
                            .white.opacity(focused && row.adjustable && row.enabled ? 0.6 : 0))
                    // Keyed by the value so a change slides the new option in instead of
                    // hard-swapping the string — a QUIET horizontal slip following the user's
                    // motion (a right-step enters from the right), crossfading over ~14 pt.
                    // Deliberately not `.push`: that travels the whole container width, loud
                    // and visibly outside the row. The ZStack is the stable home the
                    // removed/inserted texts transition within.
                    let slide: CGFloat = lastAdjustDelta >= 0 ? 14 : -14
                    ZStack {
                        Text(row.value)
                            .font(.geist(m.valueFont, .medium, relativeTo: .callout))
                            .foregroundStyle(focused ? .white : .white.opacity(0.6))
                            .lineLimit(1)
                            .id(row.value)
                            .transition(.asymmetric(
                                insertion: .offset(x: slide).combined(with: .opacity),
                                removal: .offset(x: -slide).combined(with: .opacity)))
                    }
                    .animation(.smooth(duration: 0.22), value: row.value)
                    Image(systemName: "chevron.right")
                        .font(.system(size: m.chevronFont, weight: .semibold))
                        .foregroundStyle(
                            .white.opacity(focused && row.adjustable && row.enabled ? 0.6 : 0))
                }
            }
            // Contents only — the glass and border below stay at full strength, so a dimmed row
            // still reads as a row you can sit on (which you can: its detail is the point).
            .opacity(row.enabled ? 1 : 0.45)
            .padding(.horizontal, m.rowHPad)
            .padding(.vertical, m.rowVPad)
            // Every row is Liquid Glass; the focused one takes a brand wash and reacts to press.
            .consoleGlass(
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous),
                tint: focused ? Color.brand.opacity(0.30) : nil,
                interactive: focused)
            .overlay {
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                    .strokeBorder(.white.opacity(focused ? 0.28 : 0.06), lineWidth: 1)
            }
            .scaleEffect(focused ? 1.0 : 0.98)
            .animation(.smooth(duration: 0.18), value: focused)
        }
    }

    private var focusedDetail: String {
        rows.first { $0.id == focusID }?.detail ?? " "
    }

    // MARK: - Row model

    private struct Row: Identifiable {
        let id: String
        /// Section header drawn above this row (the first row of each group carries it).
        var header: String?
        let icon: String
        let label: String
        let value: String
        /// One-line explanation shown near the hint bar while this row is focused.
        let detail: String
        /// Whether left/right means anything here — false hides the value's chevrons (the
        /// Profiles rows navigate, and the placeholder rows do nothing at all).
        var adjustable = true
        /// Dimmed and inert when false: a row whose meaning depends on another setting that is
        /// currently off. It stays in the list and stays FOCUSABLE — its `detail` is how the
        /// user learns which switch to flip first, and a row that vanished mid-list would
        /// shift everything under the cursor. Enforced centrally in `adjust(id:by:)` /
        /// `activate(id:)`, not per closure, so no row builder can forget it.
        /// (Android's `GpRow.enabled` and `pf-console-ui`'s `RowSpec.enabled` are the twins.)
        var enabled = true
        /// Left/right step; returns whether the value actually changed (false ⇒ boundary thud).
        let adjust: (Int) -> Bool
        /// A — cycle forward (wrapping) / flip.
        let activate: () -> Void
    }

    /// Dispatch by id so the focus list's stored input callbacks always act on freshly built rows
    /// (never on state captured at wire time).
    private func adjust(id: String, by delta: Int) -> Bool {
        lastAdjustDelta = delta
        guard let row = rows.first(where: { $0.id == id }), row.enabled else { return false }
        return row.adjust(delta)
    }

    private func activate(id: String) {
        lastAdjustDelta = 1 // A always cycles forward
        guard let row = rows.first(where: { $0.id == id }), row.enabled else { return }
        row.activate()
    }

    private var rows: [Row] {
        // The pin picker replaces the whole list while it's up — same screen, one layer deeper,
        // so the focus list's controller wiring (and the tvOS focus engine) carries over as is.
        if let profile = pinTarget { return pinRows(for: profile) }
        let resolution = resolutionOptions
        let refresh = SettingsOptions.refreshRates(including: hz)
            .map { (label: "\($0) Hz", tag: $0) }
        let bitrate = SettingsOptions.bitrateOptions(current: bitrateKbps)
        let controllers = SettingsOptions.controllerOptions(gamepads)
        var list: [Row] = [
            choiceRow(
                id: "resolution", header: "Stream", icon: "aspectratio",
                label: "Resolution",
                detail: "The host creates a virtual display at exactly this size — no scaling.",
                options: resolution, current: "\(width)x\(height)"
            ) { tag in
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                width = parts[0]
                height = parts[1]
            },
            choiceRow(
                id: "refresh", icon: "gauge.with.needle", label: "Refresh rate",
                detail: "Rates this display can actually show.",
                options: refresh, current: hz
            ) { hz = $0 },
            choiceRow(
                id: "bitrate", icon: "speedometer", label: "Bitrate",
                detail: "Automatic uses the host's default (20 Mbps). "
                    + "Run a speed test from the touch UI for an informed value.",
                options: bitrate, current: bitrateKbps
            ) { bitrateKbps = $0 },
            choiceRow(
                id: "compositor", icon: "macwindow", label: "Compositor",
                detail: "Which compositor drives the virtual output — honored only if "
                    + "available on the host.",
                options: SettingsOptions.compositors, current: compositor
            ) { compositor = $0 },
            toggleRow(
                id: "autoWake", icon: "power", label: "Auto-wake on connect",
                detail: "Send Wake-on-LAN to a sleeping saved host and wait for it before "
                    + "streaming. Off connects straight through.",
                value: $autoWakeEnabled),

            choiceRow(
                id: "codec", header: "Video", icon: "film", label: "Video codec",
                detail: "A preference — the host falls back if it can't encode this one "
                    + "(10-bit and 4:4:4 are HEVC-only).",
                options: SettingsOptions.codecs, current: codec
            ) { codec = $0 },
            toggleRow(
                id: "hdr", icon: "sun.max", label: "10-bit HDR",
                detail: "HDR10 — engages when the host sends HDR content and this display "
                    + "supports it.",
                value: $hdrEnabled),
            toggleRow(
                id: "chroma", icon: "textformat", label: "Full chroma (4:4:4)",
                detail: "Sharper text and UI at more bandwidth — needs host opt-in and "
                    + "hardware decode.",
                value: $enable444),
            choiceRow(
                id: "presentPriority", icon: "rectangle.stack", label: "Prioritize",
                detail: "Lowest latency shows each frame the moment the display can take it; "
                    + "Smoothness buffers a few frames to even out network hiccups. Applies "
                    + "from the next session.",
                options: SettingsOptions.presentPriorities, current: presentPriority
            ) { presentPriority = $0 },
            choiceRow(
                id: "smoothBuffer", icon: "square.stack.3d.up", label: "Smoothness buffer",
                detail: "How many frames Smoothness holds — each adds about a refresh of "
                    + "display latency and absorbs about a refresh of jitter. Only applies "
                    + "when prioritizing smoothness.",
                options: SettingsOptions.smoothBuffers(refreshHz: hz), current: smoothBuffer
            ) { smoothBuffer = $0 },

            choiceRow(
                id: "audio", header: "Audio", icon: "speaker.wave.2", label: "Audio channels",
                detail: "The speaker layout requested from the host.",
                options: SettingsOptions.audioChannels, current: audioChannels
            ) { audioChannels = $0 },
            toggleRow(
                id: "mic", icon: "mic", label: "Microphone",
                detail: "Send this device's microphone to the host's virtual mic.",
                value: $micEnabled),
            toggleRow(
                id: "echoCancel", icon: "waveform", label: "Echo cancellation",
                detail: "Cancel the audio this device plays out of the mic signal — stops "
                    + "speaker setups feeding the game back to the host.",
                value: $echoCancel),

            toggleRow(
                id: "padForward", header: "Controller", icon: "gamecontroller",
                label: "Forward controllers",
                detail: "Send this device's controllers to the host. Turn it off when your "
                    + "controller already reaches the host another way — USB passthrough such "
                    + "as VirtualHere — so games don't see two of them.",
                value: $gamepadForwarding),
            // The four rows below only mean something while something is being forwarded, so
            // they follow the switch above — the same relationship the touch settings draw with
            // `.disabled(!effective.gamepadForwarding)`. This screen could not express it until
            // `Row.enabled` existed, so it alone left them live and steppable.
            choiceRow(
                id: "pad", icon: "gamecontroller", label: "Use controller",
                detail: "Which pad is forwarded to the host, as player 1.",
                options: controllers, current: gamepads.preferredID,
                enabled: gamepadForwarding
            ) { gamepads.preferredID = $0 },
            choiceRow(
                id: "padType", icon: "dpad", label: "Controller type",
                detail: "The virtual pad the host creates — Automatic matches this controller.",
                options: SettingsOptions.padTypes, current: gamepadType,
                enabled: gamepadForwarding
            ) { gamepadType = $0 },
            choiceRow(
                id: "systemButtons", icon: "house.circle", label: "Guide button",
                detail: "Where the guide (Xbox/PS) and share presses go while streaming — "
                    + "Automatic sends them to the host whenever this device delivers them.",
                options: SettingsOptions.systemButtons, current: systemButtons,
                enabled: gamepadForwarding
            ) { systemButtons = $0 },
            choiceRow(
                id: "guideGesture", icon: "hand.point.up.left", label: "Hold Select for guide",
                detail: "Hold Select alone to press the host's guide button — keep holding "
                    + "for a Gaming-Mode host's quick-access menu. A tap still goes through.",
                options: SettingsOptions.guideGestures, current: guideGesture,
                enabled: gamepadForwarding
            ) { guideGesture = $0 },

            choiceRow(
                id: "hud", header: "Interface", icon: "chart.bar", label: "Statistics overlay",
                detail: "How much to show while streaming — Compact is a one-line pill, "
                    + "Detailed adds the latency stage breakdown.",
                options: SettingsOptions.statsVerbosities, current: statsVerbosityRaw
            ) { statsVerbosityRaw = $0 },
            choiceRow(
                id: "hudPlacement", icon: "rectangle.inset.topright.filled", label: "Overlay position",
                detail: "Which corner the statistics overlay sits in.",
                options: SettingsOptions.hudPlacements, current: hudPlacement
            ) { hudPlacement = $0 },
            toggleRow(
                id: "library", icon: "square.grid.2x2", label: "Game library",
                detail: "Browse and launch the host's games with \(buttonName(\.buttonY, "Y")).",
                value: $libraryEnabled),
            toggleRow(
                id: "gamepadUI", icon: "hand.tap", label: "Controller-optimized UI",
                detail: "Turn off to use the touch interface even with a controller connected.",
                value: $gamepadUIEnabled),
        ]
        #if os(macOS)
        // The windowed safe-present toggle slots in after "Smoothness buffer" (staying inside
        // the Video group) — macOS only, mirroring the touch SettingsView's Presentation row
        // (the DCP swapID-panic mitigation; see DefaultsKey.windowedSafePresent).
        if let at = list.firstIndex(where: { $0.id == "smoothBuffer" }) {
            list.insert(
                toggleRow(
                    id: "windowedSafePresent", icon: "macwindow.badge.plus",
                    label: "Safe windowed presentation",
                    detail: "Windowed streams present in step with the compositor — avoids a "
                        + "macOS display-driver crash on high-refresh displays, at a small "
                        + "latency cost. Fullscreen always uses the fastest path.",
                    value: $windowedSafePresent),
                at: at + 1)
        }
        #endif
        #if os(iOS)
        // The device-rumble mirror slots in after "Controller type" (staying inside the
        // Controller group — the next row carries the "Interface" header). iPhone only in
        // practice: hidden where the device itself can't play haptics (iPad).
        if CHHapticEngine.capabilitiesForHardware().supportsHaptics,
            let at = list.firstIndex(where: { $0.id == "padType" }) {
            list.insert(
                toggleRow(
                    id: "deviceRumble", icon: "iphone.radiowaves.left.and.right",
                    label: "Rumble on this iPhone",
                    detail: "Also play player 1's rumble on the phone's own Taptic Engine — "
                        + "for clip-on pads without rumble motors.",
                    value: $rumbleOnDevice),
                at: at + 1)
        }
        #endif
        return list + profileRows
    }

    // MARK: - Profiles (§5.2a)

    /// The trailing Profiles section: one row per catalog profile, its value how many saved
    /// hosts pin it, A opening the pin-to-hosts picker. Read-only beyond that — this surface
    /// pins and unpins, but profiles are created and edited elsewhere (design §5.4), so
    /// left/right is a boundary thud, not an editor.
    private var profileRows: [Row] {
        guard !profiles.profiles.isEmpty else {
            return [Row(
                id: "noProfiles", header: "Profiles", icon: "slider.horizontal.3",
                label: "No profiles yet", value: "",
                detail: emptyCatalogDetail,
                adjustable: false,
                adjust: { _ in false }, activate: {})]
        }
        return profiles.profiles.enumerated().map { i, profile in
            let pins = store.hosts
                .filter { ($0.pinnedProfileIDs ?? []).contains(profile.id) }.count
            return Row(
                id: "profile-\(profile.id)", header: i == 0 ? "Profiles" : nil,
                icon: "slider.horizontal.3", label: profile.name,
                value: pins == 0 ? "Not pinned" : "Pinned to \(pins) host\(pins == 1 ? "" : "s")",
                detail: profileDetail,
                adjustable: false,
                adjust: { _ in false },
                activate: {
                    // Focus lands on the picker's first row — the focus list's reconcile
                    // follows this id when the row set swaps underneath it.
                    focusID = store.hosts.first.map { "pinHost-\($0.id.uuidString)" } ?? "noHosts"
                    pinTarget = profile
                })
        }
    }

    /// The pin-to-hosts picker: one toggle row per SAVED host, sharing the settings rows'
    /// toggle semantics (left = unpin, right = pin, A flips; asking for the state it's in is a
    /// boundary thud). Writes ride `HostStore.setPinned` — pin appends, unpin removes — and
    /// NEVER the host's default binding (`profileID`): a pin is presentation only (§5.2a).
    private func pinRows(for profile: StreamProfile) -> [Row] {
        guard !store.hosts.isEmpty else {
            return [Row(
                id: "noHosts", icon: "desktopcomputer", label: "No saved hosts yet",
                value: "",
                detail: "Pair with a host first, then pin this profile to it.",
                adjustable: false,
                adjust: { _ in false }, activate: {})]
        }
        return store.hosts.map { host in
            let hostID = host.id
            let pinned = (host.pinnedProfileIDs ?? []).contains(profile.id)
            return Row(
                id: "pinHost-\(hostID.uuidString)", icon: "desktopcomputer",
                label: host.displayName,
                value: pinned ? "Pinned" : "Off",
                detail: "A pinned profile appears as its own card on the host — one press "
                    + "connects with it.",
                adjust: { delta in
                    let target = delta > 0
                    guard pinned != target else { return false }
                    store.setPinned(hostID, profileID: profile.id, pinned: target)
                    return true
                },
                activate: { store.setPinned(hostID, profileID: profile.id, pinned: !pinned) })
        }
    }

    /// The profile rows' explainer. tvOS gets its own: the catalog is per-device (the App Group
    /// suite — nothing syncs it) and tvOS has no profile editor at all (§5.4), so pointing a TV
    /// user at a "standard interface" would promise profiles that can never arrive there.
    private var profileDetail: String {
        #if os(tvOS)
        return "Pin this profile to a host and it appears as its own card on the home screen — "
            + "one press connects with it."
        #else
        return "Pin this profile to a host and it appears as its own card — one press connects "
            + "with it. Profiles are created and edited in Punktfunk's standard interface."
        #endif
    }

    /// What the empty catalog's placeholder explains — again honest on tvOS, where profiles
    /// cannot be created (on the device or anywhere that would reach its per-device catalog).
    private var emptyCatalogDetail: String {
        #if os(tvOS)
        return "Profiles bundle stream settings for different uses. Creating them isn't "
            + "available on Apple TV yet."
        #else
        return "Profiles bundle stream settings for different uses. Create them in Punktfunk's "
            + "standard interface, then pin them here as one-press connect cards."
        #endif
    }

    /// Resolution choices as "WxH" tags — the current size is inserted when it's a custom mode
    /// (set via the touch settings), so cycling starts from it instead of jumping.
    private var resolutionOptions: [(label: String, tag: String)] {
        var options = SettingsOptions.resolutionModes()
            .map { (label: "\($0.name) · \($0.w) × \($0.h)", tag: "\($0.w)x\($0.h)") }
        let current = "\(width)x\(height)"
        if !options.contains(where: { $0.tag == current }) {
            options.insert((label: "Custom · \(width) × \(height)", tag: current), at: 0)
        }
        return options
    }

    /// The active controller's user-facing name for a button (for detail strings).
    private func buttonName(
        _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, _ fallback: String
    ) -> String {
        gamepads.active?.controller.extendedGamepad?[keyPath: button].localizedName ?? fallback
    }

    // MARK: - Row builders

    private func choiceRow<T: Equatable>(
        id: String, header: String? = nil, icon: String, label: String, detail: String,
        options: [(label: String, tag: T)], current: T, enabled: Bool = true,
        write: @escaping (T) -> Void
    ) -> Row {
        let index = options.firstIndex { $0.tag == current }
        return Row(
            id: id, header: header, icon: icon, label: label,
            value: index.map { options[$0].label } ?? "—",
            detail: detail,
            enabled: enabled,
            adjust: { delta in
                // Unknown current value: snap to the first option on any step.
                guard let index else {
                    guard let first = options.first else { return false }
                    write(first.tag)
                    return true
                }
                let target = index + delta
                guard target >= 0, target < options.count else { return false }
                write(options[target].tag)
                return true
            },
            activate: {
                guard let index else { return write(options.first?.tag ?? current) }
                write(options[(index + 1) % options.count].tag)
            })
    }

    private func toggleRow(
        id: String, header: String? = nil, icon: String, label: String, detail: String,
        value: Binding<Bool>, enabled: Bool = true
    ) -> Row {
        Row(
            id: id, header: header, icon: icon, label: label,
            value: value.wrappedValue ? "On" : "Off",
            detail: detail,
            enabled: enabled,
            adjust: { delta in
                // Directional semantics: left = off, right = on; a no-op reads as a boundary.
                let target = delta > 0
                guard value.wrappedValue != target else { return false }
                value.wrappedValue = target
                return true
            },
            activate: { value.wrappedValue.toggle() })
    }
}

#endif
