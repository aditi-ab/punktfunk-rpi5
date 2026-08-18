// The gamepad UI's screen-shell vocabulary (iOS): which screen sits over the launcher, and the
// console push/pop choreography that presents it. On iOS the launcher's sub-screens (settings,
// add-host, library) are NOT system covers — they are transparent layers composited in
// GamepadHomeView's ZStack over ONE persistent living backdrop, exactly the model
// `pf-console-ui`'s shell renders on the desktop clients: a push slides the incoming screen up
// out of a fade while the outgoing one recedes; a pop mirrors it; the field underneath never
// moves and never leaves. A system `fullScreenCover` — an opaque sheet sliding up from the
// bottom edge, mounting its own backdrop — was exactly the wrong grammar for a console.
// (macOS keeps its windowed sheets and tvOS its focus-engine covers; this file's motion
// constants are iOS-only in practice, but compile everywhere for the shared call sites.)

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

/// The screen the shell currently shows over the launcher. Derived, not stored: the presentation
/// triggers (`showSettings`, `showAddHost`, `libraryTarget`) stay authoritative on every
/// platform — this enum is just their iOS rendering. Depth is ≤ 1 by construction (the settings
/// pin picker is an in-screen layer, and every trigger is only reachable from the launcher), so
/// there is no stack to model.
enum GamepadScreen: Identifiable {
    case settings
    case addHost
    case hostOptions(HostOptionsTarget)
    case editHost(StoredHost)
    case pair(StoredHost)
    case library(LibraryTarget)

    var id: String {
        switch self {
        case .settings: return "settings"
        case .addHost: return "addHost"
        // Keyed on the CARD (host + pinned profile), for the same reason the library is keyed on
        // the shelf — see `HostOptionsTarget.id`.
        case .hostOptions(let target): return "hostOptions-\(target.id)"
        case .editHost(let host): return "editHost-\(host.id.uuidString)"
        case .pair(let host): return "pair-\(host.id.uuidString)"
        // Keyed on the SHELF, not the host: a host and each of its pinned cards open different
        // libraries, and sharing an id would let one stand in for another mid-transition.
        case .library(let shelf): return "library-\(shelf.id)"
        }
    }

    /// The backdrop's calm target while this screen is up: the form screens quiet the field
    /// (`Bg::Form` in the console); the library keeps the launcher's full aurora.
    var isForm: Bool {
        switch self {
        case .settings, .addHost, .hostOptions, .editHost, .pair: return true
        case .library: return false
        }
    }
}

/// The console shell's motion, mapped to SwiftUI from the numbers `ConsoleMotion` pins against
/// the shared vectors' `motion_spring` block (version 2). Source of truth:
/// `crates/pf-console-ui/src/shell.rs` (the NAV spring) and `shell/render.rs` (the geometry).
enum GamepadShellMotion {
    /// One transition, both layers — the console's `springs::NAV`. A spring rather than the old
    /// 0.26 s ease-out-cubic, so a Back pressed mid-push turns the entering screen around instead
    /// of waiting for a tween to finish (`ConsoleMotion.interruptible`).
    static let screen = Animation.spring(
        response: ConsoleMotion.response, dampingFraction: ConsoleMotion.damping)
    /// Reduce Motion: a plain crossfade on the desktop's `REDUCED_NAV` — no slide, no scale.
    static let reducedScreen = Animation.spring(
        response: ConsoleMotion.reducedResponse, dampingFraction: ConsoleMotion.reducedDamping)
    /// The backdrop's calm chase. The console runs an exponential approach (τ 0.12 s); the same
    /// ease-out at 0.30 s lands within a few percent of it and settles together with the screen.
    static let calm = Animation.timingCurve(0.33, 1, 0.68, 1, duration: 0.30)
    /// The push/pop travel — the console's `36 * k`, k-floored for a landscape phone.
    static func slide(compact: Bool) -> CGFloat {
        compact ? 27 : CGFloat(ConsoleMotion.pushSlideDp)
    }
    /// The incoming screen grows from this; the revealed launcher grows back from `underScale`.
    static let inScale = CGFloat(ConsoleMotion.enterScale)
    static let underScale = CGFloat(ConsoleMotion.exitScale)
    /// When a fresh push starts accepting input other than Back (the desktop's
    /// `NAV_INPUT_OPENS` 0.85 of the spring's travel, as a time).
    static let inputOpensAfter: TimeInterval = ConsoleMotion.inputOpensAfter
}

extension AnyTransition {
    /// The console push/pop for the top layer. Insertion: up out of a fade, growing from 0.985.
    /// Removal: down into a fade at full size (the console's pop leaves scale alone). The
    /// launcher's recede underneath is NOT a transition — it never unmounts — it is the
    /// `covered` opacity/scale in GamepadHomeView, animated in the same transaction.
    ///
    /// Known deviation from the console: a pop there re-reveals the launcher from α 0.4; a
    /// SwiftUI opacity animates from 0. Same duration, same landing — the revealed screen just
    /// reads a beat later in the fade, not worth an explicitly-driven progress machine.
    static func gamepadScreen(slide: CGFloat) -> AnyTransition {
        .asymmetric(
            insertion: .opacity
                .combined(with: .offset(y: slide))
                .combined(with: .scale(scale: GamepadShellMotion.inScale)),
            removal: .opacity.combined(with: .offset(y: slide)))
    }
}

private struct GamepadHostedInShellKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    /// True for a screen mounted as one of the shell's layers: it must NOT mount its own
    /// backdrop (the shell's single persistent field is behind everything already — a second
    /// one would double the mesh cost and break the "field never moves" illusion). The same
    /// screens presented as macOS sheets / tvOS covers read the default `false` and keep
    /// mounting their own, exactly as before.
    var gamepadHostedInShell: Bool {
        get { self[GamepadHostedInShellKey.self] }
        set { self[GamepadHostedInShellKey.self] = newValue }
    }
}

#endif
