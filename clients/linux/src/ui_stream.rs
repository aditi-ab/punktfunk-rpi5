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
use crate::video::{DecodedFrame, DecodedImage};
use adw::prelude::*;
use gtk::{gdk, glib};
use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{InputEvent, InputKind};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct StreamPage {
    pub page: adw::NavigationPage,
    stats_label: gtk::Label,
    /// Median capture→paintable-set latency (ms) over the frame consumer's last 1 s
    /// window — written there, folded into the OSD on each `Stats` event.
    present_ms: Rc<Cell<f32>>,
    /// The stream is HDR (PQ) right now — set by the frame consumer from each frame's
    /// signaling (the host can flip SDR↔HDR mid-session, in-band).
    hdr: Rc<Cell<bool>>,
}

impl StreamPage {
    pub fn update_stats(&self, s: Stats) {
        let mut line = format!(
            "{:.0} fps · {:.1} Mbit/s · dec {:.1} ms · lat {:.1} ms · present {:.1} ms",
            s.fps,
            s.mbps,
            s.decode_ms,
            s.latency_ms,
            self.present_ms.get()
        );
        // Which decoder actually ran this window (vaapi/software) — tracks a fallback.
        if !s.decoder.is_empty() {
            line.push_str(" · ");
            line.push_str(s.decoder);
        }
        if self.hdr.get() {
            line.push_str(" · HDR");
        }
        self.stats_label.set_text(&line);
    }
}

/// Everything the stream page needs from the app + session that own it.
pub struct StreamPageArgs {
    pub window: adw::ApplicationWindow,
    pub connector: Arc<NativeClient>,
    pub frames: async_channel::Receiver<DecodedFrame>,
    /// Host-clock offset from the session's clock handshake — added to the local wall
    /// clock to express paintable-set time in the host's capture clock (present latency).
    pub clock_offset_ns: i64,
    /// Controller escape chord — leave fullscreen + release capture.
    pub escape_rx: async_channel::Receiver<()>,
    /// Escape chord held past the hold threshold — end the session.
    pub disconnect_rx: async_channel::Receiver<()>,
    pub stop: Arc<AtomicBool>,
    /// Grab compositor shortcuts (Alt+Tab, Super…) while input is captured.
    pub inhibit_shortcuts: bool,
    /// Show the stats OSD initially (Settings); Ctrl+Alt+Shift+S toggles it live.
    pub show_stats: bool,
    /// Gaming-Mode launch (`--fullscreen` / Deck env): build the page with NO header bar
    /// at all. gamescope displays the window fullscreen but does not reliably ACK the
    /// xdg_toplevel fullscreen state back, so anything keyed on `is_fullscreen()` (the
    /// reveal-on-notify chrome hiding) may never fire — the title bar would stay drawn
    /// over the stream. Chrome-less by construction cannot regress that way.
    pub chromeless: bool,
    pub title: String,
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
    /// Newest absolute pointer position not yet on the wire. Motion events only store
    /// here; a frame-clock tick flushes at most one `MouseMoveAbs` per tick (a 1000 Hz
    /// mouse would otherwise send a datagram — and take the connector's mode lock — per
    /// event). Button/scroll/key sends flush it first so they land at the latest
    /// position. This client has no relative-motion capture to coalesce — absolute only
    /// (pointer-lock is the stage-2 presenter's job).
    pending_abs: Cell<Option<(f64, f64)>>,
    /// VKs / GameStream button ids currently held — flushed up on release.
    held_keys: RefCell<HashSet<u8>>,
    held_buttons: RefCell<HashSet<u32>>,
}

