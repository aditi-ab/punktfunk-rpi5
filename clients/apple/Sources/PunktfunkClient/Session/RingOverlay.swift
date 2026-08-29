// The quick-action ring (design/touch-client-overlay.md §2): six round glass buttons on a circle
// under the fingers plus a centre "More" that opens the sheet — the complete catalogue with
// values. The two-finger twist drives the opening frame by frame (`RingState.handle`); the exit
// disc opens it at the corner. Closed, it leaves the view hierarchy entirely (tenet 1: any layer
// above the stream costs a refresh of display latency on iOS).

#if os(iOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

/// The ring's open/closed state and what it is showing. One per session; the overlay reads it.
@MainActor
final class RingState: ObservableObject {
    /// 0 closed … 1 open — driven by the twist until `committed`.
    @Published var progress: CGFloat = 0
    @Published var committed = false
    @Published var clockwise = true
    /// Stream-view points; the ring is centred here, clamped so it stays on screen.
    @Published var centre = CGPoint.zero
    @Published var sheet = false
    /// A destructive slot awaiting its second press (the slot id).
    @Published var armed: String?
    /// The label under the ring: a slot's name, why it is unavailable, or "tap again".
    @Published var hint: String?
    @Published var lastTouch = Date()
    /// The mode the Welcome carried, captured at first open — the Resolution row's first chip.
    var native: (w: UInt32, h: UInt32, hz: UInt32)?

    // Haptic triggers: `.sensoryFeedback` fires on change, so each is a counter. The vocabulary
    // is one tick when the twist arms, a thump at commit, a tap per press, a firm "no" on a
    // dimmed button, and a warning when a destructive slot arms.
    @Published var armTick = 0
    @Published var commitTick = 0
    @Published var pressTick = 0
    @Published var refuseTick = 0
    @Published var warnTick = 0
    private var twistArmed = false

    var visible: Bool { committed || progress > 0 }

    func handle(_ event: DialEvent) {
        switch event {
        case .turn(let p, let cw, let at):
            guard !committed else { return }
            if !twistArmed {
                twistArmed = true
                armTick &+= 1
            }
            progress = p
            clockwise = cw
            centre = at
        case .commit:
            commit()
        case .cancel:
            // Lifted short of commit, or wound back after one: the ring winds back in.
            close()
        }
    }

    func commit() {
        committed = true
        progress = 1
        commitTick &+= 1
        touch()
    }

    func openAt(_ c: CGPoint) {
        centre = c
        commit()
    }

    func close() {
        committed = false
        progress = 0
        sheet = false
        armed = nil
        hint = nil
        twistArmed = false
    }

    func touch() { lastTouch = Date() }
}

/// What the ring can do this session — the shell's live state and commands behind each slot.
struct RingActions {
    var endStream: () -> Void
    var disconnectLinger: () -> Void
    var touchMode: () -> TouchInputMode
    var cycleTouchMode: () -> Void
    var keyboard: () -> Void
    var stats: () -> StatsVerbosity
    var cycleStats: () -> Void
    var micAvailable: () -> Bool
    var micMuted: () -> Bool
    var toggleMic: () -> Void
    var hostActions: () -> [HostAction]
    var invokeHost: (HostAction) -> Void
    var sendShortcut: ([String]) -> Void
    var currentMode: () -> (w: UInt32, h: UInt32, hz: UInt32)
    var requestMode: (UInt32, UInt32, UInt32) -> Void
}

/// One button as the ring draws it: glyph or keycap chip, its state, and why it is dimmed.
private struct SlotSpec {
    var id: String
    var label: String
    var icon: String? = nil
    var chip: String? = nil
    var enabled = true
    var reason = ""
    /// Destructive: two presses.
    var armed = false
    /// A toggle leaves the ring open so the new state is visible (D6).
    var toggle = false
    var state = ""
}

