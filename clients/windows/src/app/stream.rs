//! The stream page: a `SwapChainPanel` whose composition swapchain is created (and bound) once on
//! the UI thread, then handed — presenter and all — to the dedicated render thread
//! ([`crate::render`]), which presents decoded frames at stream cadence. The page itself only
//! forwards panel size/DPI changes and draws the status-chip HUD overlay (mode · decode path ·
//! HDR · fps/goodput · end-to-end latency + stage equation · capture hint).

use super::style::{edges, uniform};
use super::Svc;
use crate::present::Presenter;
use crate::render::{self, RenderThread};
use crate::session::Stats;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use std::cell::RefCell;
use std::sync::Arc;
use windows_reactor::*;

/// One HUD refresh: the latest session stats, the input hooks' capture state, and the render
/// thread's display-side window. Mirrored into root state by the poll thread (`pf-hud`) and
/// passed down as a prop.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct HudSample {
    pub(crate) stats: Stats,
    pub(crate) captured: bool,
    /// Whether the stats overlay should be shown — the Settings default at stream start, then
    /// whatever Ctrl+Alt+Shift+S last set (see [`crate::input::hud_visible`]). Carried in the
    /// sample so a live toggle changes the sample and re-renders the page (the stream page is a
    /// child component — only a changed prop re-renders it).
    pub(crate) visible: bool,
    /// The render thread's glass-side window (presents/s, skips, end-to-end p50/p95, display
    /// stage p50) — see [`crate::render::present_stats`].
    pub(crate) present: crate::render::PresentStats,
    /// Spawn mode: the session child's latest formatted `stats:` line, for the status
    /// page. Empty in builtin mode / before the first window.
    pub(crate) stats_line: String,
}

/// Props for the stream page: the services plus the live HUD sample that drives the overlay
/// (compared by value, so each new sample re-renders the overlay).
#[derive(Clone)]
pub(crate) struct StreamProps {
    pub(crate) svc: Svc,
    pub(crate) hud: HudSample,
}

impl PartialEq for StreamProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.hud == other.hud
    }
}

thread_local! {
    /// Frames + host clock offset, stashed by the mount effect for `on_mounted` (which fires
    /// later, once the native panel exists).
    static PENDING: RefCell<Option<(crate::session::FrameRx, i64)>> = const { RefCell::new(None) };
    /// The live render thread; stopped + joined by the unmount cleanup (before panel teardown).
    static RENDER: RefCell<Option<RenderThread>> = const { RefCell::new(None) };
}

/// The app window's DPI (96 when the window can't be found — then DIPs == pixels). Reactor's
/// `on_resize` reports DIPs and exposes no CompositionScale, so the window DPI is the scale.
fn window_dpi() -> u32 {
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        FindWindowW(None, windows::core::w!("Punktfunk"))
            .ok()
            .map(|h| GetDpiForWindow(h))
            .filter(|d| *d > 0)
            .unwrap_or(96)
    }
}