impl Capture {
    /// Send the coalesced pointer position, if any — one datagram, one fresh mode read.
    fn flush_pending_motion(&self) {
        if let Some((x, y)) = self.pending_abs.take() {
            send_abs(&self.overlay, &self.connector, x, y);
        }
    }

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
        self.pending_abs.set(None); // never flush motion gathered while captured
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

pub fn new(args: StreamPageArgs) -> StreamPage {
    let StreamPageArgs {
        window,
        connector,
        frames,
        clock_offset_ns,
        escape_rx,
        disconnect_rx,
        stop,
        inhibit_shortcuts,
        show_stats,
        chromeless,
        title,
    } = args;
    let w = build_widgets(&window, &title, chromeless);
    w.stats_label.set_visible(show_stats);

    let capture = Rc::new(Capture {
        connector,
        window: window.clone(),
        overlay: w.overlay.clone(),
        hint: w.hint.clone(),
        inhibit_shortcuts,
        captured: Cell::new(false),
        pending_abs: Cell::new(None),
        held_keys: RefCell::new(HashSet::new()),
        held_buttons: RefCell::new(HashSet::new()),
    });

    let present_ms = Rc::new(Cell::new(0.0f32));
    let hdr = Rc::new(Cell::new(false));
    spawn_frame_consumer(
        &w.picture,
        frames,
        clock_offset_ns,
        present_ms.clone(),
        hdr.clone(),
    );
    attach_keyboard(&w.overlay, &window, &capture, &stop, &w.stats_label);
    attach_mouse(&w.overlay, &capture);
    attach_scroll(&w.overlay, &capture);
    if !chromeless {
        attach_edge_reveal(&w.toolbar, &w.overlay, &window, &capture);
    }
    let active_handler = attach_capture_lifecycle(&w.overlay, &window, &capture);
    let escape_future = spawn_escape_watch(&window, &capture, escape_rx);
    let disconnect_future = spawn_disconnect_watch(&window, &capture, &stop, disconnect_rx);
    wire_teardown(
        &w.page,
        &window,
        &stop,
        (w.fs_handler, active_handler),
        escape_future,
        disconnect_future,
    );

    StreamPage {
        page: w.page,
        stats_label: w.stats_label,
        present_ms,
        hdr,
    }
}

/// The page's widget tree, built in one place so `new` reads as assembly.
struct PageWidgets {
    picture: gtk::Picture,
    stats_label: gtk::Label,
    hint: gtk::Label,
    overlay: gtk::Overlay,
    toolbar: adw::ToolbarView,
    page: adw::NavigationPage,
    /// Fullscreen-notify handler on the shared window — disconnected on page teardown.
    fs_handler: glib::SignalHandlerId,
}

/// The offloaded picture under an overlay (stats HUD, capture hint, fullscreen hint), a
/// header bar with the fullscreen toggle, and the window's fullscreen behavior.
/// `chromeless` (Gaming Mode) builds NO header bar at all — see `StreamPageArgs`.
fn build_widgets(window: &adw::ApplicationWindow, title: &str, chromeless: bool) -> PageWidgets {
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
        "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats",
    ));
    hint.add_css_class("osd");
    hint.set_halign(gtk::Align::Center);
    hint.set_valign(gtk::Align::End);
    hint.set_margin_bottom(24);
    hint.set_visible(false);

    // Flashed when entering fullscreen — the exit affordances once the header bar is
    // hidden (F11 on a keyboard; the top-edge pointer reveal for mouse/trackpad-only
    // devices; the L1+R1+Start+Select chord on a controller). Gaming Mode has no F11,
    // no header to reveal, and Steam owns window management — only the chord applies.
    let fs_hint = gtk::Label::new(Some(if chromeless {
        "L1 + R1 + Start + Select  —  leave the stream (hold to disconnect)"
    } else {
        "F11  ·  mouse to the top edge  ·  L1 + R1 + Start + Select  —  exit fullscreen (hold to disconnect)"
    }));
    fs_hint.add_css_class("osd");
    fs_hint.set_halign(gtk::Align::Center);
    fs_hint.set_valign(gtk::Align::Start);
    fs_hint.set_margin_top(12);
    fs_hint.set_visible(false);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&offload));
    overlay.add_overlay(&stats_label);
    overlay.add_overlay(&hint);
    overlay.add_overlay(&fs_hint);
    overlay.set_focusable(true);

    let toolbar = adw::ToolbarView::new();
    if !chromeless {
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
        toolbar.add_top_bar(&header);
    } else {
        // No header exists to hide, and gamescope may never ACK fullscreen — flash the
        // chord hint when the stream maps instead of on the fullscreened notify.
        let fs_hint = fs_hint.clone();
        overlay.connect_map(move |_| {
            fs_hint.set_visible(true);
            let fs_hint = fs_hint.clone();
            glib::timeout_add_seconds_local_once(4, move || fs_hint.set_visible(false));
        });
    }
    toolbar.set_content(Some(&overlay));
    // Fullscreen = the stream and nothing else. (Window handlers are disconnected when
    // the page dies — the window outlives every session.)
    let fs_handler = {
        let toolbar = toolbar.clone();
        let fs_hint = fs_hint.clone();
        window.connect_fullscreened_notify(move |w| {
            let fs = w.is_fullscreen();
            toolbar.set_reveal_top_bars(!fs);
            if chromeless {
                return; // the map handler above owns the hint; there is no bar to reveal
            }
            if fs {
                fs_hint.set_visible(true);
                let fs_hint = fs_hint.clone();
                glib::timeout_add_seconds_local_once(4, move || fs_hint.set_visible(false));
            } else {
                fs_hint.set_visible(false);
            }
        })
    };

    let page = adw::NavigationPage::builder()
        .title(title)
        .tag("stream")
        .child(&toolbar)
        .build();

    PageWidgets {
        picture,
        stats_label,
        hint,
        overlay,
        toolbar,
        page,
        fs_handler,
    }
}

