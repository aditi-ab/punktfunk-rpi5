//! This capturer's side of the recovery ladder: what the supervisor is fed beyond the
//! driver's own clocks, and which rungs run on the capture thread.
//!
//! The supervisor and its step walk live in the remote source
//! ([`crate::windows::remote::source`]); this file reads what only the display knows — the
//! pointer for the cursor witness, the topology lock — and runs the canary and the
//! presentation reset. What the witness and the restart in flight mean to the ladder is the
//! tick's ([`crate::windows::tick`]). A rung the stream loop owns (encoder reset, driver
//! cycle) parks in the source instead.

use super::*;
use crate::windows::remote::source::{Local, Rungs};
use pf_frame::recovery::{Stage, StageOutcome};

impl IddPushCapturer {
    /// Feed [`CursorWitness`] the one thing it cannot read for itself. The rule — the one-call
    /// lag and the rate limit — lives there and is tested there.
    pub(super) fn sample_cursor_witness(&mut self) {
        self.cursor.sample(Instant::now(), || {
            let mut pos = POINT::default();
            // SAFETY: plain FFI; `pos` is a valid out-param for this synchronous call.
            unsafe { GetCursorPos(&mut pos) }
                .is_ok()
                .then_some((pos.x, pos.y))
        });
    }

    /// One supervisor tick over the driver's clocks, the tick's cursor travel and restart in
    /// flight, and the topology lock as it reads now.
    pub(super) fn recovery_tick(&mut self, cursor_gap_px: u32, recreating: bool) -> Result<()> {
        let local = Local {
            cursor_gap_px,
            recreating,
            topology_held: pf_win_display::topology_churn::held(),
        };
        let (remote, mut rungs) = self.source_and_rungs();
        remote.supervise(local, &mut rungs)
    }

    /// The remote source and the rungs it drives, borrowed apart so one can run the other.
    pub(super) fn source_and_rungs(&mut self) -> (&mut RemoteSource<IddTransport>, IddRungs<'_>) {
        let rungs = IddRungs {
            ccd: self.ccd,
            target_id: self.target_id,
            mode: (self.width, self.height),
            recovering_since: &mut self.tick.recovering_since,
        };
        (&mut self.remote, rungs)
    }
}

/// The compose canary and the presentation reset, over the display this capturer is bound to.
/// It borrows only the fields the rungs touch, so the remote source beside them can drive it.
pub(super) struct IddRungs<'a> {
    pub(super) ccd: pf_win_display::win_display::CcdTargetKey,
    pub(super) target_id: u32,
    pub(super) mode: (u32, u32),
    pub(super) recovering_since: &'a mut Option<Instant>,
}

impl Rungs for IddRungs<'_> {
    fn canary(&mut self) -> bool {
        let rect = pf_win_display::display_events::snapshot().source_rect(self.ccd);
        let shown = rect.is_some_and(pf_win_display::compose_probe::present);
        if shown {
            tracing::info!(
                target = %self.ccd,
                "IDD push: source suspect on weak evidence — presenting the compose canary"
            );
        }
        shown
    }

    /// Rungs with no actuator on this host report `Unsupported` and the ladder moves on
    /// unpenalised.
    fn run(&mut self, stage: Stage) -> StageOutcome {
        match stage {
            Stage::PresentationReset => {
                if super::capturer::restart_presentation(
                    self.ccd,
                    self.target_id,
                    self.mode,
                    self.recovering_since,
                ) {
                    StageOutcome::Applied
                } else {
                    StageOutcome::Failed
                }
            }
            Stage::SwapChainReset | Stage::EncoderReset | Stage::DriverCycle => {
                StageOutcome::Unsupported
            }
        }
    }
}
