//! The clack-look screens: gutter rail, inline prompts, no alternate screen.
//!
//! The transcript scrolls and stays the audit log, which is the constraint that shapes this
//! file: a frame is only ever repainted while it is still the *live* one (the settings list,
//! a row editor, the intro animation). Once a command has been echoed, nothing overwrites it.
//! That is why the progress view prints one `◆` heading per phase with its commands dim
//! beneath, rather than the collapsing checklist a repaint could draw.
//!
//! Everything reaches the terminal through `term::Terminal`, so `ScriptedTerm` can drive the
//! whole flow with a key list and assert on the frame the user would be looking at.

use std::cell::RefCell;

use crate::choices::Action;
use crate::facts::Channel;
use crate::ui::logo::{self, Intro, MARK_PX};
use crate::ui::summary::{Field, Item, Key as UiKey, Screen, Step};
use crate::ui::term::{Key, Terminal};
use crate::ui::theme::{Caps, Colors, Layer, BRAND, LENS};
use crate::ui::Reporter;

const BAR: &str = "│";
const STEP_ACTIVE: &str = "◆";
const STEP_DONE: &str = "◇";
const CURSOR: &str = "▸";
const RADIO_ON: &str = "●";
const RADIO_OFF: &str = "○";
const WARN: &str = "▲";

/// Rows the mark occupies: two pixel rows per line.
const MARK_ROWS: usize = MARK_PX / 2;

pub struct Tui<'a> {
    term: RefCell<&'a mut dyn Terminal>,
    caps: Caps,
    /// 0 in tests and under `--demo --fast`, so nothing waits on wall-clock time.
    frame_ms: u64,
}

