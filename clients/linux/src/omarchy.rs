//! Follow the Omarchy desktop theme.
//!
//! Omarchy renders every registered `~/.config/omarchy/themed/*.tpl` from the active theme's
//! semantic `colors.toml` on each `omarchy-theme-set`, dropping the result in
//! `~/.local/state/omarchy/current/theme/`. `punktfunk-omarchy setup` registers
//! `punktfunk.json.tpl` (opt-in), so the file this module reads is the desktop's theme expressed
//! in the four values a UI can act on: `mode`, `background`, `foreground`, `accent`.
//!
//! Deliberately a FILE read and not an integration: there is no Omarchy API to call, and a box
//! that never opted in simply has no file — which is why every failure here is "no theme", never
//! an error. libadwaita's own palette is the fallback and always was. The web console reads the
//! same file (`web/server/util/omarchyTheme.ts`); the rules below are that file's, ported.
//!
//! **Colours only.** The widget vocabulary stays Adwaita — this recolours the shell, it does not
//! reshape it. Rounded cards, boxed lists and the header bar are unchanged.

use gtk::{gdk, glib};
use std::cell::Cell;
use std::path::PathBuf;

/// An sRGB colour, components 0..1.
///
/// Parsed into numbers rather than passed to GTK as a string for two reasons: every derived
/// surface below is a mix of two of them, and a value that survived [`parse_hex`] cannot carry a
/// `;` into the stylesheet we build.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Rgb(f64, f64, f64);

impl Rgb {
    fn hex(self) -> String {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", c(self.0), c(self.1), c(self.2))
    }

