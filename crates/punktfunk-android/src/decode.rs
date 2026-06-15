//! Android video decode (android-only): pull HEVC access units from the connector and render them
//! to the SurfaceView via NDK `AMediaCodec` — hardware decode, zero per-frame JNI.
//!
//! One-in/one-out: the host opens every stream with an IDR carrying VPS/SPS/PPS **in-band**, so the
//! decoder needs no out-of-band codec-specific data — we configure with mime + the negotiated
//! WxH (from [`NativeClient::mode`]) and feed each access unit as it arrives. The decode thread owns
//! the codec + window for its whole life; [`crate::session`] signals it to stop via the shared flag.

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The decode loop. Runs on the `pf-decode` thread until `shutdown` is set or the session closes.
pub fn run(client: Arc<NativeClient>, window: NativeWindow, shutdown: Arc<AtomicBool>) {
    let mode = client.mode();
    let codec = match MediaCodec::from_decoder_type("video/hevc") {
        Some(c) => c,
        None => {
            log::error!("decode: no HEVC decoder on this device");
            return;
        }
    };

    let mut format = MediaFormat::new();
    format.set_str("mime", "video/hevc");
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    // Generous input buffer so a large keyframe AU is never truncated.
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    // Ask for the low-latency decode path where the decoder supports it (no reordering buffer).
    format.set_i32("low-latency", 1);

    if let Err(e) = codec.configure(&format, Some(&window), MediaCodecDirection::Decoder) {
        log::error!("decode: configure failed: {e}");
        return;
    }
    if let Err(e) = codec.start() {
        log::error!("decode: start failed: {e}");
        return;
    }
    log::info!(
        "decode: HEVC decoder started at {}x{}",
        mode.width,
        mode.height
    );

    let mut fed: u64 = 0;
    let mut rendered: u64 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(5)) {
            Ok(frame) => {
                if fed == 0 {
                    let p = &frame.data;
                    log::info!(
                        "decode: first AU {} bytes, head {:02x?}",
                        p.len(),
                        &p[..p.len().min(6)]
                    );
                }
                fed += 1;
                feed(&codec, &frame.data, frame.pts_ns / 1000);
            }
            Err(PunktfunkError::NoFrame) => {} // timeout — still drain output below
            Err(_) => break,                   // session closed
        }
        rendered += drain(&codec);
        if fed > 0 && fed % 300 == 0 {
            log::info!("decode: fed={fed} rendered={rendered}");
        }
    }

    let _ = codec.stop();
    log::info!("decode: stopped (fed={fed} rendered={rendered})");
}

/// Copy one access unit into a codec input buffer and queue it.
fn feed(codec: &MediaCodec, au: &[u8], pts_us: u64) {
    match codec.dequeue_input_buffer(Duration::from_millis(10)) {
        Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
            let n = {
                let dst = buf.buffer_mut();
                let n = au.len().min(dst.len());
                if n < au.len() {
                    log::warn!(
                        "decode: AU {} > input buffer {}, truncated",
                        au.len(),
                        dst.len()
                    );
                }
                for (slot, &b) in dst.iter_mut().zip(&au[..n]) {
                    slot.write(b);
                }
                n
            };
            if let Err(e) = codec.queue_input_buffer(buf, 0, n, pts_us, 0) {
                log::warn!("decode: queue_input_buffer: {e}");
            }
        }
        Ok(DequeuedInputBufferResult::TryAgainLater) => {
            // No input buffer free right now; the AU is dropped (FEC/keyframes recover).
        }
        Err(e) => log::warn!("decode: dequeue_input_buffer: {e}"),
    }
}

/// Release any ready output buffers to the surface (render = true), latency-first. Returns the
/// number of frames presented.
fn drain(codec: &MediaCodec) -> u64 {
    let mut n = 0;
    loop {
        match codec.dequeue_output_buffer(Duration::from_millis(0)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                if let Err(e) = codec.release_output_buffer(buf, true) {
                    log::warn!("decode: release_output_buffer: {e}");
                    break;
                }
                n += 1;
            }
            // TryAgainLater / OutputFormatChanged / OutputBuffersChanged — nothing to render now.
            Ok(_) => break,
            Err(e) => {
                log::warn!("decode: dequeue_output_buffer: {e}");
                break;
            }
        }
    }
    n
}
