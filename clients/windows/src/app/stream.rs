//! The stream page: a `SwapChainPanel` whose composition swapchain is created (and bound) once on
//! the UI thread, then handed — presenter and all — to the dedicated render thread
//! ([`crate::render`]), which presents decoded frames at stream cadence. The page itself only
//! forwards panel size/DPI changes and draws the status-chip HUD overlay (mode · decode path ·
//! HDR · fps/throughput/latency · capture hint).

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
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) struct HudSample {
    pub(crate) stats: Stats,
    pub(crate) captured: bool,
    /// `(presents/s, skipped/s, capture→presented p50 ms)` — see [`crate::render::present_stats`].
    pub(crate) present: (u32, u32, f32),
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
    /// Frames + host clock offset, stashed by the mount effect for `on_ready` (which fires later,
    /// once the native panel exists).
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
    // in a use_ref, stash frames for `on_ready`, install the input hooks. The cleanup stops the
    // render thread FIRST (it must not present into a panel that's tearing down), then removes
    // the input hooks.
    let connector_ref = cx.use_ref::<Option<Arc<NativeClient>>>(None);
    cx.use_effect_with_cleanup((), {
        let shared = ctx.shared.clone();
        let inhibit = ctx.settings.lock().unwrap().inhibit_shortcuts;
        let connector_ref = connector_ref.clone();
        move || {
            if let Some((connector, frames, stop)) = shared.handoff.lock().unwrap().take() {
                let mode = connector.mode();
                let clock_offset = connector.clock_offset_ns;
                connector_ref.set(Some(connector.clone()));
                PENDING.with(|c| *c.borrow_mut() = Some((frames, clock_offset)));
                crate::input::install(connector, mode, inhibit, stop);
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
    grid((
        swap_chain_panel()
            .on_ready(|panel| {
                // Placeholder size — the first `on_resize` (fired after the first layout pass)
                // resizes to the panel's real pixel size.
                let dpi = window_dpi();
                match Presenter::new(1280, 720, dpi) {
                    Ok(p) => {
                        if let Err(e) = panel.set_swap_chain(p.swap_chain()) {
                            tracing::error!(error = %e, "set_swap_chain");
                            return;
                        }
                        if let Some((frames, clock_offset)) =
                            PENDING.with(|c| c.borrow_mut().take())
                        {
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
            }),
        hud_overlay(&props.hud, mode, &host),
    ))
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

/// The streaming HUD overlay (top-right), mirroring the Apple client: a chip row (mode · codec ·
/// decode path · HDR), a stream line (decode fps / bitrate / decode time), a glass line (display
/// presents + end-to-end latency decoded vs on-glass), a session line (host · time · loss), and
/// the shortcut hints. Layered over the `SwapChainPanel` in the same grid cell.
fn hud_overlay(hud: &HudSample, mode: Option<Mode>, host: &str) -> Element {
    let stats = &hud.stats;
    let (pfps, skipped, glass_ms) = hud.present;
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
    let stream_line = format!(
        "{:.0} fps \u{00B7} {:.1} Mb/s \u{00B7} decode {:.1} ms",
        stats.fps, stats.mbps, stats.decode_ms
    );
    // End-to-end latency (host-clock corrected): capture→decoded from the pump, capture→on-glass
    // from the render thread's post-Present stamp. `skipped` = newest-wins drops (expected when
    // the stream outpaces the display); `lost` = unrecoverable network drops.
    let glass_line = format!(
        "display {pfps} fps \u{00B7} latency {:.1} ms decoded / {glass_ms:.1} ms on-glass",
        stats.latency_ms
    );
    let mut session_bits: Vec<String> = Vec::new();
    if !host.is_empty() {
        session_bits.push(host.to_string());
    }
    session_bits.push(fmt_uptime(stats.uptime_secs));
    session_bits.push(format!("{} lost", stats.dropped));
    if skipped > 0 {
        session_bits.push(format!("{skipped} skipped"));
    }
    let session_line = session_bits.join(" \u{00B7} ");
    let hint = if hud.captured {
        "Ctrl+Alt+Shift+Q releases the mouse \u{00B7} Ctrl+Alt+Shift+D disconnects"
    } else {
        "Click the stream to capture \u{00B7} Ctrl+Alt+Shift+D disconnects"
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
            dim(&glass_line),
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