/// Fullscreen chrome recovery for pointer-only devices (a Deck desktop has no F11): while
/// fullscreen and NOT captured, bumping the pointer against the top edge reveals the header
/// bar (back button, fullscreen toggle); moving back into the stream hides it again. While
/// captured the pointer belongs to the host — nothing reveals, and a still-revealed bar is
/// re-hidden on the first captured movement (release capture first: Ctrl+Alt+Shift+Q).
fn attach_edge_reveal(
    toolbar: &adw::ToolbarView,
    overlay: &gtk::Overlay,
    window: &adw::ApplicationWindow,
    capture: &Rc<Capture>,
) {
    let motion = gtk::EventControllerMotion::new();
    let toolbar = toolbar.clone();
    let window = window.clone();
    let cap = capture.clone();
    motion.connect_motion(move |_, _x, y| {
        if !window.is_fullscreen() {
            return; // windowed chrome is the fullscreened-notify handler's business
        }
        if cap.captured.get() {
            if toolbar.reveals_top_bars() {
                toolbar.set_reveal_top_bars(false);
            }
            return;
        }
        if y <= 2.0 {
            toolbar.set_reveal_top_bars(true);
        } else if y > 4.0 && toolbar.reveals_top_bars() {
            // Once revealed the content sits below the bar, so y stays small while the
            // pointer hovers the boundary; anything deeper means the user moved back in.
            toolbar.set_reveal_top_bars(false);
        }
    });
    overlay.add_controller(motion);
}

/// Frame consumer: each decoded frame becomes the picture's paintable as soon as it
/// arrives (the session's tiny `force_send` queue already dropped anything older); GTK
/// then draws whatever paintable is current on its own frame clock. Ends itself when the
/// channel closes or the picture is gone.
///
/// Also the capture→present-ish measurement point: at each paintable set the frame's
/// host capture pts is compared against the local wall clock expressed in the host clock
/// (`clock_offset_ns`, same math as the session's decode latency). This is
/// capture→paintable-SET — GTK's own present adds one compositor cycle after this. The
/// 1 s p50 lands on the stats OSD (via `present_ms`) and in a "present window" debug
/// line for headless validation.
/// One-entry cache of `ColorDesc` → `GdkColorState` (signaling changes at most on an
/// SDR↔HDR flip, never per frame).
#[derive(Default)]
struct ColorStateCache(Option<(crate::video::ColorDesc, Option<gdk::ColorState>)>);

impl ColorStateCache {
    /// The color state for a frame's signaling. `rgb` = the pixels are already full-range
    /// RGB (the CPU path — only transfer + primaries remain meaningful); else YUV, where
    /// H.273 "unspecified" (2) fills in as BT.709 limited, the host's SDR default. `None`
    /// = GDK can't represent the combo — the caller's default (sRGB) applies, which
    /// matches the pre-color-management behavior.
    fn get(&mut self, desc: crate::video::ColorDesc, rgb: bool) -> Option<gdk::ColorState> {
        if let Some((cached, state)) = &self.0 {
            if *cached == desc {
                return state.clone();
            }
        }
        let def = |v: u8, d: u32| if v == 2 { d } else { u32::from(v) };
        let cicp = gdk::CicpParams::new();
        if rgb {
            cicp.set_color_primaries(def(desc.primaries, 1));
            cicp.set_transfer_function(def(desc.transfer, 13)); // 13 = sRGB
            cicp.set_matrix_coefficients(0); // identity — the matrix is already undone
            cicp.set_range(gdk::CicpRange::Full);
        } else {
            cicp.set_color_primaries(def(desc.primaries, 1));
            cicp.set_transfer_function(def(desc.transfer, 1));
            cicp.set_matrix_coefficients(def(desc.matrix, 1));
            cicp.set_range(if desc.full_range {
                gdk::CicpRange::Full
            } else {
                gdk::CicpRange::Narrow
            });
        }
        let state = cicp.build_color_state().ok();
        if state.is_none() {
            tracing::warn!(
                ?desc,
                "GDK can't represent this colour signaling — using default"
            );
        }
        self.0 = Some((desc, state.clone()));
        state
    }
}

