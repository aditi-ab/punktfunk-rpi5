//! AAudio and `AudioTrack` arithmetic, split out of the device-only [`crate::audio`] so its tests
//! run on the host build. The session's resolved format is core's
//! [`PlaneFormat`](punktfunk_core::audio::plane::PlaneFormat).

/// Interleaved samples in one AAudio callback, or `None` for a length no buffer can hold.
pub(crate) fn callback_sample_count(num_frames: i32, channels: usize) -> Option<usize> {
    let frames = usize::try_from(num_frames)
        .ok()
        .filter(|&frames| frames > 0)?;
    frames.checked_mul(channels).filter(|&samples| samples > 0)
}

/// `AudioFormat.CHANNEL_OUT_*` for the wire layout: the native speaker mask shifted left by two.
pub(crate) fn java_channel_mask(channels: u8) -> i32 {
    (punktfunk_core::audio::wasapi_channel_mask(channels) << 2) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_lengths_reject_nonpositive_and_overflowing_inputs() {
        assert_eq!(callback_sample_count(128, 2), Some(256));
        assert_eq!(callback_sample_count(0, 2), None);
        assert_eq!(callback_sample_count(-1, 2), None);
        assert_eq!(callback_sample_count(1, 0), None);
        assert_eq!(callback_sample_count(i32::MAX, usize::MAX), None);
    }

    #[test]
    fn java_masks_are_the_published_layouts() {
        assert_eq!(java_channel_mask(2), 0xC); // CHANNEL_OUT_STEREO
        assert_eq!(java_channel_mask(6), 0xFC); // CHANNEL_OUT_5POINT1
        assert_eq!(java_channel_mask(8), 0x18FC); // CHANNEL_OUT_7POINT1_SURROUND
    }
}
