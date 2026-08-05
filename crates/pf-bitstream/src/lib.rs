//! The client's bitstream layer for native decode (design/client-native-decode.md §3.1):
//! everything a stateless hardware decoder needs to know about an AU before submission —
//! parsed headers, POC, DPB state, reference lists (including MMCO/LTR, which the hosts'
//! RFI recovery actively uses), recovery-point SEI — derived once here and consumed by
//! every backend (Vulkan `StdVideo*`, DXVA picparams, libva buffers).
//!
//! Parsing primitives come from the vendored cros-codecs parser layer
//! (`vendor/cros-codecs`, see its PROVENANCE.md); this crate owns what upstream keeps in
//! its Linux-only `decoder::stateless` half — the per-AU orchestration — plus the pieces
//! upstream lacks (SEI payload parsing: their parsers classify SEI NALUs but never read
//! them).
//!
//! Scope discipline: punktfunk clients decode punktfunk hosts — zero-reorder, no
//! B-frames, progressive, parameter sets from encoders we control. Implement to spec
//! where cheap; reject-with-log outside that envelope rather than half-decode.
//!
//! Nothing in this crate may touch a GPU API, an OS handle, or the network: CPU-only by
//! construction, so its tests run on every CI leg including macOS. And no `unsafe`,
//! compiler-enforced — this layer exists to replace C parsers; it does not get to
//! reintroduce their failure mode.
#![forbid(unsafe_code)]

// The vendor-pinning smoke tests below assert against byte counts and golden values from
// the vendored snapshot's own test vectors; a cros-codecs re-sync that shifts parser
// behavior must trip HERE, in our tree, not in a decode session.
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
        // 759 is upstream's own golden for this stream (chromium h264_parser_unittest lineage).
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
                // `ref_frame_update` is the parser's ref-slot bookkeeping; without it,
                // inter frames fail with "Reference is invalid" — the parser validates
                // reference integrity rather than trusting the stream.
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
        // 525 is upstream's own golden (cross-checked against GStreamer's OBU walk).
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
        // > chunks proves superframe splitting engaged.
        assert_eq!(frames, 269);
    }
}