pub(crate) fn stream_page(props: &StreamProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    // Take the connector + frames handoff once on mount; keep the connector alive (and for input)
    // in a use_ref, stash frames for `on_mounted`, install the input hooks. The cleanup stops the
    // render thread FIRST (it must not present into a panel that's tearing down), then removes
    // the input hooks.
    let connector_ref = cx.use_ref::<Option<Arc<NativeClient>>>(None);
    cx.use_effect_with_cleanup((), {
        let shared = ctx.shared.clone();
        let (inhibit, show_hud) = {
            let s = ctx.settings.lock().unwrap();
            (s.inhibit_shortcuts, s.show_hud)
        };
        let connector_ref = connector_ref.clone();
        move || {
            if let Some((connector, frames, stop)) = shared.handoff.lock().unwrap().take() {
                let mode = connector.mode();
                let clock_offset = connector.clock_offset_ns;
                connector_ref.set(Some(connector.clone()));
                PENDING.with(|c| *c.borrow_mut() = Some((frames, clock_offset)));
                crate::input::install(connector, mode, inhibit, show_hud, stop);
            }
            Some(|| {
                RENDER.with(|c| {
                    if let Some(mut rt) = c.borrow_mut().take() {
                        rt.stop_and_join();
                    }
                });
                PENDING.with(|c| c.borrow_mut().take());
                crate::input::uninstall();
            })
        }
    });

    let mode = connector_ref.borrow().as_ref().map(|c| c.mode());
    let host = ctx.shared.target.lock().unwrap().name.clone();
    let mut layers: Vec<Element> = vec![swap_chain_panel()
        .on_mounted(|panel| {
            // Placeholder size — the first `on_resize` (fired after the first layout pass)
            // resizes to the panel's real pixel size.
            let dpi = window_dpi();
            match Presenter::new(1280, 720, dpi) {
                Ok(p) => {
                    if let Err(e) = panel.set_swap_chain(p.swap_chain()) {
                        tracing::error!(error = %e, "set_swap_chain");
                        return;
                    }
                    if let Some((frames, clock_offset)) = PENDING.with(|c| c.borrow_mut().take()) {
                        let shared = render::RenderShared::new(1280, 720, dpi);
                        RENDER.with(|cell| {
                            *cell.borrow_mut() =
                                Some(render::spawn(p, frames, shared, clock_offset));
                        });
                        tracing::info!(dpi, "stream presenter bound — render thread started");
                    }
                }
                Err(e) => tracing::error!(error = %e, "create presenter"),
            }
        })
        .on_resize(|w, h| {
            // DIPs → physical pixels; the presenter maps back via SetMatrixTransform.
            let dpi = window_dpi();
            let px = |v: f64| (v * f64::from(dpi) / 96.0).round() as u32;
            RENDER.with(|cell| {
                if let Some(rt) = cell.borrow().as_ref() {
                    rt.shared().set_dpi(dpi);
                    rt.shared().set_size(px(w), px(h));
                }
            });
        })
        .into()];
    // The overlay follows the LIVE visibility (Settings default, then Ctrl+Alt+Shift+S): the page
    // re-renders on every HUD sample (~400 ms), so a toggle takes effect promptly mid-stream.
    if props.hud.visible {
        layers.push(hud_overlay(&props.hud, mode, &host));
    }
    // Flash the shortcut key set for the first few seconds of every session, regardless of the
    // HUD setting — so "how do I get back out" is answered the moment the stream comes up (parity
    // with the GTK client's stream-start hint). Uptime drives it, so it needs no timer/state: the
    // HUD poll re-renders the page each second and the banner drops once the session passes the
    // threshold.
    if props.hud.stats.uptime_secs < START_HINT_SECS {
        layers.push(start_hint());
    }
    grid(layers).into()
}

