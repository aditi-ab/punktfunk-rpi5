//! [`FakeCapturer`]: a source the stream loops' tests script — frames and events in order,
//! colour as a value, and only the roles a test gives it. Behind `test-support`.

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::Result;
use pf_frame::health::EncoderTelemetry;
use pf_frame::recovery::{Stage, StageOutcome};
use pf_frame::{CapturedFrame, CursorOverlay};

use crate::{
    CaptureHealth, Capturer, ColourFacts, CursorSource, Feed, FirstFrame, SourceEvent, Supervised,
};

/// A scripted source. A role answers only when its field is `Some`.
#[derive(Default)]
pub struct FakeCapturer {
    /// `try_latest` and `next_frame` take one each. Empty, `try_latest` is `Ok(None)`;
    /// `next_frame` fails on an empty script or a scripted `Ok(None)`.
    pub frames: VecDeque<Result<Option<CapturedFrame>>>,
    /// One batch per `events` call.
    pub events: VecDeque<Vec<SourceEvent>>,
    pub colour: ColourFacts,
    /// `is_alive` answers `!dead`.
    pub dead: bool,
    pub feed: Option<FakeFeed>,
    pub cursor: Option<FakeCursor>,
    pub supervised: Option<FakeSupervised>,
}

/// The [`Feed`] role, recording what the loop asked of it.
#[derive(Default)]
pub struct FakeFeed {
    pub keepalive: Option<Box<dyn Send>>,
    /// The last `set_active`.
    pub active: Option<bool>,
    pub waits: Vec<Instant>,
}

/// The [`CursorSource`] role.
#[derive(Default)]
pub struct FakeCursor {
    pub overlay: Option<CursorOverlay>,
    /// Every `set_forward`, in order.
    pub forwards: Vec<bool>,
}

/// The [`Supervised`] role. `health` panics unless the test scripted one.
#[derive(Default)]
pub struct FakeSupervised {
    pub health: Option<CaptureHealth>,
    pub observed: Vec<Option<EncoderTelemetry>>,
    pub stages: Vec<(Stage, StageOutcome)>,
}

impl Capturer for FakeCapturer {
    fn next_frame(&mut self, _budget: FirstFrame) -> Result<CapturedFrame> {
        match self.frames.pop_front() {
            Some(Ok(Some(frame))) => Ok(frame),
            Some(Err(e)) => Err(e),
            Some(Ok(None)) | None => anyhow::bail!("no scripted frame"),
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.frames.pop_front().unwrap_or(Ok(None))
    }

    fn is_alive(&self) -> bool {
        !self.dead
    }

    fn events(&mut self) -> Vec<SourceEvent> {
        self.events.pop_front().unwrap_or_default()
    }

    fn colour(&self) -> ColourFacts {
        self.colour
    }

    fn feed(&mut self) -> Option<&mut dyn Feed> {
        self.feed.as_mut().map(|f| f as &mut dyn Feed)
    }

    fn cursor(&mut self) -> Option<&mut dyn CursorSource> {
        self.cursor.as_mut().map(|c| c as &mut dyn CursorSource)
    }

    fn supervised(&mut self) -> Option<&mut dyn Supervised> {
        self.supervised.as_mut().map(|s| s as &mut dyn Supervised)
    }
}

impl Feed for FakeFeed {
    fn wait_arrival(&mut self, deadline: Instant) {
        self.waits.push(deadline);
    }

    fn set_active(&mut self, active: bool) {
        self.active = Some(active);
    }

    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }
}

impl CursorSource for FakeCursor {
    fn overlay(&mut self) -> Option<CursorOverlay> {
        self.overlay.clone()
    }

    fn set_forward(&mut self, client_draws: bool) {
        self.forwards.push(client_draws);
    }
}

impl Supervised for FakeSupervised {
    fn observe(&mut self, telemetry: Option<EncoderTelemetry>) {
        self.observed.push(telemetry);
    }

    fn health(&self) -> CaptureHealth {
        self.health.clone().expect("a scripted health")
    }

    fn stage_done(&mut self, stage: Stage, outcome: StageOutcome) {
        self.stages.push((stage, outcome));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntheticCapturer;

    #[test]
    fn the_script_plays_in_order_and_only_given_roles_answer() {
        let frame = SyntheticCapturer::new(8, 4, 60)
            .next_frame(FirstFrame::Default)
            .unwrap();
        let mut fake = FakeCapturer {
            frames: [Ok(Some(frame)), Ok(None), Err(anyhow::anyhow!("lost"))].into(),
            events: [vec![SourceEvent::ReferenceRisk]].into(),
            feed: Some(FakeFeed::default()),
            ..Default::default()
        };
        assert!(fake.try_latest().unwrap().is_some());
        assert!(fake.try_latest().unwrap().is_none());
        assert!(fake.try_latest().is_err());
        assert!(
            fake.try_latest().unwrap().is_none(),
            "an empty script repeats"
        );
        assert!(fake.next_frame(FirstFrame::Default).is_err());
        assert_eq!(fake.events(), [SourceEvent::ReferenceRisk]);
        assert!(fake.events().is_empty());
        fake.feed().unwrap().set_active(true);
        assert_eq!(fake.feed.as_ref().unwrap().active, Some(true));
        assert!(fake.cursor().is_none() && fake.supervised().is_none());
        assert!(fake.display().is_none() && fake.remote_encoder().is_none());
    }
}
