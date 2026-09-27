//! AAudio callback arithmetic, split out of the device-only [`crate::audio`] so its tests run on
//! the host build. The session's resolved format is core's
//! [`PlaneFormat`](punktfunk_core::audio::plane::PlaneFormat).

/// Interleaved samples in one AAudio callback, or `None` for a length no buffer can hold.
pub(crate) fn callback_sample_count(num_frames: i32, channels: usize) -> Option<usize> {
    let frames = usize::try_from(num_frames)
        .ok()
        .filter(|&frames| frames > 0)?;
    frames.checked_mul(channels).filter(|&samples| samples > 0)
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
}