fn spawn_frame_consumer(
    picture: &gtk::Picture,
    frames: async_channel::Receiver<DecodedFrame>,
    clock_offset_ns: i64,
    present_ms: Rc<Cell<f32>>,
    hdr: Rc<Cell<bool>>,
) {
    let picture = picture.downgrade();
    // The colour state follows the FRAMES' own signaling (the Windows host switches an HDR
    // desktop to BT.2020 PQ in-band while the Welcome still says SDR): unspecified falls
    // back to BT.709 limited — without an explicit state GDK would convert NV12 dmabufs
    // with the (BT.601) dmabuf default. Cached per distinct signaling; a change mid-stream
    // (SDR↔HDR flip) just rebuilds once.
    let mut yuv_state = ColorStateCache::default();
    let mut rgb_state = ColorStateCache::default();
    glib::spawn_future_local(async move {
        let mut win_lat_us: Vec<u64> = Vec::with_capacity(256);
        let mut win_start = Instant::now();
        while let Ok(f) = frames.recv().await {
            let Some(picture) = picture.upgrade() else {
                break;
            };
            let mut presented = false;
            match &f.image {
                DecodedImage::Cpu(c) => hdr.set(c.color.is_pq()),
                DecodedImage::Dmabuf(d) => hdr.set(d.color.is_pq()),
            }
            match f.image {
                DecodedImage::Cpu(c) => {
                    let bytes = glib::Bytes::from_owned(c.rgba);
                    // swscale undid the YUV matrix (full-range RGB) — but a PQ/BT.2020
                    // stream keeps transfer + primaries baked in, so tag the texture and
                    // let GTK tone-map. Plain SDR keeps the untagged (sRGB) fast path.
                    let tagged = (c.color.is_pq() || c.color.primaries == 9)
                        .then(|| rgb_state.get(c.color, true))
                        .flatten();
                    let tex: gdk::Texture = if let Some(state) = tagged {
                        gdk::MemoryTextureBuilder::new()
                            .set_width(c.width as i32)
                            .set_height(c.height as i32)
                            .set_format(gdk::MemoryFormat::R8g8b8a8)
                            .set_bytes(Some(&bytes))
                            .set_stride(c.stride)
                            .set_color_state(&state)
                            .build()
                            .upcast()
                    } else {
                        gdk::MemoryTexture::new(
                            c.width as i32,
                            c.height as i32,
                            gdk::MemoryFormat::R8g8b8a8,
                            &bytes,
                            c.stride,
                        )
                        .upcast()
                    };
                    picture.set_paintable(Some(&tex));
                    presented = true;
                }
                DecodedImage::Dmabuf(d) => {
                    let mut b = gdk::DmabufTextureBuilder::new()
                        .set_display(&picture.display())
                        .set_width(d.width)
                        .set_height(d.height)
                        .set_fourcc(d.fourcc)
                        .set_modifier(d.modifier)
                        .set_n_planes(d.planes.len() as u32)
                        .set_color_state(yuv_state.get(d.color, false).as_ref());
                    for (i, p) in d.planes.iter().enumerate() {
                        b = unsafe { b.set_fd(i as u32, p.fd) }
                            .set_offset(i as u32, p.offset)
                            .set_stride(i as u32, p.stride);
                    }
                    let guard = d.guard;
                    // GDK runs the release func whether the import succeeds or not.
                    match unsafe { b.build_with_release_func(move || drop(guard)) } {
                        Ok(tex) => {
                            picture.set_paintable(Some(&tex));
                            presented = true;
                        }
                        Err(e) => {
                            // Import rejected (format/modifier) — surfaces once per
                            // session in practice; the stream continues on the next
                            // frame, and PUNKTFUNK_DECODER=software is the escape.
                            tracing::warn!(error = %e, "dmabuf texture import failed");
                        }
                    }
                }
            }
            // Capture→paintable-set latency, host-clock corrected (same math and sanity
            // bound as the session's decode-latency window).
            if presented {
                let lat = (crate::session::now_ns() as i128 + clock_offset_ns as i128
                    - f.pts_ns as i128)
                    .max(0) as u64;
                if lat > 0 && lat < 10_000_000_000 {
                    win_lat_us.push(lat / 1000);
                }
            }
            if win_start.elapsed() >= Duration::from_secs(1) {
                win_lat_us.sort_unstable();
                let p50 = win_lat_us.get(win_lat_us.len() / 2).copied().unwrap_or(0);
                tracing::debug!(
                    frames = win_lat_us.len(),
                    present_p50_us = p50,
                    "present window"
                );
                present_ms.set(p50 as f32 / 1000.0);
                win_lat_us.clear();
                win_start = Instant::now();
            }
        }
    });
}

