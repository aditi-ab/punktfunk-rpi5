//! DualSense pad audio (`0xD1`), the platform-free half: the mixer that interleaves the haptics
//! and speaker lanes into the pad's four-channel frame, seq-gap PLC sizing, and the liveness
//! clock that hands the coils between haptics and wire rumble. Decoding and the sink stay in
//! each client.

use crate::audio::{AudioGapTracker, SAMPLE_RATE_HZ};
use crate::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Speaker FL/FR on 0/1, voice coils on 2/3: the DualSense USB audio function's layout.
pub const PAD_CHANNELS: usize = 4;

/// A kind silent this long stops holding the other back: the host gate closed its lane.
/// 25 ms = 2.5 speaker frames.
pub const LANE_LIVE: Duration = Duration::from_millis(25);

/// Interleave the two stereo lanes into one four-channel stream.
///
/// The kinds arrive on different cadences (haptics 5 ms, speaker 10 ms), so each has its own
/// write cursor and [`pop`](Self::pop) emits what every live kind has written. A kind that has
/// not pushed for [`LANE_LIVE`] reads silence, so a haptics-only session plays the coils with a
/// silent speaker pair instead of stalling. `S` is the sink's sample type.
pub struct QuadMixer<S> {
    /// Interleaved four-channel samples; the front is the next frame out. Always
    /// `ready_frames() * PAD_CHANNELS` long.
    ring: VecDeque<S>,
    /// Per-kind write cursor in frames from the ring front, indexed by wire `kind`.
    written: [usize; 2],
    /// Per-kind last push; live within [`LANE_LIVE`].
    pushed: [Option<Instant>; 2],
    /// Frames dropped to the ceiling: a stalled sink.
    dropped: u64,
    max_frames: usize,
}

impl<S: Copy + Default> QuadMixer<S> {
    /// `max_frames` caps decoder backlog when the sink stalls; overflow drops the oldest.
    pub fn new(max_frames: usize) -> QuadMixer<S> {
        QuadMixer {
            ring: VecDeque::new(),
            written: [0; 2],
            pushed: [None; 2],
            dropped: 0,
            max_frames,
        }
    }

    /// Write one decoded stereo chunk for `kind` at that kind's cursor. Both cursors shift
    /// together on overflow, so the kinds never skew. An unknown kind is dropped: folding it into
    /// the coil pair would play an unknown stream on the actuators.
    pub fn push(&mut self, kind: u8, stereo: &[S], now: Instant) {
        let (k, off) = match kind {
            PAD_AUDIO_KIND_HAPTICS => (0usize, 2usize),
            PAD_AUDIO_KIND_SPEAKER => (1, 0),
            _ => return,
        };
        self.pushed[k] = Some(now);
        let frames = stereo.len() / 2;
        let base = self.written[k];
        let need = (base + frames) * PAD_CHANNELS;
        if self.ring.len() < need {
            self.ring.resize(need, S::default());
        }
        for (i, fr) in stereo.chunks_exact(2).enumerate() {
            let at = (base + i) * PAD_CHANNELS + off;
            self.ring[at] = fr[0];
            self.ring[at + 1] = fr[1];
        }
        self.written[k] = base + frames;
        let over = self.ready_frames().saturating_sub(self.max_frames);
        if over > 0 {
            self.dropped += over as u64;
            self.drop_front(over);
        }
    }

    /// Frames ready to output: the further-ahead kind's cursor.
    pub fn ready_frames(&self) -> usize {
        self.written[0].max(self.written[1])
    }

    /// Frames dropped to the ceiling since construction.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Append the frames every live kind has written to `out`, or every ready frame once none is
    /// live; returns the frame count. Popping past a live kind's cursor zeros its pair for that
    /// span. Both cursors move back; a lagging kind resumes at the new front.
    pub fn pop(&mut self, out: &mut Vec<S>, now: Instant) -> usize {
        let frames = (0..2)
            .filter(|&k| self.pushed[k].is_some_and(|t| now.duration_since(t) < LANE_LIVE))
            .map(|k| self.written[k])
            .min()
            .unwrap_or_else(|| self.ready_frames());
        let n = frames * PAD_CHANNELS;
        out.extend(self.ring.drain(..n.min(self.ring.len())));
        for w in &mut self.written {
            *w = w.saturating_sub(frames);
        }
        frames
    }

