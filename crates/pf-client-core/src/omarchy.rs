//! The desktop's Omarchy theme, for every surface this client draws.
//!
//! Omarchy keeps the active theme at `~/.local/state/omarchy/current/theme/`, and two files
//! there can tell us its colours:
//!
//! - `punktfunk.json` — our own template, rendered by Omarchy on each `omarchy-theme-set` when
//!   `punktfunk-omarchy setup` (host package) registered it. Exact, but host-side opt-in.
//! - `colors.toml` — the theme's native semantic palette, present on EVERY Omarchy box. This is
//!   what makes a client-only install follow the theme with zero setup.
//!
//! Deliberately a FILE read and not an integration: there is no Omarchy API to call, and a box
//! that is not Omarchy simply has no directory — every failure here is "no theme", never an
//! error. The web console reads the same rendered template (`web/server/util/omarchyTheme.ts`);
//! the JSON rules here are that file's, ported.
//!
//! Consumers: the GTK shell (`clients/linux/src/omarchy.rs` turns this into libadwaita
//! `@define-color`s) and the console shell (`pf-console-ui` builds its field and ink from it).
//! The colour type and maths are platform-neutral so a later "follow OS" on another platform
//! can feed the same shape; only the file reading is Linux.

/// An sRGB colour, components 0..1.
///
/// Parsed into numbers rather than passed around as strings for two reasons: every derived
/// surface is a mix of two of them, and a value that survived [`parse_hex`] cannot smuggle
/// syntax into a stylesheet.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub fn hex(self) -> String {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", c(self.0), c(self.1), c(self.2))
    }

    /// Straight sRGB lerp — `t` is how far toward `other`. At the few-percent mixes the
    /// consumers use, oklab would be indistinguishable, and this needs no colour library.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: f64, b: f64| a + (b - a) * t;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let lin = |v: f64| {
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }
}

