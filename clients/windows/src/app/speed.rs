//! Per-host network speed test (the GTK/Swift clients' "Test Network Speed…"): connect over the
//! real data plane, have the host burst probe filler for 2 s up to its 3 Gbps ceiling, and
//! report goodput · loss · a recommended bitrate (≈70 % of measured), applied in one tap.

use super::style::*;
use super::{Screen, Svc};
use crate::session::run_speed_probe;
use windows_reactor::*;

/// Speed-test lifecycle. Held as ROOT state (the probe worker completes it via
/// `Svc::set_speed`, and thread-driven updates only re-render through a prop change — see the
/// app module docs). The hosts page resets it to `Running` before navigating here.
#[derive(Clone, PartialEq)]
pub(crate) enum SpeedState {
    Running,
    Failed(String),
    Done {
        mbps: f64,
        loss_pct: f32,
        recommended_kbps: u32,
    },
}

/// Props for the speed page: the services plus the probe lifecycle that drives its re-render.
#[derive(Clone)]
pub(crate) struct SpeedProps {
    pub(crate) svc: Svc,
    pub(crate) state: SpeedState,
}

impl PartialEq for SpeedProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.state == other.state
    }
}

pub(crate) fn speed_page(props: &SpeedProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let set_screen = &props.svc.set_screen;
    let target = ctx.shared.target.lock().unwrap().clone();

    // One probe run per mount (navigating here again re-mounts and re-runs).
    cx.use_effect((), {
        let set_speed = props.svc.set_speed.clone();
        let shared = ctx.shared.clone();
        let identity = ctx.identity.clone();
        let target = target.clone();
        move || {
            use std::sync::atomic::Ordering;
            // The generation the hosts page stamped for THIS run; a stale worker (user backed
            // out and started another test) must not publish over the newer run.
            let generation = shared.speed_gen.load(Ordering::SeqCst);
            std::thread::Builder::new()
                .name("pf-speedtest".into())
                .spawn(move || {
                    let outcome = run_speed_probe(
                        &target.addr,
                        target.port,
                        target.fp_hex.as_deref(),
                        identity,
                    );
                    if shared.speed_gen.load(Ordering::SeqCst) != generation {
                        return; // superseded
                    }
                    set_speed.call(match outcome {
                        Ok(r) => {
                            let mbps = f64::from(r.throughput_kbps) / 1000.0;
                            SpeedState::Done {
                                mbps,
                                loss_pct: r.loss_pct,
                                // ≈70 % of measured: headroom for FEC overhead + real-world loss.
                                recommended_kbps: r.throughput_kbps / 10 * 7,
                            }
                        }
                        Err(msg) => SpeedState::Failed(msg),
                    });
                })
                .ok();
        }
    });

    let back_btn = {
        let ss = set_screen.clone();
        button("Close")
            .icon(Symbol::Back)
            .on_click(move || ss.call(Screen::Hosts))
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    let headline = if target.name.is_empty() {
        "Network speed test".to_string()
    } else {
        format!("Network speed test \u{00B7} {}", target.name)
    };

    match &props.state {
        SpeedState::Running => busy_page(
            &headline,
            "Measuring the path over the real data plane \u{2014} a 2 s probe burst\u{2026}",
            vec![back_btn.into()],
        ),
        SpeedState::Failed(msg) => {
            let content = vstack((
                text_block(headline)
                    .font_size(18.0)
                    .semibold()
                    .horizontal_alignment(HorizontalAlignment::Center),
                InfoBar::new("Speed test failed")
                    .message(msg.clone())
                    .error()
                    .is_closable(false),
                back_btn,
            ))
            .spacing(16.0)
            .max_width(480.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center);
            content.into()
        }
        SpeedState::Done {
            mbps,
            loss_pct,
            recommended_kbps,
        } => {
            let recommended_mbps = f64::from(*recommended_kbps) / 1000.0;
            let apply_btn = {
                let (ctx, ss, kbps) = (ctx.clone(), set_screen.clone(), *recommended_kbps);
                button(format!("Use {recommended_mbps:.0} Mb/s"))
                    .accent()
                    .icon(Symbol::Accept)
                    .on_click(move || {
                        let mut s = ctx.settings.lock().unwrap();
                        s.bitrate_kbps = kbps;
                        s.save();
                        ss.call(Screen::Hosts);
                    })
            };
            let results = card(
                vstack((
                    text_block(format!("{mbps:.0} Mbit/s"))
                        .font_size(34.0)
                        .bold()
                        .horizontal_alignment(HorizontalAlignment::Center),
                    text_block(format!("measured \u{00B7} {loss_pct:.1} % loss"))
                        .font_size(12.0)
                        .foreground(ThemeRef::SecondaryText)
                        .horizontal_alignment(HorizontalAlignment::Center),
                    text_block(format!(
                    "Recommended bitrate: {recommended_mbps:.0} Mb/s (\u{2248}70 % of measured, \
                     leaving headroom for FEC and loss)"
                ))
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText)
                    .horizontal_alignment(HorizontalAlignment::Center),
                    hstack((apply_btn, {
                        let ss = set_screen.clone();
                        button("Close")
                            .icon(Symbol::Cancel)
                            .on_click(move || ss.call(Screen::Hosts))
                    }))
                    .spacing(8.0)
                    .horizontal_alignment(HorizontalAlignment::Center),
                ))
                .spacing(12.0),
            );
            vstack((
                text_block(headline)
                    .font_size(18.0)
                    .semibold()
                    .horizontal_alignment(HorizontalAlignment::Center),
                results,
            ))
            .spacing(16.0)
            .max_width(480.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
        }
    }
}