    /// Throw the ready frames away: no sink to render them on right now.
    pub fn discard(&mut self) {
        let f = self.ready_frames();
        self.drop_front(f);
    }

    fn drop_front(&mut self, frames: usize) {
        let n = (frames * PAD_CHANNELS).min(self.ring.len());
        self.ring.drain(..n);
        let f = n / PAD_CHANNELS;
        for w in &mut self.written {
            *w = w.saturating_sub(f);
        }
    }
}

/// Concealment frames to synthesise before decoding `seq`, capped at 50 ms of `frame_samples`
/// (per channel at 48 kHz; speaker frames are 10 ms, haptics 5 ms). 0 until a first decode, but
/// the tracker is always fed so a gap before it cannot replay later.
pub fn plc_frames(gaps: &mut AudioGapTracker, seq: u32, frame_samples: usize) -> u32 {
    if frame_samples == 0 {
        gaps.missing_before(seq);
        return 0;
    }
    gaps.set_frame_us((frame_samples as u64 * 1_000_000 / SAMPLE_RATE_HZ as u64) as u32);
    gaps.missing_before(seq)
}

/// The host gates haptics at −60 dBFS with a 250 ms hangover, so a title that only rumbles sends
/// no frames at all. Twice the hangover covers wire jitter.
pub const HAPTICS_IDLE_MS: u64 = 500;

/// Per-wire-pad stamp of the last haptics frame, the evidence that the game drives the coils.
/// Haptics own the coils only while frames arrive, so a rumble-only title keeps its rumble.
/// Written by the render thread, read by the rumble path.
pub struct HapticsLiveness([AtomicU64; 16]);

impl HapticsLiveness {
    pub const fn new() -> HapticsLiveness {
        HapticsLiveness([const { AtomicU64::new(0) }; 16])
    }

    /// Stamp a haptics frame for `pad`. Concealment must not: filling a gap is no evidence.
    pub fn note(&self, pad: u8) {
        self.0[usize::from(pad & 0x0f)].store(seen_clock(), Ordering::Relaxed);
    }

    /// Slot teardown: wire indices are reused, and a stale stamp would take the next pad's rumble.
    pub fn clear(&self, pad: u8) {
        self.0[usize::from(pad & 0x0f)].store(0, Ordering::Relaxed);
    }

    /// Whether `pad`'s haptics frames are arriving right now.
    pub fn live(&self, pad: u8) -> bool {
        live_at(
            self.0[usize::from(pad & 0x0f)].load(Ordering::Relaxed),
            seen_clock(),
        )
    }
}

