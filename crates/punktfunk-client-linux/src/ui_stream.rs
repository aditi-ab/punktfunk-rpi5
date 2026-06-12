//! The stream page: decoded frames into a `GtkGraphicsOffload`-wrapped picture, local
//! input captured and forwarded on the wire contract.
//!
//! Input mapping: keys are hardware keycodes (evdev + 8 on Wayland) → VK via `keymap`,
//! layout-independent. Mouse is absolute (`MouseMoveAbs` scaled into the negotiated mode
//! through the letterbox transform) — relative/pointer-lock capture is the stage-2
//! presenter's job. While streaming, compositor shortcuts are inhibited (configurable);
//! Ctrl+Alt+Shift+Q ends the session, F11 toggles fullscreen — everything else goes to
//! the host.

use crate::keymap;
use crate::session::Stats;
use crate::video::DecodedFrame;
use adw::prelude::*;
use gtk::{gdk, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct StreamPage {
    pub page: adw::NavigationPage,
    stats_label: gtk::Label,
}

impl StreamPage {
    pub fn update_stats(&self, s: Stats) {
        self.stats_label.set_text(&format!(
            "{:.0} fps · {:.1} Mbit/s · dec {:.1} ms · lat {:.1} ms",
            s.fps, s.mbps, s.decode_ms, s.latency_ms
        ));
    }
}

fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

/// Widget coordinates → video pixel coordinates through the Contain-fit letterbox.
fn map_xy(widget: &impl IsA<gtk::Widget>, connector: &NativeClient, x: f64, y: f64) -> (i32, i32) {
    let w = widget.as_ref();
    let mode = connector.mode();
    let (ww, wh) = (w.width().max(1) as f64, w.height().max(1) as f64);
    let (vw, vh) = (mode.width.max(1) as f64, mode.height.max(1) as f64);
    let scale = (ww / vw).min(wh / vh);
    let (ox, oy) = ((ww - vw * scale) / 2.0, (wh - vh * scale) / 2.0);
    (
        (((x - ox) / scale).round()).clamp(0.0, vw - 1.0) as i32,
        (((y - oy) / scale).round()).clamp(0.0, vh - 1.0) as i32,
    )
}

#[allow(clippy::too_many_lines)]
pub fn new(
    window: &adw::ApplicationWindow,
    connector: Arc<NativeClient>,
    frames: async_channel::Receiver<DecodedFrame>,
    stop: Arc<AtomicBool>,
    inhibit_shortcuts: bool,
    title: &str,
) -> StreamPage {
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);

    // The offload path: with a dmabuf-backed texture (stage 1.5) this becomes a
    // subsurface the compositor can scan out directly; with memory textures it is a
    // no-op wrapper. Black letterboxing keeps fullscreen scanout-eligible.
    let offload = gtk::GraphicsOffload::new(Some(&picture));
    offload.set_black_background(true);

    let stats_label = gtk::Label::new(None);
    stats_label.add_css_class("osd");
    stats_label.add_css_class("numeric");
    stats_label.set_halign(gtk::Align::Start);
    stats_label.set_valign(gtk::Align::Start);
    stats_label.set_margin_start(12);
    stats_label.set_margin_top(12);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&offload));
    overlay.add_overlay(&stats_label);
    overlay.set_focusable(true);
    // The remote cursor is in the video — hide the local one over the stream.
    overlay.set_cursor(gdk::Cursor::from_name("none", None).as_ref());

    let header = adw::HeaderBar::new();
    let fullscreen_btn = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    fullscreen_btn.set_tooltip_text(Some("Fullscreen (F11)"));
    {
        let window = window.clone();
        fullscreen_btn.connect_clicked(move |_| {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        });
    }
    header.pack_end(&fullscreen_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&overlay));
    // Fullscreen = the stream and nothing else.
    {
        let toolbar = toolbar.clone();
        window.connect_fullscreened_notify(move |w| {
            toolbar.set_reveal_top_bars(!w.is_fullscreen());
        });
    }

    let page = adw::NavigationPage::builder()
        .title(title)
        .tag("stream")
        .child(&toolbar)
        .build();

    // --- Frame consumer: newest texture wins, set on the GTK frame clock's cadence. ---
    {
        let picture = picture.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(f) = frames.recv().await {
                let Some(picture) = picture.upgrade() else {
                    break;
                };
                let bytes = glib::Bytes::from_owned(f.rgba);
                let tex = gdk::MemoryTexture::new(
                    f.width as i32,
                    f.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    f.stride,
                );
                picture.set_paintable(Some(&tex));
            }
        });
    }

    // --- Keyboard ---
    {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let conn = connector.clone();
        let stop_k = stop.clone();
        let window_k = window.clone();
        key.connect_key_pressed(move |_, keyval, keycode, state| {
            let chord = gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SHIFT_MASK;
            if state.contains(chord) && keyval.to_lower() == gdk::Key::q {
                stop_k.store(true, Ordering::SeqCst); // ends the session → page pops
                return glib::Propagation::Stop;
            }
            if keyval == gdk::Key::F11 {
                if window_k.is_fullscreen() {
                    window_k.unfullscreen();
                } else {
                    window_k.fullscreen();
                }
                return glib::Propagation::Stop;
            }
            if let Some(vk) = keycode
                .checked_sub(8)
                .and_then(|c| keymap::evdev_to_vk(c as u16))
            {
                send(&conn, InputKind::KeyDown, vk as u32, 0, 0, 0);
            }
            glib::Propagation::Stop
        });
        let conn = connector.clone();
        key.connect_key_released(move |_, _keyval, keycode, _state| {
            if let Some(vk) = keycode
                .checked_sub(8)
                .and_then(|c| keymap::evdev_to_vk(c as u16))
            {
                send(&conn, InputKind::KeyUp, vk as u32, 0, 0, 0);
            }
        });
        overlay.add_controller(key);
    }

    // --- Mouse: absolute motion, buttons, wheel ---
    {
        let motion = gtk::EventControllerMotion::new();
        let conn = connector.clone();
        let target = overlay.downgrade();
        motion.connect_motion(move |_, x, y| {
            if let Some(w) = target.upgrade() {
                let (px, py) = map_xy(&w, &conn, x, y);
                send(&conn, InputKind::MouseMoveAbs, 0, px, py, 0);
            }
        });
        overlay.add_controller(motion);
    }
    {
        let click = gtk::GestureClick::builder().button(0).build();
        let conn = connector.clone();
        let target = overlay.downgrade();
        click.connect_pressed(move |g, _n, x, y| {
            if let Some(w) = target.upgrade() {
                w.grab_focus();
                let (px, py) = map_xy(&w, &conn, x, y);
                send(&conn, InputKind::MouseMoveAbs, 0, px, py, 0);
            }
            if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
                send(&conn, InputKind::MouseButtonDown, gs, 0, 0, 0);
            }
        });
        let conn = connector.clone();
        click.connect_released(move |g, _n, _x, _y| {
            if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
                send(&conn, InputKind::MouseButtonUp, gs, 0, 0, 0);
            }
        });
        overlay.add_controller(click);
    }
    {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let conn = connector.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            // The wire carries WHEEL_DELTA(120) units, positive = up / right; GTK's dy is
            // positive = down. Smooth fractions survive — libei's discrete scroll is
            // 120-based too.
            let vy = (-dy * 120.0) as i32;
            if vy != 0 {
                send(&conn, InputKind::MouseScroll, 0, vy, 0, 0);
            }
            let vx = (dx * 120.0) as i32;
            if vx != 0 {
                send(&conn, InputKind::MouseScroll, 1, vx, 0, 0);
            }
            glib::Propagation::Stop
        });
        overlay.add_controller(scroll);
    }

    // --- Capture lifecycle: grab focus + compositor shortcuts while mapped. ---
    {
        let window = window.clone();
        overlay.connect_map(move |w| {
            tracing::debug!("stream overlay mapped");
            w.grab_focus();
            if inhibit_shortcuts {
                if let Some(tl) = window
                    .surface()
                    .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
                {
                    tl.inhibit_system_shortcuts(None::<&gdk::Event>);
                }
            }
        });
    }
    {
        let window = window.clone();
        overlay.connect_unmap(move |_| {
            if let Some(tl) = window
                .surface()
                .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
            {
                tl.restore_system_shortcuts();
            }
        });
    }
    // The page's `hidden` fires once navigation away completes (back button, pop on
    // session end) — NOT on the transient unmap/map cycle a NavigationView push performs.
    {
        let window = window.clone();
        let stop_h = stop.clone();
        page.connect_hidden(move |_| {
            tracing::debug!("stream page hidden — ending session");
            if window.is_fullscreen() {
                window.unfullscreen();
            }
            stop_h.store(true, Ordering::SeqCst);
        });
    }

    StreamPage { page, stats_label }
}
