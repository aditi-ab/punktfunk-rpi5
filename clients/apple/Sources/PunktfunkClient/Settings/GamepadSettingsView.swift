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
// The rows are split across SECTION TABS (`GpSettingsTab`) — L1/R1 on a pad, a tap elsewhere. They
// used to be one long scroll with inline group headers, which meant thumbing past Video and Audio
// to reach the controller settings; a tab is one shoulder press, and each tab remembers where its
// focus was. The tab names match the desktop console's and the Android client's, so a setting is
// found under the same word wherever you look for it.
//
// The trailing Profiles tab (design/client-settings-profiles.md §5.2a/§5.4) is the pin manager
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

/// The settings screen's sections. Order IS the strip order and the L1/R1 cycle order; the names
/// match `pf-console-ui`'s `TABS` and the Android client's `GpTab`.
enum GpSettingsTab: String, CaseIterable, Hashable {
    case stream = "Stream"
    case video = "Video"
    case audio = "Audio"
    case controller = "Controller"
    case interface = "Interface"
    case profiles = "Profiles"
    /// Trailing, like Profiles: both are built from something other than the settings store, and
    /// About is where the strip ends because it is the one section that changes nothing.
    case about = "About"
}

struct GamepadSettingsView: View {
    /// Resolved from `paletteID` below, NOT from `\.gamepadInk` — this screen publishes that value
    /// itself and so sits above its own copy (see `GamepadInk.stored`). Reading the environment
    /// here is what left the title, the tab pills and every row label white-on-pale on tvOS.
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    @Environment(\.displayBottomInset) private var displayBottomInset
    @Environment(\.dismiss) private var dismiss
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    /// The About section's link rows (never used on tvOS, which has no browser).
    @Environment(\.openURL) private var openURL
    /// The saved-host store — the pin picker writes `setPinned` through it and the profile rows
    /// count pins from its live hosts. Threaded in from GamepadHomeView like the home screen
    /// itself (ContentView owns the instance).
    @ObservedObject var store: HostStore
    /// How the in-place shell (iOS) closes this screen; nil (the macOS sheet, the tvOS cover)
    /// falls back to the environment dismiss. See `performClose`.
    var close: (() -> Void)?
    /// Whether this screen owns the controller. The shell holds it false during a push/pop (the
    /// console's input drop) and while the connect takeover is up; a system presentation never
    /// needs the gate and keeps the default.
    var controllerActive = true
    /// Whether this device has a microphone at all — passed through to the About page's shortcuts
    /// reference, which must not list a mute key on a device that can't mute anything.
    var micAvailable = true
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
    @AppStorage(DefaultsKey.audioFormat) private var audioFormat = AudioFormatChoice.opus.rawValue
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
    /// The library's arrangement (shelf/grid) — one key, two surfaces: the library's own view/sort
    /// bar writes it too, so the field and this row can never disagree.
    @AppStorage(DefaultsKey.libraryView) private var libraryViewRaw = LibraryArrangement.shelf.stored
    @AppStorage(DefaultsKey.libraryCollections) private var libraryCollections = false
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    /// When the switch above takes over — the row is only built while it is on.
    @AppStorage(DefaultsKey.gamepadUIMode) private var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    /// The gamepad UI's background colour family — the backdrop BEHIND this screen re-colours as
    /// the row steps, which is why the picker lives here and not in a sheet.
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    @AppStorage(DefaultsKey.autoWake) private var autoWakeEnabled = true
    @AppStorage(DefaultsKey.presentPriority) private var presentPriority =
        SettingsOptions.presentPriorityDefault
    @AppStorage(DefaultsKey.smoothBuffer) private var smoothBuffer = 0
    #if os(macOS)
    @AppStorage(DefaultsKey.windowedSafePresent) private var windowedSafePresent = true
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.rumbleOnDevice) private var rumbleOnDevice = false
    @AppStorage(DefaultsKey.gyroFromDevice) private var gyroFromDevice = false
    #endif
    @ObservedObject private var gamepads = GamepadManager.shared
    /// The profile catalog (ProfileStore.shared, like every other surface that reads it) — the
    /// Profiles rows re-derive from it each render, so a rename/delete made in the standard
    /// interface shows up live.
    @ObservedObject private var profiles = ProfileStore.shared

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so more rows fit.
    @Environment(\.verticalSizeClass) private var vSizeClass
    /// `.regular` only on an iPad-class window — see `showsSectionHint`.
    @Environment(\.horizontalSizeClass) private var hSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized generously
    #endif
    @State private var focusID: String?
    /// The section showing. The pin picker ignores it — that layer replaces the whole list.
    @State private var tab: GpSettingsTab = .stream
    /// Where each tab's focus was when it was last left, so a detour doesn't lose your place.
    @State private var tabFocus: [GpSettingsTab: String] = [:]
    @Namespace private var tabHighlight
    #if os(tvOS)
    /// Real focus on the strip — the tvOS route to the sections (see `tabStrip`).
    @FocusState private var focusedTab: GpSettingsTab?
    #endif
    /// The pin-to-hosts picker's profile — non-nil swaps the row list for one toggle row per
    /// saved host (§5.2a); B (Menu on tvOS) peels back to the settings rows.
    @State private var pinTarget: StreamProfile?
    /// The direction of the last value step (+1 right/forward, -1 left) — picks which edge the
    /// changed value slides in from, so the animation follows the user's motion.
    @State private var lastAdjustDelta = 1
    /// A reading surface opened from the About tab, replacing the row list the way the pin picker
    /// does. Depth is 1: neither page opens anything further.
    private enum AboutPage: Equatable {
        case shortcuts
        case licenses
    }

    @State private var aboutPage: AboutPage?

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onAdjust: { row, delta in adjust(id: row.id, by: delta) },
            onActivate: { activate(id: $0.id) },
            onBack: { back() },
            onShoulder: { step(tabBy: $0) },
            isActive: controllerActive
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: metrics.rowMaxWidth)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: gamepadHeaderSpacing(compact: compact)) {
                // Leading, like a console section heading — centred read as a floating label,
                // and a gamepad UI needs no close chrome next to it (B is the exit).
                Text(title)
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 24)
                // The picker and the About reading pages are one layer deeper — their rows aren't
                // sections of anything, so the strip would be a control that does nothing.
                if pinTarget == nil, aboutPage == nil { tabStrip }
            }
            .padding(.top, gamepadTitleTopPadding(compact: compact))
            .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
            .background { GamepadTrayBlur(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text(focusedDetail)
                    .font(.geist(metrics.detailFont, relativeTo: .caption))
                    .foregroundStyle(ink.fg(0.55))
                    .lineLimit(2, reservesSpace: true)
                    .animation(.smooth(duration: 0.2), value: focusID)
                GamepadHintBar(hints: hints)
            }
            // Equal distance from the left and bottom edges for the legend pill (see GamepadHomeView).
            .padding(.leading, compact ? 12 : 18)
            .padding(.trailing, 22)
            .padding(
                .bottom,
                gamepadLegendBottomPadding(
                    compact ? 12 : 18, tier: metrics.tier, displayBottom: displayBottomInset))
            .padding(.top, compact ? 6 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayBlur(edge: .bottom) }
        }
        // The launcher's living field, calmed (GamepadFormBackground) — the glass rows keep real
        // colour and luminance to lens without the launcher's contrast, and the palette setting
        // applies here too, so this screen previews the row you're stepping. Hosted in the
        // shell, the field is the SHELL's (one persistent backdrop, calm-chased) — mounting a
        // second would double the mesh and snap where the shell crossfades.
        .background {
            if !hostedInShell { GamepadFormBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a
        // pale palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
        .onAppear {
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        .onDisappear { gamepads.stopDiscovery() }
        #if !os(tvOS)
        // The visible close ✕ is gone (a gamepad UI exits with B) — this keeps a hardware
        // keyboard's Esc and the macOS sheet's cancel working without chrome.
        .background {
            Button("Close") { performClose() }
                .keyboardShortcut(.cancelAction)
                .buttonStyle(.plain)
                .frame(width: 0, height: 0)
                .opacity(0)
                .accessibilityHidden(true)
        }
        #endif
    }

    /// The section switcher. Horizontally scrollable so a narrow phone in landscape never has to
    /// squeeze six pills — the selected one is always scrolled into view, whether it was reached
    /// by shoulder button, tap, or (tvOS) the focus engine.
    private var tabStrip: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal) {
                HStack(spacing: 6) {
                    ForEach(GpSettingsTab.allCases, id: \.self) { t in
                        #if os(tvOS)
                        // Focusable, because L1/R1 is NOT a route here: a Siri Remote has no
                        // extended gamepad profile, so it never reaches GamepadMenuList's poll.
                        // As focusable Buttons the pills are simply above the rows, and moving
                        // focus up onto one switches section — the standard tvOS tab bar.
                        Button { select(tab: t) } label: { pill(t) }
                            .buttonStyle(ConsoleBareButtonStyle())
                            .focused($focusedTab, equals: t)
                            .id(t)
                        #else
                        pill(t)
                            .contentShape(Capsule())
                            .onTapGesture { select(tab: t) }
                            .id(t)
                        #endif
                    }
                }
                .padding(.horizontal, 24)
            }
            .scrollIndicators(.never)
            .animation(.smooth(duration: 0.22), value: tab)
            .onChange(of: tab) { _, t in
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo(t) }
            }
            #if os(tvOS)
            .onChange(of: focusedTab) { _, t in
                // Focus IS selection on a tab bar; nil means focus dropped back into the rows.
                if let t { select(tab: t) }
            }
            #endif
        }
    }

    private func pill(_ t: GpSettingsTab) -> some View {
        let selected = t == tab
        return Text(t.rawValue)
            .font(.geist(compact ? 12 : metrics.tabFont, .semibold, relativeTo: .footnote))
            // `onAccent`, not `fg` — the selected pill is FILLED with the palette accent, and
            // `onAccent` is the colour picked (by the accent's own luminance) to read on top of
            // it; its doc calls out "a filled pill's label" for exactly this surface. Using the
            // foreground meant white-on-white wherever a palette's accent is pale: Graphite's is
            // a light grey (luma ≈ 0.80), so its selected tab was unreadable.
            .foregroundStyle(selected ? ink.onAccent : ink.fg(0.55))
            // Proportional to the row metrics rather than fixed, so the strip grows with the
            // fields under it — a tab bar at phone scale above iPad-scale rows was half the
            // "does not adapt to larger screens" complaint.
            .padding(.horizontal, metrics.rowHPad * 0.8)
            .padding(.vertical, metrics.rowVPad * 0.55)
            .background {
                // One shared capsule that MOVES between pills, rather than one per pill fading
                // in and out — the highlight travels the way the press did. A Liquid Glass
                // surface (accent-tinted through consoleGlass), so the strip wears the same
                // material language as the rows it sits above.
                if selected {
                    Color.clear
                        .consoleGlass(Capsule(), tint: ink.accent(0.85))
                        .matchedGeometryEffect(id: "tab", in: tabHighlight)
                }
            }
    }

    /// Whether the legend advertises the shoulder shortcut. Held back on an iPhone, whose legend
    /// is already at its width and would push "Done" off the edge — the strip is visible and
    /// tappable there anyway. Never on tvOS: a Siri Remote has no shoulders, and its route to the
    /// sections is the focus engine (see `tabStrip`).
    private var showsSectionHint: Bool {
        #if os(tvOS)
        false
        #elseif os(iOS)
        hSizeClass == .regular
        #else
        true
        #endif
    }

    /// L1/R1 — one section along, wrapping (the strip is a ring, like A's value cycle).
    private func step(tabBy delta: Int) {
        guard pinTarget == nil else { return }
        let all = GpSettingsTab.allCases
        guard let i = all.firstIndex(of: tab) else { return }
        let n = all.count
        select(tab: all[((i + delta) % n + n) % n])
    }

    private func select(tab next: GpSettingsTab) {
        guard next != tab else { return }
        tabFocus[tab] = focusID
        // Restore where this tab was, if that row is still in it (a row can come and go with the
        // hardware it depends on); otherwise the focus list seeds its first row. Resolved against
        // `allRows` rather than `rows` so it doesn't depend on `tab`'s write being visible yet.
        let landing = tabFocus[next].flatMap { id in
            allRows.contains { $0.tab == next && $0.id == id } ? id : nil
        }
        tab = next
        focusID = landing
    }

    /// Close this screen through whichever mechanism presents it: the shell's layer pop on iOS,
    /// the environment dismiss under a macOS sheet / tvOS cover.
    private func performClose() {
        if let close { close() } else { dismiss() }
    }

    /// Where the product actually lives — kept together so the three can be checked against the
    /// README in one glance (the touch `AboutView` holds the same three).
    private enum Destination {
        static let docs = URL(string: "https://docs.punktfunk.unom.io")!
        static let community = URL(string: "https://discord.gg/kaPNvzMuGU")!
        static let source = URL(string: "https://git.unom.io/unom/punktfunk")!
    }

    /// "Version 0.29.0 (100000)" — the build number only when it says something the version does
    /// not. Mirrors `AboutView.versionLine`; a bug report is worth more with it.
    private static var versionLine: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "—"
        let build = info?["CFBundleVersion"] as? String
        guard let build, !build.isEmpty, build != short else { return "Version \(short)" }
        return "Version \(short) (\(build))"
    }

    /// "Settings", or "Pin “Work”" while the pin picker is up — the title is what says which
    /// layer the row list currently is.
    private var title: String {
        if let profile = pinTarget { return "Pin “\(profile.name)”" }
        switch aboutPage {
        case .shortcuts: return "Shortcuts"
        case .licenses: return "Acknowledgements"
        case nil: return "Settings"
        }
    }

    /// The legend follows the layer: value-editing hints on the settings rows, pin/unpin on the
    /// picker — where B reads "Back" (it peels to the settings rows, GamepadAddHostView's "one
    /// layer" rule), and a hostless picker has nothing to pin, so only Back remains.
    private var hints: [GamepadHint] {
        // A reading page is scrolled, not operated: offering A would be the same lie a dimmed row
        // used to tell. Only Back remains.
        if aboutPage != nil {
            return [.init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
                action: { back() })]
        }
        // The About rows open things rather than change them, so A reads "Open" and there is no
        // Adjust cell — left/right genuinely does nothing there.
        if pinTarget == nil, tab == .about {
            let sections: [GamepadHint] = showsSectionHint
                ? [.init(glyph: buttonGlyph(\.leftShoulder, fallback: "l1.rectangle.roundedbottom"),
                         text: "Section", action: { step(tabBy: 1) })]
                : []
            return sections + [
                .init(
                    glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Open",
                    action: { if let focusID { activate(id: focusID) } }),
                .init(
                    glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done",
                    action: { back() }),
            ]
        }
        guard pinTarget != nil else {
            // The shoulders change section, so that cell leads — where it fits and where the
            // shoulders exist at all (see `showsSectionHint`).
            let sections: [GamepadHint] = showsSectionHint
                ? [.init(glyph: buttonGlyph(\.leftShoulder, fallback: "l1.rectangle.roundedbottom"),
                         text: "Section", action: { step(tabBy: 1) })]
                : []
            // A dimmed row takes neither, so offering them would be the same lie the row itself
            // used to tell — only Done remains, and the detail line says what to turn on first.
            guard rows.first(where: { $0.id == focusID })?.enabled ?? true else {
                return sections
                    + [.init(
                        glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done",
                        action: { back() })]
            }
            return sections + [
                // The stick itself, not an action — nothing to tap (see GamepadHint.action).
                .init(glyph: "arrow.left.and.right", text: "Adjust"),
                .init(
                    glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Change",
                    action: { if let focusID { activate(id: focusID) } }),
                .init(
                    glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done",
                    action: { back() }),
            ]
        }
        guard !store.hosts.isEmpty else {
            return [.init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
                action: { back() })]
        }
        return [
            .init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Pin / Unpin",
                action: { if let focusID { activate(id: focusID) } }),
            .init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
                action: { back() }),
        ]
    }

    /// B peels one layer: the pin picker back to the settings rows — focus returning to the
    /// profile row it came from — then the screen itself.
    private func back() {
        if let profile = pinTarget {
            pinTarget = nil
            focusID = "profile-\(profile.id)"
        } else if let page = aboutPage {
            aboutPage = nil
            focusID = page == .shortcuts ? "shortcuts" : "licenses"
        } else {
            performClose()
        }
    }

    // MARK: - Row rendering

    @ViewBuilder
    private func rowView(_ row: Row, focused: Bool) -> some View {
        switch row.kind {
        case .control: controlRow(row, focused: focused)
        case .footer:
            Text(row.label)
                .font(.geist(metrics.detailFont, .medium, relativeTo: .caption))
                .monospacedDigit()
                .foregroundStyle(ink.fg(focused ? 0.7 : 0.45))
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.top, 18)
                .animation(.smooth(duration: 0.18), value: focused)
        case .heading:
            Text(row.label)
                .font(.geist(metrics.labelFont, .bold, relativeTo: .headline))
                .foregroundStyle(ink.fg(0.75))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, metrics.rowHPad)
                .padding(.top, 14)
                .padding(.bottom, 2)
        case .prose:
            // Focus here means "this is the part you are scrolled to", not "press A" — so it is a
            // quiet wash rather than the control rows' full glass.
            VStack(alignment: .leading, spacing: 4) {
                Text(row.label)
                    .font(.geistFixed(metrics.valueFont, .medium))
                    .foregroundStyle(ink.fg(0.95))
                    .fixedSize(horizontal: false, vertical: true)
                if !row.value.isEmpty {
                    Text(row.value)
                        .font(.geist(metrics.detailFont, relativeTo: .caption))
                        .foregroundStyle(ink.fg(0.6))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, metrics.rowHPad)
            .padding(.vertical, metrics.rowVPad * 0.7)
            .background {
                RoundedRectangle(cornerRadius: metrics.rowCorner, style: .continuous)
                    .fill(ink.fg(focused ? 0.08 : 0))
            }
            .animation(.smooth(duration: 0.18), value: focused)
        }
    }

    private func controlRow(_ row: Row, focused: Bool) -> some View {
        let m = metrics
        // No section header: the tab strip names the section now, and repeating it above the
        // first row of every tab was just a second label saying the same word.
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 14) {
                Image(systemName: row.icon)
                    .font(.system(size: m.iconFont))
                    .foregroundStyle(focused ? ink.accent : ink.fg(0.55))
                    .frame(width: m.iconWidth)
                Text(row.label)
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                Spacer(minLength: 12)
                HStack(spacing: 9) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: m.chevronFont, weight: .semibold))
                        .foregroundStyle(
                            ink.fg(focused && row.adjustable && row.enabled ? 0.6 : 0))
                    if let labels = row.optionLabels, let idx = row.selectedIndex {
                        // A choice row's value is a REAL band — the options ride a rotating
                        // drum, so fast repeated steps spin it instead of restarting a fade.
                        GamepadOptionBand(
                            options: labels, selection: idx, focused: focused, width: bandWidth)
                            .font(.geist(m.valueFont, .medium, relativeTo: .callout))
                            .foregroundStyle(focused ? ink.fg : ink.fg(0.6))
                    } else {
                        // The flat rows (profile pin counts, placeholders) keep the quiet slip:
                        // keyed by the value so a change slides the new string in following the
                        // user's motion, crossfading over ~14 pt. The ZStack is the stable home
                        // the removed/inserted texts transition within.
                        let slide: CGFloat = lastAdjustDelta >= 0 ? 14 : -14
                        ZStack {
                            Text(row.value)
                                .font(.geist(m.valueFont, .medium, relativeTo: .callout))
                                .foregroundStyle(focused ? ink.fg : ink.fg(0.6))
                                .lineLimit(1)
                                .id(row.value)
                                .transition(.asymmetric(
                                    insertion: .offset(x: slide).combined(with: .opacity),
                                    removal: .offset(x: -slide).combined(with: .opacity)))
                        }
                        .animation(.smooth(duration: 0.22), value: row.value)
                    }
                    Image(systemName: "chevron.right")
                        .font(.system(size: m.chevronFont, weight: .semibold))
                        .foregroundStyle(
                            ink.fg(focused && row.adjustable && row.enabled ? 0.6 : 0))
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
                tint: focused ? ink.accent(0.30) : nil,
                interactive: focused)
            .overlay {
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                    .strokeBorder(ink.fg(focused ? 0.28 : 0.06), lineWidth: 1)
            }
            .scaleEffect(focused ? 1.0 : 0.98)
            .animation(.smooth(duration: 0.18), value: focused)
        }
    }

    private var focusedDetail: String {
        rows.first { $0.id == focusID }?.detail ?? " "
    }

    /// The option band's fixed stage. A portrait phone is the one place the full 240 pt starves
    /// the row's label (everywhere else the 620 pt row cap leaves room to spare), so it alone
    /// narrows the stage.
    private var bandWidth: CGFloat {
        #if os(iOS)
        hSizeClass == .compact && vSizeClass == .regular ? 170 : metrics.bandWidth
        #else
        metrics.bandWidth
        #endif
    }

    // MARK: - Row model

    private struct Row: Identifiable {
        let id: String
        /// Which section tab this row belongs to. Every row has exactly one, and `rows` shows
        /// only the current tab's — see `allRows`.
        var tab: GpSettingsTab = .stream
        let icon: String
        let label: String
        let value: String
        /// One-line explanation shown near the hint bar while this row is focused.
        let detail: String
        /// A choice row's full option list (labels only — the tags stay inside the closures)
        /// and where its drum currently rests. nil ⇒ the value renders as plain text (toggles,
        /// actions, profiles — a two-position switch is not a drum; see GamepadOptionBand).
        var optionLabels: [String]?
        var selectedIndex: Int?
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
        /// How this row DRAWS. Every tab but About is `.control` — the glass row with a label and
        /// a value. About is a reading surface as much as a menu, so it also has a heading and a
        /// block of prose, which are rows only so the focus list can scroll them (the same trick
        /// `Licenses.chunked` plays for tvOS focus).
        var kind: Kind = .control
        /// Left/right step; returns whether the value actually changed (false ⇒ boundary thud).
        let adjust: (Int) -> Bool
        /// A — cycle forward (wrapping) / flip.
        let activate: () -> Void

        enum Kind {
            case control
            case heading
            case prose
            /// Quiet, centred trailing text — the About tab's version line.
            case footer
        }
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

    /// What the focus list actually shows: the current tab's rows — or the pin picker's, which
    /// replaces the whole list while it's up (same screen, one layer deeper, so the focus list's
    /// controller wiring and the tvOS focus engine carry over as is).
    private var rows: [Row] {
        if let profile = pinTarget { return pinRows(for: profile) }
        if let page = aboutPage {
            switch page {
            case .shortcuts: return shortcutRows
            case .licenses: return licenseRows
            }
        }
        if tab == .about { return aboutRows }
        return allRows.filter { $0.tab == tab }
    }

    // MARK: - About

    /// The About section: the ways out, plus the two reading surfaces. The identity itself (icon,
    /// name, version, tagline) is the HEADER while this tab is up — see `aboutIdentity` — not a
    /// row, so the list holds no focus stop that does nothing when pressed.
    private var aboutRows: [Row] {
        var list: [Row] = [
            aboutAction(
                id: "shortcuts", icon: "command", label: "Shortcuts", value: "While streaming",
                detail: "What to press during a session on this device — and on a controller.",
                open: .shortcuts),
            aboutAction(
                id: "licenses", icon: "text.document", label: "Acknowledgements",
                value: "MIT or Apache-2.0",
                detail: "Punktfunk's own licence and the third-party components it uses.",
                open: .licenses),
        ]
        list.append(contentsOf: [
            aboutLink(id: "docs", icon: "book", label: "Documentation", url: Destination.docs),
            aboutLink(
                id: "community", icon: "bubble.left.and.bubble.right", label: "Community",
                url: Destination.community),
            aboutLink(
                id: "source", icon: "chevron.left.forwardslash.chevron.right",
                label: "Source code", url: Destination.source),
        ])
        // The version sits UNDER the rows rather than in a header card above them. The card that
        // used to head this tab carried the app icon, and on tvOS that icon is a 400x240
        // rectangle that would not survive contact with a layout built for square art — three
        // attempts at framing it were still cropping it on the real TV. A version string answers
        // the only question anyone actually opens About to ask, and has no aspect ratio to get
        // wrong. `.footer` draws it quiet and centred, so it reads as a footer and not a row you
        // failed to press.
        list.append(Row(
            id: "version", tab: .about, icon: "", label: Self.versionLine, value: "",
            detail: "", adjustable: false, enabled: true, kind: .footer,
            adjust: { _ in false }, activate: {}))
        return list
    }

    private func aboutAction(
        id: String, icon: String, label: String, value: String, detail: String, open: AboutPage
    ) -> Row {
        Row(
            id: id, tab: .about, icon: icon, label: label, value: value, detail: detail,
            adjustable: false,
            adjust: { _ in false },
            activate: {
                // Focus lands on the page's first row — the focus list's reconcile follows this
                // id when the row set swaps underneath it (the pin picker's pattern).
                focusID = open == .shortcuts ? shortcutRows.first?.id : licenseRows.first?.id
                aboutPage = open
            })
    }

    /// tvOS has no browser and no `openURL`, so an address there is text to read off the screen
    /// rather than a link to nowhere — the same call the touch About page makes.
    private func aboutLink(id: String, icon: String, label: String, url: URL) -> Row {
        let shown = url.absoluteString.replacingOccurrences(of: "https://", with: "")
        #if os(tvOS)
        return Row(
            id: id, tab: .about, icon: icon, label: label, value: shown,
            detail: "Open this address on a phone or computer.",
            adjustable: false, adjust: { _ in false }, activate: {})
        #else
        return Row(
            id: id, tab: .about, icon: icon, label: label, value: shown,
            detail: "Opens in your browser.",
            adjustable: false, adjust: { _ in false }, activate: { openURL(url) })
        #endif
    }

    /// The shortcuts reference — the same `ShortcutsCatalog` the touch About page renders, so the
    /// two can never drift.
    private var shortcutRows: [Row] {
        ShortcutsCatalog.groups(micAvailable: micAvailable).flatMap { group -> [Row] in
            [aboutText(id: "group-\(group.title)", label: group.title, kind: .heading)]
                + group.items.map { item in
                    aboutText(
                        id: "sc-\(group.title)-\(item.keys)", label: item.keys, value: item.text,
                        kind: .prose)
                }
        }
    }

    /// The licence wall, one row per pre-chunked page (`Licenses.chunked`, which exists so tvOS
    /// can page it by focus steps) — so it scrolls with the stick and needs no machinery here.
    private var licenseRows: [Row] {
        var list: [Row] = [
            aboutText(id: "lic-heading", label: "Punktfunk", kind: .heading),
            aboutText(
                id: "lic-summary",
                label: "Punktfunk's source is open under MIT or Apache-2.0. It ships the Geist "
                    + "typeface under the SIL Open Font License 1.1, and uses the third-party "
                    + "components below, each under its own license.",
                kind: .prose),
        ]
        for (i, chunk) in Licenses.chunked(Licenses.appLicense).enumerated() {
            list.append(aboutText(id: "lic-app-\(i)", label: chunk, kind: .prose))
        }
        list.append(aboutText(
            id: "lic-third-heading", label: "Third-party software", kind: .heading))
        for (i, chunk) in Licenses.thirdPartyNoticesChunks.enumerated() {
            list.append(aboutText(id: "lic-third-\(i)", label: chunk, kind: .prose))
        }
        return list
    }

    private func aboutText(
        id: String, label: String, value: String = "", kind: Row.Kind
    ) -> Row {
        Row(
            id: id, tab: .about, icon: "", label: label, value: value, detail: "",
            adjustable: false, enabled: true, kind: kind,
            adjust: { _ in false }, activate: {})
    }

    /// Every row on the screen, tagged with its section. Built as one list (not per tab) so the
    /// platform-conditional insertions below can still place a row RELATIVE to another by id.
    private var allRows: [Row] {
        let resolution = resolutionOptions
        let refresh = SettingsOptions.refreshRates(including: hz)
            .map { (label: "\($0) Hz", tag: $0) }
        let bitrate = SettingsOptions.bitrateOptions(current: bitrateKbps)
        let controllers = SettingsOptions.controllerOptions(gamepads)
        var list: [Row] = [
            choiceRow(
                id: "resolution", tab: .stream, icon: "aspectratio",
                label: "Resolution",
                detail: "The host creates a real display at exactly this size.",
                options: resolution, current: "\(width)x\(height)"
            ) { tag in
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                width = parts[0]
                height = parts[1]
            },
            choiceRow(
                id: "refresh", tab: .stream, icon: "gauge.with.needle", label: "Refresh rate",
                detail: "Rates this display can actually show.",
                options: refresh, current: hz
            ) { hz = $0 },
            choiceRow(
                id: "bitrate", tab: .stream, icon: "speedometer", label: "Bitrate",
                detail: "Automatic uses the host's default, 20 Mbps.",
                options: bitrate, current: bitrateKbps
            ) { bitrateKbps = $0 },
            choiceRow(
                id: "compositor", tab: .stream, icon: "macwindow", label: "Compositor",
                detail: "Which compositor drives the virtual output — honored only if available.",
                options: SettingsOptions.compositors, current: compositor
            ) { compositor = $0 },
            choiceRow(
                id: "codec", tab: .video, icon: "film", label: "Video codec",
                detail: "A preference — the host falls back if it can't encode it.",
                options: SettingsOptions.codecs, current: codec
            ) { codec = $0 },
            toggleRow(
                id: "hdr", tab: .video, icon: "sun.max", label: "10-bit HDR",
                detail: "HDR10 when the host sends it and this display supports it. HEVC only.",
                value: $hdrEnabled),
            toggleRow(
                id: "chroma", tab: .video, icon: "textformat", label: "Full chroma (4:4:4)",
                detail: "Sharper text and UI, at more bandwidth. For desktop work; HEVC only.",
                value: $enable444),
            choiceRow(
                id: "presentPriority", tab: .video, icon: "rectangle.stack", label: "Prioritize",
                detail: "Lowest latency shows each frame immediately; Smoothness buffers a few.",
                options: SettingsOptions.presentPriorities, current: presentPriority
            ) { presentPriority = $0 },
            choiceRow(
                id: "smoothBuffer", tab: .video, icon: "square.stack.3d.up",
                label: "Smoothness buffer",
                detail: "Each frame held costs one refresh of latency and absorbs one of jitter.",
                options: SettingsOptions.smoothBuffers(refreshHz: hz), current: smoothBuffer
            ) { smoothBuffer = $0 },

            choiceRow(
                id: "audio", tab: .audio, icon: "speaker.wave.2", label: "Audio channels",
                detail: "The speaker layout requested from the host.",
                options: SettingsOptions.audioChannels, current: audioChannels
            ) { audioChannels = $0 },
            // No longer chained to the row above. The lossless plane was stereo-only because a
            // surround frame did not fit one datagram; the frame ladder is sized per channel
            // count, so 5.1/7.1 negotiate a SHORTER frame instead — a higher packet rate, not an
            // impossibility — and only the top of the rate ladder genuinely has nowhere to go.
            choiceRow(
                id: "audioFormat", tab: .audio, icon: "waveform.badge.magnifyingglass",
                label: "Audio quality",
                detail: "Bit-exact PCM — 2.3 Mbps at 48 kHz, up to 8.5 at 176.4. Falls back to "
                    + "Standard if the host or this device declines it.",
                options: SettingsOptions.audioFormats, current: audioFormat
            ) { audioFormat = $0 },
            toggleRow(
                id: "mic", tab: .audio, icon: "mic", label: "Microphone",
                detail: "Send this device's microphone to the host's virtual mic.",
                value: $micEnabled),
            toggleRow(
                id: "echoCancel", tab: .audio, icon: "waveform", label: "Echo cancellation",
                detail: "Filters the stream's own audio out of the mic pickup.",
                value: $echoCancel),

            toggleRow(
                id: "padForward", tab: .controller, icon: "gamecontroller",
                label: "Forward controllers",
                detail: "Sends this device's controllers to the host. Off if they already reach "
                    + "it another way.",
                value: $gamepadForwarding),
            // The four rows below only mean something while something is being forwarded, so
            // they follow the switch above — the same relationship the touch settings draw with
            // `.disabled(!effective.gamepadForwarding)`. This screen could not express it until
            // `Row.enabled` existed, so it alone left them live and steppable.
            choiceRow(
                id: "pad", tab: .controller, icon: "gamecontroller", label: "Use controller",
                detail: "Which pad is player 1. Automatic picks the newest connection.",
                options: controllers, current: gamepads.preferredID,
                enabled: gamepadForwarding
            ) { gamepads.preferredID = $0 },
            choiceRow(
                id: "padType", tab: .controller, icon: "dpad", label: "Controller type",
                detail: "The virtual pad the host creates — Automatic matches this controller.",
                options: SettingsOptions.padTypes, current: gamepadType,
                enabled: gamepadForwarding
            ) { gamepadType = $0 },
            choiceRow(
                id: "systemButtons", tab: .controller, icon: "house.circle",
                label: "Guide button",
                detail: "Where guide and share presses go while streaming.",
                options: SettingsOptions.systemButtons, current: systemButtons,
                enabled: gamepadForwarding
            ) { systemButtons = $0 },
            choiceRow(
                id: "guideGesture", tab: .controller, icon: "hand.point.up.left",
                label: "Hold Select for guide",
                detail: "Hold Select for the host's guide button; keep holding for its "
                    + "quick-access menu.",
                options: SettingsOptions.guideGestures, current: guideGesture,
                enabled: gamepadForwarding
            ) { guideGesture = $0 },

            choiceRow(
                id: "palette", tab: .interface, icon: "paintpalette", label: "Background",
                detail: "The colour family this backdrop drifts through.",
                options: GamepadPalette.all.map { (label: $0.name, tag: $0.id) },
                current: GamepadPalette.named(paletteID).id
            ) { paletteID = $0 },
            toggleRow(
                id: "autoWake", tab: .interface, icon: "power", label: "Auto-wake on connect",
                detail: "Sends Wake-on-LAN to a sleeping saved host and waits for it.",
                value: $autoWakeEnabled),
            choiceRow(
                id: "hud", tab: .interface, icon: "chart.bar", label: "Statistics overlay",
                detail: "Compact is a one-line pill; Detailed adds the latency breakdown.",
                options: SettingsOptions.statsVerbosities, current: statsVerbosityRaw
            ) { statsVerbosityRaw = $0 },
            choiceRow(
                id: "hudPlacement", tab: .interface, icon: "rectangle.inset.topright.filled",
                label: "Overlay position",
                detail: "Which corner the statistics overlay sits in.",
                options: SettingsOptions.hudPlacements, current: hudPlacement
            ) { hudPlacement = $0 },
            toggleRow(
                id: "library", tab: .interface, icon: "square.grid.2x2", label: "Game library",
                detail: "Browse and launch the host's games with \(buttonName(\.buttonY, "Y")).",
                value: $libraryEnabled),
            // The two console-parity library rows (the desktop's `library_view` and
            // `library_collections`). Inert, not hidden, while the library is off: the rows keep
            // their place so the tab doesn't reflow under a toggle.
            choiceRow(
                id: "libraryView", tab: .interface, icon: "rectangle.grid.3x2",
                label: "Library view",
                detail: "Shelf is the coverflow; Grid shows more titles at once.",
                options: LibraryArrangement.all.map { (label: $0.label, tag: $0.stored) },
                current: LibraryArrangement(stored: libraryViewRaw).stored,
                enabled: libraryEnabled
            ) { libraryViewRaw = $0 },
            toggleRow(
                id: "libraryCollections", tab: .interface, icon: "square.stack.3d.up",
                label: "Start in collections",
                detail: "Opens a library on its platform groups first; one-platform libraries "
                    + "still open on the shelf.",
                value: $libraryCollections, enabled: libraryEnabled),
            toggleRow(
                id: "gamepadUI", tab: .interface, icon: "hand.tap",
                label: "Controller-optimized UI",
                detail: "Turn off to use the touch interface even with a controller connected.",
                value: $gamepadUIEnabled),
        ]
        // WHEN the switch above takes over. Built only while it is on: with the switch off this
        // screen is unreachable in the first place (no gamepad UI to open it from), so a row
        // that decides nothing would exist purely to be found in a screenshot.
        if gamepadUIEnabled, let at = list.firstIndex(where: { $0.id == "gamepadUI" }) {
            list.insert(
                choiceRow(
                    id: "gamepadUIMode", tab: .interface, icon: "gamecontroller.circle",
                    label: "Show it",
                    detail: "Always keeps it up — otherwise touch returns when the last "
                        + "controller disconnects.",
                    options: SettingsOptions.gamepadUIModes, current: gamepadUIMode
                ) { gamepadUIMode = $0 },
                at: at + 1)
        }
        #if os(macOS)
        // The windowed safe-present toggle slots in after "Smoothness buffer" (staying inside
        // the Video tab) — macOS only, mirroring the touch SettingsView's Presentation row
        // (the DCP swapID-panic mitigation; see DefaultsKey.windowedSafePresent).
        if let at = list.firstIndex(where: { $0.id == "smoothBuffer" }) {
            list.insert(
                toggleRow(
                    id: "windowedSafePresent", tab: .video, icon: "macwindow.badge.plus",
                    label: "Safe windowed presentation",
                    detail: "Windowed streams present in step with the compositor — avoids a "
                        + "macOS display-driver crash, at a small latency cost.",
                    value: $windowedSafePresent),
                at: at + 1)
        }
        #endif
        #if os(iOS)
        // The device-rumble mirror slots in after "Controller type", inside the Controller tab.
        // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
        if CHHapticEngine.capabilitiesForHardware().supportsHaptics,
            let at = list.firstIndex(where: { $0.id == "padType" }) {
            list.insert(
                toggleRow(
                    id: "deviceRumble", tab: .controller,
                    icon: "iphone.radiowaves.left.and.right",
                    label: "Rumble on this iPhone",
                    detail: "Also plays player 1's rumble on the phone itself — for pads "
                        + "without motors.",
                    value: $rumbleOnDevice),
                at: at + 1)
        }
        // The phone-gyro mirror sits beside the rumble mirror: same clip-on-pad audience,
        // opposite data direction. Hidden where the device has no motion hardware; engages
        // in-session only while player 1's controller reports no rotation rate of its own.
        if DeviceGyro.isAvailable,
            let anchor = list.firstIndex(where: { $0.id == "deviceRumble" })
                ?? list.firstIndex(where: { $0.id == "padType" }) {
            list.insert(
                toggleRow(
                    id: "deviceGyro", tab: .controller,
                    icon: "gyroscope",
                    label: "Gyro from this device",
                    detail: "Sends this device's motion as player 1's when the controller has "
                        + "no gyro.",
                    value: $gyroFromDevice),
                at: anchor + 1)
        }
        #endif
        // The smoothness buffer only decides anything under Smoothness. Every other settings
        // surface — touch, tvOS, the GTK and WinUI shells — hides it under Lowest latency; this
        // screen alone left it live and steppable, which is a row that thuds or silently stores
        // a value nothing reads. Removed here rather than omitted from the literal above so the
        // macOS safe-present insertion can still anchor on it.
        if presentPriority != "smooth" {
            list.removeAll { $0.id == "smoothBuffer" }
        }
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
                id: "noProfiles", tab: .profiles, icon: "slider.horizontal.3",
                label: "No profiles yet", value: "",
                detail: emptyCatalogDetail,
                adjustable: false,
                adjust: { _ in false }, activate: {})]
        }
        return profiles.profiles.map { profile in
            let pins = store.hosts
                .filter { ($0.pinnedProfileIDs ?? []).contains(profile.id) }.count
            return Row(
                id: "profile-\(profile.id)", tab: .profiles,
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
                id: "noHosts", tab: .profiles, icon: "desktopcomputer",
                label: "No saved hosts yet",
                value: "",
                detail: "Pair with a host first, then pin this profile to it.",
                adjustable: false,
                adjust: { _ in false }, activate: {})]
        }
        return store.hosts.map { host in
            let hostID = host.id
            let pinned = (host.pinnedProfileIDs ?? []).contains(profile.id)
            return Row(
                id: "pinHost-\(hostID.uuidString)", tab: .profiles, icon: "desktopcomputer",
                label: host.displayName,
                value: pinned ? "Pinned" : "Off",
                detail: "A pinned profile gets its own card — one press connects with it.",
                optionLabels: ["Off", "Pinned"],
                selectedIndex: pinned ? 1 : 0,
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
        return "Pin a profile to a host and it gets its own card — one press connects."
        #else
        return "Pin a profile to a host and it gets its own card — one press connects. Create "
            + "profiles in the standard interface."
        #endif
    }

    /// What the empty catalog's placeholder explains — again honest on tvOS, where profiles
    /// cannot be created (on the device or anywhere that would reach its per-device catalog).
    private var emptyCatalogDetail: String {
        #if os(tvOS)
        return "Profiles bundle stream settings. Creating them isn't available on Apple TV yet."
        #else
        return "Profiles bundle stream settings. Create them in the standard interface, then "
            + "pin them here."
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
        id: String, tab: GpSettingsTab, icon: String, label: String, detail: String,
        options: [(label: String, tag: T)], current: T, enabled: Bool = true,
        write: @escaping (T) -> Void
    ) -> Row {
        let index = options.firstIndex { $0.tag == current }
        return Row(
            id: id, tab: tab, icon: icon, label: label,
            value: index.map { options[$0].label } ?? "—",
            detail: detail,
            // The band mounts only once the value is a known option — the "—" of an unknown
            // current renders flat, and the first step's snap-to-first seats the drum.
            optionLabels: index != nil ? options.map(\.label) : nil,
            selectedIndex: index,
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
        id: String, tab: GpSettingsTab, icon: String, label: String, detail: String,
        value: Binding<Bool>, enabled: Bool = true
    ) -> Row {
        Row(
            id: id, tab: tab, icon: icon, label: label,
            value: value.wrappedValue ? "On" : "Off",
            detail: detail,
            // Toggles ride the band too (field ask): Off sits left of On, matching the
            // directional semantics below, so a right-step slides On in from the right.
            optionLabels: ["Off", "On"],
            selectedIndex: value.wrappedValue ? 1 : 0,
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
