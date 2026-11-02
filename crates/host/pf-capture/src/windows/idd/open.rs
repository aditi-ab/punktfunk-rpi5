//! One-shot IDD-push construction: the open's plan run against the display, the WUDFHost
//! duplication broker, the remote source over it, and the cursor opt-in.
//!
//! [`plan_open`] decides every step; [`run_colour`] runs the display's half over the colour
//! seam; `open_inner` runs the rest in plan order and builds the capturer. Steady-state capture
//! (`try_consume`, pollers, `Capturer`) lives in `capturer`. A `#[path]` child sees the
//! parent's private items through `use super::*`. Evidence: `design/idd-push-security.md`.

use super::*;
use crate::windows::cursor_state::Opening;
use crate::windows::depth_pin::{DepthPin, Wanted};
use crate::windows::open_plan::{
    plan_open, run_colour, Diagnostics, DisplayFacts, OpenAsk, OpenStep, OpenedDisplay,
};
use crate::windows::remote::qpc;
use crate::windows::remote::source::Geometry;
use crate::windows::remote::supervisor::Supervision;
use pf_win_display::colour::{DisplayColour, WinColour};

impl IddPushCapturer {
    /// Open the IDD-push capturer. Success attaches `keepalive` (the capturer
    /// owns the virtual display). Failure returns it so the caller retires or
    /// retries the monitor — this function never tears the display down.
    ///
    /// There is no fallback capture path: the driver is the only Windows video
    /// source, and its encoder opens later, at `SET_ENCODE`.
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
        opts: crate::IddPushOpts,
    ) -> std::result::Result<Self, (anyhow::Error, Box<dyn Send>)> {
        // Idempotent: first capturer starts it so stall logs can correlate DWM holes
        // with OS display events for the session's life.
        pf_win_display::display_events::spawn_once();
        match Self::open_inner(target, preferred, opts) {
            Ok(mut me) => {
                me._keepalive = keepalive;
                Ok(me)
            }
            Err(e) => Err((e, keepalive)),
        }
    }

    /// Plan the open, run the display's steps, then the rest in plan order, then build the
    /// capturer over what they opened.
    fn open_inner(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        opts: crate::IddPushOpts,
    ) -> Result<Self> {
        let (pw, ph, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the encoder's input")?;
        // The complete CCD identity every display-global helper selects paths by (the packed
        // LUID in the capture target is the IddCx display adapter's).
        let ccd =
            pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id);
        let colour: Arc<dyn DisplayColour> = Arc::new(WinColour);
        let ask = OpenAsk {
            want_hdr: opts.want_hdr,
            pyrowave: opts.pyrowave,
            cursor_channel: opts.cursor_sender.is_some(),
            cursor_forward: opts.cursor_forward.is_some(),
        };
        let facts = DisplayFacts {
            wide_colour: opts.ten_bit_sdr && super::wcg_available(),
            diag: super::diag_dir().is_some(),
            flow: super::flow_watch(),
        };
        let plan = plan_open(&ask, facts);
        let display = run_colour(&*colour, ccd, target.target_id, (pw, ph), &ask, &plan);
        for verdict in &display.verdicts {
            super::display::apply_wcg_verdict(*verdict);
        }
        let steps = run_steps(&plan, &target, ccd, &display, &opts)?;
        let ((w, h), composed) = (display.mode, display.composed);
        let wanted = Wanted {
            hdr: opts.want_hdr,
            ten_bit_sdr: opts.ten_bit_sdr,
            pyrowave: opts.pyrowave,
        };
        let depth = DepthPin::new(wanted, composed);
        let crate::IddPushOpts {
            want_444,
            cursor_sender,
            cursor_forward,
            set_encode,
            encode_ctl,
            ..
        } = opts;
        let mut me = Self {
            target_id: target.target_id,
            ccd,
            // The format is a placeholder: the tick hands the source its geometry before the
            // first delivery.
            remote: RemoteSource::new(
                IddTransport::new(steps.broker.clone(), ccd, set_encode, encode_ctl),
                Geometry {
                    width: w,
                    height: h,
                    format: PixelFormat::Nv12,
                },
                Supervision::Ladder,
                qpc::CLOCKS,
                Box::new(()),
            ),
            broker: steps.broker,
            width: w,
            height: h,
            desc_poller: DescriptorPoller::spawn(colour.clone(), ccd, depth.descriptor((w, h))),
            depth,
            colour,
            want_444,
            tick: Tick::new(),
            cursor: CursorWitness::new(Instant::now()),
            probes: steps.diagnostics.probes().then(super::probes::acquire),
            etw: (steps.diagnostics.etw())
                .then(super::dxgkrnl_etw::acquire)
                .flatten(),
            cursor_shared: steps.cursor_shared,
            cursor_poll: steps.cursor_poll,
            shape_from_section: None,
            cursor_forward,
            cursor_sender,
            cursor_model: steps.cursor_model,
            sdr_white_scale: 2.5,
            sdr_white_logged: false,
            // Held until the capturer drops, so the display cannot idle off under the session.
            _display_wake: (steps.wake)
                .then(pf_frame::session_tuning::DisplayWakeRequest::new)
                .flatten(),
            // Placeholder. `open()` attaches the real keepalive only on success
            // so a failed open can hand it back.
            _keepalive: Box::new(()),
        };
        log_gpu_posture();
        // The driver's blend needs this and session 0 cannot query it. No-op on SDR.
        me.refresh_sdr_white_scale();
        Ok(me)
    }
}

