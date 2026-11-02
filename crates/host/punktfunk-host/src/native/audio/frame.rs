//! One protocol frame of the audio plane, from the slot the [`Pacer`](super::Pacer) released:
//! real audio drained from the capture queue, or silence over a capture hole. Digital zero is
//! a step from the last real sample, so a hole fades out of what the listener heard last and
//! the first real frame after it fades back in (raised cosine, [`EDGE_FADE_US`] each edge).
//!
//! Pure: no clock, no I/O, and the buffers are reused frame to frame. The table in this file's
//! tests pins it, one row per behaviour.

use super::Slot;
use punktfunk_core::audio::pcm;

/// Fade at each edge of a capture hole, in µs. 1 ms is a slope, not an edge, and too short to
/// read as a swell.
const EDGE_FADE_US: u64 = 1_000;

/// One frame ready for the wire.
pub(super) struct Frame<'a> {
    pub samples: &'a [f32],
    /// Silence over a hole, the partial it opened on included.
    pub infilled: bool,
}

/// Builds each frame the pacer releases, and carries the fade state between them.
pub(super) struct FrameAssembler {
    /// Interleaved samples in one protocol frame.
    frame_len: usize,
    channels: usize,
    /// Interleaved: whole frames × channels, so the curve spans whole frames.
    edge_fade: usize,
    /// Operator capture gain, applied to every captured sample: real frames and a hole's partial.
    gain: f32,
    buf: Vec<f32>,
    /// The fade-out source when a hole opens on an empty partial; recorded post-gain, so the
    /// fade sits at the level the listener was hearing.
    last_real: Vec<f32>,
    /// The next real frame starts mid-waveform and fades in.
    resume_fade: bool,
}

impl FrameAssembler {
    pub(super) fn new(rate_hz: u32, channels: u8, frame_len: usize, gain: f32) -> FrameAssembler {
        let channels = channels as usize;
        FrameAssembler {
            frame_len,
            channels,
            edge_fade: (rate_hz as u64 * EDGE_FADE_US / 1_000_000) as usize * channels,
            gain,
            buf: Vec::with_capacity(frame_len),
            last_real: Vec::with_capacity(frame_len),
            resume_fade: false,
        }
    }

    /// The frame `slot` sends, its samples taken from `acc`; `None` on [`Slot::Wait`]. A real
    /// frame drains `frame_len` samples. A hole frame takes the whole partial and pads it with
    /// silence: waiting for post-gap samples to complete it would straddle the hole, and
    /// dropping it would come back as schedule lag.
    pub(super) fn next(&mut self, slot: Slot, acc: &mut Vec<f32>) -> Option<Frame<'_>> {
        self.buf.clear();
        let infilled = match slot {
            Slot::Wait => return None,
            Slot::Frame => {
                self.buf.extend(acc.drain(..self.frame_len));
                if self.gain != 1.0 {
                    punktfunk_core::audio::apply_gain(&mut self.buf, self.gain);
                }
                if std::mem::take(&mut self.resume_fade) {
                    pcm::raised_cosine_head(&mut self.buf, self.edge_fade);
                }
                self.last_real.clear();
                self.last_real.extend_from_slice(&self.buf);
                false
            }
            Slot::Silence { first } => {
                let partial = acc.len();
                self.buf.append(acc);
                // The partial is captured audio: it leaves at the level of the frames before
                // it, or the hole opens on a step. The seed below is already post-gain.
                if self.gain != 1.0 {
                    punktfunk_core::audio::apply_gain(&mut self.buf, self.gain);
                }
                if first {
                    // Fade the tail we have — the partial, or the last real frame if it is
                    // empty — so the hole is a slope from the listener's level, not a step.
                    if partial == 0 && !self.last_real.is_empty() {
                        extend_hole_seed(
                            &mut self.buf,
                            &self.last_real,
                            self.edge_fade,
                            self.channels,
                        );
                    }
                    let n = self.buf.len().min(self.edge_fade);
                    pcm::raised_cosine_tail(&mut self.buf, n);
                }
                self.buf.resize(self.frame_len, 0.0);
                // Whatever follows this hole starts mid-waveform.
                self.resume_fade = true;
                true
            }
        };
        Some(Frame {
            samples: &self.buf,
            infilled,
        })
    }

    /// The next real frame fades in: the frame before it never reached the listener.
    pub(super) fn fade_in_next(&mut self) {
        self.resume_fade = true;
    }
}

