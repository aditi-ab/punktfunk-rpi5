//! Overlay UI scale: how large the streaming chrome — stats HUD, quick-action
//! ring — draws over the video. Independent of [`render_scale`](crate::render_scale),
//! which sizes the stream itself.
//!
//! The stored preference is a multiplier, or [`AUTO`] to take the device class's
//! default. It multiplies the platform's own density unit (dp, pt, the window
//! display scale), so it expresses "bigger than this screen's normal UI", not an
//! absolute pixel size. Twin of `OsdScale.swift` and Kotlin's `OsdScale`. Pure;
//! tested here.

/// Stored preference meaning "derive from [`DeviceClass`]". Also the default.
pub const AUTO: f64 = 0.0;

/// Manual floor. Below this the stats text stops being legible at any distance.
pub const MIN_SCALE: f64 = 0.5;

/// Manual ceiling — the same 4× the desktop's `PUNKTFUNK_OSD_SCALE` has always allowed, and
/// [`render_scale`](crate::render_scale)'s. Past it the ring covers the game it sits over.
pub const MAX_SCALE: f64 = 4.0;

/// Picker stops, 25 % apart. Values off this list are reachable as a custom entry
/// anywhere in `[MIN_SCALE, MAX_SCALE]`.
pub const PRESETS: [f64; 7] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// How far the viewer sits, as far as any platform can honestly tell us. Physical
/// diagonal is not it: Android TV boxes report invented `xdpi`, and EDID sizes are
/// missing or wrong often enough that a screen-inch rule mis-sizes the very
/// living-room case it exists for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceClass {
    /// Phone or handheld PC, held at arm's length or closer.
    Handheld,
    /// Tablet, held or propped within reach.
    Tablet,
    /// Monitor at a desk — a mouse's reach away.
    Desktop,
    /// TV across a room. The only class whose viewing distance breaks the
    /// arm's-length assumption baked into dp/pt.
    Tv,
}

/// The multiplier a class gets when the preference is [`AUTO`].
///
/// Every near-field class is 1.0: dp/pt already normalise density there, and the
/// overlays are drawn for that distance. A TV sits roughly 3× further away than a
/// desk monitor, but the chrome need not grow 3× — it is read in glances, not
/// stared at, and the ring is a touch/stick target rather than dense text. 1.75
/// lands the 13 pt stats line near the 10-foot UI floor without walling off the game.
pub fn auto_scale(class: DeviceClass) -> f64 {
    match class {
        DeviceClass::Handheld | DeviceClass::Tablet | DeviceClass::Desktop => 1.0,
        DeviceClass::Tv => 1.75,
    }
}

/// True when `pref` asks for the class default — the stored [`AUTO`], or any value
/// too broken to honour (a corrupted preference file, a `0` from an older client).
pub fn is_auto(pref: f64) -> bool {
    !pref.is_finite() || pref <= 0.0
}

/// Clamp a manual multiplier into `[MIN_SCALE, MAX_SCALE]`. [`AUTO`] and junk stay
/// [`AUTO`], so a round-trip through storage cannot silently become 0.5.
pub fn sanitize(raw: f64) -> f64 {
    if is_auto(raw) {
        return AUTO;
    }
    raw.clamp(MIN_SCALE, MAX_SCALE)
}

/// The multiplier to draw with: the class default under [`AUTO`], else the clamped
/// manual value. Always finite and `>= MIN_SCALE`.
pub fn resolve(pref: f64, class: DeviceClass) -> f64 {
    if is_auto(pref) {
        auto_scale(class)
    } else {
        pref.clamp(MIN_SCALE, MAX_SCALE)
    }
}

/// Step the picker ladder one `dir` from `cur`, wrapping. Automatic is rung 0, then
/// [`PRESETS`]; a value off the ladder (a typed custom entry) has no rung and snaps
/// to Automatic on the first step.
pub fn step(cur: f64, dir: i32) -> f64 {
    let rungs = PRESETS.len() as i32 + 1;
    let at = if is_auto(cur) {
        0
    } else {
        match PRESETS.iter().position(|&p| p == cur) {
            Some(i) => i as i32 + 1,
            // Off the ladder there is no rung to stand on: the step lands on Automatic.
            None => return AUTO,
        }
    };
    let target = (at + dir).rem_euclid(rungs);
    if target == 0 {
        AUTO
    } else {
        PRESETS[(target - 1) as usize]
    }
}

/// Round-trip helpers for the percentage the pickers speak. `1.25` ↔ `125`.
pub fn to_percent(scale: f64) -> u32 {
    (sanitize(scale) * 100.0).round() as u32
}

