//! Test support for the decoder crates: the vendored cros-codecs vectors and
//! the Annex-B and IVF access-unit splitters.
//!
//! Behind the `test-vectors` feature, which a crate turns on from its
//! `[dev-dependencies]`, so no shipped build carries the vectors.

use cros_codecs::bitstream_utils::IvfIterator;

pub const H264_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");
/// Carries a B slice; the non-high `64x64-I-P-B-P.h264` is constrained
/// baseline, where x264 dropped it.
pub const H264_64X64_I_P_B_P_HIGH: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/64x64-I-P-B-P-high.h264");
pub const H265_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265");
pub const H265_64X64_I_P_B_P: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/64x64-I-P-B-P.h265");
pub const H265_BEAR: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bear.h265");
pub const H265_BBB: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bbb.h265");
/// IVF: 250 temporal units, 274 coded frames.
pub const AV1_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1");
pub const VP9_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/vp9/test_data/test-25fps.vp9");

/// Offsets of every Annex-B NAL header. Emulation prevention keeps `00 00 01`
/// out of payloads, so a byte scan finds exactly the start codes.
fn nal_headers(stream: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < stream.len() {
        if stream[i..i + 3] == [0x00, 0x00, 0x01] {
            out.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Split into access units given `(is_slice, first_in_picture)` per NAL
/// header. A new AU begins at a non-VCL NAL after slices, or at a
/// first-of-picture slice once the current AU has slices.
fn split_aus(stream: &[u8], classify: impl Fn(&[u8], usize) -> (bool, bool)) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut au_start = 0usize;
    let mut au_has_slice = false;
    for header in nal_headers(stream) {
        let (is_slice, first_in_picture) = classify(stream, header);
        // The start code owning this header, with the four-byte form's zero.
        let mut start = header - 3;
        if start > 0 && stream[start - 1] == 0x00 {
            start -= 1;
        }
        if au_has_slice && (!is_slice || first_in_picture) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// H.264: a one-byte NAL header, slices are types 1 and 5, and
/// `first_mb_in_slice == 0` (ue(v) `1`) is the top bit of the next byte.
pub fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
    split_aus(stream, |s, h| {
        let is_slice = matches!(s[h] & 0x1f, 1 | 5);
        let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
        (is_slice, first)
    })
}

/// H.265: a two-byte NAL header, slices are types below 32, and
/// `first_slice_segment_in_pic_flag` is the top bit at `+2`.
pub fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
    split_aus(stream, |s, h| {
        let is_slice = (s[h] >> 1) & 0x3f < 32;
        let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
        (is_slice, first)
    })
}

/// AV1 temporal units: the IVF container's frames, since OBUs carry no
/// start codes to scan for.
pub fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
    IvfIterator::new(stream).collect()
}