/// What a hole fades out of when no partial is queued: the last `n` interleaved samples of
/// the last real frame, frame order reversed and channels kept, so the first replayed frame is
/// the one the listener heard last and the fade starts at their level.
fn extend_hole_seed(buf: &mut Vec<f32>, last_real: &[f32], n: usize, channels: usize) {
    let channels = channels.max(1);
    let n = n.min(last_real.len()) / channels * channels;
    let tail = &last_real[last_real.len() - n..];
    buf.extend(tail.chunks_exact(channels).rev().flatten());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2 kHz stereo: a 1 ms edge fade is 2 frames, 4 interleaved samples.
    const RATE: u32 = 2_000;
    const EDGE: usize = 4;
    const LEN: usize = 8;

    /// A call before the judged frame: a released slot with the samples queued for it, or a mute.
    enum Step {
        Slot(Slot, Vec<f32>),
        Mute,
    }

    struct Row {
        pins: &'static str,
        rate_hz: u32,
        frame_len: usize,
        gain: f32,
        before: Vec<Step>,
        slot: Slot,
        acc: Vec<f32>,
        /// The judged frame's samples and `infilled`; `None` is no frame.
        want: Option<(Vec<f32>, bool)>,
        /// Samples still queued after it.
        left: usize,
    }

    fn row(pins: &'static str, slot: Slot, acc: Vec<f32>, want: Option<(Vec<f32>, bool)>) -> Row {
        Row {
            pins,
            rate_hz: RATE,
            frame_len: LEN,
            gain: 1.0,
            before: Vec::new(),
            slot,
            acc,
            want,
            left: 0,
        }
    }

    fn ramp(from: u16, to: u16) -> Vec<f32> {
        (from..=to).map(f32::from).collect()
    }

    fn tail(mut v: Vec<f32>, n: usize) -> Vec<f32> {
        pcm::raised_cosine_tail(&mut v, n);
        v
    }

    fn head(mut v: Vec<f32>, n: usize) -> Vec<f32> {
        pcm::raised_cosine_head(&mut v, n);
        v
    }

    fn pad(mut v: Vec<f32>, len: usize) -> Vec<f32> {
        v.resize(len, 0.0);
        v
    }

    const FIRST: Slot = Slot::Silence { first: true };
    const LATER: Slot = Slot::Silence { first: false };

    fn rows() -> Vec<Row> {
        let heard = || Step::Slot(Slot::Frame, ramp(1, 8));
        let hole = || Step::Slot(FIRST, Vec::new());
        vec![
            Row {
                left: 2,
                ..row(
                    "a real frame drains one frame and leaves the rest queued",
                    Slot::Frame,
                    ramp(1, 10),
                    Some((ramp(1, 8), false)),
                )
            },
            row(
                "a partial at a hole fades at its tail and pads with silence",
                FIRST,
                ramp(1, 6),
                Some((pad(tail(ramp(1, 6), EDGE), LEN), true)),
            ),
            Row {
                before: vec![heard()],
                ..row(
                    "S3-4: an empty hole fades out of the last frame, reversed, channels kept",
                    FIRST,
                    Vec::new(),
                    Some((pad(tail(vec![7.0, 8.0, 5.0, 6.0], EDGE), LEN), true)),
                )
            },
            row(
                "a hole before any real frame is plain silence",
                FIRST,
                Vec::new(),
                Some((vec![0.0; LEN], true)),
            ),
            Row {
                before: vec![heard(), hole()],
                ..row(
                    "a hole's second frame is silence with no fade",
                    LATER,
                    Vec::new(),
                    Some((vec![0.0; LEN], true)),
                )
            },
            Row {
                before: vec![heard(), hole()],
                ..row(
                    "a partial past a hole's first frame pads without a fade",
                    LATER,
                    ramp(1, 2),
                    Some((pad(ramp(1, 2), LEN), true)),
                )
            },
            Row {
                before: vec![heard(), hole()],
                ..row(
                    "the first real frame after a hole fades in",
                    Slot::Frame,
                    ramp(11, 18),
                    Some((head(ramp(11, 18), EDGE), false)),
                )
            },
            Row {
                before: vec![heard(), hole(), Step::Slot(Slot::Frame, ramp(11, 18))],
                ..row(
                    "only the first real frame after a hole fades in",
                    Slot::Frame,
                    ramp(21, 28),
                    Some((ramp(21, 28), false)),
                )
            },
            Row {
                before: vec![heard(), Step::Mute],
                ..row(
                    "the first real frame after a mute fades in",
                    Slot::Frame,
                    ramp(11, 18),
                    Some((head(ramp(11, 18), EDGE), false)),
                )
            },
            Row {
                gain: 2.0,
                ..row(
                    "the gain lifts a real frame",
                    Slot::Frame,
                    vec![0.25; LEN],
                    Some((vec![0.5; LEN], false)),
                )
            },
            Row {
                gain: 2.0,
                ..row(
                    "the gain lifts a hole's partial before its fade",
                    FIRST,
                    vec![0.25; 6],
                    Some((pad(tail(vec![0.5; 6], EDGE), LEN), true)),
                )
            },
            Row {
                gain: 2.0,
                before: vec![Step::Slot(Slot::Frame, vec![0.25; LEN])],
                ..row(
                    "a hole fades out of the last frame as heard, after the gain",
                    FIRST,
                    Vec::new(),
                    Some((pad(tail(vec![0.5; EDGE], EDGE), LEN), true)),
                )
            },
            Row {
                before: vec![
                    heard(),
                    hole(),
                    Step::Slot(LATER, Vec::new()),
                    Step::Slot(Slot::Frame, ramp(11, 18)),
                ],
                ..row(
                    "only a real frame becomes the next hole's seed",
                    FIRST,
                    Vec::new(),
                    Some((pad(tail(vec![17.0, 18.0, 15.0, 16.0], EDGE), LEN), true)),
                )
            },
            Row {
                left: 3,
                ..row(
                    "a wait sends nothing and leaves the queue",
                    Slot::Wait,
                    ramp(1, 3),
                    None,
                )
            },
            Row {
                rate_hz: 44_100,
                frame_len: pcm::samples_per_frame(44_100, 5_000, 2),
                ..row(
                    "a 44.1 kHz hole pads to the floored 440 and fades 44 whole frames",
                    FIRST,
                    ramp(1, 100),
                    Some((pad(tail(ramp(1, 100), 88), 440), true)),
                )
            },
        ]
    }

    /// Each row replays its `before` steps on a fresh assembler, then judges one frame.
    #[test]
    fn each_slot_assembles_the_frame_its_row_names() {
        for row in rows() {
            let mut asm = FrameAssembler::new(row.rate_hz, 2, row.frame_len, row.gain);
            for step in row.before {
                match step {
                    Step::Slot(slot, mut acc) => {
                        asm.next(slot, &mut acc);
                    }
                    Step::Mute => asm.fade_in_next(),
                }
            }
            let mut acc = row.acc;
            let got = asm
                .next(row.slot, &mut acc)
                .map(|f| (f.samples.to_vec(), f.infilled));
            assert_eq!(got, row.want, "{}", row.pins);
            assert_eq!(acc.len(), row.left, "{}: samples left queued", row.pins);
        }
    }

    /// A hole with no partial replays the end of the last frame backwards, so its first
    /// frame is the last one sent and each channel stays on its own lane.
    #[test]
    fn a_hole_fades_out_of_the_last_frame_sent() {
        let last_real = [1.0, 10.0, 2.0, 20.0, 3.0, 30.0];
        let mut buf = Vec::new();
        extend_hole_seed(&mut buf, &last_real, 4, 2);
        assert_eq!(buf, [3.0, 30.0, 2.0, 20.0]);
        let mut short = Vec::new();
        extend_hole_seed(&mut short, &last_real[..2], 4, 2);
        assert_eq!(
            short,
            [1.0, 10.0],
            "a frame shorter than the fade replays what it has"
        );
    }
}