/// WCAG contrast ratio: 1.0 for two identical colours, 21.0 for black on white.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (a.luminance(), b.luminance());
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`. Alpha is parsed and dropped — every colour here
/// names a solid surface.
///
/// Hex only, deliberately. The theme files carry hex in practice, the consumers need the
/// numbers anyway, and **an unrendered template still holds its literal `{{ accent }}`** —
/// which is not a colour, and this is what refuses it.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let w = match h.len() {
        3 | 4 => 1,
        6 | 8 => 2,
        _ => return None,
    };
    let chan = |i: usize| -> Option<f64> {
        let raw = u8::from_str_radix(h.get(i * w..i * w + w)?, 16).ok()?;
        // A single nibble is the byte repeated: `f` is `ff`, not `f0`.
        Some(f64::from(raw) * if w == 1 { 17.0 } else { 1.0 } / 255.0)
    };
    Some(Rgb(chan(0)?, chan(1)?, chan(2)?))
}

/// The theme's accent, nudged toward the foreground until it reads as **text** on the theme's
/// background. 3:1 is WCAG's floor for large text and non-text, which is what an accent is
/// used for — labels, outlines, a focus dot. ⚠ Not every theme can reach it (an accent already
/// equal to the foreground has nowhere to move), so this returns the best it managed.
pub fn readable(accent: Rgb, bg: Rgb, fg: Rgb) -> Rgb {
    let mut c = accent;
    for _ in 0..10 {
        if contrast(c, bg) >= 3.0 {
            break;
        }
        c = c.mix(fg, 0.15);
    }
    c
}

/// Ink drawn ON the accent (a filled button, a selected pill): whichever pole reads better.
pub fn on_accent(accent: Rgb) -> Rgb {
    let (black, white) = (Rgb(0.0, 0.0, 0.0), Rgb(1.0, 1.0, 1.0));
    if contrast(accent, white) >= contrast(accent, black) {
        white
    } else {
        black
    }
}

/// The desktop's theme, in the four values every consumer acts on.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsTheme {
    pub dark: bool,
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
}

/// Parse our rendered template (`punktfunk.json`).
///
/// All three colours are required together: the template renders them as a set, so a file
/// missing one is a file we do not understand, and half a palette reads worse than none. An
/// unparsable `mode` is treated as `dark`, matching the web console.
pub fn parse_punktfunk_json(raw: &str) -> Option<OsTheme> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let colour = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .and_then(parse_hex)
    };
    Some(OsTheme {
        dark: v.get("mode").and_then(serde_json::Value::as_str) != Some("light"),
        bg: colour("background")?,
        fg: colour("foreground")?,
        accent: colour("accent")?,
    })
}

/// Parse the theme's native `colors.toml` — flat `key = "value"` lines, nothing nested, so a
/// line scan is the whole parser and no toml dependency enters the graph. Only the four
/// semantic keys are read; the ANSI ramp is the terminal's business.
pub fn parse_colors_toml(raw: &str) -> Option<OsTheme> {
    let value = |key: &str| {
        raw.lines().find_map(|l| {
            let rest = l.trim().strip_prefix(key)?.trim_start();
            rest.strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .split('"')
                .next()
        })
    };
    Some(OsTheme {
        dark: value("mode") != Some("light"),
        bg: parse_hex(value("background")?)?,
        fg: parse_hex(value("foreground")?)?,
        accent: parse_hex(value("accent")?)?,
    })
}

/// Omarchy's own state directory. `XDG_STATE_HOME` first, because that is what the spec says
/// and what a non-default setup uses; `~/.local/state` is the default it falls back to.
fn omarchy_state_dir() -> Option<std::path::PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(state.join("omarchy"))
}

/// Is this box Omarchy at all? The state directory exists on every Omarchy box — Omarchy
/// itself writes indicators and rendered templates under it — and on no other. This is what
/// gates "show the Follow-system row" and what keeps every other Linux box at one `stat`.
pub fn present() -> bool {
    omarchy_state_dir().is_some_and(|d| d.is_dir())
}

/// The active theme, or `None` when this box has none — which is every box that is not
/// Omarchy. Our rendered template wins when the operator registered it (it is the same values,
/// rendered by Omarchy itself); the theme's own `colors.toml` covers the client-only box that
/// never ran the host's setup. Read per call, not cached: `omarchy-theme-set` swaps the
/// `current` symlink whenever the user changes theme.
pub fn current() -> Option<OsTheme> {
    let theme = omarchy_state_dir()?.join("current/theme");
    // A half-written file during a theme switch parses as nothing; the fallback then reads
    // the other file or reports "no theme" for a tick, both of which are correct.
    std::fs::read_to_string(theme.join("punktfunk.json"))
        .ok()
        .and_then(|raw| parse_punktfunk_json(&raw))
        .or_else(|| {
            std::fs::read_to_string(theme.join("colors.toml"))
                .ok()
                .and_then(|raw| parse_colors_toml(&raw))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(mode: &str, bg: &str, fg: &str, accent: &str) -> String {
        format!(
            r#"{{"schema":1,"mode":"{mode}","background":"{bg}","foreground":"{fg}","accent":"{accent}"}}"#
        )
    }

    #[test]
    fn an_unrendered_template_is_not_a_theme() {
        // The failure this catches for real: the template copied into place without Omarchy
        // ever rendering it. `{{ accent }}` is a string, and JSON-valid.
        assert!(
            parse_punktfunk_json(&rendered("dark", "#1a1b26", "#c0caf5", "{{ accent }}")).is_none()
        );
        assert!(
            parse_punktfunk_json(r##"{"mode":"dark","accent":"#89b4fa"}"##).is_none(),
            "a partial palette"
        );
        assert!(
            parse_punktfunk_json(r#"{"mode":"dark","acc"#).is_none(),
            "half-written during a switch"
        );
        assert!(parse_punktfunk_json(&rendered("dark", "#1a1b26", "#c0caf5", "#zzzzzz")).is_none());
    }

    #[test]
    fn hex_shorthand_and_alpha_both_parse() {
        assert_eq!(parse_hex("#fff"), Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(parse_hex("#ffffff"), Some(Rgb(1.0, 1.0, 1.0)));
        assert_eq!(
            parse_hex("#ffffffff"),
            parse_hex("#ffffff"),
            "alpha is dropped"
        );
        assert_eq!(parse_hex("#000f"), parse_hex("#000"));
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#ff"), None);
    }

    #[test]
    fn mode_defaults_to_dark_and_light_is_honoured() {
        let t = |m| parse_punktfunk_json(&rendered(m, "#1a1b26", "#c0caf5", "#7aa2f7")).unwrap();
        assert!(t("dark").dark);
        assert!(!t("light").dark);
        assert!(
            t("nonsense").dark,
            "an unparsable mode is dark, as in the console"
        );
    }

    #[test]
    fn colors_toml_reads_the_semantic_keys_and_nothing_else() {
        // Verbatim shape from a real box (2026-08-31): flat keys, quoted hex, and sibling
        // keys that PREFIX the ones we want (`dark_background`, `bright_foreground`).
        let raw = r##"
mode = "dark"

accent = "#7d82d9"
muted = "#6d7db6"

background = "#060B1E"
dark_background = "#040816"
lighter_background = "#131a3a"

foreground = "#ffcead"
bright_foreground = "#ffddcc"
"##;
        let t = parse_colors_toml(raw).unwrap();
        assert!(t.dark);
        assert_eq!(t.bg.hex(), "#060b1e");
        assert_eq!(t.fg.hex(), "#ffcead");
        assert_eq!(t.accent.hex(), "#7d82d9");
        assert!(
            parse_colors_toml("mode = \"dark\"\naccent = \"#fff\"").is_none(),
            "half a palette"
        );
    }

    #[test]
    fn a_washed_out_accent_is_lifted_until_it_reads_as_text() {
        // Everforest Light's shape: a pale accent on a pale background.
        let (bg, fg, accent) = (
            parse_hex("#fdf6e3").unwrap(),
            parse_hex("#5c6a72").unwrap(),
            parse_hex("#dfa000").unwrap(),
        );
        assert!(
            contrast(accent, bg) < 3.0,
            "the premise: unreadable as text"
        );
        assert!(contrast(readable(accent, bg, fg), bg) >= 3.0);
    }

    #[test]
    fn an_accent_that_already_reads_is_left_alone() {
        let (bg, fg, accent) = (
            parse_hex("#1a1b26").unwrap(),
            parse_hex("#c0caf5").unwrap(),
            parse_hex("#7aa2f7").unwrap(),
        );
        assert_eq!(readable(accent, bg, fg), accent);
    }

    #[test]
    fn text_on_the_accent_picks_the_readable_pole() {
        assert_eq!(
            on_accent(parse_hex("#dfa000").unwrap()),
            Rgb(0.0, 0.0, 0.0),
            "light fill"
        );
        assert_eq!(
            on_accent(parse_hex("#1a1b26").unwrap()),
            Rgb(1.0, 1.0, 1.0),
            "dark fill"
        );
    }
}
