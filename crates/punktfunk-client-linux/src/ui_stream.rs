//! The stream page: decoded frames into a `GtkGraphicsOffload`-wrapped picture, local
//! input captured and forwarded on the wire contract.
//!
//! Input capture is a deliberate, reversible STATE (Moonlight-style, mirroring the Swift
//! client): engaged when the stream starts and when the user clicks into the video (that
//! click is suppressed toward the host); released by Ctrl+Alt+Shift+Q (toggles) or focus
//! loss — held keys/buttons are flushed host-side on release so nothing sticks down.
//! While captured the local cursor is hidden (the host renders its own) and compositor
//! shortcuts are inhibited (configurable); while released nothing is forwarded and the
//! HUD says how to recapture.
//!
//! Keys are hardware keycodes (evdev + 8 on Wayland) → VK via `keymap`, layout-
//! independent. Mouse is absolute (`MouseMoveAbs` scaled into the negotiated mode through
//! the letterbox transform, surface size packed in `flags`) — pointer-lock relative
//! capture is the stage-2 presenter's job. F11 toggles fullscreen locally.

use crate::keymap;
use crate::session::Stats;
use crate::video::DecodedFrame;
use adw::prelude::*;
use gtk::{gdk, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
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

/// Forward an absolute pointer position: widget coordinates → video pixels through the
/// Contain-fit letterbox. `flags` packs the coordinate-space size (`(w << 16) | h`, the
/// same contract as touch) — the host normalizes against it before mapping into the EIS
/// region; without it the event is dropped.
fn send_abs(widget: &impl IsA<gtk::Widget>, connector: &NativeClient, x: f64, y: f64) {
    let w = widget.as_ref();
    let mode = connector.mode();
    let (ww, wh) = (w.width().max(1) as f64, w.height().max(1) as f64);
    let (vw, vh) = (mode.width.max(1) as f64, mode.height.max(1) as f64);
    let scale = (ww / vw).min(wh / vh);
    let (ox, oy) = ((ww - vw * scale) / 2.0, (wh - vh * scale) / 2.0);
    let px = (((x - ox) / scale).round()).clamp(0.0, vw - 1.0) as i32;
    let py = (((y - oy) / scale).round()).clamp(0.0, vh - 1.0) as i32;
    let flags = (mode.width << 16) | (mode.height & 0xffff);
    send(connector, InputKind::MouseMoveAbs, 0, px, py, flags);
}

/// The capture state shared by every input controller on the page.
struct Capture {
    connector: Arc<NativeClient>,
    window: adw::ApplicationWindow,
    overlay: gtk::Overlay,
    hint: gtk::Label,
    inhibit_shortcuts: bool,
    captured: Cell<bool>,
    /// VKs / GameStream button ids currently held — flushed up on release.
    held_keys: RefCell<HashSet<u8>>,
    held_buttons: RefCell<HashSet<u32>>,
}

impl Capture {
    fn engage(&self) {
        if self.captured.replace(true) {
            return;
        }
        self.overlay
            .set_cursor(gdk::Cursor::from_name("none", None).as_ref());
        self.hint.set_visible(false);
        if self.inhibit_shortcuts {
            if let Some(tl) = self
                .window
                .surface()
                .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
            {
                tl.inhibit_system_shortcuts(None::<&gdk::Event>);
            }
        }
    }

    fn release(&self) {
        if !self.captured.replace(false) {
            return;
        }
        self.overlay.set_cursor(None);
        self.hint.set_visible(true);
        if let Some(tl) = self
            .window
            .surface()
            .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
        {
            tl.restore_system_shortcuts();
        }
        // Flush everything held so nothing sticks down on the host.
        for vk in self.held_keys.borrow_mut().drain() {
            send(&self.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
        }
        for b in self.held_buttons.borrow_mut().drain() {
            send(&self.connector, InputKind::MouseButtonUp, b, 0, 0, 0);
        }
    }
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

    let hint = gtk::Label::new(Some(
        "Click the stream to capture input · Ctrl+Alt+Shift+Q releases",
    ));
    hint.add_css_class("osd");
    hint.set_halign(gtk::Align::Center);
    hint.set_valign(gtk::Align::End);
    hint.set_margin_bottom(24);
    hint.set_visible(false);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&offload));
    overlay.add_overlay(&stats_label);
    overlay.add_overlay(&hint);
    overlay.set_focusable(true);

    let capture = Rc::new(Capture {
        connector: connector.clone(),
        window: window.clone(),
        overlay: overlay.clone(),
        hint: hint.clone(),
        inhibit_shortcuts,
        captured: Cell::new(false),
        held_keys: RefCell::new(HashSet::new()),
        held_buttons: RefCell::new(HashSet::new()),
    });

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
    // Fullscreen = the stream and nothing else. (Window handlers are disconnected when
    // the page dies — the window outlives every session.)
    let fs_handler = {
        let toolbar = toolbar.clone();
        window.connect_fullscreened_notify(move |w| {
            toolbar.set_reveal_top_bars(!w.is_fullscreen());
        })
    };

    let page = adw::NavigationPage::builder()
        .title(title)
        .tag("stream")
        .child(&toolbar)
        .build();

    // --- Frame consumer: newest texture wins, set on the GTK frame clock's cadence. ---
    {
        let picture = picture.downgrade();
        // The host encodes BT.709 limited-range; without an explicit color state GDK
        // would convert NV12 dmabufs with the (BT.601) dmabuf default.
        let rec709 = {
            let cicp = gdk::CicpParams::new();
            cicp.set_color_primaries(1);
            cicp.set_transfer_function(1);
            cicp.set_matrix_coefficients(1);
            cicp.set_range(gdk::CicpRange::Narrow);
            cicp.build_color_state().ok()
        };
        glib::spawn_future_local(async move {
            while let Ok(f) = frames.recv().await {
                let Some(picture) = picture.upgrade() else {
                    break;
                };
                match f {
                    DecodedFrame::Cpu(c) => {
                        let bytes = glib::Bytes::from_owned(c.rgba);
                        let tex = gdk::MemoryTexture::new(
                            c.width as i32,
                            c.height as i32,
                            gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
                            c.stride,
                        );
                        picture.set_paintable(Some(&tex));
                    }
                    DecodedFrame::Dmabuf(d) => {
                        let mut b = gdk::DmabufTextureBuilder::new()
                            .set_display(&picture.display())
                            .set_width(d.width)
                            .set_height(d.height)
                            .set_fourcc(d.fourcc)
                            .set_modifier(d.modifier)
                            .set_n_planes(d.planes.len() as u32)
                            .set_color_state(rec709.as_ref());
                        for (i, p) in d.planes.iter().enumerate() {
                            b = unsafe { b.set_fd(i as u32, p.fd) }
                                .set_offset(i as u32, p.offset)
                                .set_stride(i as u32, p.stride);
                        }
                        let guard = d.guard;
                        // GDK runs the release func whether the import succeeds or not.
                        match unsafe { b.build_with_release_func(move || drop(guard)) } {
                            Ok(tex) => picture.set_paintable(Some(&tex)),
                            Err(e) => {
                                // Import rejected (format/modifier) — surfaces once per
                                // session in practice; the stream continues on the next
                                // frame, and PUNKTFUNK_DECODER=software is the escape.
                                tracing::warn!(error = %e, "dmabuf texture import failed");
                            }
                        }
                    }
                }
            }
        });
    }

    // --- Keyboard ---
    {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let cap = capture.clone();
        let window_k = window.clone();
        key.connect_key_pressed(move |_, keyval, keycode, state| {
            let chord = gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SHIFT_MASK;
            if state.contains(chord) && keyval.to_lower() == gdk::Key::q {
                if cap.captured.get() {
                    cap.release();
                } else {
                    cap.engage();
                }
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
            if !cap.captured.get() {
                return glib::Propagation::Proceed;
            }
            if let Some(vk) = keycode
                .checked_sub(8)
                .and_then(|c| keymap::evdev_to_vk(c as u16))
            {
                cap.held_keys.borrow_mut().insert(vk);
                send(&cap.connector, InputKind::KeyDown, vk as u32, 0, 0, 0);
            }
            glib::Propagation::Stop
        });
        let cap = capture.clone();
        key.connect_key_released(move |_, _keyval, keycode, _state| {
            if let Some(vk) = keycode
                .checked_sub(8)
                .and_then(|c| keymap::evdev_to_vk(c as u16))
            {
                // Flush-on-release may have beaten us to it — only forward if still held.
                if cap.held_keys.borrow_mut().remove(&vk) {
                    send(&cap.connector, InputKind::KeyUp, vk as u32, 0, 0, 0);
                }
            }
        });
        overlay.add_controller(key);
    }

    // --- Mouse: absolute motion, buttons, wheel — forwarded only while captured ---
    {
        let motion = gtk::EventControllerMotion::new();
        let cap = capture.clone();
        motion.connect_motion(move |_, x, y| {
            if cap.captured.get() {
                send_abs(&cap.overlay, &cap.connector, x, y);
            }
        });
        overlay.add_controller(motion);
    }
    {
        let click = gtk::GestureClick::builder().button(0).build();
        let cap = capture.clone();
        click.connect_pressed(move |g, _n, x, y| {
            cap.overlay.grab_focus();
            if !cap.captured.get() {
                cap.engage(); // the engaging click is suppressed toward the host
                return;
            }
            send_abs(&cap.overlay, &cap.connector, x, y);
            if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
                cap.held_buttons.borrow_mut().insert(gs);
                send(&cap.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
            }
        });
        let cap = capture.clone();
        click.connect_released(move |g, _n, _x, _y| {
            if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
                if cap.held_buttons.borrow_mut().remove(&gs) {
                    send(&cap.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
                }
            }
        });
        overlay.add_controller(click);
    }
    {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let cap = capture.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            if !cap.captured.get() {
                return glib::Propagation::Proceed;
            }
            // The wire carries WHEEL_DELTA(120) units, positive = up / right; GTK's dy is
            // positive = down. Smooth fractions survive — libei's discrete scroll is
            // 120-based too.
            let vy = (-dy * 120.0) as i32;
            if vy != 0 {
                send(&cap.connector, InputKind::MouseScroll, 0, vy, 0, 0);
            }
            let vx = (dx * 120.0) as i32;
            if vx != 0 {
                send(&cap.connector, InputKind::MouseScroll, 1, vx, 0, 0);
            }
            glib::Propagation::Stop
        });
        overlay.add_controller(scroll);
    }

    // --- Capture lifecycle ---
    {
        // Engaged when the stream starts (trust is already confirmed by then).
        let cap = capture.clone();
        overlay.connect_map(move |w| {
            w.grab_focus();
            cap.engage();
        });
    }
    // Focus loss releases (Alt-Tab away, another window) — Swift does the same.
    let active_handler = {
        let cap = capture.clone();
        window.connect_is_active_notify(move |w| {
            if !w.is_active() {
                cap.release();
            }
        })
    };
    {
        let cap = capture.clone();
        overlay.connect_unmap(move |_| cap.release());
    }
    // The page's `hidden` fires once navigation away completes (back button, pop on
    // session end) — NOT on the transient unmap/map cycle a NavigationView push performs.
    {
        let window = window.clone();
        let stop_h = stop.clone();
        let handlers = RefCell::new(Some((fs_handler, active_handler)));
        page.connect_hidden(move |_| {
            tracing::debug!("stream page hidden — ending session");
            if let Some((fs, active)) = handlers.borrow_mut().take() {
                window.disconnect(fs);
                window.disconnect(active);
            }
            if window.is_fullscreen() {
                window.unfullscreen();
            }
            stop_h.store(true, Ordering::SeqCst);
        });
    }

    StreamPage { page, stats_label }
}
