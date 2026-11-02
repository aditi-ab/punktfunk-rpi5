//! `pf-inject` at `crate::inject::*`, plus the host's aim of the injectors' absolute mapping
//! (pen, touch, absolute mouse) at the head a session's capture attached to. The wire
//! normalizes over the streamed frame, so the mapping covers that head, never the whole desktop.

pub(crate) use pf_inject::*;

/// The head's mode as the pointer warp's extent ([`set_stream_extent`]), in pixels. A display
/// mode never exceeds `u16`; anything that does is not one, so it publishes nothing.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn head_extent(mode: Option<(u32, u32, u32)>) -> Option<(u16, u16)> {
    let (w, h, _) = mode?;
    Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?))
}

/// Aim absolute input for screen `screen` at the head its capture attached to, at `mode`: the
/// first screen's by name and with a warp, a further one's into its own slot.
#[cfg(target_os = "linux")]
pub(crate) fn aim_at_head(screen: u8, name: Option<String>, mode: Option<(u32, u32, u32)>) {
    let extent = head_extent(mode);
    if screen == 0 {
        // The extent goes first: the output bumps the aim generation, and a warp that reads it
        // must find this head's size.
        set_stream_extent(extent);
        set_stream_output(name);
    } else {
        set_screen_head(screen, name, extent);
    }
}

/// [`aim_at_head`] by CCD target.
#[cfg(target_os = "windows")]
pub(crate) fn aim_at_target(
    screen: u8,
    key: pf_win_display::win_display::CcdTargetKey,
    mode: Option<(u32, u32, u32)>,
) {
    let extent = head_extent(mode);
    if screen == 0 {
        set_stream_extent(extent);
        set_stream_target(Some(key));
    } else {
        set_screen_head(screen, None, extent);
        set_screen_target(screen, Some(key));
    }
}