    /// Straight sRGB lerp — `t` is how far toward `other`. The console mixes in oklab; at the
    /// 3–7 % this file uses, the two are indistinguishable, and this needs no colour library.
    fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: f64, b: f64| a + (b - a) * t;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// WCAG relative luminance.
    fn luminance(self) -> f64 {
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
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (a.luminance(), b.luminance());
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`. Alpha is parsed and dropped — every colour here
/// names a solid surface.
///
/// Hex only, deliberately. The console also accepts `rgb()`/`oklch()`, but this module needs the
/// numbers anyway, GTK's CSS parser does not know `oklch`, and **an unrendered template still
/// holds its literal `{{ accent }}`** — which is not a colour, and this is what refuses it.
fn parse_hex(s: &str) -> Option<Rgb> {
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

/// The desktop's theme, in the four values the template carries.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Theme {
    dark: bool,
    bg: Rgb,
    fg: Rgb,
    accent: Rgb,
}

/// Omarchy's own state directory. `XDG_STATE_HOME` first, because that is what the spec says and
/// what a non-default setup uses; `~/.local/state` is the default it falls back to. Same order as
/// the console's `themePath()`.
fn omarchy_state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| glib::home_dir().join(".local/state"))
        .join("omarchy")
}

/// Where Omarchy renders our template.
fn theme_path() -> PathBuf {
    omarchy_state_dir().join("current/theme/punktfunk.json")
}

/// Parse the rendered template.
///
/// All three colours are required together: the template renders them as a set, so a file missing
/// one is a file we do not understand, and half a palette reads worse than plain Adwaita. An
/// unparsable `mode` is treated as `dark`, matching the console.
fn parse(raw: &str) -> Option<Theme> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let colour = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .and_then(parse_hex)
    };
    Some(Theme {
        dark: v.get("mode").and_then(serde_json::Value::as_str) != Some("light"),
        bg: colour("background")?,
        fg: colour("foreground")?,
        accent: colour("accent")?,
    })
}

/// The theme's accent, nudged toward the foreground until it reads as **text** on the theme's
/// background.
///
/// libadwaita spends `accent_color` on accented text and outlines, and `accent_bg_color` on the
/// fill behind `accent_fg_color`. Only the first has to survive as a foreground, so the fill keeps
/// the theme's exact colour and only the text form is corrected. A theme is free to pick an accent
/// that is handsome as a fill and illegible as text; several Omarchy light themes do.
///
/// 3:1 is WCAG's floor for large text and non-text, which is what the accent is used for here —
/// pill labels, outlines, a dot. ⚠ Not every theme can reach it (an accent already equal to the
/// foreground has nowhere to move), so this returns the best it managed rather than looping.
fn readable(accent: Rgb, bg: Rgb, fg: Rgb) -> Rgb {
    let mut c = accent;
    for _ in 0..10 {
        if contrast(c, bg) >= 3.0 {
            break;
        }
        c = c.mix(fg, 0.15);
    }
    c
}

/// Text drawn on top of the accent fill: whichever pole reads better against it.
fn on_accent(accent: Rgb) -> Rgb {
    let (black, white) = (Rgb(0.0, 0.0, 0.0), Rgb(1.0, 1.0, 1.0));
    if contrast(accent, white) >= contrast(accent, black) {
        white
    } else {
        black
    }
}

/// libadwaita's named colours, redefined from the theme.
///
/// Surfaces are mixed out of the background/foreground pair — "toward the foreground" is darker on
/// a light theme and lighter on a dark one, so neither branch has to know which it is.
///
/// 🛑 `success_*`, `warning_*` and `destructive_*`/`error_*` are deliberately absent: a theme with
/// a red accent must not make "Unpair" and "Connect" the same colour. The console's stylesheet
/// records the same rule.
fn css(t: &Theme) -> String {
    let s = |k: f64| t.bg.mix(t.fg, k).hex();
    let (bg, fg) = (t.bg.hex(), t.fg.hex());
    let (surface, sidebar, header) = (s(0.07), s(0.03), s(0.05));
    format!(
        "@define-color window_bg_color {bg};\n\
         @define-color window_fg_color {fg};\n\
         @define-color view_bg_color {bg};\n\
         @define-color view_fg_color {fg};\n\
         @define-color headerbar_bg_color {header};\n\
         @define-color headerbar_fg_color {fg};\n\
         @define-color sidebar_bg_color {sidebar};\n\
         @define-color sidebar_fg_color {fg};\n\
         @define-color secondary_sidebar_bg_color {sidebar};\n\
         @define-color secondary_sidebar_fg_color {fg};\n\
         @define-color card_bg_color {surface};\n\
         @define-color card_fg_color {fg};\n\
         @define-color dialog_bg_color {surface};\n\
         @define-color dialog_fg_color {fg};\n\
         @define-color popover_bg_color {surface};\n\
         @define-color popover_fg_color {fg};\n\
         @define-color accent_color {text};\n\
         @define-color accent_bg_color {fill};\n\
         @define-color accent_fg_color {on};\n",
        text = readable(t.accent, t.bg, t.fg).hex(),
        fill = t.accent.hex(),
        on = on_accent(t.accent).hex(),
    )
}

/// Read the file and turn it into a stylesheet, or `None` when this box has no theme.
fn current() -> Option<(Theme, String)> {
    let raw = std::fs::read_to_string(theme_path()).ok()?;
    // A half-written file during a theme switch parses as nothing, which is the same answer as
    // "no theme" and needs no separate branch.
    let t = parse(&raw)?;
    Some((t, css(&t)))
}

/// Apply the Omarchy theme, if this box has one, and keep following it.
///
/// Called from `app::load_css`, which is where the display-wide providers are installed. Does
/// nothing at all off Omarchy: the one `exists()` below is the whole cost on every other box.
pub fn install() {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else {
        return;
    };
    // The state directory exists on every Omarchy box — Omarchy itself writes indicators and
    // rendered templates under it — and on no other. Checking the DIRECTORY rather than our own
    // file is what lets `punktfunk-omarchy setup` be run against an already-open client: the
    // poll below picks the theme up without a restart. Off Omarchy this one stat is the whole
    // cost, and no timer is ever armed.
    if !omarchy_state_dir().is_dir() {
        return;
    }
    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        // Above `app::CSS`, which sits at APPLICATION and *uses* `@accent_color`, and above
        // libadwaita's own sheet, which defines these same names at THEME priority.
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let mut last: Option<Theme> = None;
    let mut tick = move || {
        let now = current();
        // Reloading an unchanged sheet would re-run the whole cascade every tick.
        if now.as_ref().map(|(t, _)| *t) == last {
            return;
        }
        match &now {
            Some((t, sheet)) => {
                provider.load_from_string(sheet);
                adw::StyleManager::default().set_color_scheme(if t.dark {
                    adw::ColorScheme::ForceDark
                } else {
                    adw::ColorScheme::ForceLight
                });
            }
            // Opted back out, or mid-rewrite: drop our definitions and hand the palette back.
            None => {
                provider.load_from_string("");
                adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
            }
        }
        last = now.map(|(t, _)| t);
    };
    tick();
    // ponytail: a 2 s poll, not a `GFileMonitor` — `~/.local/state/omarchy/current` is a SYMLINK
    // that `omarchy-theme-set` re-points, so a monitor on the resolved path would sit watching the
    // PREVIOUS theme's file forever. The web console polls the same file on the same interval.
    // Swap for an inotify watch on the symlink's parent if this ever shows up in a profile.
    glib::timeout_add_seconds_local(2, move || {
        tick();
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered template, as `omarchy-theme-set` writes it.
    fn rendered(mode: &str, bg: &str, fg: &str, accent: &str) -> String {
        format!(
            r#"{{"schema":1,"mode":"{mode}","background":"{bg}","foreground":"{fg}","accent":"{accent}"}}"#
        )
    }

    #[test]
    fn an_unrendered_template_is_not_a_theme() {
        // The failure this catches for real: `punktfunk.json.tpl` copied into place without
        // Omarchy ever rendering it. `{{ accent }}` is a string, and JSON-valid.
        assert!(parse(&rendered("dark", "#1a1b26", "#c0caf5", "{{ accent }}")).is_none());
        assert!(
            parse(r##"{"mode":"dark","accent":"#89b4fa"}"##).is_none(),
            "a partial palette"
        );
        assert!(
            parse(r#"{"mode":"dark","acc"#).is_none(),
            "half-written during a switch"
        );
        assert!(parse(&rendered("dark", "#1a1b26", "#c0caf5", "#zzzzzz")).is_none());
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
        let t = |m| parse(&rendered(m, "#1a1b26", "#c0caf5", "#7aa2f7")).unwrap();
        assert!(t("dark").dark);
        assert!(!t("light").dark);
        assert!(
            t("nonsense").dark,
            "an unparsable mode is dark, as in the console"
        );
    }

    #[test]
    fn a_washed_out_accent_is_lifted_until_it_reads_as_text() {
        // Everforest Light's shape: a pale accent on a pale background. Raw, it fails as text.
        let (bg, fg, accent) = (
            parse_hex("#fdf6e3").unwrap(),
            parse_hex("#5c6a72").unwrap(),
            parse_hex("#dfa000").unwrap(),
        );
        assert!(
            contrast(accent, bg) < 3.0,
            "the premise: this accent is unreadable as text"
        );
        assert!(contrast(readable(accent, bg, fg), bg) >= 3.0);
        // The FILL keeps the theme's exact colour — only the text form moves.
        assert!(css(&Theme {
            dark: false,
            bg,
            fg,
            accent
        })
        .contains("@define-color accent_bg_color #dfa000;"));
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

    #[test]
    fn the_semantic_colours_are_left_to_libadwaita() {
        // A red-accented theme must not make "Unpair" and "Connect" the same colour.
        let t = parse(&rendered("dark", "#1a1b26", "#c0caf5", "#f7768e")).unwrap();
        let sheet = css(&t);
        for name in [
            "success_color",
            "warning_color",
            "destructive_color",
            "error_color",
        ] {
            assert!(!sheet.contains(name), "{name} must keep libadwaita's value");
        }
    }

    #[test]
    fn every_emitted_value_is_a_plain_hex_literal() {
        // The injection guard, stated as a property: nothing reaches the stylesheet that could
        // close a declaration and open another.
        let t = parse(&rendered("dark", "#1a1b26", "#c0caf5", "#7aa2f7")).unwrap();
        for line in css(&t).lines().filter(|l| !l.is_empty()) {
            let value = line.rsplit(' ').next().unwrap().trim_end_matches(';');
            assert!(parse_hex(value).is_some(), "not a hex literal: {line}");
        }
    }

    /// The load-bearing assumption, stated as a test: a `@define-color` in OUR provider reaches a
    /// widget styled by a DIFFERENT provider that only ever *names* `@accent_color` — which is
    /// exactly what `app::CSS` does. Every test above would still pass if this were false and the
    /// shell quietly ignored the theme, so this is the one that proves the feature works at all.
    ///
    /// 🛑 GTK initialises ONCE per process, from ONE thread, and libtest gives every test its own
    /// thread — `--test-threads=1` included, which serialises them without sharing a thread. So
    /// this crate's three display tests must each run in their OWN process; whichever starts first
    /// wins and the rest panic with "Attempted to initialize GTK from two different threads". Run
    /// it by name:
    ///
    ///     cargo test -p punktfunk-client-linux -- --ignored the_theme_reaches_a_widget
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn the_theme_reaches_a_widget_through_the_app_stylesheet() {
        use gtk::prelude::*;
        gtk::init().expect("gtk init");
        let display = gdk::Display::default().expect("a display");

        // `app::CSS` as the shell installs it: it names the colour and never defines it.
        let app = gtk::CssProvider::new();
        app.load_from_string(".pf-probe { color: @accent_color; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &app,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let theme = Theme {
            dark: true,
            bg: parse_hex("#1a1b26").unwrap(),
            fg: parse_hex("#c0caf5").unwrap(),
            accent: parse_hex("#7aa2f7").unwrap(),
        };
        let ours = gtk::CssProvider::new();
        ours.load_from_string(&css(&theme));
        gtk::style_context_add_provider_for_display(
            &display,
            &ours,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        let label = gtk::Label::new(None);
        label.add_css_class("pf-probe");
        let window = gtk::Window::new();
        window.set_child(Some(&label));
        window.present();
        // Style resolves when the widget is mapped, which happens on the loop, not on `present`.
        while glib::MainContext::default().iteration(false) {}

        let c = label.color();
        assert_eq!(
            Rgb(
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue())
            )
            .hex(),
            "#7aa2f7",
            "the theme's accent never reached the widget"
        );
    }
}
