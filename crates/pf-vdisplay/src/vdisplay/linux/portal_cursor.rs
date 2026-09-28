//! ScreenCast cursor mode. The ladder is [`pf_frame::cursor_mode`], shared with
//! `pf-capture`'s portal monitor; the Linux negotiation against
//! `AvailableCursorModes` is `pf_capture::portal_rt::negotiate_cursor_mode`.

#[cfg(target_os = "linux")]
pub(crate) use pf_capture::portal_rt::negotiate_cursor_mode as negotiate;
pub use pf_frame::cursor_mode::Mode;