/// What the open's session steps opened, for the capturer to hold.
struct Opened {
    broker: Arc<ChannelBroker>,
    cursor_shared: Option<cursor::CursorShared>,
    cursor_model: CursorModel,
    cursor_poll: Option<cursor_poll::CursorPoller>,
    diagnostics: Diagnostics,
    wake: bool,
}

/// Run the plan's session steps in order; the display's ran in `run_colour`.
fn run_steps(
    plan: &[OpenStep],
    target: &WinCaptureTarget,
    ccd: pf_win_display::win_display::CcdTargetKey,
    settled: &OpenedDisplay,
    opts: &crate::IddPushOpts,
) -> Result<Opened> {
    let (mut broker, mut cursor_shared, mut cursor) = (None, None, None);
    let (mut diagnostics, mut wake) = (Diagnostics::Off, false);
    for step in plan {
        match *step {
            OpenStep::OpenBroker => {
                broker = Some(Arc::new(ChannelBroker::open(target.wudf_pid)?));
            }
            OpenStep::DeliverCursorChannel => {
                cursor_shared = broker
                    .as_deref()
                    .zip(opts.cursor_sender.as_ref())
                    .and_then(|(broker, send)| cursor_channel(broker, target.target_id, send));
            }
            OpenStep::OpenCursorModel => {
                let channel = cursor_shared.is_some();
                cursor = Some(open_cursor(target, ccd, channel, opts.forwards_to_client));
            }
            OpenStep::ResetCursorForward => {
                let reset = cursor.as_ref().and_then(|c| c.2);
                reset_cursor_forward(reset, opts.cursor_forward.as_ref());
            }
            OpenStep::Bound => tracing::info!(
                target_id = target.target_id,
                wudf_pid = target.wudf_pid,
                mode = format!("{}x{}", settled.mode.0, settled.mode.1),
                display_hdr = settled.composed.hdr,
                display_wcg = settled.composed.wcg,
                want_hdr = opts.want_hdr,
                ten_bit_sdr = opts.ten_bit_sdr,
                want_444 = opts.want_444,
                "IDD push(host): virtual display bound; the driver encodes what DWM composes"
            ),
            OpenStep::Diagnostics(d) => {
                log_diagnostics(d);
                diagnostics = d;
            }
            OpenStep::HoldDisplayWake => wake = true,
            _ => {}
        }
    }
    let (cursor_model, cursor_poll, _) = cursor.context("the open plan opened no cursor")?;
    Ok(Opened {
        broker: broker.context("the open plan opened no broker")?,
        cursor_shared,
        cursor_model,
        cursor_poll,
        diagnostics,
        wake,
    })
}