/// Spawn mode's Stream screen: the stream runs in the punktfunk-session child's own
/// window, so the shell shows a status card in the app's card language — monogram +
/// host header, the child's live `stats:` line as a chip row + stage lines, the
/// in-window shortcuts, and a Disconnect that kills the child (its exit event routes
/// the app back to the host list, same as the child's window closing). No hooks.
pub(crate) fn session_page(ctx: &Arc<super::AppCtx>, hud: &HudSample) -> Element {
    use super::style::{avatar, card, pill, Pill};
    let host = ctx.shared.target.lock().unwrap().name.clone();
    let title = if host.is_empty() {
        "Streaming".to_string()
    } else {
        format!("Streaming to {host}")
    };

    // Header: monogram + title + the one thing worth knowing (where the video went).
    let header: Element = grid((
        avatar(&host)
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
        vstack((
            text_block(&title).font_size(18.0).semibold(),
            text_block("The stream runs in its own window \u{2014} click it to capture input.")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(2.0)
        .grid_column(1)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(edges(12.0, 0.0, 0.0, 0.0)),
    ))
    .columns([GridLength::Auto, GridLength::Star(1.0)])
    .into();

    // The child prints one formatted stats line per 1 s window:
    // "<mode> · <fps> · <Mb/s> · <path> [· HDR] | e2e … | …" — the first segment becomes
    // a chip row (the decode path gets the status colour), the rest dim stage lines.
    let mut body: Vec<Element> = vec![header];
    if hud.stats_line.is_empty() {
        body.push(
            text_block("Waiting for the first stats window\u{2026}")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .into(),
        );
    } else {
        let mut segments = hud.stats_line.split(" | ");
        if let Some(first) = segments.next() {
            let chips: Vec<Element> = first
                .split(" \u{00B7} ")
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(|c| {
                    let kind = match c {
                        "vulkan" | "vaapi" => Pill::Good,
                        "software" => Pill::Info,
                        _ => Pill::Neutral,
                    };
                    pill(c, kind).into()
                })
                .collect();
            body.push(hstack(chips).spacing(6.0).into());
        }
        for seg in segments {
            body.push(
                text_block(seg.trim())
                    .font_size(11.0)
                    .foreground(ThemeRef::SecondaryText)
                    .into(),
            );
        }
    }

    body.push(
        text_block(
            "Ctrl+Alt+Shift+Q releases input \u{00B7} Ctrl+Alt+Shift+D disconnects \u{00B7} \
             Ctrl+Alt+Shift+S stats \u{00B7} F11 fullscreen",
        )
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .margin(edges(0.0, 4.0, 0.0, 0.0))
        .into(),
    );
    body.push({
        let ctx = ctx.clone();
        button("Disconnect")
            .icon(Symbol::Cancel)
            .on_click(move || {
                // Kill the child; its exit event (the reader thread) navigates to the
                // host list, exactly like the session window closing.
                ctx.shared.session.lock().unwrap().kill();
            })
            .margin(edges(0.0, 6.0, 0.0, 0.0))
            .into()
    });

    // One centred card, sized like the app's dialogs.
    border(card(vstack(body).spacing(12.0).width(520.0)))
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// How long the stream-start shortcut banner stays up (seconds of session uptime).
const START_HINT_SECS: u32 = 6;

/// The stream-start shortcut banner: the full client key set on a translucent pill, bottom-centre,
/// shown for [`START_HINT_SECS`] at the start of every session (see the call site). Independent of
/// the stats overlay, so it appears even with the HUD turned off.
fn start_hint() -> Element {
    border(
        text_block(
            "Click the stream to capture \u{00B7} Ctrl+Alt+Shift+Q releases \u{00B7} \
             Ctrl+Alt+Shift+D disconnects \u{00B7} Ctrl+Alt+Shift+S stats \u{00B7} F11 fullscreen",
        )
        .font_size(12.0)
        .semibold()
        .foreground(Color::rgb(235, 235, 235)),
    )
    .background(Color::rgb(0, 0, 0))
    .corner_radius(10.0)
    .padding(edges(14.0, 8.0, 14.0, 8.0))
    .opacity(0.82)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Bottom)
    .margin(edges(0.0, 0.0, 0.0, 28.0))
    .into()
}

/// A small chip for the dark HUD: coloured text on a translucent dark fill.
fn hud_chip(text: &str, color: Color) -> Border {
    border(
        text_block(text)
            .font_size(11.0)
            .semibold()
            .foreground(color),
    )
    .background(Color::rgb(38, 38, 38))
    .corner_radius(8.0)
    .padding(edges(8.0, 2.0, 8.0, 2.0))
}

/// The negotiated wire codec's display name (`quic::CODEC_*` bit → label).
fn codec_name(bits: u8) -> &'static str {
    match bits {
        punktfunk_core::quic::CODEC_H264 => "H.264",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        _ => "HEVC",
    }
}

/// `mm:ss` (or `h:mm:ss`) session time.
fn fmt_uptime(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The streaming HUD overlay (top-right), unified stats vocabulary (design/stats-unification.md):
/// a chip row (mode · codec · decode path · HDR), a stream line (received fps · goodput ·
/// presenter fps), the end-to-end headline (capture→on-glass p50/p95, host-clock corrected), the
/// stage equation (= host + network + decode + display when the host reports 0xCF timings, else
/// the combined = host+network + decode + display; stage p50s), a session line
/// (host · time · loss/skips), and the shortcut hints. Layered over the `SwapChainPanel` in the
/// same grid cell.
fn hud_overlay(hud: &HudSample, mode: Option<Mode>, host: &str) -> Element {
    let stats = &hud.stats;
    let present = &hud.present;
    let res = mode
        .map(|m| format!("{}\u{00D7}{}@{}", m.width, m.height, m.refresh_hz))
        .unwrap_or_else(|| "\u{2014}".into());
    let mut chips: Vec<Element> = vec![
        hud_chip(&res, Color::rgb(235, 235, 235)).into(),
        hud_chip(codec_name(stats.codec), Color::rgb(180, 190, 255)).into(),
    ];
    chips.push(if stats.hardware {
        hud_chip("GPU decode", Color::rgb(120, 220, 150)).into()
    } else {
        hud_chip("CPU decode", Color::rgb(240, 190, 90)).into()
    });
    if stats.hdr {
        chips.push(hud_chip("HDR", Color::rgb(255, 205, 90)).into());
    }
    // Received fps + goodput, plus the presenter's own rate (Moonlight's "Rendering frame rate"
    // analog — how often the display actually gets a new frame).
    let stream_line = format!(
        "{:.0} fps \u{00B7} {:.1} Mb/s \u{00B7} display {} fps",
        stats.fps, stats.mbps, present.fps
    );
    // The headline: end-to-end capture→displayed, measured directly post-Present (never the sum
    // of the stage percentiles). `(same-host clock)` flags an uncorrected clock (offset == 0:
    // same host, or the host skipped the skew handshake).
    let mut e2e_line = format!(
        "end-to-end {:.1} ms p50 \u{00B7} {:.1} p95 \u{00B7} capture\u{2192}on-glass",
        present.e2e_p50_ms, present.e2e_p95_ms
    );
    if stats.same_host {
        e2e_line.push_str(" (same-host clock)");
    }
    // The equation: the stages tile the headline interval per frame; the window p50s only
    // approximately sum (percentiles aren't additive). With per-AU 0xCF host timings the opaque
    // `host+network` term splits into `host` (host capture→sent) + `network` (the remainder);
    // an old host emits none and the combined term stays.
    let stage_line = if stats.split {
        format!(
            "= host {:.1} + network {:.1} + decode {:.1} + display {:.1}",
            stats.host_ms, stats.net_ms, stats.decode_ms, present.display_p50_ms
        )
    } else {
        format!(
            "= host+network {:.1} + decode {:.1} + display {:.1}",
            stats.hostnet_ms, stats.decode_ms, present.display_p50_ms
        )
    };
    let mut session_bits: Vec<String> = Vec::new();
    if !host.is_empty() {
        session_bits.push(host.to_string());
    }
    // `lost` = unrecoverable network drops (session-cumulative); `skipped` = the render thread's
    // newest-wins drops last window (expected when the stream outpaces the display).
    session_bits.push(fmt_uptime(stats.uptime_secs));
    session_bits.push(format!("{} lost", stats.dropped));
    if present.skipped > 0 {
        session_bits.push(format!("{} skipped", present.skipped));
    }
    let session_line = session_bits.join(" \u{00B7} ");
    let hint = if hud.captured {
        "Ctrl+Alt+Shift+Q releases the mouse \u{00B7} Ctrl+Alt+Shift+D disconnects \u{00B7} \
         Ctrl+Alt+Shift+S stats \u{00B7} F11 fullscreen"
    } else {
        "Click the stream to capture \u{00B7} Ctrl+Alt+Shift+D disconnects \u{00B7} \
         Ctrl+Alt+Shift+S stats \u{00B7} F11 fullscreen"
    };
    let dim = |t: &str| {
        text_block(t)
            .font_size(11.0)
            .foreground(Color::rgb(210, 210, 210))
    };
    border(
        vstack((
            hstack(chips).spacing(6.0),
            dim(&stream_line),
            dim(&e2e_line),
            dim(&stage_line),
            dim(&session_line),
            text_block(hint)
                .font_size(11.0)
                .foreground(Color::rgb(150, 150, 150)),
        ))
        .spacing(6.0),
    )
    .background(Color::rgb(0, 0, 0))
    .corner_radius(10.0)
    .padding(uniform(10.0))
    .opacity(0.82)
    .horizontal_alignment(HorizontalAlignment::Right)
    .vertical_alignment(VerticalAlignment::Top)
    .margin(uniform(12.0))
    .into()
}
