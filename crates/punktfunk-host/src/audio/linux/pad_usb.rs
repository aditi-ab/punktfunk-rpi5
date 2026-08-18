//! Pad audio captured at the **USB layer** — the counterpart to [`super::pad_sink`] for a pad that
//! is a real USB device ([`pf_inject::dualsense_usbip`]) rather than a minted PipeWire graph.
//!
//! When the pad arrives over `vhci_hcd` it carries its own USB Audio Class sound card, so the host
//! mints nothing: `snd-usb-audio` creates a real ALSA card and PipeWire's own ALSA monitor builds
//! the `…HiFi__Speaker__sink` / `…HiFi__SpeakerHaptic__sink` nodes from the distro's DualSense UCM.
//! Everything a game writes — whether PipeWire mixed it or a raw `hw:X,0` grab produced it —
//! converges on the pad's isochronous OUT endpoint, and *that* is what we capture.
//!
//! Capturing one layer lower is what makes the USB pad worth the complexity:
//!
//! - it is the same point a physical pad's samples reach, so any route a game takes lands here;
//! - there is no impersonated node graph left to keep faithful; and
//! - the sinks that do exist are real, which is precisely what lets wine derive a matching
//!   ContainerId for the pad and its speaker (see [`pf_inject::dualsense_usbip`] for why that is
//!   the whole point).
//!
//! The decode itself lives in the USB handler; this type is just the [`AudioCapturer`] face of the
//! channel it publishes.

use crate::audio::AudioCapturer;
use anyhow::{anyhow, Result};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// How long [`next_chunk`](AudioCapturer::next_chunk) waits before reporting "nothing right now".
/// Matches [`super::pad_sink::PadSinkCapturer`]'s idle timeout so a quiet pad behaves identically
/// on both transports.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Interleaved `f32` quad frames lifted straight off the pad's isochronous OUT endpoint.
pub(crate) struct PadUsbCapturer {
    rx: Receiver<Vec<f32>>,
    pad: u8,
}

/// Map the pad's **hardware** quad onto the wire's **logical** layout.
///
/// The isochronous endpoint carries the DualSense's own channel map — `ch0` = headphone LEFT,
/// `ch1` = headphone RIGHT *and* the built-in mono speaker, `ch2`/`ch3` = the voice coils
/// (confirmed twice independently: the UCM split positions `[AUX1,AUX1,AUX2,AUX3]` and the
/// on-glass channel sweep). The 0xD1 wire contract instead puts the *speaker pair* on ch0/1.
/// Forwarding the hardware quad verbatim therefore ships headphone-left (silence, or content no
/// remote pad can render — the jack is on the other end of the stream) as wire speaker-left, and
/// the actual speaker channel as wire speaker-right — which the client then plays into the ONE
/// split-sink channel that a current PipeWire never wires to the physical speaker.
/// Field-diagnosed 2026-08-18: haptics felt, speaker dead, the tone measured on exactly one
/// channel at each hop.
///
/// So: duplicate the hardware speaker channel (`ch1`) across the wire's speaker pair, pass the
/// coils through. Headphone-left is dropped deliberately — the remote pad's jack is not a wire
/// surface, and a game that routes to the jack has the pad's audio *off* the speaker anyway.
fn normalize_hw_quad(mut chunk: Vec<f32>) -> Vec<f32> {
    for frame in chunk.chunks_exact_mut(4) {
        frame[0] = frame[1];
    }
    chunk
}