impl Default for HapticsLiveness {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-clock ms, 1-based so 0 stays "never stamped": a frame in the first millisecond
/// must still read as live.
fn seen_clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Never-stamped is never live; a stamp ahead of `now` is live, not wrapped.
fn live_at(seen_ms: u64, now_ms: u64) -> bool {
    seen_ms != 0 && now_ms.saturating_sub(seen_ms) < HAPTICS_IDLE_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 2_880;

    #[test]
    fn speaker_lands_on_the_front_pair_and_haptics_on_the_coils() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[100, 200], t);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[300, 400], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![100, 200, 300, 400]);
    }

    /// `pad_speaker = "off"` must not stall the coils waiting for a kind that never arrives.
    #[test]
    fn a_missing_kind_plays_as_a_silent_pair() {
        let t = Instant::now();
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[0.5, -0.5, 0.25, -0.25], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 2);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.25, -0.25]);
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.5, -0.5], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    /// With both live, a pop takes only what both have written; a lane gone quiet stops holding
    /// the other back.
    #[test]
    fn interleaving_survives_uneven_cadences() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 1, 2, 2], t);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[9, 9], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![9, 9, 1, 1]);
        out.clear();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[5, 5], t);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[6, 6], t);
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![5, 5, 2, 2]);
        out.clear();
        assert_eq!(m.pop(&mut out, t), 0, "the speaker is live and behind");
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), 1);
        assert_eq!(out, vec![0, 0, 6, 6]);
    }

    /// The loop pops after every datagram. With both lanes live that must play wall time once:
    /// popping the further-ahead kind played 20 ms per 10 ms and zeroed each pair in turn.
    #[test]
    fn both_live_lanes_play_in_real_time() {
        let t0 = Instant::now();
        let mut m = QuadMixer::<f32>::new(4_800);
        let (hap, spk) = (vec![0.5f32; 240 * 2], vec![1.0f32; 480 * 2]);
        let mut out = Vec::new();
        for ms in (0..1_000u64).step_by(5) {
            let now = t0 + Duration::from_millis(ms);
            m.push(PAD_AUDIO_KIND_HAPTICS, &hap, now);
            m.pop(&mut out, now);
            if ms % 10 == 0 {
                m.push(PAD_AUDIO_KIND_SPEAKER, &spk, now);
                m.pop(&mut out, now);
            }
        }
        let frames = out.len() / PAD_CHANNELS;
        assert!(
            (47_500..=48_000).contains(&frames),
            "{frames} frames for 1 s"
        );
        // Past the first speaker frame, every frame carries both pairs.
        assert!(out[480 * PAD_CHANNELS..]
            .chunks_exact(4)
            .all(|f| f[0] == 1.0 && f[2] == 0.5));
    }

    /// A stalled sink cannot grow the ring past the cap; the oldest frames drop and both cursors
    /// shift, so a late marker on the other kind still lands at the front.
    #[test]
    fn the_ceiling_drops_the_oldest_without_skewing_the_kinds() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &vec![1i16; (CAP + 500) * 2], t);
        assert_eq!(m.dropped_frames(), 500);
        assert_eq!(m.ready_frames(), CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[42, 43], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), CAP);
        assert_eq!(&out[..4], &[42, 43, 1, 1]);
    }

    #[test]
    fn discard_empties_without_disturbing_alignment() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 2, 3, 4], t);
        m.discard();
        assert_eq!(m.ready_frames(), 0);
        let mut out = Vec::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[8, 9], t);
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), 1);
        assert_eq!(out, vec![8, 9, 0, 0]);
    }

    /// An unknown kind never reaches a channel pair, least of all the coils.
    #[test]
    fn an_unknown_kind_is_dropped_rather_than_rendered_into_the_coils() {
        let t = Instant::now();
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(2, &[0.9, 0.9], t);
        assert_eq!(m.ready_frames(), 0, "an unknown kind occupies no pair");
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.1, 0.2], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![0.1, 0.2, 0.0, 0.0]);
    }

    /// 0 for first/in-order, the exact gap for a loss, 50 ms of the stream's own frames for a
    /// burst. A gap before the first decode is consumed, not replayed.
    #[test]
    fn plc_counts_gaps_in_the_streams_own_frames() {
        let mut g = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut g, 0, 480), 0);
        assert_eq!(plc_frames(&mut g, 1, 480), 0);
        assert_eq!(plc_frames(&mut g, 5, 480), 3);
        assert_eq!(plc_frames(&mut g, 5, 480), 0);
        assert_eq!(plc_frames(&mut g, 1000, 480), 5, "10 ms speaker frames");
        assert_eq!(plc_frames(&mut g, 2000, 240), 10, "5 ms haptics frames");
        let mut g = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut g, 7, 0), 0);
        assert_eq!(plc_frames(&mut g, 12, 0), 0);
        assert_eq!(plc_frames(&mut g, 13, 480), 0);
    }

    #[test]
    fn haptics_own_the_coils_only_while_frames_arrive() {
        assert!(!live_at(0, 10_000), "never stamped");
        assert!(live_at(10_000, 10_000));
        assert!(live_at(10_000, 10_000 + HAPTICS_IDLE_MS - 1));
        assert!(!live_at(10_000, 10_000 + HAPTICS_IDLE_MS));
        assert!(live_at(10_000, 9_000), "a stamp ahead of now is live");
        assert!(live_at(1, 1), "a frame in the first millisecond is live");
    }

    #[test]
    fn clearing_a_pad_hands_its_coils_back() {
        let l = HapticsLiveness::new();
        l.note(0x12);
        assert!(l.live(2), "the pad index wraps into the 16 wire slots");
        assert!(!l.live(3));
        l.clear(2);
        assert!(!l.live(2));
    }
}