/// Create the cursor section and hand it to the driver. Non-fatal: without it the driver never
/// declares a hardware cursor, so this session has no pointer to forward and none to blend.
fn cursor_channel(
    broker: &ChannelBroker,
    target_id: u32,
    send: &crate::CursorChannelSender,
) -> Option<cursor::CursorShared> {
    match cursor::CursorShared::create() {
        // Shared helper: also re-delivers after a driver monitor re-arrival.
        Ok(cs) => deliver_cursor_channel(broker, target_id, &cs, send).then_some(cs),
        Err(e) => {
            tracing::warn!(
                "cursor section creation failed — the driver will not declare a \
                 hardware cursor, so this session cannot forward the pointer: {e:#}"
            );
            None
        }
    }
}

/// The cursor render model the session opens in, the GDI poller under any declare (the
/// pointer's position and visibility, and the shape for a cursor the section holds none of),
/// and the forward flag to reset the driver with.
fn open_cursor(
    target: &WinCaptureTarget,
    ccd: pf_win_display::win_display::CcdTargetKey,
    channel: bool,
    client_draws: bool,
) -> (CursorModel, Option<cursor_poll::CursorPoller>, Option<bool>) {
    let (model, reset) = CursorModel::open(Opening {
        excluded: target.cursor_excluded,
        channel,
        client_draws,
    });
    if model.pinned() {
        tracing::info!(
            target_id = target.target_id,
            forwards_to_client = client_draws,
            have_channel = channel,
            "target carries an irrevocable hardware-cursor declare and no client draws the \
             pointer this session — the driver's pool blends it. have_channel=false ⇒ the \
             blend has no shape source either"
        );
    }
    let poll = model.declared().then(|| {
        let rect = pf_win_display::win_display::source_desktop_rect(ccd).unwrap_or((
            0,
            0,
            i32::MAX,
            i32::MAX,
        ));
        cursor_poll::CursorPoller::spawn(ccd, rect)
    });
    (model, poll, reset)
}

/// An earlier session may have died under the secure desktop with the flag down, and delivery
/// would then start undeclared. `poll_secure_desktop` stands it down again if the desktop is
/// still locked.
fn reset_cursor_forward(reset: Option<bool>, forward: Option<&crate::CursorForwardSender>) {
    if let (Some(on), Some(fwd)) = (reset, forward) {
        if let Err(e) = fwd(on) {
            tracing::debug!("cursor-forward reset at open failed: {e:#}");
        }
    }
}

/// The diagnostic posture, once per session: active probes and an ETW session alter the very
/// path a disturbance report describes, so every report needs this A/B label.
fn log_diagnostics(posture: Diagnostics) {
    match (posture, super::diag_dir()) {
        (Diagnostics::Full, Some(dir)) => tracing::info!(
            au_dump_dir = %dir.display(),
            "IDD push: PUNKTFUNK_IDD_DIAG is ON — micro-probes, the DxgKrnl ETW session and \
             the access-unit dump are running for this session"
        ),
        (Diagnostics::Flow, _) => tracing::info!(
            "IDD push: PUNKTFUNK_IDD_FLOW is ON — the DxgKrnl ETW session is running and \
             prints a present-flow line every ten seconds"
        ),
        _ => tracing::info!(
            "IDD push: diagnostics off (set PUNKTFUNK_IDD_DIAG=1 for probes, DxgKrnl ETW and \
             an access-unit dump)"
        ),
    }
}

/// Both REALTIME GPU-priority opt-ins, once per session, with the HAGS setting that decides
/// who schedules them. Stall WARNs repeat the opt-ins only when they fire, so a quiet stalling
/// log would otherwise omit the posture.
fn log_gpu_posture() {
    tracing::info!(
        rt_gpu_driver = super::stall::rt_gpu_driver_posture(),
        rt_gpu_host = super::stall::rt_gpu_host_posture(),
        hags = pf_win_display::hags_setting(),
        "GPU-priority posture for this capture session"
    );
}