impl<'a> Tui<'a> {
    pub fn new(term: &'a mut dyn Terminal, caps: Caps, frame_ms: u64) -> Tui<'a> {
        Tui {
            term: RefCell::new(term),
            caps,
            frame_ms,
        }
    }

    fn write(&self, text: &str) {
        self.term.borrow_mut().write(text);
    }

    fn dim(&self, text: &str) -> String {
        if self.caps.colors == Colors::None {
            return text.to_string();
        }
        format!("\x1b[2m{text}\x1b[0m")
    }

    fn accent(&self, text: &str) -> String {
        format!(
            "{}{text}{}",
            self.caps.paint(BRAND, Layer::Fg),
            self.caps.reset()
        )
    }

    fn highlight(&self, text: &str) -> String {
        format!(
            "{}{text}{}",
            self.caps.paint(LENS, Layer::Fg),
            self.caps.reset()
        )
    }

    fn bar(&self) -> String {
        self.dim(BAR)
    }

    /// Can this terminal draw the mark at all?
    fn marked(&self) -> bool {
        logo::intro_level(&self.caps, false) != Intro::Plain
    }

    /// The mark, animated or still, per D7's ladder. Returns the rows it left on screen, which
    /// the settings loop clears before drawing its own copy — so the two never stack up.
    pub fn intro(&self, level: Intro, parts: logo::Parts) -> usize {
        match level {
            Intro::Plain => return 0,
            Intro::Static => self.write(&logo::still(&self.caps, 2, parts)),
            Intro::Animated => {
                for i in 0..logo::FRAMES {
                    let t = i as f32 / (logo::FRAMES - 1) as f32;
                    if i > 0 {
                        self.term.borrow_mut().clear_last_lines(MARK_ROWS);
                    }
                    self.write(&logo::render(&logo::frame_parts(t, parts), &self.caps, 2));
                    if self.frame_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(self.frame_ms));
                    }
                }
            }
        }
        MARK_ROWS
    }

    /// The settings list. Returns what the user chose to do with it.
    ///
    /// `already` is what the intro left on screen; the first repaint clears exactly that, so the
    /// mark the intro animated is replaced by the one this frame owns rather than pushed down.
    pub fn settings(&self, screen: &mut Screen, already: usize) -> Step {
        self.term.borrow_mut().hide_cursor();
        let mut drawn = already;
        let outcome = loop {
            let frame = self.frame(screen);
            if drawn > 0 {
                self.term.borrow_mut().clear_last_lines(drawn);
            }
            drawn = frame.lines().count();
            self.write(&frame);

            let key = self.key();
            match screen.key(key) {
                Step::Idle => {}
                Step::Edit(field) => {
                    // The editor draws below the list, so the list is redrawn from scratch.
                    self.term.borrow_mut().clear_last_lines(drawn);
                    drawn = 0;
                    self.edit(screen, field);
                }
                done => break done,
            }
        };
        self.term.borrow_mut().show_cursor();
        outcome
    }

    fn key(&self) -> UiKey {
        let key = self.term.borrow_mut().read_key();
        match key {
            Key::Up | Key::Char('k') => UiKey::Up,
            Key::Down | Key::Char('j') => UiKey::Down,
            Key::Enter | Key::Space => UiKey::Enter,
            Key::Cancel | Key::Char('q') => UiKey::Cancel,
            _ => UiKey::Ignored,
        }
    }

    fn frame(&self, screen: &Screen) -> String {
        let mut out = String::new();
        let bar = self.bar();
        let rule = self.dim(&"─".repeat(58));
        // The mark is part of the frame, not a one-off banner, so muting follows the Components
        // row live: pick Client and the host circle greys out under the cursor.
        if self.marked() {
            let parts = logo::Parts {
                host: screen.choices.components.host,
                client: screen.choices.components.client,
            };
            out.push_str(&logo::render(&logo::frame_parts(1.0, parts), &self.caps, 2));
        }
        out.push_str(&format!("{bar}\n"));
        out.push_str(&format!(
            "{}  {}\n",
            self.accent(STEP_ACTIVE),
            self.highlight(&screen.title())
        ));
        out.push_str(&format!("{bar}  {}\n", self.dim(&screen.facts.docs_page)));
        out.push_str(&format!("{bar}\n"));

        for (index, item) in screen.items.iter().enumerate() {
            let on = index == screen.cursor;
            let mark = if on { self.accent(CURSOR) } else { " ".into() };
            match item {
                Item::Row(field) => {
                    let label = format!("{:<18}", Screen::label(*field));
                    let value = screen.value(*field);
                    let text = if on {
                        format!("{}{}", self.highlight(&label), value)
                    } else {
                        format!("{label}{}", self.dim(&value))
                    };
                    out.push_str(&format!("{bar}  {mark} {text}\n"));
                }
                Item::Go => {
                    let label = if screen.manage_mode() {
                        "Apply these changes"
                    } else {
                        "Install now with these settings"
                    };
                    out.push_str(&format!("{bar}  {mark} {}\n", self.highlight(label)));
                    out.push_str(&format!("{bar}  {rule}\n"));
                }
                Item::DryRun => {
                    out.push_str(&format!("{bar}  {rule}\n"));
                    out.push_str(&format!(
                        "{bar}  {mark} {}\n",
                        self.dim("Dry run — print every command, change nothing")
                    ));
                }
                Item::Uninstall => out.push_str(&format!(
                    "{bar}  {mark} {}\n",
                    self.dim("Uninstall — remove the packages and the repo")
                )),
            }
        }
        out.push_str(&format!("{bar}\n"));
        out.push_str(&format!(
            "{bar}  {}\n",
            self.dim("↑↓ move · Enter select/edit · q quit — nothing runs until you confirm")
        ));
        out
    }

    /// Open the row's editor, then fold the answer back into the screen.
    fn edit(&self, screen: &mut Screen, field: Field) {
        let why = Screen::why(field);
        match field {
            Field::Channel => {
                let current = usize::from(screen.choices.channel == Channel::Canary);
                if let Some(pick) = self.choose(why, &["stable", "canary"], current) {
                    screen.set_channel(if pick == 0 {
                        Channel::Stable
                    } else {
                        Channel::Canary
                    });
                }
            }
            Field::Components => {
                let c = screen.choices.components;
                let current = match (c.host, c.client) {
                    (true, false) => 0,
                    (true, true) => 1,
                    _ => 2,
                };
                let options = ["Host only", "Host and client", "Client only"];
                if let Some(pick) = self.choose(why, &options, current) {
                    screen.set_components(pick != 2, pick != 0);
                }
            }
            other => {
                let current = usize::from(!self.current_bool(screen, other));
                if let Some(pick) = self.choose(why, &["yes", "no"], current) {
                    screen.set_bool(other, pick == 0);
                }
            }
        }
    }

    fn current_bool(&self, screen: &Screen, field: Field) -> bool {
        let c = &screen.choices;
        match field {
            Field::Group => c.punktfunk_group,
            Field::Gamestream => c.gamestream,
            Field::Clipboard => c.clipboard,
            Field::Linger => c.linger,
            Field::Start => c.start,
            _ => false,
        }
    }

    /// One inline radio list. `None` when the user backed out.
    fn choose(&self, prompt: &str, options: &[&str], initial: usize) -> Option<usize> {
        let mut cursor = initial.min(options.len().saturating_sub(1));
        let mut drawn = 0usize;
        loop {
            let mut frame = String::new();
            let bar = self.bar();
            frame.push_str(&format!("{bar}\n"));
            frame.push_str(&format!("{}  {}\n", self.accent(STEP_ACTIVE), prompt));
            for (index, option) in options.iter().enumerate() {
                let (glyph, text) = if index == cursor {
                    (self.accent(RADIO_ON), self.highlight(option))
                } else {
                    (self.dim(RADIO_OFF), self.dim(option))
                };
                frame.push_str(&format!("{bar}  {glyph} {text}\n"));
            }
            frame.push_str(&format!("{bar}\n"));
            if drawn > 0 {
                self.term.borrow_mut().clear_last_lines(drawn);
            }
            drawn = frame.lines().count();
            self.write(&frame);

            // Bound before the match: an arm that clears lines needs the RefCell back, and a
            // match scrutinee holds its borrow for the whole match.
            let key = self.term.borrow_mut().read_key();
            match key {
                Key::Up | Key::Char('k') => {
                    cursor = (cursor + options.len() - 1) % options.len();
                }
                Key::Down | Key::Char('j') => cursor = (cursor + 1) % options.len(),
                Key::Enter | Key::Space => {
                    // Leave the answer on screen as one collapsed line, clack-style.
                    self.term.borrow_mut().clear_last_lines(drawn);
                    self.write(&format!(
                        "{}  {}\n",
                        self.dim(STEP_DONE),
                        self.dim(&format!("{prompt} → {}", options[cursor]))
                    ));
                    return Some(cursor);
                }
                Key::Cancel | Key::Char('q') => {
                    self.term.borrow_mut().clear_last_lines(drawn);
                    return None;
                }
                _ => {}
            }
        }
    }

    /// Step 7's text, inside the rail.
    pub fn outro(&self, lines: &[String]) {
        self.write(&format!("{}\n", self.bar()));
        for line in lines {
            self.write(&format!("{}  {line}\n", self.dim("└")));
        }
    }

    pub fn failure(&self, message: &str) {
        self.write(&format!("{}\n", self.bar()));
        let mark = if self.caps.colors == Colors::None {
            WARN.to_string()
        } else {
            format!("\x1b[31m{WARN}\x1b[0m")
        };
        self.write(&format!("{mark}  {message}\n"));
    }
}