/// Parse a typed percentage. Out-of-range input clamps rather than falling back to
/// [`AUTO`] — someone typing `500` wants the biggest chrome offered, not the default.
pub fn from_percent(percent: u32) -> f64 {
    if percent == 0 {
        return AUTO;
    }
    (f64::from(percent) / 100.0).clamp(MIN_SCALE, MAX_SCALE)
}

/// Picker label: `"Automatic (175%)"` for [`AUTO`] on `class`, else `"125%"`.
pub fn label(pref: f64, class: DeviceClass) -> String {
    if is_auto(pref) {
        format!("Automatic ({}%)", to_percent(auto_scale(class)))
    } else {
        format!("{}%", to_percent(pref))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_the_zero_sentinel_and_survives_sanitize() {
        assert!(is_auto(AUTO));
        assert!(is_auto(0.0));
        assert!(is_auto(-1.0));
        assert!(is_auto(f64::NAN));
        assert_eq!(sanitize(AUTO), AUTO);
        assert_eq!(sanitize(f64::NAN), AUTO);
    }

    #[test]
    fn manual_values_clamp_into_range() {
        assert_eq!(sanitize(0.1), MIN_SCALE);
        assert_eq!(sanitize(9.0), MAX_SCALE);
        assert_eq!(sanitize(1.25), 1.25);
    }

    #[test]
    fn only_tv_departs_from_native_size() {
        assert_eq!(auto_scale(DeviceClass::Handheld), 1.0);
        assert_eq!(auto_scale(DeviceClass::Tablet), 1.0);
        assert_eq!(auto_scale(DeviceClass::Desktop), 1.0);
        assert_eq!(auto_scale(DeviceClass::Tv), 1.75);
    }

    #[test]
    fn resolve_prefers_the_manual_value_over_the_class() {
        assert_eq!(resolve(AUTO, DeviceClass::Tv), 1.75);
        assert_eq!(resolve(1.0, DeviceClass::Tv), 1.0);
        assert_eq!(resolve(2.0, DeviceClass::Handheld), 2.0);
    }

    #[test]
    fn resolve_is_always_drawable() {
        for pref in [AUTO, f64::NAN, -5.0, 0.01, 99.0, 1.5] {
            for class in [DeviceClass::Handheld, DeviceClass::Desktop, DeviceClass::Tv] {
                let s = resolve(pref, class);
                assert!(s.is_finite(), "{pref} on {class:?} is not finite");
                assert!(
                    (MIN_SCALE..=MAX_SCALE).contains(&s),
                    "{pref} on {class:?} → {s}"
                );
            }
        }
    }

    #[test]
    fn percent_round_trips() {
        for p in PRESETS {
            assert_eq!(from_percent(to_percent(p)), p);
        }
        assert_eq!(to_percent(1.75), 175);
        assert_eq!(from_percent(125), 1.25);
    }

    #[test]
    fn typed_percent_clamps_but_zero_means_auto() {
        assert_eq!(from_percent(0), AUTO);
        assert_eq!(from_percent(5), MIN_SCALE);
        assert_eq!(from_percent(500), MAX_SCALE);
    }

    #[test]
    fn presets_are_ordered_25_apart_and_in_range() {
        for pair in PRESETS.windows(2) {
            assert_eq!(to_percent(pair[1]) - to_percent(pair[0]), 25);
        }
        assert_eq!(PRESETS[0], MIN_SCALE);
        assert!(PRESETS
            .iter()
            .all(|&p| (MIN_SCALE..=MAX_SCALE).contains(&p)));
        assert!(PRESETS.contains(&1.0));
    }

    #[test]
    fn step_walks_automatic_and_the_presets_and_wraps() {
        assert_eq!(step(1.0, 1), 1.25);
        assert_eq!(step(1.0, -1), 0.75);
        assert_eq!(step(1.0, 0), 1.0);
        assert_eq!(step(AUTO, 1), PRESETS[0]);
        assert_eq!(step(PRESETS[0], -1), AUTO);
        assert_eq!(step(*PRESETS.last().unwrap(), 1), AUTO);
        // A custom entry has no rung; the first step snaps to Automatic.
        assert_eq!(step(1.6, 1), AUTO);
        assert_eq!(step(1.6, -1), AUTO);
    }

    #[test]
    fn labels_name_the_auto_value() {
        assert_eq!(label(AUTO, DeviceClass::Tv), "Automatic (175%)");
        assert_eq!(label(AUTO, DeviceClass::Desktop), "Automatic (100%)");
        assert_eq!(label(1.25, DeviceClass::Tv), "125%");
    }
}