private func spec(_ slot: SlotId, _ cfg: OverlayConfig, _ a: RingActions) -> SlotSpec {
    switch slot {
    case .endStream:
        return SlotSpec(id: "end_stream", label: "End stream", icon: "xmark", armed: true)
    case .disconnectLinger:
        return SlotSpec(id: "disconnect_linger", label: "Disconnect, keep the game running",
                        icon: "rectangle.portrait.and.arrow.right")
    case .touchMode:
        let m = a.touchMode()
        let icon: String
        switch m {
        case .trackpad: icon = "hand.point.up.left"
        case .pointer: icon = "cursorarrow"
        case .touch: icon = "hand.tap"
        }
        return SlotSpec(id: "touch_mode", label: "Touch mode", icon: icon, toggle: true,
                        state: m.rawValue.capitalized)
    case .keyboard:
        return SlotSpec(id: "keyboard", label: "Keyboard", icon: "keyboard")
    case .stats:
        return SlotSpec(id: "stats", label: "Statistics", icon: "chart.bar", toggle: true,
                        state: a.stats().label)
    case .mic:
        return SlotSpec(id: "mic", label: "Microphone", icon: a.micMuted() ? "mic.slash.fill" : "mic.fill",
                        enabled: a.micAvailable(), reason: "No microphone is running this session",
                        toggle: true, state: a.micMuted() ? "Muted" : "On")
    case .pad:
        return SlotSpec(id: "pad", label: "Virtual controller", icon: "gamecontroller",
                        enabled: false, reason: "The virtual controller arrives in a later release")
    case .sendText:
        return SlotSpec(id: "send_text", label: "Send text", icon: "textformat",
                        enabled: false, reason: "Use the keyboard on this device")
    case .host(let id):
        let act = a.hostActions().first { $0.id == id }
        return SlotSpec(id: "host:\(id)", label: act?.label ?? id, icon: "power",
                        enabled: act?.available == true,
                        reason: act?.unavailableReason ?? "This host does not offer it",
                        armed: act?.danger ?? true)
    case .shortcut(let id):
        let s = cfg.shortcut(id)
        let keys = s?.keys ?? []
        let known = !keys.isEmpty && keys.allSatisfy { keyVk($0) != nil }
        return SlotSpec(id: "shortcut:\(id)", label: (s?.label.isEmpty == false ? s!.label : chordChip(keys)),
                        chip: chordChip(keys), enabled: known, reason: "A key in this chord is unknown")
    }
}

private let ringRadius: CGFloat = 120
private let slotSize: CGFloat = 56
private let centreSize: CGFloat = 64
/// Button k lags the previous one by this much of the twist, so the ring visibly unwinds.
private let slotLag: CGFloat = 0.06

/// The ring and its sheet. Sits above the stream view; a tap on the scrim outside closes it.
struct RingOverlay: View {
    @ObservedObject var state: RingState
    let cfg: OverlayConfig
    let actions: RingActions
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var shown: CGFloat = 0

