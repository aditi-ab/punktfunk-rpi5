//! The data pump's own recovery asks: a frame whose last shard lands short asks for its
//! missing shards or for recovery, before the next frame shows the gap.

use super::*;
use crate::quic::v2::dgram::NACK_SHORT;

impl DataPump {
    /// Ask for recovery the moment a frame's last shard lands short of what its parity can
    /// rebuild, a frame interval before the next frame shows the gap, so the host's next
    /// encode is the anchor. A frame at most [`NACK_SHORT`] shards past its parity, on a
    /// round trip inside a frame interval, asks for its missing shards instead, and the
    /// frames after it wait for them. One that completed meanwhile asks nothing. The decode
    /// side's gap still arms the freeze. An all-intra frame references nothing, so it asks
    /// only for its shards, and without them goes up partial once its head is whole
    /// ([`Self::release_waiting_partials`]). A stream nobody decodes yet starts on an IDR
    /// and asks nothing.
    pub(super) fn ask_for_short_tails(&mut self) {
        let tails: Vec<u32> = self.session.take_short_tails().collect();
        if tails.is_empty() || !self.shared.frames.consumer_seen() {
            return;
        }
        let all_intra = self.negotiated_codec == crate::quic::CODEC_PYROWAVE;
        let now = Instant::now();
        for idx in tails {
            if !self.session.frame_in_flight(idx) {
                continue;
            }
            if let Some((missing, recovery)) = self.session.missing_beyond_parity(idx) {
                self.shared
                    .short_frames
                    .lock()
                    .unwrap()
                    .note(idx, missing, recovery);
                if missing <= NACK_SHORT && self.ask_nack(idx, now) {
                    continue;
                }
            }
            if all_intra {
                if self.partial_wait.len() < 8 {
                    self.partial_wait.push(idx);
                }
                continue;
            }
            if self.on_anchors {
                continue;
            }
            self.recovery(crate::client::RecoveryEvent::TailShort { index: idx }, now);
        }
        self.release_waiting_partials();
    }

    /// Hand each waiting all-intra frame up partial once its head block is whole; one that
    /// left the reassembler some other way stops waiting.
    pub(super) fn release_waiting_partials(&mut self) {
        let session = &mut self.session;
        self.partial_wait
            .retain(|&idx| session.frame_in_flight(idx) && !session.release_partial(idx, true));
    }

    /// Fold one of the pump's own events into the recovery driver and send what it asks.
    pub(super) fn recovery(&self, ev: crate::client::RecoveryEvent, now: Instant) {
        let ask = self.shared.recovery.lock().unwrap().step(ev, now).ask;
        crate::client::send_recovery(&self.shared, ask);
    }

    /// Ask for frame `idx`'s missing shards when the round trip fits a frame interval, and
    /// hold the frames after it `rtt + frame interval`: the resend waits out the frame the
    /// host is pacing, then the round trip. Past two intervals an RFI costs less. `false`
    /// when it asked nothing.
    fn ask_nack(&mut self, idx: u32, now: Instant) -> bool {
        use crate::quic::v2::dgram::{Nack, NACK_MAX};
        let rtt = Duration::from_micros(u64::from(self.shared.rtt_us.load(Ordering::Relaxed)));
        let period = Duration::from_micros(1_000_000 / u64::from(self.refresh_hz.max(1)));
        if !self.nack || rtt.is_zero() || rtt > period {
            return false;
        }
        let Some(nack) = self
            .session
            .missing_shards(idx, NACK_MAX)
            .and_then(|s| Nack::new(idx, &s))
        else {
            return false;
        };
        if !self.shared.frames.hold(idx, now + rtt + period) {
            return false;
        }
        tracing::debug!(frame = idx, rtt_us = rtt.as_micros() as u64, "nack asked");
        self.shared.ask_nack(nack);
        self.session.note_nack(false);
        true
    }
}
