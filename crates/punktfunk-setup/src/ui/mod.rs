//! Everything that renders. Nothing below this module knows which surface is attached.
//!
//! `exec` and `report` speak only `Reporter`, so the same `Plan` drives today's `==>` lines
//! and the TUI's phase checklist without either knowing the other exists. `plain` is the D5
//! compatibility contract — byte-for-byte the sh installer's output — and `tui` is the clack
//! screens, hand-rolled over `term` after S1 (see `term`'s header for why).
//!
//! `summary` is the settings screen as pure state, `theme` the brand colour and capability
//! ladder, `logo` the animated lens mark.

pub mod logo;
pub mod plain;
pub mod summary;
pub mod term;
pub mod theme;
pub mod tui;

pub use plain::Plain;

/// The surface a running plan reports itself through.
///
/// Deliberately the sh installer's vocabulary: a step heading, an ok, a warning, an echoed
/// command. A renderer that wants a checklist builds it from `say` and `plus`; one that wants
/// today's transcript prints them.
pub trait Reporter {
    fn say(&self, msg: &str);
    fn ok(&self, msg: &str);
    fn warn(&self, msg: &str);
    fn die(&self, msg: &str);
    /// The dim `+ cmd` echo. Every command prints before it runs — that transparency is a
    /// trust feature, and the scrollback is the audit log.
    fn plus(&self, cmd: &str);
    fn detail(&self, msg: &str);
    fn line(&self, msg: &str);
    fn blank(&self);
}
