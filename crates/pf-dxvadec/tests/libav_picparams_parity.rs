//! The byte-diff harness: pf-dxvadec's `DXVA_PicParams_H264` against libavcodec's, for the
//! same access units of the same elementary stream.
//!
//! # Why this exists
//!
//! M3's WP-D closed the native Vulkan rung by comparing DECODED PIXELS against libavcodec —
//! 250 AUs, bit-exact, three drivers. This milestone has no equivalent. Every claim it makes
//! about the DXVA structures rests on reading the specification and reading libavcodec, and
//! reading is exactly the method that produced the defects review 13 found: a quantization
//! matrix submitted unconditionally, a `NumMBsInBuffer` left at zero, a `RefFrameList` holding
//! the wrong set. None of those is visible in a smoke test, and all three are visible in one
//! `memcmp` against the path every Windows player exercises.
//!
//! This harness is the cheap version of that `memcmp`. It is `#[ignore]`d because it needs a
//! capture the repository cannot carry: libavcodec's own picture parameters, produced by a
//! patched FFmpeg on a Windows box with a D3D11VA-capable GPU.
//!
//! # Capturing the libavcodec side
//!
//! On the Windows box (192.168.1.173 — see the box notes for the ssh identity), with an
//! FFmpeg source tree:
//!
//! 1. Patch `libavcodec/dxva2_h264.c`. At the very END of `fill_picture_parameters` — after
//!    the `RefFrameList` loop and the `UsedForReferenceFlags` writes, so every field is
//!    final — add:
//!
//!    ```c
//!    {
//!        static unsigned au_index;
//!        const uint8_t *raw = (const uint8_t *)pp;
//!        char line[2 * sizeof(*pp) + 32];
//!        unsigned i;
//!        for (i = 0; i < sizeof(*pp); i++)
//!            snprintf(line + 2 * i, 3, "%02x", raw[i]);
//!        av_log(avctx, AV_LOG_INFO, "PFPP h264 %u %s\n", au_index++, line);
//!    }
//!    ```
//!
//!    The identical block goes at the end of `dxva2_hevc.c`'s `fill_picture_parameters`, with
//!    `h264` replaced by `hevc`, once this harness grows the HEVC half.
//!
//! 2. Build FFmpeg (`--enable-d3d11va` is on by default on Windows) and decode the SAME
//!    elementary stream this test plans — the vendored vector, which is in the repository at
//!    `crates/pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264`:
//!
//!    ```text
//!    ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 \
//!           -i test-25fps.h264 -f null - 2> capture.log
//!    ```
//!
//!    A software fallback produces no lines at all, which is the check that the hwaccel
//!    actually engaged: 250 `PFPP h264` lines or the capture is void.
//!
//! 3. `grep '^PFPP h264 ' capture.log > libav-h264.picparams`, copy it back, and run:
//!
//!    ```text
//!    PF_LIBAV_PICPARAMS=libav-h264.picparams cargo test -p pf-dxvadec --test \
//!        libav_picparams_parity -- --ignored --nocapture
//!    ```
//!
//! `PF_DXVA_DUMP=<path>` writes THIS side in the same format without needing a capture, so the
//! two files can also be diffed by hand.
//!
//! # Differences that are EXPECTED, and must not be read as defects
//!
//! A raw `memcmp` will differ in three places by design. The harness reports offsets rather
//! than a verdict for exactly this reason — read the offsets against this list:
//!
//! * **Surface indices.** `CurrPic` and every `RefFrameList` entry carry a decode-surface
//!   index. libavcodec's comes from its own frame pool's allocation order; ours comes from
//!   [`pf_dxvadec::SlotMap`]. The two are a BIJECTION over the same pictures, never equal
//!   numbers. A real comparison must build the mapping from the first AU that names a picture
//!   and then check it holds; a differing index alone proves nothing.
//! * **`RefFrameList` ORDER.** libavcodec emits `short_ref` then `long_ref`; this crate emits
//!   the AU's own references first and the rest of the marked DPB after (see
//!   `pic.rs`'s module docs for why). Both are correct — DXVA imposes no order — so the array
//!   must be compared as a SET of (marking, `FrameNumList`, `FieldOrderCntList`, used-flags)
//!   tuples, with `UsedForReferenceFlags` re-indexed to the compared order.
//! * **Padding.** `DXVA_PicParams_H264` has no interior padding by construction (dxva.rs
//!   proves the offsets at compile time), but the `Reserved*` fields are only defined where
//!   this crate writes them deliberately; libavcodec zeroes the struct once at the top of the
//!   frame and writes a subset. Any difference in a reserved field is a REAL finding for this
//!   crate, not an expected divergence — that is what `Reserved16Bits = 3` is about.
//!
//! Everything else — every parameter-set field, every flag word, `frame_num`,
//! `CurrFieldOrderCnt`, `ContinuationFlag`, `StatusReportFeedbackNumber` (both count from 1
//! per picture) — must match byte for byte, and a difference there is the finding this harness
//! exists to produce.
//!
//! # What is not covered yet
//!
//! The HEVC half (`DXVA_PicParams_HEVC` + `DXVA_Qmatrix_HEVC`), and the buffer DESCRIPTORS —
//! `NumMBsInBuffer`, `DataSize`, and whether the quantization-matrix buffer is submitted at
//! all. The descriptors are where two of review 13's three structural defects lived, and they
//! are not in `pp`: capturing them needs the same treatment applied to
//! `commit_bitstream_and_slice_buffer` and `ff_dxva2_commit_buffer`.

