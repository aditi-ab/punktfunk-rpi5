//! Client bitstream layer for native decode (`design/client-native-decode.md`).
//!
//! Per AU: headers, POC, DPB, reference lists (MMCO/LTR), recovery-point SEI —
//! derived once, consumed by every stateless backend (Vulkan `StdVideo*`, DXVA,
//! libva). Parsing primitives come from vendored `vendor/cros-codecs` (see its
//! PROVENANCE.md). This crate owns the per-AU orchestration upstream keeps in
//! Linux-only `decoder::stateless`, plus SEI payload parsing (upstream classifies
//! SEI NALUs but never reads them).
//!
//! Scope: punktfunk hosts — zero-reorder, no B-frames, progressive, controlled
//! parameter sets. Implement to spec where cheap; reject-with-log outside that
//! envelope. CPU-only: no GPU API, OS handle, or network.
#![forbid(unsafe_code)]

pub mod av1;
pub mod clean;
pub mod h264;
pub mod h265;
pub mod sei;

// Golden counts from the vendored snapshot's own vectors. A cros-codecs re-sync that
// shifts parser behaviour must trip here, not in a decode session.
#[cfg(test)]
mod vendor_smoke {
    use std::io::Cursor;

    use cros_codecs::bitstream_utils::IvfIterator;
    use cros_codecs::codec::av1::parser::ObuAction;
    use cros_codecs::codec::av1::parser::ParsedObu;
    use cros_codecs::codec::h264::parser::Nalu as H264Nalu;
    use cros_codecs::codec::h264::parser::Parser as H264Parser;
    use cros_codecs::codec::h265::parser::Nalu as H265Nalu;
    use cros_codecs::codec::h265::parser::Parser as H265Parser;

    const H264_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");
    const H265_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265");
    const AV1_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1");
    const VP9_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/vp9/test_data/test-25fps.vp9");

    #[test]
    fn h264_parses_the_vendored_vector_to_its_goldens() {
        let mut cursor = Cursor::new(H264_25FPS);
        let mut parser = H264Parser::default();
        let (mut nalus, mut sps, mut slices) = (0u32, 0u32, 0u32);
        let mut coded = (0u32, 0u32);
        while let Ok(nalu) = H264Nalu::next(&mut cursor) {
            nalus += 1;
            if let Ok(s) = parser.parse_sps(&nalu) {
                sps += 1;
                coded = (
                    (s.pic_width_in_mbs_minus1 as u32 + 1) * 16,
                    (s.pic_height_in_map_units_minus1 as u32 + 1) * 16,
                );
                continue;
            }
            if parser.parse_pps(&nalu).is_ok() {
                continue;
            }
            if parser.parse_slice_header(nalu).is_ok() {
                slices += 1;
            }
        }
        // 759 is upstream's golden (chromium h264_parser_unittest lineage).
        assert_eq!(nalus, 759);
        assert_eq!(sps, 4);
        assert_eq!(slices, 500);
        assert_eq!(coded, (320, 240));
    }

    #[test]
    fn h265_parses_the_vendored_vector() {
        let mut cursor = Cursor::new(H265_25FPS);
        let mut parser = H265Parser::default();
        let (mut nalus, mut sps, mut slices) = (0u32, 0u32, 0u32);
        while let Ok(nalu) = H265Nalu::next(&mut cursor) {
            nalus += 1;
            if parser.parse_sps(&nalu).is_ok() {
                sps += 1;
                continue;
            }
            if parser.parse_pps(&nalu).is_ok() {
                continue;
            }
            if parser.parse_slice_header(nalu).is_ok() {
                slices += 1;
            }
        }
        assert_eq!(nalus, 254);
        assert_eq!(sps, 1);
        assert_eq!(slices, 250);
    }

    #[test]
    fn av1_walks_obus_and_maintains_ref_slots_across_the_stream() {
        let mut parser = cros_codecs::codec::av1::parser::Parser::default();
        let (mut obus, mut frames) = (0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            let mut consumed = 0;
            while let Ok(action) = parser.read_obu(&packet[consumed..]) {
                let obu = match action {
                    ObuAction::Process(obu) => obu,
                    ObuAction::Drop(n) => {
                        consumed += n as usize;
                        continue;
                    }
                };
                consumed += obu.bytes_used;
                obus += 1;
                // Without `ref_frame_update` the next inter frame fails with "Reference is invalid".
                match parser.parse_obu(obu).expect("parse_obu") {
                    ParsedObu::FrameHeader(fh) => {
                        frames += 1;
                        parser.ref_frame_update(&fh).expect("ref slot update");
                    }
                    ParsedObu::Frame(f) => {
                        frames += 1;
                        parser.ref_frame_update(&f.header).expect("ref slot update");
                    }
                    _ => {}
                }
            }
        }
        // 525 is upstream's golden (cross-checked against GStreamer's OBU walk).
        assert_eq!(obus, 525);
        assert_eq!(frames, 274);
    }

    #[test]
    fn vp9_splits_superframes_and_parses_headers() {
        let mut parser = cros_codecs::codec::vp9::parser::Parser::default();
        let (mut chunks, mut frames) = (0u32, 0u32);
        for packet in IvfIterator::new(VP9_25FPS) {
            chunks += 1;
            frames += parser
                .parse_chunk(packet.as_ref())
                .expect("vp9 chunk")
                .len() as u32;
        }
        assert_eq!(chunks, 250);
        // frames > chunks: superframe splitting engaged.
        assert_eq!(frames, 269);
    }
}