/// Keyboard, capture-phase: the release (Ctrl+Alt+Shift+Q) / disconnect (Ctrl+Alt+Shift+D)
/// / stats (Ctrl+Alt+Shift+S) chords and F11 are handled locally; everything else becomes
/// a VK on the wire while captured.
fn attach_keyboard(
    overlay: &gtk::Overlay,
    window: &adw::ApplicationWindow,
    capture: &Rc<Capture>,
    stop: &Arc<AtomicBool>,
    stats: &gtk::Label,
) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let cap = capture.clone();
    let window_k = window.clone();
    let stop_kb = stop.clone();
    let stats = stats.clone();
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
        // Ctrl+Alt+Shift+D — leave the session. Now that Steam / QAM pass through to the host,
        // the capture toggle alone can't end a stream, so this is the keyboard's explicit exit.
        if state.contains(chord) && keyval.to_lower() == gdk::Key::d {
            cap.release();
            stop_kb.store(true, Ordering::SeqCst);
            return glib::Propagation::Stop;
        }
        // Ctrl+Alt+Shift+S — toggle the stats OSD live (initial state = Settings).
        if state.contains(chord) && keyval.to_lower() == gdk::Key::s {
            stats.set_visible(!stats.is_visible());
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
            // Keep the wire ordered: the host must see the cursor where the user does
            // when the key lands (e.g. "press E at the crosshair").
            cap.flush_pending_motion();
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

/// Mouse: absolute motion + buttons — forwarded only while captured; the click that
/// engages capture is suppressed toward the host. Motion is COALESCED: each event only
/// stores the newest position; the overlay's frame-clock tick flushes at most one
/// `MouseMoveAbs` per tick (the paintable set on every stream frame keeps the clock
/// ticking while streaming). Buttons flush the pending position first so a click lands
/// exactly where the cursor last was.
fn attach_mouse(overlay: &gtk::Overlay, capture: &Rc<Capture>) {
    let motion = gtk::EventControllerMotion::new();
    let cap = capture.clone();
    motion.connect_motion(move |_, x, y| {
        if cap.captured.get() {
            cap.pending_abs.set(Some((x, y)));
        }
    });
    overlay.add_controller(motion);

    // The per-tick flush. (The tick callback dies with the overlay, so no teardown.)
    let cap = capture.clone();
    overlay.add_tick_callback(move |_, _| {
        cap.flush_pending_motion();
        glib::ControlFlow::Continue
    });

    let click = gtk::GestureClick::builder().button(0).build();
    let cap = capture.clone();
    click.connect_pressed(move |g, _n, x, y| {
        cap.overlay.grab_focus();
        if !cap.captured.get() {
            cap.engage(); // the engaging click is suppressed toward the host
            return;
        }
        // The click's own coordinates are the freshest position — supersede any pending
        // motion, then flush so the button-down lands there.
        cap.pending_abs.set(Some((x, y)));
        cap.flush_pending_motion();
        if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
            cap.held_buttons.borrow_mut().insert(gs);
            send(&cap.connector, InputKind::MouseButtonDown, gs, 0, 0, 0);
        }
    });
    let cap = capture.clone();
    click.connect_released(move |g, _n, _x, _y| {
        cap.flush_pending_motion(); // the release must not beat the motion before it
        if let Some(gs) = keymap::gdk_button_to_gs(g.current_button()) {
            if cap.held_buttons.borrow_mut().remove(&gs) {
                send(&cap.connector, InputKind::MouseButtonUp, gs, 0, 0, 0);
            }
        }
    });
    overlay.add_controller(click);
}