use std::collections::BTreeMap;
use std::io::Cursor;

use cros_codecs::codec::h264::parser::Nalu;
use cros_codecs::codec::h264::parser::NaluType;
use pf_dxvadec::{plan_to_dxva, AuPlan, H264Planner, SlotMap};

const TEST_25FPS: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);

/// The same AU splitter every test in this program uses: a new AU starts at a non-slice NALU
/// following a slice, or at a slice whose `first_mb_in_slice` is 0 following a slice.
fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let nalu_offset = cursor.position() as usize;
        let start = nalu_offset - nalu.offset;
        let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
        let first_mb_zero = is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_mb_zero) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// This crate's picture parameters for the vendored vector, one entry per planned AU, as the
/// bytes a `SubmitDecoderBuffers` call would carry.
fn our_picparams() -> Vec<(usize, Vec<u8>)> {
    let mut planner = H264Planner::new();
    let mut slots: Option<SlotMap> = None;
    let mut out = Vec::new();
    for (i, au) in split_into_aus(TEST_25FPS).into_iter().enumerate() {
        let plan: AuPlan = match planner.plan_au(au) {
            Ok(plan) => plan,
            Err(_) => continue,
        };
        let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
        if map.capacity() != plan.picture.max_dpb_frames + 1 {
            *map = SlotMap::new(plan.picture.max_dpb_frames);
        }
        let dxva = plan_to_dxva(&plan, map, out.len() as u32 + 1).expect("conversion");
        out.push((i, pf_dxvadec::as_bytes(&dxva.pic_params).to_vec()));
    }
    out
}

/// `PFPP h264 <au_index> <hex>` lines → index → bytes.
fn parse_capture(text: &str) -> BTreeMap<usize, Vec<u8>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let rest = match line.trim().strip_prefix("PFPP h264 ") {
            Some(rest) => rest,
            None => continue,
        };
        let mut parts = rest.split_whitespace();
        let (Some(index), Some(hex)) = (parts.next(), parts.next()) else {
            continue;
        };
        let index: usize = index.parse().expect("a decimal AU index");
        assert!(hex.len() % 2 == 0, "AU {index}: odd hex length");
        let bytes = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
            .collect();
        out.insert(index, bytes);
    }
    out
}

/// Emit this crate's side in the capture's own format — usable without a capture at all, so
/// the two files can be diffed by any tool.
#[test]
#[ignore = "writes a dump: PF_DXVA_DUMP=<path>"]
fn dump_our_h264_picture_parameters() {
    let path = std::env::var("PF_DXVA_DUMP").expect("PF_DXVA_DUMP=<path> names the output file");
    let mut text = String::new();
    for (index, (_, bytes)) in our_picparams().into_iter().enumerate() {
        text.push_str(&format!("PFPP h264 {index} "));
        for b in bytes {
            text.push_str(&format!("{b:02x}"));
        }
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write the dump");
    println!("wrote {path}");
}

/// Diff this crate's picture parameters against a libavcodec capture, reporting the offsets
/// that differ rather than a pass/fail — the module docs list the three divergences that are
/// expected, and an offset outside them is the finding.
#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_PICPARAMS=<file> (see the module docs)"]
fn our_h264_picture_parameters_match_libavcodecs() {
    let path = std::env::var("PF_LIBAV_PICPARAMS")
        .expect("PF_LIBAV_PICPARAMS=<file> names a capture (see the module docs)");
    let capture = parse_capture(&std::fs::read_to_string(&path).expect("read the capture"));
    let ours = our_picparams();
    assert!(!capture.is_empty(), "{path} holds no PFPP h264 lines");
    assert_eq!(
        capture.len(),
        ours.len(),
        "the capture covers {} AUs and this crate plans {} — the two sides must decode the \
         same elementary stream, split the same way",
        capture.len(),
        ours.len()
    );

    // Offsets that differ, and on how many AUs: a field that differs on every picture is a
    // systematic divergence (read it against the module docs' list), while one that differs
    // on a handful is a case-specific defect.
    let mut differing: BTreeMap<usize, usize> = BTreeMap::new();
    let mut first_example: BTreeMap<usize, (usize, u8, u8)> = BTreeMap::new();
    for ((index, ours), (_, theirs)) in ours.iter().zip(capture.values().enumerate()) {
        assert_eq!(
            ours.len(),
            theirs.len(),
            "AU {index}: the capture's picture parameters are {} bytes and ours are {} — the \
             hand-declared layout and the header disagree, which is a finding on its own",
            theirs.len(),
            ours.len()
        );
        for (offset, (a, b)) in ours.iter().zip(theirs).enumerate() {
            if a != b {
                *differing.entry(offset).or_default() += 1;
                first_example.entry(offset).or_insert((*index, *a, *b));
            }
        }
    }

    if differing.is_empty() {
        println!("{} AUs, byte-identical to libavcodec", ours.len());
        return;
    }
    println!(
        "{} differing byte offsets over {} AUs:",
        differing.len(),
        ours.len()
    );
    for (offset, count) in &differing {
        let (au, ours, theirs) = first_example[offset];
        println!("  offset {offset:#06x}: {count} AUs, first at AU {au} (ours {ours:#04x}, libav {theirs:#04x})");
    }
    panic!(
        "picture parameters diverge at {} offsets — check each against the module docs' list \
         of expected divergences before treating it as a defect",
        differing.len()
    );
}
