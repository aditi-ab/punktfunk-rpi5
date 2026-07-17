// Location-based modifier mapping — which Windows VK each PHYSICAL modifier position forwards to
// the host. A Mac keyboard's bottom row is `⌃ Control / ⌥ Option / ⌘ Command / space`; a Windows
// keyboard's is `Ctrl / ⊞ Super / Alt / space`. So the key NEAREST the space bar is Command on a
// Mac but Alt on Windows, and the next one out is Option on a Mac but the Windows key. A user who
// grew up on Windows reaches for Alt where the Mac has Command; this setting lets them get that
// muscle memory back WITHOUT relabelling keycaps — it remaps by physical position, not by a blunt
// "swap these two VKs" that would also drag Control/Shift or lose the left/right distinction.
//
// The model: keep the physical detection (which side, which key) exactly as the OS reports it, and
// swap only the Alt-vs-Super ROLE between the Option and Command keys, per side. So under `.windows`
// the ⌘ position emits Alt (VK_L/RMENU) and the ⌥ position emits the Windows key (VK_L/RWIN), while
// left stays left and right stays right. Control and Shift are in the same place on both keyboards,
// so they never move. Lives in PunktfunkShared because both the Settings UI (PunktfunkClient) and
// the wire-boundary remap (`InputCapture.applyModifierLayout`, PunktfunkKit) resolve it.

import Foundation

/// How the physical ⌥ Option / ⌘ Command keys map to host modifiers. The raw values are stable on
/// disk — rename the cases freely, never the strings.
public enum ModifierLayout: String, CaseIterable, Sendable {
    /// Apple positions (default): ⌥ Option → Alt, ⌘ Command → Super/Windows. The current behaviour.
    case mac
    /// Windows positions: the key nearest the space bar (⌘ Command) → Alt, the next one (⌥ Option)
    /// → the Windows/Super key. Side (left/right) is preserved.
    case windows

    /// User-facing label (Settings picker).
    public var label: String {
        switch self {
        case .mac: return "Mac (⌥ Alt · ⌘ Super)"
        case .windows: return "Windows (⌘ Alt · ⌥ Super)"
        }
    }

    /// A one-line explanation for the setting's footer.
    public var detail: String {
        switch self {
        case .mac:
            return "The ⌥ Option key sends Alt and the ⌘ Command key sends the Windows key — the Apple layout."
        case .windows:
            return "The key nearest the space bar sends Alt and the next one sends the Windows key, matching a PC keyboard. Client shortcuts (⌘⎋ and friends) still use the physical ⌘ key."
        }
    }

    /// The persisted layout (default `.mac` when unset).
    public static var current: ModifierLayout {
        guard let raw = UserDefaults.standard.string(forKey: DefaultsKey.modifierLayout) else {
            return .mac
        }
        return ModifierLayout(rawValue: raw) ?? .mac
    }
}
