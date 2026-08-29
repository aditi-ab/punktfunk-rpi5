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
    /// The pad's highlight: a slot 0…5, or 6 for the centre (the initial one — `Select+A`
    /// then A opens the sheet in two presses). `nil` until a pad moves it.
    @Published var highlight: Int?
    /// The sheet row the pad is on.
    @Published var sheetCursor = 0
    /// A pad press awaiting the overlay (`RingOverlay` consumes it); `navSeq` makes each an event.
    var pendingNav: RingNav?
    @Published var navSeq = 0

    func nav(_ n: RingNav) {
        pendingNav = n
        navSeq &+= 1
    }

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
        highlight = nil
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
                                   armed: s != nil && state.armed == s?.id,
                                   highlighted: state.highlight == k) {
                            if let slot, let s { fire(s, slot) }
                        }
                        .position(x: cx + ringRadius * q * cos(rad), y: cy + ringRadius * q * sin(rad))
                    }
                }
                // The centre arrives last and opens the sheet.
                let cq = min(max((shown - 6 * slotLag) / (1 - 6 * slotLag), 0), 1)
                if cq > 0 {
                    slotButton(SlotSpec(id: "more", label: "More", icon: "ellipsis"),
                               size: centreSize, scale: 0.6 + 0.4 * cq, alpha: cq, armed: false,
                               highlighted: state.highlight == 6) {
                        state.touch()
                        state.pressTick &+= 1
                        state.sheet = true
                    }
                    .position(x: cx, y: cy)
                }
                // The label under the ring: a hint, else the highlighted slot's name.
                let label: String? = state.hint ?? state.highlight.flatMap { h in
                    h == 6 ? "More" : cfg.ring[h].map { spec($0, cfg, actions).label }
                }
                if let hint = label {
                    Text(hint)
                        .font(.geist(13, .medium, relativeTo: .caption))
                        .foregroundStyle(.white.opacity(0.9))
                        .padding(.horizontal, 14).padding(.vertical, 8)
                        .glassBackground(Capsule())
                        .position(x: cx, y: cy + ringRadius + slotSize)
                }
                if state.sheet {
                    RingSheet(state: state, rows: sheetRows())
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
        .onChange(of: state.navSeq) { _, _ in
            if let n = state.pendingNav {
                state.pendingNav = nil
                handleNav(n)
            }
        }
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

    /// The pad (design §2.6): Right steps the highlight clockwise, Left anticlockwise, Up jumps
    /// to 12 o'clock, Down to 6, Y returns it to the centre; A fires the highlight (the centre
    /// opens the sheet), B closes. In the sheet, Up/Down walk the rows, Left/Right adjust one.
    private func handleNav(_ n: RingNav) {
        state.touch()
        if state.sheet {
            let rows = sheetRows()
            switch n {
            case .up: state.sheetCursor = max(state.sheetCursor - 1, 0); state.pressTick &+= 1
            case .down: state.sheetCursor = min(state.sheetCursor + 1, max(rows.count - 1, 0)); state.pressTick &+= 1
            case .left, .right:
                if let adjust = rows[safe: state.sheetCursor]?.adjust {
                    adjust(n == .left ? -1 : 1)
                    state.pressTick &+= 1
                } else {
                    state.refuseTick &+= 1
                }
            case .confirm:
                if let row = rows[safe: state.sheetCursor] {
                    if row.enabled { state.pressTick &+= 1 } else { state.refuseTick &+= 1 }
                    row.tap()
                }
            case .back: state.sheet = false; state.pressTick &+= 1
            case .centre: break
            }
            return
        }
        let h = state.highlight ?? 6
        switch n {
        case .right: state.highlight = h >= 6 ? 0 : (h + 1) % 6; state.pressTick &+= 1
        case .left: state.highlight = h >= 6 ? 5 : (h + 5) % 6; state.pressTick &+= 1
        case .up: state.highlight = 0; state.pressTick &+= 1
        case .down: state.highlight = 3; state.pressTick &+= 1
        case .centre: state.highlight = 6; state.pressTick &+= 1
        case .confirm:
            if h >= 6 {
                state.pressTick &+= 1
                state.sheetCursor = 0
                state.sheet = true
            } else if let slot = cfg.ring[h] {
                fire(spec(slot, cfg, actions), slot)
            } else {
                state.refuseTick &+= 1
            }
        case .back: state.close()
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
                            highlighted: Bool = false, action: @escaping () -> Void) -> some View {
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
            .overlay(Circle().strokeBorder(
                Color.white.opacity(highlighted ? 0.85 : (armed ? 0.6 : 0.18)),
                lineWidth: highlighted ? 2 : 1))
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

/// One row of the sheet as data, so a finger and a pad drive the same list.
private struct SheetRowSpec {
    var header: String? = nil
    var label: String
    var value = ""
    var enabled = true
    /// Left/Right on a pad: cycle a value (the resolution rows); a tap cycles forward.
    var adjust: ((Int) -> Void)? = nil
    var tap: () -> Void
}

private let resPresets: [(String, UInt32, UInt32)] = [("1440p", 2560, 1440), ("1080p", 1920, 1080), ("720p", 1280, 720)]
private let hzPresets: [UInt32] = [120, 60]

extension Collection {
    fileprivate subscript(safe i: Index) -> Element? { indices.contains(i) ? self[i] : nil }
}

extension RingOverlay {
    /// Depth two: the complete catalogue in a fixed order (D2).
    fileprivate func sheetRows() -> [SheetRowSpec] {
        let a = actions
        let mode = a.currentMode()
        let native = state.native ?? mode
        let resLabel: String = {
            if mode.w == native.w && mode.h == native.h { return "Native (\(mode.w)×\(mode.h))" }
            return resPresets.first { $0.1 == mode.w && $0.2 == mode.h }?.0 ?? "\(mode.w)×\(mode.h)"
        }()
        func adjustRes(_ dir: Int) {
            let options = [(native.w, native.h)] + resPresets.map { ($0.1, $0.2) }
            let i = options.firstIndex { $0 == (mode.w, mode.h) } ?? 0
            let n = options.count
            let next = options[((i + dir) % n + n) % n]
            a.requestMode(next.0, next.1, mode.hz)
        }
        func adjustHz(_ dir: Int) {
            var options = [native.hz]
            for hz in hzPresets where !options.contains(hz) { options.append(hz) }
            let i = options.firstIndex(of: mode.hz) ?? 0
            let n = options.count
            a.requestMode(mode.w, mode.h, options[((i + dir) % n + n) % n])
        }
        var rows: [SheetRowSpec] = []
        rows.append(SheetRowSpec(header: "Session", label: "End stream",
                                 value: state.armed == "end_stream" ? "tap again" : "") { [state] in
            if state.armed == "end_stream" { state.close(); a.endStream() } else { state.warnTick &+= 1; state.armed = "end_stream" }
        })
        rows.append(SheetRowSpec(label: "Disconnect, keep the game running") { [state] in state.close(); a.disconnectLinger() })
        rows.append(SheetRowSpec(header: "Resolution", label: "Resolution", value: resLabel, adjust: adjustRes) { adjustRes(1) })
        rows.append(SheetRowSpec(label: "Refresh", value: "\(mode.hz) Hz", adjust: adjustHz) { adjustHz(1) })
        let tm = spec(.touchMode, cfg, a)
        rows.append(SheetRowSpec(header: "Input", label: tm.label, value: tm.state) { a.cycleTouchMode() })
        rows.append(SheetRowSpec(label: "Keyboard") { [state] in state.close(); a.keyboard() })
        let pad = spec(.pad, cfg, a)
        rows.append(SheetRowSpec(label: pad.label, value: pad.reason, enabled: false) {})
        rows.append(SheetRowSpec(header: "View", label: "Statistics", value: a.stats().label) { a.cycleStats() })
        let mic = spec(.mic, cfg, a)
        rows.append(SheetRowSpec(header: "Audio", label: mic.label, value: mic.enabled ? mic.state : mic.reason,
                                 enabled: mic.enabled) { if mic.enabled { a.toggleMic() } })
        for (i, act) in a.hostActions().enumerated() {
            let id = "host:\(act.id)"
            let value = !act.available ? (act.unavailableReason ?? "") : (state.armed == id ? "tap again" : "")
            rows.append(SheetRowSpec(header: i == 0 ? "Host" : nil, label: act.label, value: value,
                                     enabled: act.available) { [state] in
                guard act.available else { state.refuseTick &+= 1; return }
                if act.danger, state.armed != id { state.warnTick &+= 1; state.armed = id } else { state.close(); a.invokeHost(act) }
            })
        }
        for (i, sc) in cfg.shortcuts.enumerated() {
            let ok = !sc.keys.isEmpty && sc.keys.allSatisfy { keyVk($0) != nil }
            rows.append(SheetRowSpec(header: i == 0 ? "Shortcuts" : nil,
                                     label: sc.label.isEmpty ? chordChip(sc.keys) : sc.label,
                                     value: chordChip(sc.keys), enabled: ok) { [state] in
                guard ok else { state.refuseTick &+= 1; return }
                state.close()
                a.sendShortcut(sc.keys)
            })
        }
        return rows
    }
}

/// The sheet as a scrollable glass panel; the pad's cursor row is tinted.
private struct RingSheet: View {
    @ObservedObject var state: RingState
    let rows: [SheetRowSpec]

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(rows.indices, id: \.self) { i in
                    let r = rows[i]
                    if let h = r.header { header(h) }
                    Button {
                        state.touch()
                        state.sheetCursor = i
                        if r.enabled { state.pressTick &+= 1 } else { state.refuseTick &+= 1 }
                        r.tap()
                    } label: {
                        HStack {
                            Text(r.label).font(.geist(15, .regular)).foregroundStyle(.white)
                            Spacer()
                            if !r.value.isEmpty {
                                Text(r.value).font(.geist(15, .regular)).foregroundStyle(.white.opacity(0.7))
                            }
                        }
                        .padding(.horizontal, 20).padding(.vertical, 12)
                        .background(state.sheetCursor == i ? Color.white.opacity(0.12) : Color.clear)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .opacity(r.enabled ? 1 : 0.45)
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
}
#endif
