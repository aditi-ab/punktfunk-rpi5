// Overlay UI scale — the pure arithmetic behind `DefaultsKey.osdScale`. Sizes the streaming chrome
// (the stats HUD, the quick-action ring) over the video; `RenderScale` sizes the video itself.
//
// The stored value is a multiplier on top of the platform's own density unit (pt here, dp on
// Android, the window display scale on desktop), so it means "larger than this screen's normal UI"
// rather than an absolute pixel size. `auto` (0) defers to the device class. Physical screen inches
// deliberately play no part: UIKit exposes no diagonal, and the EDID/`xdpi` numbers other platforms
// report are wrong often enough to mis-size the living-room case the setting exists for. Kept
// dependency-free + side-effect-free so it's unit-tested (`OsdScaleTests`); the Rust twin is
// `punktfunk-core`'s `osd_scale`.

import Foundation

public enum OsdScale {
    /// The stored value meaning "derive from `DeviceClass`". Also the default.
    public static let auto: Double = 0

    /// The manual range — the same 0.5–4 the desktop client's OSD knob and `RenderScale` use.
    /// Below 0.5 the stats text stops being legible; past 4 the ring covers the game under it.
    public static let range: ClosedRange<Double> = 0.5...4.0

    /// The multipliers the picker offers, 25 % apart. Anything else in `range` is reachable by
    /// typing a percentage.
    public static let presets: [Double] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0]

    /// Picker sentinel for "Custom". Negative, so it collides with neither a real multiplier nor
    /// `auto`, and it is never stored — selecting it seeds the slider with the effective size.
    public static let customTag: Double = -1

    /// How far the viewer sits, to the extent a platform can honestly report it.
    public enum DeviceClass {
        /// iPhone, or a handheld PC — arm's length or closer.
        case handheld
        /// iPad, held or propped within reach.
        case tablet
        /// A Mac at a desk.
        case desktop
        /// Apple TV across a room. The only class whose distance breaks the arm's-length
        /// assumption baked into pt.
        case tv
    }

    /// The multiplier a class gets under `auto`. Near-field classes stay at 1.0 — pt already
    /// normalises density and the overlays are drawn for that distance. A TV sits roughly 3×
    /// further than a desk monitor, but the chrome need not grow 3×: it is read in glances, and the
    /// ring is a stick target rather than dense text. 1.75 clears the 10-foot legibility floor
    /// without walling off the game.
    public static func autoScale(for deviceClass: DeviceClass) -> Double {
        switch deviceClass {
        case .handheld, .tablet, .desktop: return 1.0
        case .tv: return 1.75
        }
    }

    /// True when the stored value asks for the class default — `auto`, or anything too broken to
    /// honour (a stale preference, a 0 written by an older build).
    public static func isAuto(_ pref: Double) -> Bool {
        !pref.isFinite || pref <= 0
    }

    /// Clamp a manual multiplier into `range`. `auto` and junk stay `auto`, so a round-trip through
    /// `UserDefaults` can't silently become 0.5.
    public static func sanitize(_ raw: Double) -> Double {
        guard !isAuto(raw) else { return auto }
        return min(max(raw, range.lowerBound), range.upperBound)
    }

    /// The multiplier to draw with: the class default under `auto`, else the clamped manual value.
    /// Always finite and at least `range.lowerBound`.
    public static func resolve(_ pref: Double, for deviceClass: DeviceClass) -> Double {
        guard !isAuto(pref) else { return autoScale(for: deviceClass) }
        return min(max(pref, range.lowerBound), range.upperBound)
    }

    /// `1.25` → `125`, for the percentage the pickers speak.
    public static func toPercent(_ scale: Double) -> Int {
        Int((sanitize(scale) * 100).rounded())
    }

    /// Parse a typed percentage. Out-of-range input clamps rather than falling back to `auto` —
    /// someone typing 500 wants the largest chrome offered, not the default.
    public static func fromPercent(_ percent: Int) -> Double {
        guard percent != 0 else { return auto }
        return min(max(Double(percent) / 100, range.lowerBound), range.upperBound)
    }

    /// Step the ring's picker ladder one `dir` from `cur`, wrapping. Automatic is rung 0, then
    /// `presets`; a value off the ladder (a typed custom entry) has no rung and snaps to `auto`
    /// on the first step.
    public static func step(_ cur: Double, dir: Int) -> Double {
        let rungs = presets.count + 1
        let at: Int
        if isAuto(cur) {
            at = 0
        } else if let i = presets.firstIndex(of: cur) {
            at = i + 1
        } else {
            // Off the ladder there is no rung to stand on: the step lands on Automatic.
            return auto
        }
        let target = ((at + dir) % rungs + rungs) % rungs
        return target == 0 ? auto : presets[target - 1]
    }

    /// Picker label: "Automatic (175%)" for `auto` on `deviceClass`, else "125%".
    public static func label(_ pref: Double, for deviceClass: DeviceClass) -> String {
        isAuto(pref)
            ? "Automatic (\(toPercent(autoScale(for: deviceClass)))%)"
            : "\(toPercent(pref))%"
    }
}