    var body: some View {
        GeometryReader { geo in
            let margin = ringRadius + slotSize / 2 + 16
            let cx = min(max(state.centre.x, margin), max(geo.size.width - margin, margin))
            let cy = min(max(state.centre.y, margin), max(geo.size.height - margin, margin))
            ZStack {
                // The scrim: a tap outside closes the ring, and nothing reaches the stream
                // while it is open.
                Color.black.opacity(0.18 * shown)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        if state.sheet { state.sheet = false } else { state.close() }
                    }
                ForEach(0..<OverlayConfig.ringSlots, id: \.self) { k in
                    let q = min(max((shown - CGFloat(k) * slotLag) / (1 - 5 * slotLag), 0), 1)
                    if q > 0 {
                        // Slot k sits at 12, 2, 4… o'clock and travels out along a short
                        // spiral that turns the way the hand turns.
                        let turn: CGFloat = state.clockwise ? -40 : 40
                        let deg = -90 + 60 * CGFloat(k) + (1 - q) * turn
                        let rad = deg * .pi / 180
                        let slot = cfg.ring[k]
                        let s = slot.map { spec($0, cfg, actions) }
                        slotButton(s, size: slotSize, scale: 0.6 + 0.4 * q, alpha: q,
                                   armed: s != nil && state.armed == s?.id) {
                            if let slot, let s { fire(s, slot) }
                        }
                        .position(x: cx + ringRadius * q * cos(rad), y: cy + ringRadius * q * sin(rad))
                    }
                }
                // The centre arrives last and opens the sheet.
                let cq = min(max((shown - 6 * slotLag) / (1 - 6 * slotLag), 0), 1)
                if cq > 0 {
                    slotButton(SlotSpec(id: "more", label: "More", icon: "ellipsis"),
                               size: centreSize, scale: 0.6 + 0.4 * cq, alpha: cq, armed: false) {
                        state.touch()
                        state.pressTick &+= 1
                        state.sheet = true
                    }
                    .position(x: cx, y: cy)
                }
                if let hint = state.hint {
                    Text(hint)
                        .font(.geist(13, .medium, relativeTo: .caption))
                        .foregroundStyle(.white.opacity(0.9))
                        .padding(.horizontal, 14).padding(.vertical, 8)
                        .glassBackground(Capsule())
                        .position(x: cx, y: cy + ringRadius + slotSize)
                }
                if state.sheet {
                    RingSheet(state: state, cfg: cfg, actions: actions)
                        .frame(maxHeight: geo.size.height * 0.6)
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
                        .padding(.bottom, 16)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .environment(\.colorScheme, .dark) // reads over any frame
        }
        .ignoresSafeArea()
        .onAppear {
            sync()
            if state.native == nil { state.native = actions.currentMode() }
        }
        .onChange(of: state.progress) { _, _ in sync() }
        .onChange(of: state.committed) { _, _ in sync() }
        .animation(reduceMotion ? .easeOut(duration: 0.12) : .smooth(duration: 0.25), value: state.sheet)
        // Idle: the exit disc's 8 s rule, for the same latency reason — unless the sheet is up.
        .task(id: "\(state.lastTouch.timeIntervalSinceReferenceDate)-\(state.sheet)") {
            guard state.committed, !state.sheet else { return }
            try? await Task.sleep(for: .seconds(8))
            if !Task.isCancelled { state.close() }
        }
        // An armed slot and a hint both time out.
        .task(id: "\(state.armed ?? "")-\(state.hint ?? "")-\(state.lastTouch.timeIntervalSinceReferenceDate)") {
            guard state.armed != nil || state.hint != nil else { return }
            try? await Task.sleep(for: .seconds(2))
            if !Task.isCancelled {
                state.armed = nil
                state.hint = nil
            }
        }
        .sensoryFeedback(.selection, trigger: state.armTick)
        .sensoryFeedback(.impact(weight: .medium), trigger: state.commitTick)
        .sensoryFeedback(.impact(weight: .light), trigger: state.pressTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: state.refuseTick)
        .sensoryFeedback(.warning, trigger: state.warnTick)
    }

    /// The twist drives the opening frame by frame; the commit settles with a spring.
    private func sync() {
        if state.committed {
            withAnimation(reduceMotion ? .easeOut(duration: 0.12) : .spring(response: 0.35, dampingFraction: 0.6)) {
                shown = 1
            }
        } else {
            var t = Transaction()
            t.disablesAnimations = true
            withTransaction(t) { shown = state.progress }
        }
    }

    private func fire(_ s: SlotSpec, _ slot: SlotId) {
        state.touch()
        guard s.enabled else {
            state.refuseTick &+= 1
            state.armed = nil
            state.hint = s.reason
            return
        }
        if s.armed, state.armed != s.id {
            state.warnTick &+= 1
            state.armed = s.id
            state.hint = "\(s.label)? Tap again"
            return
        }
        state.pressTick &+= 1
        state.armed = nil
        state.hint = nil
        switch slot {
        case .endStream: state.close(); actions.endStream()
        case .disconnectLinger: state.close(); actions.disconnectLinger()
        case .touchMode: actions.cycleTouchMode()
        case .keyboard: state.close(); actions.keyboard()
        case .stats: actions.cycleStats()
        case .mic: actions.toggleMic()
        case .pad, .sendText: break
        case .host(let id):
            if let act = actions.hostActions().first(where: { $0.id == id }) {
                state.close()
                actions.invokeHost(act)
            }
        case .shortcut(let id):
            if let sc = cfg.shortcut(id) {
                state.close()
                actions.sendShortcut(sc.keys)
            }
        }
        if s.toggle {
            let after = spec(slot, cfg, actions)
            state.hint = "\(after.label): \(after.state)"
        }
    }

    /// One round glass button — the exit disc's primitive, at ring size.
    private func slotButton(_ s: SlotSpec?, size: CGFloat, scale: CGFloat, alpha: CGFloat, armed: Bool,
                            action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Group {
                if let chip = s?.chip {
                    Text(chip).font(.geistFixed(11, .semibold))
                } else {
                    Image(systemName: s?.icon ?? "circle.dashed")
                        .font(.system(size: size * 0.4, weight: .semibold))
                }
            }
            .foregroundStyle(armed ? Color.red : (s?.enabled ?? false ? Color.white : Color.white.opacity(0.35)))
            .frame(width: size, height: size)
            .glassBackground(Circle(), interactive: true)
            .overlay(Circle().strokeBorder(Color.white.opacity(armed ? 0.6 : 0.18), lineWidth: 1))
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(s == nil)
        .scaleEffect(scale)
        .opacity(alpha)
        .accessibilityLabel(s?.label ?? "Empty slot")
        .accessibilityValue(
            armed ? "armed — press again" : (s?.enabled == false ? (s?.reason ?? "") : (s?.state ?? "")))
    }
}

/// Depth two: the complete catalogue in a fixed order (D2), as a scrollable glass panel.
private struct RingSheet: View {
    @ObservedObject var state: RingState
    let cfg: OverlayConfig
    let actions: RingActions

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                header("Session")
                row("End stream", state.armed == "end_stream" ? "tap again" : "") {
                    if state.armed == "end_stream" {
                        state.close()
                        actions.endStream()
                    } else {
                        state.warnTick &+= 1
                        state.armed = "end_stream"
                    }
                }
                row("Disconnect, keep the game running") { state.close(); actions.disconnectLinger() }

