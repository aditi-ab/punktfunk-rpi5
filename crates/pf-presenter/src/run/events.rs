//! Input: SDL events into capture, touch into the gesture engine, and the pad mask.

use super::*;

pub(super) fn ui_wants_pad_mask(
    focus_lost: bool,
    overlay_open: bool,
    capture_active: Option<bool>,
) -> bool {
    focus_lost || overlay_open || capture_active == Some(false)
}

/// One SDL mouse/touch event as the overlay wants it: swapchain pixels. `None` for
/// events the console cannot use.
///
/// Two conversions: mouse positions are window (logical) coordinates; fingers arrive
/// window-normalized (0..1). Mixing them puts every click off by the display scale.
/// Only DIRECT touch devices; an indirect trackpad already drives the mouse.
pub(super) fn overlay_pointer(event: &Event, window: &sdl3::video::Window) -> Option<PointerInput> {
    // SDL's mouse id on mouse events synthesized from a touch (`SDL_TOUCH_MOUSEID`,
    // not re-exported by the sdl3 crate). The finger arms already forward the real
    // touch; the synthesized twin would land every tap twice.
    const TOUCH_MOUSEID: u32 = u32::MAX;
    let (pw, ph) = window.size_in_pixels();
    let (lw, lh) = window.size();
    // Logical → physical. A zero-sized window (minimized) would divide by zero.
    let sx = pw as f32 / lw.max(1) as f32;
    let sy = ph as f32 / lh.max(1) as f32;
    let button = |b: sdl3::mouse::MouseButton| match b {
        sdl3::mouse::MouseButton::Left => Some(PointerButton::Primary),
        sdl3::mouse::MouseButton::Right => Some(PointerButton::Secondary),
        _ => None,
    };
    Some(match event {
        Event::MouseMotion { which, x, y, .. } if *which != TOUCH_MOUSEID => PointerInput::Move {
            x: x * sx,
            y: y * sy,
        },
        Event::MouseButtonDown {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Down {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
            touch: false,
        },
        Event::MouseButtonUp {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Up {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
        },
        Event::MouseWheel {
            y,
            mouse_x,
            mouse_y,
            ..
        } => PointerInput::Wheel {
            x: mouse_x * sx,
            y: mouse_y * sy,
            dy: *y,
        },
        Event::FingerDown { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Down {
                x: x * pw as f32,
                y: y * ph as f32,
                button: PointerButton::Primary,
                touch: true,
            }
        }
        Event::FingerMotion { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Move {
                x: x * pw as f32,
                y: y * ph as f32,
            }
        }
        Event::FingerUp { touch_id, x, y, .. } if is_direct_touch(*touch_id) => PointerInput::Up {
            x: x * pw as f32,
            y: y * ph as f32,
            button: PointerButton::Primary,
        },
        // The pointer left the window mid-press: drop the press rather than let a release
        // that never comes leave a widget armed forever.
        Event::Window {
            win_event: WindowEvent::MouseLeave,
            ..
        } => PointerInput::Cancel,
        _ => return None,
    })
}

/// Every touch device SDL sees, as `(id, kind, name)` — logged at connect. Under
/// gamescope this is the tell for whether Steam Input hands the touchscreen through.
pub(super) fn touch_devices() -> Vec<(u64, &'static str, String)> {
    use sdl3::sys::stdinc::SDL_free;
    use sdl3::sys::touch::{
        SDL_GetTouchDeviceName, SDL_GetTouchDeviceType, SDL_GetTouchDevices, SDL_TouchDeviceType,
    };
    let kind = |t: SDL_TouchDeviceType| {
        if t == SDL_TouchDeviceType::DIRECT {
            "direct"
        } else if t == SDL_TouchDeviceType::INDIRECT_ABSOLUTE {
            "indirect-absolute"
        } else if t == SDL_TouchDeviceType::INDIRECT_RELATIVE {
            "indirect-relative"
        } else {
            "invalid"
        }
    };
    let mut n: std::ffi::c_int = 0;
    // SAFETY: SDL hands back an array it owns (freed here once read, and never touched
    // after) and names it owns (copied out before the free, never kept); a null array or
    // name is checked before use.
    unsafe {
        let ids = SDL_GetTouchDevices(&mut n);
        if ids.is_null() {
            return Vec::new();
        }
        let out = std::slice::from_raw_parts(ids, usize::try_from(n).unwrap_or(0))
            .iter()
            .map(|id| {
                let name = SDL_GetTouchDeviceName(*id);
                let name = if name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned()
                };
                (id.0, kind(SDL_GetTouchDeviceType(*id)), name)
            })
            .collect();
        SDL_free(ids.cast());
        out
    }
}

/// Is this SDL touch device a real touchscreen (DIRECT, window-relative)? Trackpads
/// report INDIRECT and drive the mouse — their finger events must not be forwarded
/// as touch passthrough. An unknown/invalid id reads as not-direct.
pub(super) fn is_direct_touch(touch_id: u64) -> bool {
    use sdl3::sys::touch::{SDL_GetTouchDeviceType, SDL_TouchDeviceType, SDL_TouchID};
    // SAFETY: `SDL_GetTouchDeviceType` is a query on an id SDL issued; the TouchID
    // wrapper is a newtype over that id and does not take ownership of any handle.
    unsafe { SDL_GetTouchDeviceType(SDL_TouchID(touch_id)) == SDL_TouchDeviceType::DIRECT }
}

/// Route one SDL touchscreen finger into the session's [`Capture`]. SDL delivers
/// window-normalized `x`/`y` (0..1); the dispatcher hands physical window pixels
/// (trackpad ballistics) and the frame position under `fit` (pointer + passthrough).
/// Down/Move before the first decoded frame are dropped; an Up always dispatches so
/// a lift can release a held contact.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_finger(
    phase: FingerPhase,
    window: &sdl3::video::Window,
    stream: &mut Option<StreamState>,
    finger_id: u64,
    x: f32,
    y: f32,
    timestamp: u64,
    fit: VideoFit,
) -> Vec<Act> {
    let Some(st) = stream.as_mut() else {
        return Vec::new();
    };
    let (pw, ph) = window.size_in_pixels();
    let (wx, wy) = (x * pw as f32, y * ph as f32);
    let abs = match st.last_video {
        Some(video) => finger_to_frame(fit, (pw, ph), video, x, y),
        None if phase == FingerPhase::Up => Abs {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        None => return Vec::new(),
    };
    let Some(cap) = st.capture.as_mut() else {
        return Vec::new();
    };
    // `wx`/`wy` are physical px; the gesture engine prices scroll in DIP.
    cap.set_touch_density(window.display_scale());
    cap.dispatch_finger(
        phase,
        finger_id,
        wx,
        wy,
        abs,
        timestamp as f64 / 1_000_000.0,
    )
}

/// Three-finger tap bumps the stats tier; a two-finger twist turns the quick-action ring.
pub(super) fn on_touch_act(
    act: Act,
    verbosity: &mut StatsVerbosity,
    stream: &mut Option<StreamState>,
    overlay: &mut Option<Box<dyn Overlay>>,
) {
    let input = match act {
        Act::CycleStats => return bump_stats_tier(verbosity, stream),
        Act::Dial {
            progress,
            clockwise,
            x,
            y,
        } => RingInput::Turn {
            progress,
            clockwise,
            x,
            y,
        },
        Act::DialCommit => RingInput::Commit,
        Act::DialCancel => RingInput::Cancel,
        _ => return,
    };
    if let Some(o) = overlay.as_mut() {
        o.ring_input(input);
    }
}

/// A touchscreen finger while the ring is up goes to the ring as a pointer, not the
/// gesture engine. Returns true when the ring took it.
pub(super) fn ring_finger(
    overlay: &mut Option<Box<dyn Overlay>>,
    window: &sdl3::video::Window,
    phase: FingerPhase,
    x: f32,
    y: f32,
) -> bool {
    let Some(o) = overlay.as_mut().filter(|o| o.ring_open()) else {
        return false;
    };
    let (pw, ph) = window.size_in_pixels();
    let (x, y) = (x * pw as f32, y * ph as f32);
    let input = match phase {
        FingerPhase::Down => PointerInput::Down {
            x,
            y,
            button: PointerButton::Primary,
            touch: true,
        },
        FingerPhase::Move => PointerInput::Move { x, y },
        FingerPhase::Up => PointerInput::Up {
            x,
            y,
            button: PointerButton::Primary,
        },
    };
    o.handle_pointer(input);
    true
}

/// Window-normalized position → frame pixel, with the frame size as the wire extent.
/// Same placement as the blit; a finger in a bar or on a cropped edge clamps onto the
/// visible frame.
pub(super) fn finger_to_frame(
    fit: VideoFit,
    surface: (u32, u32),
    video: (u32, u32),
    x: f32,
    y: f32,
) -> Abs {
    let p = video_fit::place(fit, surface, video);
    let (fx, fy) = p.to_frame(
        f64::from(x) * f64::from(surface.0),
        f64::from(y) * f64::from(surface.1),
    );
    Abs {
        x: fx.round() as i32,
        y: fy.round() as i32,
        w: video.0,
        h: video.1,
    }
}

/// Inverse of [`finger_to_frame`] for the reappear warp: a host-frame pixel → logical
/// window coordinates (what `warp_mouse_in_window` takes). Coordinates outside the
/// visible frame clamp onto it.
pub(super) fn content_to_window(
    fit: VideoFit,
    logical: (u32, u32),
    surface: (u32, u32),
    video: (u32, u32),
    x: i32,
    y: i32,
) -> (f32, f32) {
    let p = video_fit::place(fit, surface, video);
    let (px, py) = p.to_view(f64::from(x), f64::from(y));
    // Physical → logical (HiDPI): the window's logical size over its pixel size.
    let lx = px * f64::from(logical.0) / f64::from(surface.0.max(1));
    let ly = py * f64::from(logical.1) / f64::from(surface.1.max(1));
    (lx as f32, ly as f32)
}

/// The scale the backend applies to a custom cursor surface on its own. Wayland applies the
/// display scale: SDL hands the compositor the bitmap's pixel size as a surface-local viewport
/// destination. X11 and Windows show the surface at 1:1 physical pixels.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; dividing by 0
/// would blow the cursor up to nothing usable.
pub(super) fn cursor_surface_scale(window: &sdl3::video::Window) -> f32 {
    if window.subsystem().current_video_driver() != "wayland" {
        return 1.0;
    }
    let scale = window.display_scale();
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// How long the host's relative hint must hold before the mouse model follows it: longer
/// than the hide Windows does for a click, short against a game grabbing the pointer.
pub(super) const HINT_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Mouse stillness before the local cursor follows host-driven motion: past a round trip, so
/// the host's echo of the user's own motion is never mistaken for someone else's.
pub(super) const FOLLOW_HOST_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// How long motion events after a follow-warp are its echo, not the user.
pub(super) const WARP_ECHO: std::time::Duration = std::time::Duration::from_millis(50);

/// Rounding between window and frame pixels; a smaller difference is the same spot.
pub(super) const FOLLOW_SLACK_PX: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_capture_masks_pads_until_capture_returns() {
        assert!(ui_wants_pad_mask(false, false, Some(false)));
        assert!(!ui_wants_pad_mask(false, false, Some(true)));
        assert!(!ui_wants_pad_mask(false, false, None));
        assert!(ui_wants_pad_mask(true, false, Some(true)));
        assert!(ui_wants_pad_mask(false, true, Some(true)));
    }

    #[test]
    fn content_to_window_inverts_the_letterbox() {
        // 1920×1080 video letterboxed in a 1600×1200 (4:3) window at 2× HiDPI: scale =
        // 1600/1920, dh = 900, oy = 150 (physical).
        let logical = (800u32, 600u32);
        let surface = (1600u32, 1200u32);
        let video = (1920u32, 1080u32);
        let (wx, wy) = content_to_window(VideoFit::Fit, logical, surface, video, 960, 540);
        assert!((wx - 400.0).abs() < 1.0, "wx = {wx}");
        assert!((wy - 300.0).abs() < 1.0, "wy = {wy}");
        // Roundtrip: normalized window pos → the same host frame pixel.
        let (nx, ny) = (wx / logical.0 as f32, wy / logical.1 as f32);
        let abs = finger_to_frame(VideoFit::Fit, surface, video, nx, ny);
        assert_eq!((abs.w, abs.h), video);
        assert!((abs.x - 960).abs() <= 1, "x = {}", abs.x);
        assert!((abs.y - 540).abs() <= 1, "y = {}", abs.y);
        // Out-of-range host coords clamp onto the video, never the bars.
        let (_, wy_clamped) = content_to_window(VideoFit::Fit, logical, surface, video, 0, 10_000);
        assert!(wy_clamped <= 300.0 + 225.0 + 1.0, "wy = {wy_clamped}"); // ≤ bottom of content
    }

    #[test]
    fn crop_maps_the_window_onto_the_visible_frame() {
        // 1920×1080 cropped into a 3216×1440 phone-shaped window: sides full, the top
        // and bottom ~110 frame rows cut. The window's top edge is frame row ~110.
        let surface = (3216, 1440);
        let video = (1920, 1080);
        let top = finger_to_frame(VideoFit::Crop, surface, video, 0.0, 0.0);
        assert_eq!((top.x, top.y, top.w, top.h), (0, 110, 1920, 1080));
        let corner = finger_to_frame(VideoFit::Crop, surface, video, 1.0, 1.0);
        assert_eq!((corner.x, corner.y), (1920, 970));
        // Stretch reaches every frame edge from every window edge.
        let s = finger_to_frame(VideoFit::Stretch, surface, video, 1.0, 1.0);
        assert_eq!((s.x, s.y), (1920, 1080));
    }

    fn frame_at(fit: VideoFit, surface: (u32, u32), x: f32, y: f32) -> (i32, i32, u32, u32) {
        let a = finger_to_frame(fit, surface, (1920, 1080), x, y);
        (a.x, a.y, a.w, a.h)
    }

    #[test]
    fn finger_maps_across_a_perfectly_filled_surface() {
        // Video exactly fills the window: normalized finger maps straight through.
        let s = (1920, 1080);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.0, 0.0), (0, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
    }

    #[test]
    fn finger_rebases_onto_the_letterboxed_frame() {
        // 16:9 video in 16:10 glass (1280×800) letterboxes: the picture is 1280×720,
        // centered with 40px bars. A finger in the top bar clamps to the frame's top edge.
        let s = (1280, 800);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
        // y=0.01 → window pixel 8, above the 40px bar → clamps to frame top (0).
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.01), (960, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
    }
}