impl PadUsbCapturer {
    /// Claim wire pad `pad`'s USB audio stream.
    ///
    /// Fails while no usbip pad has published one — which is the normal state for the moment
    /// between the pad-audio thread starting and the pad attaching. The streamer's
    /// open-with-backoff loop retries, so this costs a late start rather than a silent pad.
    pub(crate) fn open(pad: u8) -> Result<PadUsbCapturer> {
        let rx = pf_inject::dualsense_usbip::take_audio_rx(pad)
            .ok_or_else(|| anyhow!("no usbip pad audio published for pad {pad} (not attached?)"))?;
        tracing::info!(pad, "pad audio capturing from the USB isochronous endpoint");
        // The pad's ALSA card is real, so WirePlumber greets it with its global 40 % default —
        // which is -23.88 dB applied BEFORE the isochronous endpoint we capture from, and which
        // stacks with the same default on the client. Undo it once the card shows up; see
        // [`super::pad_card_volume`]. Best effort, off-thread, never fatal.
        super::pad_card_volume::spawn_pin(pad);
        Ok(PadUsbCapturer { rx, pad })
    }
}

impl AudioCapturer for PadUsbCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(IDLE_TIMEOUT)
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.rx.recv_timeout(budget.min(IDLE_TIMEOUT)) {
            Ok(chunk) => Ok(normalize_hw_quad(chunk)),
            // Nothing arrived in the budget. The game isn't writing (or the stream is stopped) —
            // a quiet pad, not a dead one, exactly as the sink capturer reports it.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            // The sender lives inside the attached USB device, so this can only mean the pad went
            // away. Err tells the streamer to reopen, which is what re-arrival should do.
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!(
                "usbip pad {} detached — audio endpoint gone",
                self.pad
            )),
        }
    }

    fn channels(&self) -> u32 {
        pf_inject::dualsense_usbip::PAD_AUDIO_CHANNELS as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn capturer() -> (std::sync::mpsc::SyncSender<Vec<f32>>, PadUsbCapturer) {
        let (tx, rx) = sync_channel(4);
        (tx, PadUsbCapturer { rx, pad: 0 })
    }

    /// The capture is the pad's hardware quad — the channel count the 0xD1 framer splits into
    /// speaker (ch0/1) and coils (ch2/3).
    #[test]
    fn reports_the_hardware_quad() {
        assert_eq!(capturer().1.channels(), 4);
    }

    /// A quiet pad must read as an empty chunk, never an error: the streamer keeps a capturer
    /// through silence and only reopens on a genuine death.
    #[test]
    fn silence_is_an_empty_chunk_not_an_error() {
        let (_tx, mut c) = capturer();
        let got = c
            .next_chunk_within(Duration::from_millis(10))
            .expect("silence must not error");
        assert!(got.is_empty());
    }

    /// A detached pad must surface as `Err` so the streamer reopens rather than spinning on a dead
    /// channel forever.
    #[test]
    fn detach_surfaces_as_an_error() {
        let (tx, mut c) = capturer();
        drop(tx);
        assert!(c.next_chunk_within(Duration::from_millis(10)).is_err());
    }

    /// The hardware quad is normalized to the wire layout: hw ch1 (the pad's one real speaker
    /// channel) is duplicated across the wire speaker pair, the coils pass through, and hw ch0
    /// (headphone-left — not a wire surface) is dropped. Forwarding the quad verbatim shipped
    /// the speaker on wire ch1 only, which the client's split-sink render never got to the
    /// physical speaker (field, 2026-08-18: haptics felt, speaker dead).
    #[test]
    fn normalizes_the_hardware_quad_to_the_wire_layout() {
        let (tx, mut c) = capturer();
        tx.send(vec![0.9, 0.5, 0.25, -0.25, 0.8, 0.4, 0.2, -0.2])
            .expect("send");
        assert_eq!(
            c.next_chunk_within(Duration::from_millis(50))
                .expect("chunk"),
            vec![0.5, 0.5, 0.25, -0.25, 0.4, 0.4, 0.2, -0.2]
        );
    }

    /// The normalizer itself, on one frame: `[hpL, spk, coilA, coilB]` → `[spk, spk, coilA, coilB]`.
    #[test]
    fn normalize_duplicates_the_speaker_channel() {
        assert_eq!(
            normalize_hw_quad(vec![0.9, 0.5, 0.25, -0.25]),
            vec![0.5, 0.5, 0.25, -0.25]
        );
    }
}