                header("Resolution")
                let mode = actions.currentMode()
                let native = state.native ?? mode
                HStack(spacing: 6) {
                    chip("Native", selected: mode.w == native.w && mode.h == native.h) {
                        actions.requestMode(native.w, native.h, mode.hz)
                    }
                    ForEach([("1440p", 2560, 1440), ("1080p", 1920, 1080), ("720p", 1280, 720)], id: \.0) { p in
                        chip(p.0, selected: mode.w == UInt32(p.1) && mode.h == UInt32(p.2)) {
                            actions.requestMode(UInt32(p.1), UInt32(p.2), mode.hz)
                        }
                    }
                }
                .padding(.horizontal, 16).padding(.vertical, 4)
                HStack(spacing: 6) {
                    chip("Native", selected: mode.hz == native.hz) { actions.requestMode(mode.w, mode.h, native.hz) }
                    ForEach([120, 60], id: \.self) { hz in
                        chip("\(hz) Hz", selected: mode.hz == UInt32(hz)) { actions.requestMode(mode.w, mode.h, UInt32(hz)) }
                    }
                }
                .padding(.horizontal, 16).padding(.vertical, 4)

                header("Input")
                let tm = spec(.touchMode, cfg, actions)
                row(tm.label, tm.state) { actions.cycleTouchMode() }
                row("Keyboard") { state.close(); actions.keyboard() }
                let pad = spec(.pad, cfg, actions)
                row(pad.label, pad.reason, enabled: false) {}

                header("View")
                row("Statistics", actions.stats().label) { actions.cycleStats() }

                header("Audio")
                let mic = spec(.mic, cfg, actions)
                row(mic.label, mic.enabled ? mic.state : mic.reason, enabled: mic.enabled) {
                    if mic.enabled { actions.toggleMic() }
                }

                let hosts = actions.hostActions()
                if !hosts.isEmpty {
                    header("Host")
                    ForEach(hosts) { act in
                        let id = "host:\(act.id)"
                        row(act.label,
                            !act.available ? (act.unavailableReason ?? "") : (state.armed == id ? "tap again" : ""),
                            enabled: act.available) {
                            guard act.available else { state.refuseTick &+= 1; return }
                            if act.danger, state.armed != id {
                                state.warnTick &+= 1
                                state.armed = id
                            } else {
                                state.close()
                                actions.invokeHost(act)
                            }
                        }
                    }
                }
                if !cfg.shortcuts.isEmpty {
                    header("Shortcuts")
                    ForEach(cfg.shortcuts, id: \.id) { s in
                        let ok = !s.keys.isEmpty && s.keys.allSatisfy { keyVk($0) != nil }
                        row(s.label.isEmpty ? chordChip(s.keys) : s.label, chordChip(s.keys), enabled: ok) {
                            guard ok else { state.refuseTick &+= 1; return }
                            state.close()
                            actions.sendShortcut(s.keys)
                        }
                    }
                }
            }
            .padding(.vertical, 8)
        }
        .frame(maxWidth: 520)
        .glassBackground(RoundedRectangle(cornerRadius: 16))
        .padding(.horizontal, 16)
    }

    private func header(_ text: String) -> some View {
        Text(text)
            .font(.geist(12, .semibold, relativeTo: .caption))
            .foregroundStyle(.white.opacity(0.6))
            .padding(.horizontal, 20).padding(.top, 12).padding(.bottom, 4)
    }

    private func row(_ label: String, _ value: String = "", enabled: Bool = true,
                     action: @escaping () -> Void) -> some View {
        Button {
            state.touch()
            state.pressTick &+= 1
            action()
        } label: {
            HStack {
                Text(label).font(.geist(15, .regular)).foregroundStyle(.white)
                Spacer()
                if !value.isEmpty {
                    Text(value).font(.geist(15, .regular)).foregroundStyle(.white.opacity(0.7))
                }
            }
            .padding(.horizontal, 20).padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .opacity(enabled ? 1 : 0.45)
    }

    private func chip(_ label: String, selected: Bool, action: @escaping () -> Void) -> some View {
        Button {
            state.touch()
            state.pressTick &+= 1
            action()
        } label: {
            Text(label)
                .font(.geist(13, .medium, relativeTo: .caption))
                .foregroundStyle(.white)
                .padding(.horizontal, 12).padding(.vertical, 6)
                .background(Color.white.opacity(selected ? 0.28 : 0.10), in: Capsule())
        }
        .buttonStyle(.plain)
    }
}
#endif