/// During execution the same `Plan` reports through here instead of the plain transcript.
impl Reporter for Tui<'_> {
    fn say(&self, msg: &str) {
        self.write(&format!("{}\n", self.bar()));
        self.write(&format!(
            "{}  {}\n",
            self.accent(STEP_ACTIVE),
            self.highlight(msg)
        ));
    }

    fn ok(&self, msg: &str) {
        self.write(&format!("{}  {} {msg}\n", self.bar(), self.dim(STEP_DONE)));
    }

    fn warn(&self, msg: &str) {
        let mark = if self.caps.colors == Colors::None {
            WARN.to_string()
        } else {
            format!("\x1b[33m{WARN}\x1b[0m")
        };
        self.write(&format!("{}  {mark} {msg}\n", self.bar()));
    }

    fn die(&self, msg: &str) {
        self.failure(msg);
    }

    fn plus(&self, cmd: &str) {
        self.write(&format!(
            "{}    {}\n",
            self.bar(),
            self.dim(&format!("+ {cmd}"))
        ));
    }

    fn detail(&self, msg: &str) {
        self.write(&format!("{}    {}\n", self.bar(), self.dim(msg)));
    }

    fn line(&self, msg: &str) {
        self.write(&format!("{}  {msg}\n", self.bar()));
    }

    fn blank(&self) {
        self.write(&format!("{}\n", self.bar()));
    }
}

/// What the caller does with a settings screen that ended.
pub fn action_of(step: Step) -> Option<Action> {
    match step {
        Step::Run(action) => Some(action),
        _ => None,
    }
}
