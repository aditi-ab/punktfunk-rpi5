//! The stream page: a `SwapChainPanel` bound to the D3D11 composition swapchain in
//! [`crate::present`], driven by reactor's per-frame `on_rendering`, with a status-chip HUD
//! overlay (mode · decode path · HDR · fps/throughput/latency · capture hint).

use super::style::{edges, uniform};
use super::Svc;
use crate::present::Presenter;
use crate::session::Stats;
use crate::video::DecodedFrame;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use std::cell::RefCell;
use std::sync::Arc;
use windows_reactor::*;

/// One HUD refresh: the latest session stats plus the input hooks' capture state. Mirrored into
/// root state by the poll thread (`pf-hud`) and passed down as a prop.
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) struct HudSample {
    pub(crate) stats: Stats,
    pub(crate) captured: bool,
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

/// UI-thread-only present context: the D3D11 presenter plus the decoded-frame receiver.
struct PresentCtx {
    presenter: Presenter,
    frames: async_channel::Receiver<DecodedFrame>,
}

thread_local! {
    static PRESENT: RefCell<Option<PresentCtx>> = const { RefCell::new(None) };
    static PENDING_FRAMES: RefCell<Option<async_channel::Receiver<DecodedFrame>>> =
        const { RefCell::new(None) };
}

fn present_newest(ctx: &mut PresentCtx) {
    // Apply the latest source HDR mastering metadata (from the session pump's 0xCE drain) before
    // presenting — a cheap no-op in the presenter when unchanged.
    if let Some(meta) = *crate::present::LATEST_HDR_META.lock().unwrap() {
        ctx.presenter.set_hdr_metadata(meta);
    }
    // Drain to the newest decoded frame (drop any backlog) and hand it to the presenter by value —
    // the GPU zero-copy path retains the decoder surface across re-presents, so ownership matters.
    let mut newest = None;
    while let Ok(f) = ctx.frames.try_recv() {
        newest = Some(f);
    }
    ctx.presenter.present(newest);
}

pub(crate) fn stream_page(props: &StreamProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    // Take the connector + frames handoff once on mount; keep the connector alive (and for input)
    // in a use_ref, stash frames for `on_ready`, install the input hooks (and remove on unmount).
    let connector_ref = cx.use_ref::<Option<Arc<NativeClient>>>(None);
    cx.use_effect_with_cleanup((), {
        let shared = ctx.shared.clone();
        let inhibit = ctx.settings.lock().unwrap().inhibit_shortcuts;
        let connector_ref = connector_ref.clone();
        move || {
            if let Some((connector, frames)) = shared.handoff.lock().unwrap().take() {
                let mode = connector.mode();
                connector_ref.set(Some(connector.clone()));
                PENDING_FRAMES.with(|c| *c.borrow_mut() = Some(frames));
                crate::input::install(connector, mode, inhibit);
            }
            Some(crate::input::uninstall)
        }
    });

    let rendering = cx.use_ref::<Option<Rendering>>(None);
    cx.use_effect((), {
        let rendering = rendering.clone();
        move || {
            if let Ok(r) = on_rendering(move || {
                PRESENT.with(|cell| {
                    if let Some(ctx) = cell.borrow_mut().as_mut() {
                        present_newest(ctx);
                    }
                });
            }) {
                rendering.set(Some(r));
            }
        }
    });

    let mode = connector_ref.borrow().as_ref().map(|c| c.mode());
    grid((
        swap_chain_panel()
            .on_ready(|panel| match Presenter::new(1280, 720) {
                Ok(p) => {
                    if let Err(e) = panel.set_swap_chain(p.swap_chain()) {
                        tracing::error!(error = %e, "set_swap_chain");
                    }
                    if let Some(frames) = PENDING_FRAMES.with(|c| c.borrow_mut().take()) {
                        PRESENT.with(|cell| {
                            *cell.borrow_mut() = Some(PresentCtx {
                                presenter: p,
                                frames,
                            });
                        });
                        tracing::info!("stream presenter bound to SwapChainPanel");
                    }
                }
                Err(e) => tracing::error!(error = %e, "create presenter"),
            })
            .on_resize(|w, h| {
                PRESENT.with(|cell| {
                    if let Some(ctx) = cell.borrow_mut().as_mut() {
                        ctx.presenter.resize(w as u32, h as u32);
                    }
                });
            }),
        hud_overlay(&props.hud, mode),
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

/// The streaming HUD overlay (top-right), mirroring the Apple client: a chip row (mode · decode
/// path · HDR), the fps/throughput/latency line, and the capture-state hint. Layered over the
/// `SwapChainPanel` in the same grid cell.
fn hud_overlay(hud: &HudSample, mode: Option<Mode>) -> Element {
    let stats = &hud.stats;
    let res = mode
        .map(|m| format!("{}\u{00D7}{}@{}", m.width, m.height, m.refresh_hz))
        .unwrap_or_else(|| "\u{2014}".into());
    let mut chips: Vec<Element> = vec![hud_chip(&res, Color::rgb(235, 235, 235)).into()];
    chips.push(if stats.hardware {
        hud_chip("GPU decode", Color::rgb(120, 220, 150)).into()
    } else {
        hud_chip("CPU decode", Color::rgb(240, 190, 90)).into()
    });
    if stats.hdr {
        chips.push(hud_chip("HDR", Color::rgb(255, 205, 90)).into());
    }
    let line = format!(
        "{:.0} fps \u{00B7} {:.1} Mb/s \u{00B7} {:.1} ms p50 \u{00B7} decode {:.1} ms",
        stats.fps, stats.mbps, stats.latency_ms, stats.decode_ms
    );
    let hint = if hud.captured {
        "Ctrl+Alt+Shift+Q releases the mouse"
    } else {
        "Click the stream to capture the mouse"
    };
    border(
        vstack((
            hstack(chips).spacing(6.0),
            text_block(line)
                .font_size(11.0)
                .foreground(Color::rgb(210, 210, 210)),
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