/// Wheel — forwarded only while captured.
fn attach_scroll(overlay: &gtk::Overlay, capture: &Rc<Capture>) {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let cap = capture.clone();
    scroll.connect_scroll(move |_, dx, dy| {
        if !cap.captured.get() {
            return glib::Propagation::Proceed;
        }
        cap.flush_pending_motion(); // scroll happens at the latest cursor position
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

/// Capture lifecycle: engaged when the page maps (the stream just started — trust is
/// already confirmed by then), released on focus loss (Alt-Tab away, another window —
/// Swift does the same) and on unmap. Returns the window-level focus handler for
/// teardown (the window outlives the page).
fn attach_capture_lifecycle(
    overlay: &gtk::Overlay,
    window: &adw::ApplicationWindow,
    capture: &Rc<Capture>,
) -> glib::SignalHandlerId {
    {
        let cap = capture.clone();
        overlay.connect_map(move |w| {
            w.grab_focus();
            cap.engage();
        });
    }
    {
        let cap = capture.clone();
        overlay.connect_unmap(move |_| cap.release());
    }
    let cap = capture.clone();
    window.connect_is_active_notify(move |w| {
        if !w.is_active() {
            cap.release();
        }
    })
}

/// Controller escape chord (gamepad service) → leave fullscreen + release capture. The
/// chord is the only fullscreen exit a controller has (no F11 key; fullscreen hides the
/// chrome). Aborted on page-hidden so a stale future can't act on the shared window.
fn spawn_escape_watch(
    window: &adw::ApplicationWindow,
    capture: &Rc<Capture>,
    escape_rx: async_channel::Receiver<()>,
) -> glib::JoinHandle<()> {
    let window = window.clone();
    let cap = capture.clone();
    glib::spawn_future_local(async move {
        while escape_rx.recv().await.is_ok() {
            if window.is_fullscreen() {
                window.unfullscreen();
            }
            cap.release();
        }
    })
}

/// Controller disconnect (escape chord held past the hold threshold) → end the session,
/// the controller equivalent of Ctrl+Alt+Shift+D. Setting `stop` ends the session pump,
/// which pops this page (and fires `hidden` — see `wire_teardown`). One-shot — the
/// session is going away.
fn spawn_disconnect_watch(
    window: &adw::ApplicationWindow,
    capture: &Rc<Capture>,
    stop: &Arc<AtomicBool>,
    disconnect_rx: async_channel::Receiver<()>,
) -> glib::JoinHandle<()> {
    let window = window.clone();
    let cap = capture.clone();
    let stop_d = stop.clone();
    glib::spawn_future_local(async move {
        if disconnect_rx.recv().await.is_ok() {
            cap.release();
            if window.is_fullscreen() {
                window.unfullscreen();
            }
            stop_d.store(true, Ordering::SeqCst);
        }
    })
}

/// The page's `hidden` fires once navigation away completes (back button, pop on
/// session end) — NOT on the transient unmap/map cycle a NavigationView push performs:
/// disconnect the window-level handlers, abort the chord futures, and stop the session.
fn wire_teardown(
    page: &adw::NavigationPage,
    window: &adw::ApplicationWindow,
    stop: &Arc<AtomicBool>,
    handlers: (glib::SignalHandlerId, glib::SignalHandlerId),
    escape_future: glib::JoinHandle<()>,
    disconnect_future: glib::JoinHandle<()>,
) {
    let window = window.clone();
    let stop_h = stop.clone();
    let handlers = RefCell::new(Some(handlers));
    let escape_future = RefCell::new(Some(escape_future));
    let disconnect_future = RefCell::new(Some(disconnect_future));
    page.connect_hidden(move |_| {
        tracing::debug!("stream page hidden — ending session");
        if let Some((fs, active)) = handlers.borrow_mut().take() {
            window.disconnect(fs);
            window.disconnect(active);
        }
        if let Some(f) = escape_future.borrow_mut().take() {
            f.abort();
        }
        if let Some(f) = disconnect_future.borrow_mut().take() {
            f.abort();
        }
        if window.is_fullscreen() {
            window.unfullscreen();
        }
        stop_h.store(true, Ordering::SeqCst);
    });
}
