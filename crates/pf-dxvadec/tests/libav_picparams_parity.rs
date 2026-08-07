//! The parity harness: pf-dxvadec's DXVA submission against libavcodec's, for the same access
//! units of the same elementary stream — picture parameters, quantization matrices AND the
//! buffer descriptors, for both codecs.
//!
//! (The file is still called `libav_picparams_parity` although it long since outgrew the picture
//! parameters: the name is what the milestone's plan, the capture recipe below and the gate
//! `cargo test -p pf-dxvadec --test libav_picparams_parity` all say, and a rename would cost
//! every one of those a stale reference to buy nothing.)
//!
//! # Why this exists
//!
//! M3's WP-D closed the native Vulkan rung by comparing DECODED PIXELS against libavcodec —
//! 250 AUs, bit-exact, three drivers. This milestone has no equivalent. Every claim it makes
//! about the DXVA structures rests on reading the specification and reading libavcodec, and
//! reading is exactly the method that produced the defects review 13 found: a quantization
//! matrix submitted unconditionally, a `NumMBsInBuffer` left at zero, a `RefFrameList` holding
//! the wrong set. None of those is visible in a smoke test, and all three are visible in one
//! comparison against the path every Windows player exercises.
//!
//! Two halves, and the split matters:
//!
//! * **What needs no capture at all** — the descriptor set's internal consistency, that the
//!   quantization-matrix buffer is submitted exactly when the stream has one, that
//!   `NumMBsInBuffer` is `mb_width * mb_height` for H.264 and 0 for HEVC, that the slice
//!   records tile the bitstream buffer exactly. Those are ORDINARY tests here, not `#[ignore]`d,
//!   because they run on any host over every AU of the vendored vectors. Two of review 13's
//!   three structural defects would have failed one of them.
//! * **What needs libavcodec's own bytes** — the picture parameters, the matrices' contents,
//!   and the descriptor VALUES as libav computes them. Those tests are `#[ignore]`d because they
//!   need a capture the repository cannot carry: a patched FFmpeg on a Windows box with a
//!   D3D11VA-capable GPU.
//!
//! # Capturing the libavcodec side
//!
//! Verified against **FFmpeg n8.1**, which is the version the Windows CI runs; the names below
//! are that tree's (they changed — the fill functions are non-static and codec-prefixed now).
//! On the Windows box (192.168.1.173 — see the box notes for the ssh identity), with an FFmpeg
//! source tree:
//!
//! **1. One AU counter, shared by every line.** In `libavcodec/dxva2.c`, at file scope above
//! `ff_dxva2_commit_buffer` (dxva2.c:802):
//!
//! ```c
//! static unsigned pf_au_index;
//! ```
//!
//! and bump it exactly once per picture, at the TOP of `ff_dxva2_common_end_frame` — that
//! function runs once per submitted picture, so `pf_au_index` is the same number for all of one
//! AU's lines:
//!
//! ```c
//! const unsigned pf_au = pf_au_index++;   /* first line of the function body */
//! ```
//!
//! (The lines below that live in other functions read the file-scope `pf_au_index - 1`; the
//! block for each says which.) Two assumptions, both of which the harness's preflight catches if
//! they fail: the DXVA hwaccel decodes one picture at a time (`start_frame` → `decode_slice`* →
//! `end_frame`, no frame threading), and no picture is refused BETWEEN its
//! `fill_picture_parameters` and its `ff_dxva2_common_end_frame` — a codec-level `end_frame` that
//! returns early on `slice_count <= 0` would log a `PFPP` line with no matching descriptors, and
//! the preflight refuses a capture whose AU indices are not exactly `0..250`.
//!
//! **2. The buffer descriptors.** `ff_dxva2_commit_buffer` (dxva2.c:802) is the choke point for
//! three of the four buffers — it writes `dsc11->BufferType/DataSize/NumMBsInBuffer` at
//! dxva2.c:836-840. Immediately AFTER that write:
//!
//! ```c
//! av_log(NULL, AV_LOG_INFO, "PFBD %s %u %u %u %u %u\n",
//!        avcodec_get_name(avctx->codec_id), pf_au_index - 1,
//!        (unsigned)type, (unsigned)size, (unsigned)mb_count, 0u);
//! ```
//!
//! ⚠ **The BITSTREAM descriptor does NOT pass through that function** — the bitstream buffer is
//! packed in place, so each codec's `commit_bitstream_and_slice_buffer` fills its descriptor
//! itself (`dxva2_h264.c:412` D3D11 / `:425` DXVA2, `dxva2_hevc.c:338` / `:349`). Add the same
//! line after each of those two fills, with the codec spelled literally:
//!
//! ```c
//! av_log(NULL, AV_LOG_INFO, "PFBD h264 %u 6 %u %u 0\n",
//!        pf_au_index - 1, (unsigned)current, mb_count);        /* dxva2_h264.c */
//! av_log(NULL, AV_LOG_INFO, "PFBD hevc %u 6 %u 0 0\n",
//!        pf_au_index - 1, (unsigned)current);                  /* dxva2_hevc.c */
//! ```
//!
//! A capture whose AUs carry three `PFBD` lines instead of four is this patch site missed, and
//! the harness says so by name rather than reporting a missing buffer as a defect.
//!
//! **3. The picture parameters.** At the very END of `ff_dxva2_h264_fill_picture_parameters`
//! (`dxva2_h264.c:51`) — after the `RefFrameList` loop and the `UsedForReferenceFlags` writes,
//! so every field is final:
//!
//! ```c
//! {
//!     const uint8_t *raw = (const uint8_t *)pp;
//!     char line[2 * sizeof(*pp) + 1];
//!     unsigned i;
//!     for (i = 0; i < sizeof(*pp); i++)
//!         snprintf(line + 2 * i, 3, "%02x", raw[i]);
//!     av_log(NULL, AV_LOG_INFO, "PFPP h264 %u %s\n", pf_au_index, line);
//! }
//! ```
//!
//! `pf_au_index` un-decremented here on purpose: `fill_picture_parameters` runs from
//! `start_frame`, BEFORE `ff_dxva2_common_end_frame` bumps the counter for the same picture.
//! The identical block goes at the end of `ff_dxva2_hevc_fill_picture_parameters`
//! (`dxva2_hevc.c:60`) with `h264` replaced by `hevc`.
//!
//! **4. The quantization matrices, including whether they are submitted at all.** In
//! `ff_dxva2_common_end_frame`, where its `qm`/`qm_size` arguments are in scope (that is where
//! the codec's decision arrives: `dxva2_h264.c:513-516` passes `&ctx_pic->qm` with `sizeof(qm)`
//! UNCONDITIONALLY, while `dxva2_hevc.c:417,423-426` passes `NULL`/0 unless
//! `pp.dwCodingParamToolFlags & 1`):
//!
//! ```c
//! if (qm_size > 0) {
//!     const uint8_t *raw = qm;
//!     char *line = av_malloc(2 * qm_size + 1);
//!     unsigned i;
//!     for (i = 0; i < qm_size; i++)
//!         snprintf(line + 2 * i, 3, "%02x", raw[i]);
//!     av_log(NULL, AV_LOG_INFO, "PFQM %s %u %s\n",
//!            avcodec_get_name(avctx->codec_id), pf_au, line);
//!     av_free(line);
//! } else {
//!     av_log(NULL, AV_LOG_INFO, "PFQM %s %u absent\n",
//!            avcodec_get_name(avctx->codec_id), pf_au);
//! }
//! ```
//!
//! The `absent` spelling is required rather than an omitted line: an omitted line is
//! indistinguishable from a missed patch, and "was the buffer submitted" is the single fact
//! review 13's HEVC defect turned on.
//!
//! **5. The slice-control format.** One line per AU (or one for the whole run — the parser takes
//! either), from the same place, so an inverted short/long-format number cannot pass unseen:
//!
//! ```c
//! av_log(NULL, AV_LOG_INFO, "PFCFG %s %u %u\n",
//!        avcodec_get_name(avctx->codec_id), pf_au,
//!        (unsigned)DXVA_CONTEXT_CFG_BITSTREAM(avctx, ctx));
//! ```
//!
//! `ConfigBitstreamRaw`'s short-format value is **2 for H.264 and 1 for HEVC** — one number
//! with two spellings, and an inverted pair swaps which slice-control STRUCT the driver reads
//! while every other byte still looks right. If the macro is spelled differently in the tree,
//! any expression yielding the negotiated config's `ConfigBitstreamRaw` will do.
//!
//! **5b. AV1.** The identical `PFPP` block goes at the very END of
//! `ff_dxva2_av1_fill_picture_parameters` (`dxva2_av1.c:60`), with `h264` replaced by `av1` —
//! after the film-grain block, so every field is final. Three things differ from the two codecs
//! above and each of them changes what a capture MEANS:
//!
//! * **The AU index is a FRAME, not a temporal unit.** `ff_dxva2_common_end_frame` runs once per
//!   submitted picture and an AV1 temporal unit may decode several, so `pf_au_index` walks
//!   decoded frames. This crate's [`our_av1_submissions`] emits one entry per decoded frame for
//!   the same reason, and the vendored vector is **274** frames in 250 units — a capture with
//!   250 `PFPP av1` lines is a capture of something else. (A `show_existing_frame` unit submits
//!   nothing on either side; this vector has none.)
//! * **No `PFQM` line and no matrix buffer.** `dxva2_av1_end_frame` passes `NULL, 0` for the qm
//!   pair, so the `qm_size > 0` branch of the block in step 4 logs `absent` on every frame. That
//!   is the expected reading, not a missed patch site.
//! * **No `PFCFG` check.** [`preflight`]'s `ConfigBitstreamRaw` assertion is about the two short
//!   slice-control formats; AV1's slice-control record is `DXVA_Tile_AV1` and has no short/long
//!   pair, so a captured `PFCFG av1` line is ignored rather than compared.
//!
//! The stream is the vendored IVF at
//! `crates/pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1`:
//!
//! ```text
//! ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 -i test-25fps.ivf.av1 -f null - 2> av1.log
//! grep -oE 'PF(PP|QM|BD|CFG) .*' av1.log > libav-av1.capture
//! ```
//!
//! ⚠ Add `-export_side_data +film_grain` to NOTHING: `apply_grain` and `pp->coding.film_grain`
//! both turn OFF when film grain is exported as side data, and this crate always applies it in
//! the decoder. The vendored vector codes no grain either way.
//!
//! **No such capture has been taken.** As of 2026-08-07 the AV1 comparison
//! (`our_av1_picture_parameters_match_libavcodecs`) has never run against libavcodec's bytes,
//! and the reason is stated rather than left as an absent result: `.221`, the only box in this
//! fleet with a D3D11VA GPU to spare, has no MSYS2, no gcc and no make, so producing a patched
//! FFmpeg there is a toolchain bring-up rather than a build. What DID localise the AV1 defect
//! of 2026-08-07 was `video_d3d11_native`'s frame-hash parity harness plus a CPU invariant
//! (`no_av1_submission_names_its_decode_surface_in_the_reference_store`, below) — so this file's
//! AV1 half is currently the no-capture half only, and every claim it makes about libavcodec's
//! AV1 side is READ out of `dxva2_av1.c` (n8.1) rather than measured. Tier one, in the
//! provenance section's terms, for all of it.
//!
//! **6. Run it.** `--enable-d3d11va` is on by default on Windows. Decode the SAME elementary
//! streams this test plans — the vendored vectors, in the repository at
//! `crates/pf-bitstream/vendor/cros-codecs/src/codec/{h264,h265}/test_data/test-25fps.{h264,h265}`:
//!
//! ```text
//! ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 -i test-25fps.h264 -f null - 2> h264.log
//! ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 -i test-25fps.h265 -f null - 2> hevc.log
//! grep -oE 'PF(PP|QM|BD|CFG) .*' h264.log > libav-h264.capture
//! grep -oE 'PF(PP|QM|BD|CFG) .*' hevc.log > libav-hevc.capture
//! ```
//!
//! `av_log(NULL, …)` rather than `av_log(avctx, …)` throughout, and `grep -o` rather than an
//! anchored match, for one reason: FFmpeg's logger prefixes a message logged against a context
//! with `[h264 @ 0x…] `, which no anchored grep would match. The parser finds its marker anywhere
//! in a line, so a capture made either way is readable — but the flat form is what the recipe
//! asks for, because a capture that greps cleanly is a capture whose format can be eyeballed.
//!
//! A software fallback produces no lines at all, which is the check that the hwaccel actually
//! engaged: 250 `PFPP` lines per stream or the capture is void. Then:
//!
//! ```text
//! PF_LIBAV_CAPTURE_H264=libav-h264.capture PF_LIBAV_CAPTURE_HEVC=libav-hevc.capture \
//!     PF_LIBAV_CAPTURE_AV1=libav-av1.capture \
//!     cargo test -p pf-dxvadec --test libav_picparams_parity -- --ignored --nocapture
//! ```
//!
//! `PF_DXVA_DUMP=<path>` writes THIS side in the same format, both codecs, without needing a
//! capture — so the two files can also be diffed by hand.
//!
//! # Differences that are EXPECTED, and must not be read as defects
//!
//! A raw `memcmp` of the picture parameters differs by design, which is why this harness does
//! not do one. Each expected divergence is handled STRUCTURALLY instead, so what is left over is
//! a finding:
//!
//! * **Surface indices.** `CurrPic` and every reference entry carry a decode-surface index.
//!   libavcodec's comes from its own frame pool's allocation order; ours from
//!   [`pf_dxvadec::SlotMap`]. The two are a BIJECTION over the same pictures, never equal
//!   numbers. The harness therefore tracks the mapping per PICTURE — identified by the pair-key
//!   DXVA itself resolves references by — and reports only a mapping that CHANGES while the
//!   picture is still in the DPB, or two live pictures collapsing onto one surface. A differing
//!   index alone proves nothing; an index that stops agreeing does.
//! * **Reference-array ORDER.** For H.264 libavcodec emits `short_ref` then `long_ref`; this
//!   crate emits the AU's own references first and the rest of the marked DPB after (see
//!   `pic.rs`'s module docs for why). For HEVC libavcodec walks its DPB array in slot order
//!   while this crate leads with the three current RPS sets (`pic_h265.rs`'s docs). Both are
//!   correct — DXVA imposes no order, a driver resolves an entry by its keys — so the arrays are
//!   compared as SETS of `(marking, key, POC, use-flags)` tuples, with the per-entry bits of
//!   `UsedForReferenceFlags`/`NonExistingFrameFlags` carried inside each tuple, which is what
//!   "re-indexed to the compared order" amounts to.
//! * **HEVC's RPS index arrays** (`RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr`) hold INDICES
//!   into `RefPicList`, so a different array order means different index VALUES for the same
//!   pictures. They are compared by resolving each index through its own side's `RefPicList` and
//!   comparing the PICTURES named, position by position — position order is 8.3.4's and does
//!   matter.
//! * **The bitstream buffer's `DataSize`** may legitimately differ; see the descriptor section
//!   below, which states exactly how a legitimate difference is told from a defect. (On the
//!   capture described below it does not differ at all, on any of 500 AUs.)
//! * **libavcodec's POC BASE.** Measured, not predicted: every `CurrFieldOrderCnt` and
//!   `FieldOrderCntList` value in the H.264 capture is the specification's plus exactly **65536**
//!   — FFmpeg seeds `prev_poc_msb = 1 << 16` at each IDR. The progression is identical; only the
//!   base differs, and it is uniform across the current picture and every reference entry, so
//!   every difference a driver computes from these fields (temporal direct, implicit weighted
//!   prediction, co-located selection) is unaffected. This crate keeps 8.2.1's values and the
//!   harness compares POCs RELATIVE to a base it derives from the first AU and then REQUIRES of
//!   every AU after it — so a genuinely wrong POC still reports. libavcodec's HEVC POCs carry no
//!   such offset (measured: base 0 on all 250 AUs). See `PocBase`.
//! * **HEVC `loop_filter_across_tiles_enabled_flag`** (bit 10 of
//!   `dwCodingSettingPicturePropertyFlags`): ours 1, libavcodec's 0, on all 250 AUs. 7.4.3.3.1
//!   infers 1 when the PPS codes no tiles, which is what the vendored parser reports; libav's
//!   parser evidently leaves it 0. Inert either way — with `tiles_enabled_flag` clear there is no
//!   tile boundary for a loop filter to cross — so it is DOCUMENTED rather than changed: matching
//!   libav would mean overriding a spec inference on the strength of one measurement of another
//!   decoder's parser default. The allowance is exactly bit 10, exactly ours-set-theirs-clear, and
//!   only while both sides agree tiles are off; see `hevc_allowance`.
//!
//! The last two are reported on every run as DOCUMENTED divergences with their AU counts, never
//! silently dropped, and each one's allowance is narrow enough that the next difference in the same
//! field is still a finding — which two non-ignored tests
//! (`libavcodecs_constant_poc_base_is_documented_and_anything_else_about_a_poc_is_a_finding`,
//! `the_hevc_tiles_flag_allowance_is_exactly_bit_ten_with_tiles_disabled_and_nothing_else`) prove
//! by synthesising the differences an allowance must NOT absorb.
//!
//! Everything else — every parameter-set field, every flag word, `frame_num`, `ContinuationFlag`,
//! `StatusReportFeedbackNumber` (both count from 1 per picture), and every reserved field — must
//! match byte for byte, and a difference there is the finding this harness exists to produce.
//!
//! # What the first real run said
//!
//! Run on 2026-08-06 against a patched FFmpeg n8.1 capture from the RTX 4090 box, 250 AUs per
//! codec, both vendored vectors. **Four comparisons, zero undocumented divergences**: H.264
//! picture parameters, HEVC picture parameters, the buffer descriptors of both codecs, and the
//! quantization matrices of both. The two divergences above were the entire delta.
//!
//! The measured ground truth, so the next reader needs no capture to know what libavcodec emits:
//!
//! | | H.264 | HEVC |
//! |---|---|---|
//! | picture parameters | 1040 bytes | 232 bytes |
//! | `ConfigBitstreamRaw` | 2 | 1 |
//! | IQ matrix | submitted on all 250 (224 bytes) | `absent` on all 250 |
//! | descriptors per AU | 4 (types 0, 4, 6, 5) | 3 (types 0, 6, 5) |
//! | `SLICE_CONTROL.DataSize` | 20 (2 slices × 10) | 10 (1 slice × 10) |
//! | `BITSTREAM.DataSize` | 256..6272, all ≡ 0 (mod 128) | 128..8320, all ≡ 0 (mod 128) |
//! | `NumMBsInBuffer` | 300 on BITSTREAM and SLICE_CONTROL, 0 on the other two | 0 on all |
//! | `DataOffset` | 0 on all | 0 on all |
//!
//! Three things that settles beyond this harness: the short slice record is **ten** bytes (20/2 and
//! 10/1, two codecs and two slice counts agreeing); every `BITSTREAM.DataSize` matches this crate's
//! packer exactly, so the start-code/rebase/padding rules were right; and the HEVC vector exercises
//! case 1 of the quantization matrix's three cases (`scaling_list_enabled_flag` clear) — cases 2
//! and 3 remain CPU-only, which is stated rather than papered over.
//!
//! ## The two libavcodec workarounds, and why a capture can be VOID
//!
//! `Reserved16Bits = 3` is the notable field: libavcodec writes 3 for every standard profile and
//! 0 only under one of two workarounds, both of which also change other bytes.
//!
//! * `FF_DXVA2_WORKAROUND_INTEL_CLEARVIDEO` is set iff the negotiated decoder GUID is the legacy
//!   `ff_DXVADDI_Intel_ModeH264_E` (dxva2.c:302-303), and it changes two H.264 things
//!   (dxva2_h264.c:128 and :257). **Our side cannot select that GUID**: config.rs's table holds
//!   three standard GUIDs — [`pf_dxvadec::H264_VLD_NOFGT`] is `DXVA2_ModeH264_E` — and
//!   `video_d3d11_native.rs` asks the device for nothing else. The exposure is entirely on the
//!   CAPTURE side: on an old Intel part, the FFmpeg producing the capture may negotiate
//!   ClearVideo, and then its bytes are not ours to compare against. ⚠ Worth re-reading at the
//!   Intel bring-up: modern parts negotiate the standard GUID, so this should not fire — but
//!   "should not" is what a preflight is for.
//! * `FF_DXVA2_WORKAROUND_SCALING_LIST_ZIGZAG` (old ATI/AMD UVD) is never auto-set in the modern
//!   hwaccel path — it is user-set through the legacy context only — so libav takes the
//!   `ff_zigzag_scan`-indexed branch of `ff_dxva2_h264_fill_scaling_lists`, which emits the
//!   matrices in CODED (zig-zag) order. That is the order this crate's matrices are already in
//!   (the vendored parser stores each list as coded), which is what makes the H.264
//!   quantization-matrix comparison a straight byte compare.
//!
//! A capture whose `Reserved16Bits` is 0 was therefore made against a workaround path and is VOID
//! for comparison; the harness refuses it up front rather than reporting 250 findings.
//!
//! # The buffer descriptors
//!
//! [`pf_dxvadec::descriptors`] models them, and its module docs carry the value table and the
//! libavcodec citation for every field. What matters for a COMPARISON:
//!
//! * `CompressedBufferType` (D3D11's `BufferType`), the buffer SET and its ORDER cannot
//!   legitimately differ. A type present on one side only is a finding — and for HEVC's
//!   quantization matrix that finding IS review 13's defect.
//! * `DataOffset` is 0 on both sides, always.
//! * `NumMBsInBuffer` cannot legitimately differ: it is `mb_width * mb_height` on H.264's
//!   bitstream and slice-control buffers, 0 everywhere else and on all of HEVC's.
//! * `DataSize` for the picture parameters, the matrices and the slice control cannot
//!   legitimately differ either — they are `sizeof` a structure and `slices * 10` (the short
//!   slice record is TEN bytes, packed; see `dxva.rs`'s alignment section for the measurement and
//!   for what twelve would have cost). The slice control's size is therefore also a slice COUNT:
//!   if it differs, the two sides disagree about how many slices the AU has, which voids that
//!   AU's bitstream comparison and is reported as its own finding.
//! * `DataSize` for the BITSTREAM buffer is the one field with a legitimate divergence class.
//!   Both sides pack slice NALUs only, each behind a normalised three-byte start code, and pad
//!   the total to 128 bytes — this crate because the DXVA specs say so and because non-VCL NALUs
//!   inside the decode range hang AMD's VCN firmware (the same discipline pf-vkdecode's
//!   recording layer follows), libavcodec in `commit_bitstream_and_slice_buffer` for its own
//!   reasons. So the sizes normally match exactly. They may differ by a FEW bytes per slice when
//!   the two NALU splitters delimit a slice differently — trailing `zero_byte`s ahead of the next
//!   start code belong to neither NALU, and a splitter may keep or drop them. That difference is
//!   legitimate, and it is recognisable: the slice COUNT agrees, and the difference is under four
//!   bytes per slice before padding. Anything else — a differing slice count, a size that is not
//!   a multiple of 128 (which means the driver's mapping was too small for the padding), a
//!   difference of hundreds of bytes — is a defect, and the harness classifies the two cases
//!   apart rather than lumping them into one "DataSize differs".
//!
//! # Provenance, and what a reviewer must re-check
//!
//! There is no FFmpeg source in this worktree, so nothing here can verify a claim about
//! libavcodec. Two tiers, and the difference matters:
//!
//! * **Read out of an FFmpeg n8.1 tree** by this work package's coordinator: the function names
//!   and every `file:line`, the qmatrix predicate's codec-asymmetry, the `NumMBsInBuffer`
//!   asymmetry, and both workaround conditions.
//! * **Read out of the same tree, then CONFIRMED by the capture**: that
//!   `commit_bitstream_and_slice_buffer` writes a three-byte start code ahead of each slice,
//!   rebases `BSNALunitDataLocation` into the buffer, counts the start code in
//!   `SliceBytesInBuffer`, pads with `FFMIN(128 - ((current - dxva_data) & 127), end - current)`
//!   and charges that padding to the LAST record (`slice->SliceBytesInBuffer += padding`). This was
//!   flagged as an unverified assumption in the first revision of this file, because the
//!   `BITSTREAM.DataSize` classification rests on it; the capture then matched this crate's packed
//!   size on all 500 AUs of both codecs, which is that assumption's proof.
//!
//! Everything in either tier is a contract this harness parses and compares against, not a fact
//! it establishes. The capture is the authority; a disagreement between a capture and a claim
//! above is a claim to fix.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Cursor;
use std::mem::offset_of;
use std::mem::size_of;
use std::ops::Range;

use pf_dxvadec::descriptors::BUFFER_BITSTREAM;
use pf_dxvadec::descriptors::BUFFER_INVERSE_QUANTIZATION_MATRIX;
use pf_dxvadec::descriptors::BUFFER_PICTURE_PARAMETERS;
use pf_dxvadec::descriptors::BUFFER_SLICE_CONTROL;
use pf_dxvadec::dxva::PicParamsH264;
use pf_dxvadec::dxva::PicParamsHevc;
use pf_dxvadec::dxva::QmatrixH264;
use pf_dxvadec::dxva::QmatrixHevc;
use pf_dxvadec::dxva::SliceH264Short;
use pf_dxvadec::dxva::SliceHevcShort;
use pf_dxvadec::dxva::UNUSED_ENTRY;
use pf_dxvadec::dxva_av1::PicEntryAv1;
use pf_dxvadec::dxva_av1::UNUSED_INDEX;
use pf_dxvadec::AuPlan;
use pf_dxvadec::Av1Planner;
use pf_dxvadec::BufferDescriptor;
use pf_dxvadec::Codec;
use pf_dxvadec::H264Planner;
use pf_dxvadec::H265Planner;
use pf_dxvadec::PicParamsAv1;
use pf_dxvadec::SliceRecord;
use pf_dxvadec::SlotMap;
use pf_dxvadec::TileAv1;
use pf_dxvadec::NUM_REF_SLOTS;

const TEST_25FPS_H264: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);
const TEST_25FPS_H265: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
);
/// The AV1 vector is an IVF container, and its unit of comparison is the TEMPORAL UNIT rather
/// than the access unit: one IVF packet may decode several frames, of which at most one shows.
const TEST_25FPS_AV1: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
);

/// Both vendored vectors carry exactly this many access units — pf-bitstream's own golden, and
/// the number of `PFPP` lines a valid capture holds.
const VENDORED_AUS: usize = 250;

/// A generous stand-in for the driver's bitstream mapping. Real mappings are a few MiB; the
/// vendored vectors are 320x240, so nothing here comes close to the tail-padding clamp (which
/// [`pf_dxvadec::pack`]'s own unit tests cover).
const MAPPING_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Access-unit splitting
// ---------------------------------------------------------------------------

/// The same AU splitter every H.264 test in this program uses: a new AU starts at a non-slice
/// NALU following a slice, or at a slice whose `first_mb_in_slice` is 0 following a slice.
fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h264::parser::Nalu;
    use cros_codecs::codec::h264::parser::NaluType;

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

/// The H.265 splitter, which is NOT the H.264 one: the flag that starts a picture is
/// `first_slice_segment_in_pic_flag`, the first bit after the TWO-byte NAL header, and "slice" is
/// every NALU type below 32. Copied from the tested implementation in
/// `crates/pf-bitstream/src/h265.rs` (`fn split_into_aus`, test-private there) rather than
/// re-derived, because a splitter that disagrees with pf-bitstream's would make every AU index
/// in a capture point at a different picture.
fn split_into_aus_h265(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h265::parser::Nalu;

    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let header_start = cursor.position() as usize;
        let start = header_start - nalu.offset;
        let is_slice = (nalu.header.type_ as u32) < 32;
        let first_slice_flag =
            is_slice && stream.get(header_start + 2).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_slice_flag) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

// ---------------------------------------------------------------------------
// This crate's side
// ---------------------------------------------------------------------------

/// Everything one AU's `SubmitDecoderBuffers` call would carry, from this crate.
struct OurSubmission {
    /// The picture-parameters buffer's bytes.
    pic_params: Vec<u8>,
    /// The quantization-matrix buffer's bytes, or `None` when the buffer is not submitted at
    /// all — which for HEVC is the whole of review 13's defect.
    qmatrix: Option<Vec<u8>>,
    /// The descriptor set, in submission order.
    descriptors: Vec<BufferDescriptor>,
    /// The packer's slice records, for the internal-consistency checks. Empty on AV1, whose
    /// slice-control buffer holds [`Self::tiles`] instead.
    records: Vec<SliceRecord>,
    /// AV1's slice-control records — one `DXVA_Tile_AV1` per TILE, not per tile group. Empty
    /// on H.264 and H.265.
    tiles: Vec<TileAv1>,
    /// Bytes the packer wrote BEFORE the tail padding.
    unpadded: u32,
    /// `mb_width * mb_height` (H.264) or 0 (HEVC) — the value the descriptors must carry.
    mb_count: u32,
}

/// Plan and convert the whole vendored H.264 vector, one entry per AU.
///
/// Every AU must plan and convert: this vector is pf-bitstream's clean golden, so a skipped AU
/// is a regression rather than a stream fact. Swallowing an error here — the shape the scaffold
/// this replaced had — is how a harness reports a clean bill of health while comparing nothing.
fn our_h264_submissions() -> Vec<OurSubmission> {
    let mut planner = H264Planner::new();
    let mut slots: Option<SlotMap> = None;
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, au) in split_into_aus(TEST_25FPS_H264).into_iter().enumerate() {
        let plan: AuPlan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {i} of the vendored H.264 vector must plan: {e}"));
        let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
        if map.capacity() != plan.picture.max_dpb_frames + 1 {
            *map = SlotMap::new(plan.picture.max_dpb_frames);
        }
        // `StatusReportFeedbackNumber` counts planned pictures from 1, which is exactly what
        // libavcodec's `1 + report_id++` produces for a decoder that saw only this stream.
        let dxva = pf_dxvadec::plan_to_dxva(&plan, map, out.len() as u32 + 1)
            .unwrap_or_else(|e| panic!("AU {i} must convert: {e}"));
        let packed = pf_dxvadec::pack(au, &dxva.slice_ranges, &mut mapping)
            .unwrap_or_else(|e| panic!("AU {i} must pack: {e}"));
        let unpadded = pf_dxvadec::packed_size(au, &dxva.slice_ranges).expect("packed size") as u32;
        out.push(OurSubmission {
            pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
            qmatrix: Some(pf_dxvadec::as_bytes(&dxva.qmatrix).to_vec()),
            descriptors: pf_dxvadec::descriptors_h264(&dxva, &packed),
            records: packed.records,
            tiles: Vec::new(),
            unpadded,
            mb_count: dxva.mb_count,
        });
    }
    assert_eq!(out.len(), VENDORED_AUS);
    out
}

/// Plan and convert the whole vendored HEVC vector, one entry per AU. Same no-skipping contract
/// as the H.264 side — `RaslSkipped` cannot arise on a vector that starts at an IDR.
fn our_hevc_submissions() -> Vec<OurSubmission> {
    let mut planner = H265Planner::new();
    let mut slots: Option<SlotMap> = None;
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, au) in split_into_aus_h265(TEST_25FPS_H265).into_iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {i} of the vendored HEVC vector must plan: {e}"));
        let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
        if map.capacity() != plan.picture.max_dpb_frames + 1 {
            *map = SlotMap::new(plan.picture.max_dpb_frames);
        }
        let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, map, out.len() as u32 + 1)
            .unwrap_or_else(|e| panic!("AU {i} must convert: {e}"));
        let packed = pf_dxvadec::pack(au, &dxva.slice_ranges, &mut mapping)
            .unwrap_or_else(|e| panic!("AU {i} must pack: {e}"));
        let unpadded = pf_dxvadec::packed_size(au, &dxva.slice_ranges).expect("packed size") as u32;
        out.push(OurSubmission {
            pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
            qmatrix: dxva
                .qmatrix
                .as_ref()
                .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
            descriptors: pf_dxvadec::descriptors_h265(&dxva, &packed),
            records: packed.records,
            tiles: Vec::new(),
            unpadded,
            mb_count: 0,
        });
    }
    assert_eq!(out.len(), VENDORED_AUS);
    out
}

/// Every AV1 FRAME the vendored vector decodes — 274, of which 250 are displayed.
///
/// The unit of comparison is the frame and not the temporal unit, because that is what
/// libavcodec's hwaccel counts: `ff_dxva2_common_end_frame` runs once per submitted PICTURE, so
/// a capture's AU index walks decoded frames. A `show_existing_frame` unit submits nothing and
/// appears on neither side; this vector has none.
const VENDORED_AV1_FRAMES: usize = 274;

/// Plan, convert and pack the whole vendored AV1 vector, one entry per decoded FRAME.
///
/// ⚠ This is the only one of the three that has to speak the conversion's DEFERRED RELEASE
/// contract ([`pf_dxvadec::DecodePlanDxvaAv1::release_after_decode`]). A loop that converts
/// without it holds a surface on 268 of these 274 frames and runs the nine-slot ledger dry
/// inside ten — and, worse for a harness, it would compare a submission built by a caller that
/// is not the rung.
fn our_av1_submissions() -> Vec<OurSubmission> {
    let mut planner = Av1Planner::new();
    let mut slots = SlotMap::new(NUM_REF_SLOTS);
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, unit) in split_ivf(TEST_25FPS_AV1).into_iter().enumerate() {
        let plans = planner
            .plan_au(unit)
            .unwrap_or_else(|e| panic!("unit {i} of the vendored AV1 vector must plan: {e}"));
        for plan in &plans {
            if plan.dpb.stored.is_none() {
                continue; // `show_existing_frame`: no submission at all
            }
            let dxva = pf_dxvadec::plan_to_dxva_av1(unit, plan, &mut slots)
                .unwrap_or_else(|e| panic!("unit {i} must convert: {e}"));
            let packed = pf_dxvadec::pack_av1(unit, &dxva.bitstream, &dxva.tiles, &mut mapping)
                .unwrap_or_else(|e| panic!("unit {i} must pack: {e}"));
            let unpadded = pf_dxvadec::packed_size_av1(&dxva.bitstream) as u32;
            out.push(OurSubmission {
                pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                // AV1 transmits no quantization matrix at all: its matrices are SELECTED by
                // index out of tables the decoder already has, and `dxva2_av1_end_frame`
                // passes `NULL, 0` for the pair. `None` here is a fact about the codec, not a
                // condition on the stream the way HEVC's is.
                qmatrix: None,
                descriptors: pf_dxvadec::descriptors_av1(&packed),
                // AV1's slice-control records are `DXVA_Tile_AV1`, a different struct with a
                // different size; the shared `SliceRecord` checks do not apply to them, and
                // the tile records get their own test rather than a coerced one.
                records: Vec::new(),
                tiles: packed.tiles.clone(),
                unpadded,
                mb_count: 0,
            });
            for &id in &dxva.release_after_decode {
                assert!(
                    slots.release(id),
                    "unit {i}: a deferred release named a picture holding no surface"
                );
            }
        }
    }
    assert_eq!(out.len(), VENDORED_AV1_FRAMES);
    out
}

/// The IVF frame walk — the same one `video_d3d11_native`'s parity module and every AV1 test in
/// this program use: a 32-byte file header, then a 12-byte header per packet carrying its size.
fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 32usize;
    while at + 12 <= stream.len() {
        let size = u32::from_le_bytes([stream[at], stream[at + 1], stream[at + 2], stream[at + 3]])
            as usize;
        at += 12;
        if at + size > stream.len() {
            break;
        }
        out.push(&stream[at..at + size]);
        at += size;
    }
    out
}

// ---------------------------------------------------------------------------
// Offset → field name
// ---------------------------------------------------------------------------

/// A field table for a hand-declared DXVA struct: `(name, offset)` per field, in declaration
/// order, built from the field IDENTIFIERS so a name and the offset it reports cannot drift
/// apart — the whole point of the table is to turn a differing byte into a field name, and a
/// table with a copy-pasted mismatch would name the wrong one.
/// Nested paths (`tiles.cols`) are accepted as well as plain identifiers, and are named by
/// the whole path — AV1's picture parameters are eight nested blocks, and a table that could
/// only reach the outer members would report "segmentation differs" for a 140-byte struct.
macro_rules! field_table {
    ($ty:ty, $($($field:ident).+),+ $(,)?) => {
        &[$((stringify!($($field).+), offset_of!($ty, $($field).+))),+]
    };
}

/// Every field of `DXVA_PicParams_H264`. Lengths are DERIVED from the next field's offset rather
/// than written down: the struct has no interior padding (dxva.rs proves every offset at compile
/// time), so consecutive offsets tile it exactly — and a hand-typed length is one more thing that
/// can be wrong in a file whose whole job is to catch wrong numbers.
const H264_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsH264,
    wFrameWidthInMbsMinus1,
    wFrameHeightInMbsMinus1,
    CurrPic,
    num_ref_frames,
    wBitFields,
    bit_depth_luma_minus8,
    bit_depth_chroma_minus8,
    Reserved16Bits,
    StatusReportFeedbackNumber,
    RefFrameList,
    CurrFieldOrderCnt,
    FieldOrderCntList,
    pic_init_qs_minus26,
    chroma_qp_index_offset,
    second_chroma_qp_index_offset,
    ContinuationFlag,
    pic_init_qp_minus26,
    num_ref_idx_l0_active_minus1,
    num_ref_idx_l1_active_minus1,
    Reserved8BitsA,
    FrameNumList,
    UsedForReferenceFlags,
    NonExistingFrameFlags,
    frame_num,
    log2_max_frame_num_minus4,
    pic_order_cnt_type,
    log2_max_pic_order_cnt_lsb_minus4,
    delta_pic_order_always_zero_flag,
    direct_8x8_inference_flag,
    entropy_coding_mode_flag,
    pic_order_present_flag,
    num_slice_groups_minus1,
    slice_group_map_type,
    deblocking_filter_control_present_flag,
    redundant_pic_cnt_present_flag,
    Reserved8BitsB,
    slice_group_change_rate_minus1,
    SliceGroupMap,
);

/// Every field of `DXVA_PicParams_HEVC`, same construction.
const HEVC_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsHevc,
    PicWidthInMinCbsY,
    PicHeightInMinCbsY,
    wFormatAndSequenceInfoFlags,
    CurrPic,
    sps_max_dec_pic_buffering_minus1,
    log2_min_luma_coding_block_size_minus3,
    log2_diff_max_min_luma_coding_block_size,
    log2_min_transform_block_size_minus2,
    log2_diff_max_min_transform_block_size,
    max_transform_hierarchy_depth_inter,
    max_transform_hierarchy_depth_intra,
    num_short_term_ref_pic_sets,
    num_long_term_ref_pics_sps,
    num_ref_idx_l0_default_active_minus1,
    num_ref_idx_l1_default_active_minus1,
    init_qp_minus26,
    ucNumDeltaPocsOfRefRpsIdx,
    wNumBitsForShortTermRPSInSlice,
    ReservedBits2,
    dwCodingParamToolFlags,
    dwCodingSettingPicturePropertyFlags,
    pps_cb_qp_offset,
    pps_cr_qp_offset,
    num_tile_columns_minus1,
    num_tile_rows_minus1,
    column_width_minus1,
    row_height_minus1,
    diff_cu_qp_delta_depth,
    pps_beta_offset_div2,
    pps_tc_offset_div2,
    log2_parallel_merge_level_minus2,
    CurrPicOrderCntVal,
    RefPicList,
    ReservedBits5,
    PicOrderCntValList,
    RefPicSetStCurrBefore,
    RefPicSetStCurrAfter,
    RefPicSetLtCurr,
    ReservedBits6,
    ReservedBits7,
    StatusReportFeedbackNumber,
);

/// `DXVA_Qmatrix_H264`'s two arrays.
const H264_QMATRIX_FIELDS: &[(&str, usize)] =
    field_table!(QmatrixH264, bScalingLists4x4, bScalingLists8x8);

/// The two short slice-control records, which are the structs the twelve-vs-ten defect was in.
const H264_SLICE_FIELDS: &[(&str, usize)] = field_table!(
    SliceH264Short,
    BSNALunitDataLocation,
    SliceBytesInBuffer,
    wBadSliceChopping,
);
const HEVC_SLICE_FIELDS: &[(&str, usize)] = field_table!(
    SliceHevcShort,
    BSNALunitDataLocation,
    SliceBytesInBuffer,
    wBadSliceChopping,
);

/// `DXVA_Qmatrix_HEVC`'s six.
const HEVC_QMATRIX_FIELDS: &[(&str, usize)] = field_table!(
    QmatrixHevc,
    ucScalingLists0,
    ucScalingLists1,
    ucScalingLists2,
    ucScalingLists3,
    ucScalingListDCCoefSizeID2,
    ucScalingListDCCoefSizeID3,
);

/// Every field of `DXVA_PicParams_AV1`, same construction — but reaching INTO the eight nested
/// blocks, because they are where AV1 keeps almost the whole frame header. `tiles` alone is 260
/// bytes and `segmentation` 140; a table stopping at the outer members would turn every finding
/// in them into one useless name.
///
/// Three members stay whole on purpose. `frame_refs` is seven 36-byte entries which — unlike
/// the other two codecs' reference arrays — carry NO surface index and so compare byte for
/// byte: `Index` is `ref_frame_idx[name]`, an AV1 SLOT both sides read out of the same frame
/// header. `ref_frame_map_texture_index` is the surface array, which cannot be compared by
/// value at all ([`av1_reference_store`]). And `film_grain` is 158 bytes neither vendored
/// vector codes.
const AV1_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsAv1,
    width,
    height,
    max_width,
    max_height,
    curr_pic_texture_index,
    superres_denom,
    bitdepth,
    seq_profile,
    tiles.cols,
    tiles.rows,
    tiles.context_update_id,
    tiles.widths,
    tiles.heights,
    coding,
    format,
    primary_ref_frame,
    order_hint,
    order_hint_bits,
    frame_refs,
    ref_frame_map_texture_index,
    loop_filter.filter_level,
    loop_filter.filter_level_u,
    loop_filter.filter_level_v,
    loop_filter.sharpness_level,
    loop_filter.control_flags,
    loop_filter.ref_deltas,
    loop_filter.mode_deltas,
    loop_filter.delta_lf_res,
    loop_filter.frame_restoration_type,
    loop_filter.log2_restoration_unit_size,
    loop_filter.reserved16,
    quantization.control_flags,
    quantization.base_qindex,
    quantization.y_dc_delta_q,
    quantization.u_dc_delta_q,
    quantization.v_dc_delta_q,
    quantization.u_ac_delta_q,
    quantization.v_ac_delta_q,
    quantization.qm_y,
    quantization.qm_u,
    quantization.qm_v,
    quantization.reserved16,
    cdef.control_flags,
    cdef.y_strengths,
    cdef.uv_strengths,
    interp_filter,
    segmentation.control_flags,
    segmentation.reserved24,
    segmentation.feature_mask,
    segmentation.feature_data,
    film_grain,
    reserved32,
    status_report_feedback_number,
);

/// `DXVA_Tile_AV1` — AV1's slice-control record, and the one this crate had to derive rather
/// than measure (`dxva.h` declares it; the SIZE is what the descriptor states).
const AV1_TILE_FIELDS: &[(&str, usize)] = field_table!(
    TileAv1,
    data_offset,
    data_size,
    row,
    column,
    reserved16,
    anchor_frame,
    reserved8,
);

/// Turn a field table into `(name, byte range)`, the last field running to `total`.
fn field_ranges(
    fields: &[(&'static str, usize)],
    total: usize,
) -> Vec<(&'static str, Range<usize>)> {
    fields
        .iter()
        .enumerate()
        .map(|(i, &(name, offset))| {
            let end = fields.get(i + 1).map_or(total, |&(_, next)| next);
            (name, offset..end)
        })
        .collect()
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    u32_at(bytes, offset) as i32
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// One kind of divergence, and how often it happened.
struct Finding {
    count: usize,
    first_au: usize,
    detail: String,
}

/// Divergences, grouped by the FIELD they belong to rather than by byte offset. A byte offset is
/// nearly useless on a hand-declared struct; the crate proves its offsets at compile time, so
/// this maps them back to names and reports those.
///
/// Two channels, and the split is the whole reason this type exists rather than a `Vec`:
/// [`Findings::note`] records a FINDING (the run fails), [`Findings::document`] records a
/// divergence this program has already decided is not a defect. Documented ones are printed on
/// every run with their reason and their AU count — never dropped, because a divergence nobody
/// prints is a divergence nobody re-reads, and each of them is only allowed within a stated
/// allowance that the comparison itself enforces.
#[derive(Default)]
struct Findings {
    by_field: BTreeMap<String, Finding>,
    documented: BTreeMap<String, Finding>,
}

impl Findings {
    fn note(&mut self, field: impl Into<String>, au: usize, detail: impl Into<String>) {
        let entry = self
            .by_field
            .entry(field.into())
            .or_insert_with(|| Finding {
                count: 0,
                first_au: au,
                detail: detail.into(),
            });
        entry.count += 1;
    }

    /// A divergence the module docs list, with the reason it is not a defect.
    fn document(&mut self, field: impl Into<String>, au: usize, reason: impl Into<String>) {
        let entry = self
            .documented
            .entry(field.into())
            .or_insert_with(|| Finding {
                count: 0,
                first_au: au,
                detail: reason.into(),
            });
        entry.count += 1;
    }

    fn is_empty(&self) -> bool {
        self.by_field.is_empty()
    }

    fn fields(&self) -> Vec<&str> {
        self.by_field.keys().map(String::as_str).collect()
    }

    fn documented_fields(&self) -> Vec<&str> {
        self.documented.keys().map(String::as_str).collect()
    }

    /// Print the verdict and fail if there is one. Never silently passes: a run that classified
    /// nothing prints the AU count it did compare, so "no findings" cannot be confused with
    /// "nothing was compared".
    fn verdict(&self, what: &str, aus: usize) {
        for (field, documented) in &self.documented {
            println!(
                "{what}: {field} diverges on {} of {aus} AUs (first at AU {}) — DOCUMENTED, not a \
                 defect: {}",
                documented.count, documented.first_au, documented.detail
            );
        }
        if self.is_empty() {
            println!("{what}: {aus} AUs compared, no undocumented divergence");
            return;
        }
        println!(
            "{what}: {aus} AUs compared, {} fields diverge:",
            self.by_field.len()
        );
        for (field, finding) in &self.by_field {
            println!(
                "  {field}: {} AUs, first at AU {} — {}",
                finding.count, finding.first_au, finding.detail
            );
        }
        panic!(
            "{what}: {} fields diverge ({}) — read each against the module docs' list of \
             expected divergences before treating it as a defect",
            self.by_field.len(),
            self.fields().join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// Reference entries as pictures
// ---------------------------------------------------------------------------

/// A picture's identity as the reference arrays express it, and the key the surface mapping is
/// tracked by: `(long-term, FrameNum or LongTermFrameIdx, TopFieldOrderCnt,
/// BottomFieldOrderCnt)`. HEVC leaves the second and fourth members at 0 — it identifies a
/// reference by POC alone.
///
/// It is DXVA's own key, deliberately, so both sides express it the same way. The one consequence
/// worth naming: a picture that is re-marked long-term changes key (`FrameNum` becomes
/// `LongTermFrameIdx`), so the surface mapping loses the link to its earlier self rather than
/// reporting a change — a missed check, never a false finding. Both sides re-key identically, so
/// the SET comparison is unaffected.
type PictureKey = (bool, u16, i32, i32);

/// A reference entry with its surface index REMOVED: the identity DXVA resolves a reference by,
/// plus the per-entry flag bits that belong to it. Comparing the array as a multiset of these is
/// what makes the two sides' different orders irrelevant while keeping every fact — the flag
/// bits travel with their entry, which is "re-indexed to the compared order" in practice.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RefEntry {
    long_term: bool,
    /// `FrameNumList[i]` (H.264): `frame_num`, or `LongTermFrameIdx` for a long-term entry.
    /// Always 0 for HEVC, which has no such array.
    frame_num_or_lt_idx: u16,
    /// `FieldOrderCntList[i]` (H.264) or `PicOrderCntValList[i]` (HEVC, in `top`).
    top: i32,
    bottom: i32,
    used_top: bool,
    used_bottom: bool,
    non_existing: bool,
}

impl RefEntry {
    /// The identity half alone — what follows a picture across AUs, and therefore what the
    /// surface mapping is keyed by.
    fn key(self) -> PictureKey {
        (
            self.long_term,
            self.frame_num_or_lt_idx,
            self.top,
            self.bottom,
        )
    }
}

/// FFmpeg's H.264 POC base, and the proof that it is a CONSTANT.
///
/// libavcodec's H.264 decoder seeds `prev_poc_msb = 1 << 16` at every IDR, so every
/// `TopFieldOrderCnt`/`BottomFieldOrderCnt` it hands DXVA is the specification's plus 65536 —
/// measured, on the RTX 4090 capture: AUs 0..7 carry 65536, 65540, 65538, 65544, … where 8.2.1
/// (and this crate) derive 0, 4, 2, 8, …. The PROGRESSION is identical; only the base differs.
///
/// This crate keeps the spec's values, deliberately: the offset is an artefact of another
/// decoder's POC bookkeeping, not a DXVA requirement, and importing it would mean writing a magic
/// 65536 into a derivation that pf-bitstream shares with the Vulkan rung. It is harmless on the
/// wire because every use a driver makes of these fields is a DIFFERENCE — temporal direct
/// scaling, implicit weighted prediction, co-located picture selection — and the offset is uniform
/// across `CurrPic` and every `RefFrameList` entry of a stream, so it cancels. (References are
/// matched by `FrameNumList`, not by POC.) That it is uniform is exactly what this type checks.
///
/// So POCs are compared RELATIVE: the offset is derived from the first AU and then required to
/// hold for every POC of every later AU. A wrong POC on either side changes the offset and is
/// reported. Only 0 and 65536 are accepted as the base itself — any other constant is a finding,
/// because then it is not the quirk documented here.
#[derive(Default)]
struct PocBase {
    offset: Option<i64>,
}

impl PocBase {
    /// The offset in force, or 0 before the first AU has established one.
    fn offset(&self) -> i64 {
        self.offset.unwrap_or(0)
    }

    /// Check one POC pair, establishing the base on the first call.
    fn check(&mut self, au: usize, field: &str, ours: i32, theirs: i32, findings: &mut Findings) {
        let delta = i64::from(theirs) - i64::from(ours);
        match self.offset {
            None => {
                if delta != 0 && delta != 65536 {
                    findings.note(
                        format!("{field}[POC base]"),
                        au,
                        format!(
                            "libav's first POC is ours {ours} + {delta}, which is neither 0 nor \
                             FFmpeg's documented 65536 `prev_poc_msb` seed — an unexplained POC \
                             base is a finding, not a quirk to absorb"
                        ),
                    );
                }
                if delta == 65536 {
                    findings.document(
                        "FieldOrderCnt[POC base]",
                        au,
                        "libavcodec seeds `prev_poc_msb = 1 << 16` at every IDR, so its POCs are \
                         the specification's plus 65536; this crate keeps 8.2.1's values and the \
                         harness compares POCs RELATIVE to that constant, which it requires to \
                         hold on every AU",
                    );
                }
                self.offset = Some(delta);
            }
            Some(offset) if delta != offset => findings.note(
                format!("{field}[POC]"),
                au,
                format!(
                    "ours {ours}, libav {theirs}: a difference of {delta} where every earlier POC \
                     of this stream differed by {offset} — the POC base is not constant, so this \
                     is a real POC divergence rather than libav's base offset"
                ),
            ),
            Some(_) => {}
        }
    }
}

/// Subtract libav's POC base from a decoded reference array, so the set comparison compares
/// pictures rather than POC bases (see [`PocBase`]).
///
/// `bottom_too` is true for H.264, whose entries carry a real `FieldOrderCntList[i][2]` PAIR, and
/// false for HEVC, whose `bottom` member is a placeholder this harness leaves at 0 on both sides —
/// shifting it would invent a difference rather than absorb one.
fn shift_poc(entries: &mut [(u8, RefEntry)], offset: i64, bottom_too: bool) {
    for (_, entry) in entries.iter_mut() {
        entry.top = (i64::from(entry.top) - offset) as i32;
        if bottom_too {
            entry.bottom = (i64::from(entry.bottom) - offset) as i32;
        }
    }
}

/// A per-field allowance: `Some(reason)` when THIS difference in THIS field is a divergence the
/// module docs have already settled, rather than a finding.
///
/// Deliberately per-DIFFERENCE and not per-field: a field with an allowance still reports anything
/// outside it. An allowlist keyed by field name alone would be the "vacuous green" this program
/// has been bitten by before — it would hide the next real difference in the same word.
type Allowance = fn(&str, &[u8], &[u8]) -> Option<&'static str>;

/// H.264 has none: every difference in a scalar field is a finding.
fn no_allowance(_: &str, _: &[u8], _: &[u8]) -> Option<&'static str> {
    None
}

/// HEVC's one documented scalar divergence: bit 10 of `dwCodingSettingPicturePropertyFlags`,
/// `loop_filter_across_tiles_enabled_flag`, which this crate sets and libavcodec does not.
///
/// 7.4.3.3.1 infers the flag to be 1 when the PPS does not code it, and the PPS only codes it
/// under `tiles_enabled_flag` — so with tiles disabled, 1 is what the specification says and what
/// the vendored parser reports. libavcodec's capture carries 0 on all 250 AUs of the vendored
/// vector. Neither can change a decoded picture: with `tiles_enabled_flag` clear there are no tile
/// boundaries for a loop filter to cross, which is why this is documented rather than fixed —
/// matching libavcodec here would mean overriding a spec inference on the strength of one
/// measurement of another decoder's parser default, and that default is not readable from this
/// worktree.
///
/// The allowance is tight: ONLY bit 10, only ours-set-theirs-clear, and only while both sides
/// agree tiles are disabled. Any other difference in the same word — including bit 10 with tiles
/// ENABLED, where the flag stops being inert — is a finding.
fn hevc_allowance(field: &str, ours: &[u8], theirs: &[u8]) -> Option<&'static str> {
    /// `tiles_enabled_flag`.
    const TILES: u32 = 1 << 7;
    /// `loop_filter_across_tiles_enabled_flag`.
    const ACROSS_TILES: u32 = 1 << 10;

    if field != "dwCodingSettingPicturePropertyFlags" {
        return None;
    }
    let (Ok(ours), Ok(theirs)) = (
        <[u8; 4]>::try_from(ours).map(u32::from_le_bytes),
        <[u8; 4]>::try_from(theirs).map(u32::from_le_bytes),
    ) else {
        return None;
    };
    let only_bit_10 = ours ^ theirs == ACROSS_TILES;
    let ours_sets_it = ours & ACROSS_TILES != 0;
    let tiles_off = (ours | theirs) & TILES == 0;
    (only_bit_10 && ours_sets_it && tiles_off).then_some(
        "loop_filter_across_tiles_enabled_flag (bit 10): 7.4.3.3.1 infers 1 when the PPS codes no \
         tiles and the vendored parser reports that; libavcodec emits 0. Inert either way — with \
         tiles_enabled_flag clear there is no tile boundary for a loop filter to cross",
    )
}

/// One side's H.264 reference array, decoded: the in-use entries with their surface indices.
fn h264_ref_entries(pp: &[u8]) -> Vec<(u8, RefEntry)> {
    let list = offset_of!(PicParamsH264, RefFrameList);
    let poc = offset_of!(PicParamsH264, FieldOrderCntList);
    let nums = offset_of!(PicParamsH264, FrameNumList);
    let used = u32_at(pp, offset_of!(PicParamsH264, UsedForReferenceFlags));
    let missing = u16_at(pp, offset_of!(PicParamsH264, NonExistingFrameFlags));
    (0..16)
        .filter(|i| pp[list + i] != UNUSED_ENTRY)
        .map(|i| {
            (
                pp[list + i] & 0x7F,
                RefEntry {
                    long_term: pp[list + i] & 0x80 != 0,
                    frame_num_or_lt_idx: u16_at(pp, nums + 2 * i),
                    top: i32_at(pp, poc + 8 * i),
                    bottom: i32_at(pp, poc + 8 * i + 4),
                    used_top: used >> (2 * i) & 1 != 0,
                    used_bottom: used >> (2 * i + 1) & 1 != 0,
                    non_existing: missing >> i & 1 != 0,
                },
            )
        })
        .collect()
}

/// One side's HEVC reference array, decoded. HEVC's array carries no `FrameNum` and no use
/// flags — residency IS the statement — so those members stay at their neutral values.
fn hevc_ref_entries(pp: &[u8]) -> Vec<(u8, RefEntry)> {
    let list = offset_of!(PicParamsHevc, RefPicList);
    let poc = offset_of!(PicParamsHevc, PicOrderCntValList);
    (0..15)
        .filter(|i| pp[list + i] != UNUSED_ENTRY)
        .map(|i| {
            (
                pp[list + i] & 0x7F,
                RefEntry {
                    long_term: pp[list + i] & 0x80 != 0,
                    frame_num_or_lt_idx: 0,
                    top: i32_at(pp, poc + 4 * i),
                    bottom: 0,
                    used_top: true,
                    used_bottom: true,
                    non_existing: false,
                },
            )
        })
        .collect()
}

/// One side's AV1 reference store, read out of the SUBMITTED BYTES: `(CurrPicTextureIndex,
/// RefFrameMapTextureIndex[8], frame_refs[name].Index for the seven names)`.
///
/// AV1's reference numbering is two arrays that mean different things at once and the split is
/// exactly what a comparison has to respect. `frame_refs[i].Index` is an AV1 reference SLOT —
/// `ref_frame_idx[i]`, which both sides read out of the same frame header — so it is a VALUE
/// that must match libavcodec's exactly, and it is compared as part of the `frame_refs` field.
/// `RefFrameMapTextureIndex[slot]` and `CurrPicTextureIndex` are SURFACES, which come from each
/// side's own pool and are only ever a bijection.
///
/// So the store is compared as a SHAPE: which slots are occupied, and whether the decode target
/// collides with any of them.
fn av1_reference_store(pp: &[u8]) -> (u8, [u8; 8], [u8; 7]) {
    let curr = pp[offset_of!(PicParamsAv1, curr_pic_texture_index)];
    let mut store = [UNUSED_INDEX; 8];
    let base = offset_of!(PicParamsAv1, ref_frame_map_texture_index);
    store.copy_from_slice(&pp[base..base + 8]);
    let mut names = [UNUSED_INDEX; 7];
    // The stride and the member offset come from the TYPE, never from the two numbers
    // `dxva_av1.rs` measured (36 and 33). Those are pinned there as compile-time assertions
    // against the Windows SDK's own header, and re-typing them here would be a second copy that
    // can drift from the first — which for a reader of this array is the difference between a
    // reference slot and a warp coefficient.
    for (name, slot) in names.iter_mut().enumerate() {
        *slot = pp[offset_of!(PicParamsAv1, frame_refs)
            + name * size_of::<PicEntryAv1>()
            + offset_of!(PicEntryAv1, index)];
    }
    (curr, store, names)
}

/// The surface mapping between the two sides, tracked per PICTURE.
///
/// A global index-to-index bijection over a whole stream is the wrong model: both sides reuse a
/// surface once its picture leaves the DPB, and they need not reuse it at the same moment. What
/// must hold is that while a picture is live, its two surface numbers keep agreeing — so the
/// mapping is keyed by the picture and dropped when a side reassigns the index.
#[derive(Default)]
struct SurfaceMapping {
    live: BTreeMap<PictureKey, (u8, u8)>,
}

impl SurfaceMapping {
    /// Record one AU's pairs, reporting a mapping that changed under a live picture and two
    /// pictures collapsing onto one surface.
    fn observe(
        &mut self,
        au: usize,
        field: &str,
        pairs: &[(PictureKey, (u8, u8))],
        findings: &mut Findings,
    ) {
        let mut ours_seen: BTreeMap<u8, PictureKey> = BTreeMap::new();
        let mut theirs_seen: BTreeMap<u8, PictureKey> = BTreeMap::new();
        for &(key, (ours, theirs)) in pairs {
            if let Some(&(known_ours, known_theirs)) = self.live.get(&key) {
                if (known_ours, known_theirs) != (ours, theirs) {
                    findings.note(
                        format!("{field}[surface mapping]"),
                        au,
                        format!(
                            "picture {key:?} was surface {known_ours} (ours) = {known_theirs} \
                             (libav) and is now {ours} = {theirs}: the mapping is not a \
                             bijection over this picture's lifetime"
                        ),
                    );
                }
            }
            if let Some(other) = ours_seen.insert(ours, key) {
                if other != key {
                    findings.note(
                        format!("{field}[surface aliasing]"),
                        au,
                        format!("our surface {ours} carries both {other:?} and {key:?}"),
                    );
                }
            }
            if let Some(other) = theirs_seen.insert(theirs, key) {
                if other != key {
                    findings.note(
                        format!("{field}[surface aliasing]"),
                        au,
                        format!("libav's surface {theirs} carries both {other:?} and {key:?}"),
                    );
                }
            }
            self.live.insert(key, (ours, theirs));
        }
        // A surface either side has just reassigned no longer says anything about the picture
        // that used to hold it, so the stale entries go — that is the difference between
        // "the mapping broke" and "the pool moved on".
        self.live.retain(|key, &mut (ours, theirs)| {
            let ours_now = ours_seen.get(&ours);
            let theirs_now = theirs_seen.get(&theirs);
            ours_now.is_none_or(|k| k == key) && theirs_now.is_none_or(|k| k == key)
        });
    }
}

// ---------------------------------------------------------------------------
// The capture
// ---------------------------------------------------------------------------

/// One captured buffer descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapturedDescriptor {
    buffer_type: u32,
    data_size: u32,
    num_mbs_in_buffer: u32,
    data_offset: u32,
}

/// A parsed capture, for ONE codec.
#[derive(Default)]
struct Capture {
    pic_params: BTreeMap<usize, Vec<u8>>,
    /// `Some(bytes)` for a submitted matrix, `None` for the explicit `absent` spelling. An AU
    /// missing from this map was never reported either way, which is itself a finding.
    qmatrix: BTreeMap<usize, Option<Vec<u8>>>,
    descriptors: BTreeMap<usize, Vec<CapturedDescriptor>>,
    config_bitstream_raw: BTreeMap<usize, u32>,
    /// Lines that carry one of this harness's prefixes and could not be read. Never dropped
    /// silently: a capture whose format drifted must fail loudly, not compare less.
    unreadable: Vec<String>,
}

/// `hex` → bytes, or `None` when it is not an even-length hex string.
fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 || hex.is_empty() {
        return None;
    }
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 * bytes.len());
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Parse the lines of one codec out of a capture. `codec` is the token FFmpeg's
/// `avcodec_get_name` produces: `h264` or `hevc`.
fn parse_capture(text: &str, codec: &str) -> Capture {
    /// The markers, with their trailing space so a bare word cannot match one.
    const MARKERS: [&str; 4] = ["PFPP ", "PFQM ", "PFBD ", "PFCFG "];

    let mut out = Capture::default();
    for raw in text.lines() {
        // The marker is found ANYWHERE in the line, not required at its start: FFmpeg's logger
        // prefixes a message logged against a codec context with `[h264 @ 0x…] `, and a capture
        // made that way must still be readable (the recipe asks for `av_log(NULL, …)` so it is
        // not, but a capture is expensive and this costs nothing).
        let Some(start) = MARKERS.iter().filter_map(|m| raw.find(m)).min() else {
            continue;
        };
        let line = raw[start..].trim();
        let Some((prefix, rest)) = line.split_once(' ') else {
            continue;
        };
        if !matches!(prefix, "PFPP" | "PFQM" | "PFBD" | "PFCFG") {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // Every line is `<prefix> <codec> <au> …`, so anything shorter is malformed.
        let (Some(line_codec), Some(au)) = (fields.first(), fields.get(1)) else {
            out.unreadable.push(line.to_string());
            continue;
        };
        if *line_codec != codec {
            continue;
        }
        let Ok(au) = au.parse::<usize>() else {
            out.unreadable.push(line.to_string());
            continue;
        };
        let ok = match (prefix, &fields[2..]) {
            ("PFPP", [hex]) => match from_hex(hex) {
                Some(bytes) => out.pic_params.insert(au, bytes).is_none(),
                None => false,
            },
            ("PFQM", ["absent"]) => out.qmatrix.insert(au, None).is_none(),
            ("PFQM", [hex]) => match from_hex(hex) {
                Some(bytes) => out.qmatrix.insert(au, Some(bytes)).is_none(),
                None => false,
            },
            ("PFBD", [kind, size, mbs, offset]) => {
                match (
                    kind.parse::<u32>(),
                    size.parse::<u32>(),
                    mbs.parse::<u32>(),
                    offset.parse::<u32>(),
                ) {
                    (Ok(buffer_type), Ok(data_size), Ok(num_mbs_in_buffer), Ok(data_offset)) => {
                        out.descriptors
                            .entry(au)
                            .or_default()
                            .push(CapturedDescriptor {
                                buffer_type,
                                data_size,
                                num_mbs_in_buffer,
                                data_offset,
                            });
                        true
                    }
                    _ => false,
                }
            }
            ("PFCFG", [raw]) => match raw.parse::<u32>() {
                Ok(raw) => {
                    out.config_bitstream_raw.insert(au, raw);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        };
        if !ok {
            out.unreadable.push(line.to_string());
        }
    }
    out
}

/// Read the capture named by `var`, or `None` when it is unset.
fn capture_from_env(var: &str, codec: &str) -> Option<Capture> {
    let path = std::env::var(var).ok()?;
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{var}={path} could not be read: {e}"));
    Some(parse_capture(&text, codec))
}

/// The checks every comparison needs before it compares anything: the capture is readable, it
/// covers the same AUs, and it was not made against a workaround path.
fn preflight(capture: &Capture, ours: usize, codec: &str, reserved16: Option<usize>) {
    assert!(
        capture.unreadable.is_empty(),
        "the capture holds {} unreadable line(s) — the first is:\n  {}\nthe recipe in this \
         file's module docs is the format",
        capture.unreadable.len(),
        capture.unreadable[0]
    );
    assert!(
        !capture.pic_params.is_empty(),
        "the capture holds no `PFPP {codec}` lines: either the patch did not apply or the \
         hwaccel never engaged (a software fallback logs nothing)"
    );
    assert_eq!(
        capture.pic_params.len(),
        ours,
        "the capture covers {} AUs and this crate plans {ours} — the two sides must decode the \
         same elementary stream, split the same way",
        capture.pic_params.len()
    );
    let expected: BTreeSet<usize> = (0..ours).collect();
    let seen: BTreeSet<usize> = capture.pic_params.keys().copied().collect();
    assert_eq!(
        seen, expected,
        "the capture's AU indices are not 0..{ours}; pairing by index would compare different \
         pictures"
    );
    if let Some(offset) = reserved16 {
        let zeroed = capture
            .pic_params
            .values()
            .filter(|pp| pp.len() > offset + 1 && u16_at(pp, offset) == 0)
            .count();
        assert_eq!(
            zeroed, 0,
            "{zeroed} of {ours} captured pictures carry Reserved16Bits = 0, which libavcodec \
             writes only under FF_DXVA2_WORKAROUND_INTEL_CLEARVIDEO or \
             FF_DXVA2_WORKAROUND_SCALING_LIST_ZIGZAG — this capture is against a workaround \
             path and is VOID for comparison (module docs)"
        );
    }
    // A `PFCFG` line is optional, but a wrong one voids the slice-control comparison: it means
    // the driver read the other slice-control struct entirely.
    //
    // ⚠ AV1 is exempt, and not because the check is inconvenient: `ConfigBitstreamRaw`'s short
    // format is a property of the two SHORT SLICE-CONTROL structs, and AV1's slice-control
    // record is `DXVA_Tile_AV1`, which has no short/long pair for a config to select between.
    // Comparing an AV1 capture's number against HEVC's 1 — which a `_ =>` arm would do — is a
    // check of nothing that fails on anything.
    let want = match codec {
        "h264" => pf_dxvadec::short_slice_config(Codec::H264),
        "hevc" => pf_dxvadec::short_slice_config(Codec::H265),
        _ => return,
    };
    for (au, &raw) in &capture.config_bitstream_raw {
        assert_eq!(
            raw, want,
            "AU {au}: the capture's ConfigBitstreamRaw is {raw}, and short format for {codec} \
             is {want} — the capture used the other slice-control format, so its slice-control \
             sizes describe a different struct"
        );
    }
}

// ---------------------------------------------------------------------------
// The comparisons
// ---------------------------------------------------------------------------

/// How two versions of one field differ, in a form a reader can act on: the whole value in hex
/// when it is small enough to read, and a count plus the first differing byte when it is an array
/// (a `SliceGroupMap` printed in full is 1620 characters of nothing).
fn byte_diff_detail(ours: &[u8], theirs: &[u8]) -> String {
    if ours.len() <= 8 {
        return format!("ours {}, libav {}", to_hex(ours), to_hex(theirs));
    }
    let first = ours
        .iter()
        .zip(theirs)
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    let differing = ours.iter().zip(theirs).filter(|(a, b)| a != b).count();
    format!(
        "{differing} of {} bytes differ, first at byte {first} of the field (ours {:#04x}, libav \
         {:#04x})",
        ours.len(),
        ours[first],
        theirs[first]
    )
}

/// Compare the scalar fields — everything that is neither a surface index nor part of a
/// reference array — byte for byte, reporting by field name.
fn compare_scalars(
    au: usize,
    ours: &[u8],
    theirs: &[u8],
    ranges: &[(&'static str, Range<usize>)],
    structural: &[&str],
    allowance: Allowance,
    findings: &mut Findings,
) {
    let mut classified = vec![false; ours.len()];
    for (name, range) in ranges {
        for byte in range.clone() {
            classified[byte] = true;
        }
        if structural.contains(name) {
            continue;
        }
        if ours[range.clone()] != theirs[range.clone()] {
            match allowance(name, &ours[range.clone()], &theirs[range.clone()]) {
                Some(reason) => findings.document(*name, au, reason),
                None => findings.note(
                    *name,
                    au,
                    byte_diff_detail(&ours[range.clone()], &theirs[range.clone()]),
                ),
            }
        }
    }
    // The field table must tile the struct; if a byte ever falls outside it, report it as a raw
    // offset rather than pass over it. That is the fallback the module docs promise, and the
    // reason this harness can never silently ignore a difference it cannot name.
    for (offset, covered) in classified.iter().enumerate() {
        if !covered && ours[offset] != theirs[offset] {
            findings.note(
                format!("<unclassified byte {offset:#06x}>"),
                au,
                format!("ours {:#04x}, libav {:#04x}", ours[offset], theirs[offset]),
            );
        }
    }
}

/// Compare one AU's reference array as a SET, and feed the surface mapping.
fn compare_ref_array(
    au: usize,
    field: &str,
    ours: &[(u8, RefEntry)],
    theirs: &[(u8, RefEntry)],
    mapping: &mut SurfaceMapping,
    findings: &mut Findings,
) {
    let mut ours_sorted: Vec<RefEntry> = ours.iter().map(|&(_, e)| e).collect();
    let mut theirs_sorted: Vec<RefEntry> = theirs.iter().map(|&(_, e)| e).collect();
    ours_sorted.sort_unstable();
    theirs_sorted.sort_unstable();
    if ours_sorted != theirs_sorted {
        let only_ours: Vec<&RefEntry> = ours_sorted
            .iter()
            .filter(|e| !theirs_sorted.contains(e))
            .collect();
        let only_theirs: Vec<&RefEntry> = theirs_sorted
            .iter()
            .filter(|e| !ours_sorted.contains(e))
            .collect();
        findings.note(
            format!("{field}[set]"),
            au,
            format!(
                "{} entries ours vs {} libav; only ours: {only_ours:?}; only libav: {only_theirs:?}",
                ours.len(),
                theirs.len()
            ),
        );
    }

    // The pairs the mapping is built from: a picture present on both sides, identified by its
    // key. An ambiguous key (two entries claiming the same identity) is skipped and reported —
    // it cannot happen off a conformant stream, and guessing which is which would invent a
    // mapping.
    let mut pairs = Vec::new();
    for &(our_slot, entry) in ours {
        let key = entry.key();
        let ours_same = ours.iter().filter(|(_, e)| e.key() == key).count();
        let matches: Vec<u8> = theirs
            .iter()
            .filter(|(_, e)| e.key() == key)
            .map(|&(slot, _)| slot)
            .collect();
        if ours_same > 1 || matches.len() > 1 {
            findings.note(
                format!("{field}[ambiguous key]"),
                au,
                format!(
                    "{key:?} appears {ours_same} times ours and {} libav",
                    matches.len()
                ),
            );
            continue;
        }
        if let Some(&their_slot) = matches.first() {
            pairs.push((key, (our_slot, their_slot)));
        }
    }
    mapping.observe(au, field, &pairs, findings);
}

/// The whole H.264 picture-parameter comparison.
fn compare_h264_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(H264_FIELDS, size_of::<PicParamsH264>());
    let structural = [
        "CurrPic",
        // The POC fields are compared RELATIVE to libavcodec's base offset rather than byte for
        // byte — see `PocBase` for the measurement and for why this crate keeps 8.2.1's values.
        "CurrFieldOrderCnt",
        "RefFrameList",
        "FieldOrderCntList",
        "FrameNumList",
        "UsedForReferenceFlags",
        "NonExistingFrameFlags",
    ];
    let mut findings = Findings::default();
    let mut mapping = SurfaceMapping::default();
    let mut poc = PocBase::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this AU",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {} — the \
                     hand-declared layout and the header disagree, which is a finding on its own",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            no_allowance,
            &mut findings,
        );
        // The POC fields, compared RELATIVE to libavcodec's base offset (`PocBase`). The base is
        // established from the CURRENT picture's own count, which is the one POC both sides
        // certainly report for the same picture, and then required of every POC after it.
        let poc_at = offset_of!(PicParamsH264, CurrFieldOrderCnt);
        poc.check(
            au,
            "CurrFieldOrderCnt[0]",
            i32_at(&sub.pic_params, poc_at),
            i32_at(theirs, poc_at),
            &mut findings,
        );
        poc.check(
            au,
            "CurrFieldOrderCnt[1]",
            i32_at(&sub.pic_params, poc_at + 4),
            i32_at(theirs, poc_at + 4),
            &mut findings,
        );
        let mut their_entries = h264_ref_entries(theirs);
        shift_poc(&mut their_entries, poc.offset(), true);
        compare_ref_array(
            au,
            "RefFrameList",
            &h264_ref_entries(&sub.pic_params),
            &their_entries,
            &mut mapping,
            &mut findings,
        );
        // `CurrPic` needs no matching: the same AU is the same picture on both sides. It is fed
        // through the mapping under the current picture's own key, so that when this picture
        // shows up as a REFERENCE later, the two surfaces are checked against this pairing.
        let curr = offset_of!(PicParamsH264, CurrPic);
        let frame_num = offset_of!(PicParamsH264, frame_num);
        if sub.pic_params[curr] & 0x80 != theirs[curr] & 0x80 {
            findings.note(
                "CurrPic[AssociatedFlag]",
                au,
                format!(
                    "ours {:#04x}, libav {:#04x} — the bottom-field flag, which is 0 for every \
                     picture inside this backend's progressive envelope",
                    sub.pic_params[curr], theirs[curr]
                ),
            );
        }
        let key = (
            false,
            u16_at(&sub.pic_params, frame_num),
            i32_at(&sub.pic_params, poc_at),
            i32_at(&sub.pic_params, poc_at + 4),
        );
        mapping.observe(
            au,
            "CurrPic",
            &[(key, (sub.pic_params[curr] & 0x7F, theirs[curr] & 0x7F))],
            &mut findings,
        );
    }
    findings
}

/// One HEVC RPS index array, resolved through its own side's `RefPicList` into the pictures it
/// names — which is the only form in which the two sides' arrays are comparable.
fn hevc_rps_pictures(pp: &[u8], array: usize, entries: &[(u8, RefEntry)]) -> Vec<Option<RefEntry>> {
    let list = offset_of!(PicParamsHevc, RefPicList);
    (0..8)
        .map(|i| {
            let index = pp[array + i];
            // The array holds an index INTO `RefPicList` (15 entries), so anything outside that
            // — the `0xFF` sentinel included — names nothing, and resolving it is how a stale or
            // out-of-range index becomes a reported difference rather than a garbage read.
            if usize::from(index) >= 15 {
                return None;
            }
            let slot = pp[list + usize::from(index)];
            if slot == UNUSED_ENTRY {
                return None;
            }
            entries
                .iter()
                .find(|(s, _)| *s == slot & 0x7F)
                .map(|&(_, entry)| entry)
        })
        .collect()
}

/// The whole AV1 picture-parameter comparison.
///
/// Structurally simpler than the other two and the reason is worth stating: AV1 puts NO surface
/// index in its reference entries. `frame_refs[i].Index` is `ref_frame_idx[i]`, an AV1 SLOT both
/// sides read out of the same frame header, so the seven 36-byte entries — sizes, warp
/// parameters, warp type and slot alike — compare byte for byte with no re-indexing, no set
/// comparison and no allowance. Only two members carry surfaces, and they are handled as the
/// SHAPE of the store rather than by value ([`av1_reference_store`]).
///
/// There is no POC base to derive either: AV1's `order_hint` is a coded field, not a decoder's
/// running count, so libavcodec has nothing to seed it with.
///
/// ⚠ **`width`/`height` is a divergence waiting to be measured, and this comparison will
/// report it rather than absorb it.** libavcodec sends `avctx->width`, which
/// `update_context_with_frame_header` sets from `frame_width_minus_1 + 1` — FrameWidth, the
/// PRE-superres coded width — and the same for `frame_refs[i].width` off the reference's
/// `AVFrame`; this crate sends `UpscaledWidth`. With superres off the two are equal by
/// definition (7.20), which is every frame of the vendored vector and every frame a punktfunk
/// host emits, so a capture made from this vector cannot tell them apart. Deliberately given no
/// allowance: if a superres capture ever reaches this harness, the difference must be a finding
/// somebody reads, not a line somebody already excused. See `pic_av1.rs`'s note at `pp.width`.
fn compare_av1_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(AV1_FIELDS, size_of::<PicParamsAv1>());
    // The two surface arrays, and nothing else: every other byte of this struct is a fact about
    // the bitstream that both sides derive from the same frame header.
    let structural = ["curr_pic_texture_index", "ref_frame_map_texture_index"];
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this frame",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {}",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            no_allowance,
            &mut findings,
        );

        // The store, as a shape. Which SLOTS hold a picture is a fact about the bitstream and
        // must agree; which SURFACE each holds is each side's own pool and never can.
        let (our_curr, our_store, _) = av1_reference_store(&sub.pic_params);
        let (their_curr, their_store, _) = av1_reference_store(theirs);
        for slot in 0..8 {
            let ours_occupied = our_store[slot] != UNUSED_INDEX;
            let theirs_occupied = their_store[slot] != UNUSED_INDEX;
            if ours_occupied != theirs_occupied {
                findings.note(
                    format!("ref_frame_map_texture_index[{slot}][occupied]"),
                    au,
                    format!("ours {ours_occupied}, libav {theirs_occupied}"),
                );
            }
        }
        // The decode target must hold no store entry's surface. libavcodec cannot produce a
        // collision — it fills the store from `h->ref[i]`, which the reference update has not
        // run on yet, and takes `CurrPicTextureIndex` from `h->cur_frame.f` — so a collision on
        // our side is a defect however the surfaces are numbered. This is the check that names
        // the 2026-08-07 defect.
        //
        // ⚠ Note what is deliberately NOT checked: that two slots hold different surfaces. One
        // picture in several reference slots is ordinary AV1 and this very vector does it —
        // the key frame sits in BWDREF and ALTREF2 for the stream's whole length — so a
        // "duplicate surface" check would fire on 273 of 274 frames of a correct conversion.
        for (label, curr, store) in [
            ("ours", our_curr, our_store),
            ("libav", their_curr, their_store),
        ] {
            if store.contains(&curr) {
                findings.note(
                    "curr_pic_texture_index[aliases the store]",
                    au,
                    format!(
                        "{label}: surface {curr} is both the decode target and a reference \
                         store entry — the frame decodes into a picture it predicts from"
                    ),
                );
            }
        }
    }
    findings
}

/// The whole HEVC picture-parameter comparison.
fn compare_hevc_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(HEVC_FIELDS, size_of::<PicParamsHevc>());
    let structural = [
        "CurrPic",
        // Relative, like H.264's — though libavcodec's HEVC POCs carry no base offset (measured:
        // 0 on all 250 AUs of the vendored vector). `PocBase` derives whatever offset exists
        // rather than assuming this one, so a future FFmpeg that grows one produces a documented
        // line instead of 250 findings.
        "CurrPicOrderCntVal",
        "RefPicList",
        "PicOrderCntValList",
        "RefPicSetStCurrBefore",
        "RefPicSetStCurrAfter",
        "RefPicSetLtCurr",
    ];
    let mut findings = Findings::default();
    let mut mapping = SurfaceMapping::default();
    let mut poc = PocBase::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this AU",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {}",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            hevc_allowance,
            &mut findings,
        );
        let poc_at = offset_of!(PicParamsHevc, CurrPicOrderCntVal);
        poc.check(
            au,
            "CurrPicOrderCntVal",
            i32_at(&sub.pic_params, poc_at),
            i32_at(theirs, poc_at),
            &mut findings,
        );
        let our_entries = hevc_ref_entries(&sub.pic_params);
        let mut their_entries = hevc_ref_entries(theirs);
        // HEVC's entries carry one POC each, in `top`; `bottom` is a placeholder.
        shift_poc(&mut their_entries, poc.offset(), false);
        compare_ref_array(
            au,
            "RefPicList",
            &our_entries,
            &their_entries,
            &mut mapping,
            &mut findings,
        );
        for (name, offset) in [
            (
                "RefPicSetStCurrBefore",
                offset_of!(PicParamsHevc, RefPicSetStCurrBefore),
            ),
            (
                "RefPicSetStCurrAfter",
                offset_of!(PicParamsHevc, RefPicSetStCurrAfter),
            ),
            (
                "RefPicSetLtCurr",
                offset_of!(PicParamsHevc, RefPicSetLtCurr),
            ),
        ] {
            let ours_named = hevc_rps_pictures(&sub.pic_params, offset, &our_entries);
            let theirs_named = hevc_rps_pictures(theirs, offset, &their_entries);
            for (position, (a, b)) in ours_named.iter().zip(&theirs_named).enumerate() {
                if a != b {
                    findings.note(
                        format!("{name}[{position}]"),
                        au,
                        format!("ours names {a:?}, libav names {b:?}"),
                    );
                }
            }
        }
        let curr = offset_of!(PicParamsHevc, CurrPic);
        let key = (false, 0u16, i32_at(&sub.pic_params, poc_at), 0);
        mapping.observe(
            au,
            "CurrPic",
            &[(key, (sub.pic_params[curr] & 0x7F, theirs[curr] & 0x7F))],
            &mut findings,
        );
    }
    findings
}

/// The quantization-matrix comparison: presence FIRST, then contents by field.
fn compare_qmatrix(
    ours: &[OurSubmission],
    capture: &Capture,
    fields: &[(&'static str, usize)],
    total: usize,
) -> Findings {
    let ranges = field_ranges(fields, total);
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.qmatrix.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture reports the matrix neither present nor `absent` for this AU — the \
                 PFQM patch (recipe step 4) is missing",
            );
            continue;
        };
        match (&sub.qmatrix, theirs) {
            (None, None) => {}
            (Some(_), None) => findings.note(
                "<submitted>",
                au,
                "we submit an inverse-quantization-matrix buffer where libavcodec submits NONE — \
                 for HEVC this is review 13's defect: the picture parameters have told the \
                 driver to ignore the matrix, and a driver that honours it anyway dequantizes \
                 every residual against it",
            ),
            (None, Some(_)) => findings.note(
                "<submitted>",
                au,
                "libavcodec submits an inverse-quantization-matrix buffer and we submit none — \
                 the hardware is left to dequantize against whatever it last held",
            ),
            (Some(mine), Some(theirs)) => {
                if mine.len() != theirs.len() {
                    findings.note(
                        "<struct size>",
                        au,
                        format!("ours {} bytes, libav {}", mine.len(), theirs.len()),
                    );
                    continue;
                }
                for (name, range) in &ranges {
                    if mine[range.clone()] != theirs[range.clone()] {
                        findings.note(
                            *name,
                            au,
                            byte_diff_detail(&mine[range.clone()], &theirs[range.clone()]),
                        );
                    }
                }
            }
        }
    }
    findings
}

/// A descriptor buffer type as a name, for reports.
fn buffer_name(buffer_type: u32) -> &'static str {
    match buffer_type {
        BUFFER_PICTURE_PARAMETERS => "PICTURE_PARAMETERS",
        BUFFER_INVERSE_QUANTIZATION_MATRIX => "INVERSE_QUANTIZATION_MATRIX",
        BUFFER_SLICE_CONTROL => "SLICE_CONTROL",
        BUFFER_BITSTREAM => "BITSTREAM",
        _ => "<unknown buffer type>",
    }
}

/// The descriptor comparison. Everything but the bitstream buffer's `DataSize` must match
/// exactly; that one field has a legitimate divergence class, which is classified apart rather
/// than reported as one undifferentiated difference (module docs).
fn compare_descriptors(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.descriptors.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFBD lines for this AU",
            );
            continue;
        };
        let our_types: Vec<u32> = sub.descriptors.iter().map(|d| d.buffer_type).collect();
        let their_types: Vec<u32> = theirs.iter().map(|d| d.buffer_type).collect();
        if our_types != their_types {
            // A missing BITSTREAM on their side is the one shape that is a MISSED PATCH rather
            // than a divergence — the bitstream descriptor is the one libavcodec fills outside
            // the choke point (recipe step 2) — so it is named as such.
            let detail = if !their_types.contains(&BUFFER_BITSTREAM) {
                "the capture carries no BITSTREAM descriptor at all: recipe step 2's SECOND \
                 patch site (the inline fill in commit_bitstream_and_slice_buffer) was missed"
                    .to_string()
            } else {
                format!(
                    "ours {:?}, libav {:?}",
                    our_types
                        .iter()
                        .map(|&t| buffer_name(t))
                        .collect::<Vec<_>>(),
                    their_types
                        .iter()
                        .map(|&t| buffer_name(t))
                        .collect::<Vec<_>>()
                )
            };
            findings.note("<buffer set>", au, detail);
        }
        for our_desc in &sub.descriptors {
            let name = buffer_name(our_desc.buffer_type);
            let Some(their_desc) = theirs
                .iter()
                .find(|d| d.buffer_type == our_desc.buffer_type)
            else {
                continue; // already reported by the set comparison
            };
            if our_desc.data_offset != their_desc.data_offset {
                findings.note(
                    format!("{name}.DataOffset"),
                    au,
                    format!(
                        "ours {}, libav {}",
                        our_desc.data_offset, their_desc.data_offset
                    ),
                );
            }
            if our_desc.num_mbs_in_buffer != their_desc.num_mbs_in_buffer {
                findings.note(
                    format!("{name}.NumMBsInBuffer"),
                    au,
                    format!(
                        "ours {}, libav {} — this field cannot legitimately differ: \
                         mb_width*mb_height on H.264's bitstream and slice-control buffers, 0 \
                         everywhere else",
                        our_desc.num_mbs_in_buffer, their_desc.num_mbs_in_buffer
                    ),
                );
            }
            if our_desc.data_size == their_desc.data_size {
                continue;
            }
            if our_desc.buffer_type != BUFFER_BITSTREAM {
                findings.note(
                    format!("{name}.DataSize"),
                    au,
                    format!(
                        "ours {}, libav {} — a fixed-size buffer ({} is `sizeof` a structure or \
                         slices * 10), so this cannot legitimately differ",
                        our_desc.data_size, their_desc.data_size, name
                    ),
                );
                continue;
            }
            // The bitstream buffer. Their slice COUNT is readable from their slice-control
            // size, which is what separates "the two sides split the AU differently" from "the
            // two sides delimit each slice a couple of bytes apart". (Both codecs' short
            // records are TEN bytes — asserted in
            // `the_slice_control_descriptor_is_one_ten_byte_short_format_record_per_slice_for_both_codecs`
            // — so one divisor serves both; a slice-control buffer that is NOT a multiple of it
            // has already been reported as a `SLICE_CONTROL.DataSize` difference above.)
            let their_slices = theirs
                .iter()
                .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
                .map(|d| d.data_size as usize / size_of::<SliceH264Short>());
            let our_slices = sub.records.len();
            match their_slices {
                Some(count) if count != our_slices => findings.note(
                    "BITSTREAM.DataSize[slice count]",
                    au,
                    format!(
                        "ours {} bytes over {our_slices} slices, libav {} over {count} — the two \
                         sides disagree about how many slices this AU has, which voids the size \
                         comparison and is the finding itself",
                        our_desc.data_size, their_desc.data_size
                    ),
                ),
                _ if their_desc.data_size % 128 != 0 => findings.note(
                    "BITSTREAM.DataSize[unpadded]",
                    au,
                    format!(
                        "libav's {} is not a multiple of 128, which means its tail padding was \
                         clamped by a mapping too small for the AU",
                        their_desc.data_size
                    ),
                ),
                _ => {
                    // Classify on the UNPADDED sizes, which is the only way a small difference
                    // can be told from a large one: padding rounds up to 128, so an eight-byte
                    // delimitation difference shows as a delta of 0 or of a whole 128 depending
                    // on which side of a granule the two land. Theirs is not captured directly,
                    // but padding is 1..=128 bytes, so it lies in a known window — and the
                    // difference is legitimate exactly when that window reaches to within four
                    // bytes per slice of our own unpadded size.
                    let delta = i64::from(our_desc.data_size) - i64::from(their_desc.data_size);
                    let tolerance = 4 * our_slices.max(1) as i64;
                    let their_low = i64::from(their_desc.data_size) - 128;
                    let their_high = i64::from(their_desc.data_size) - 1;
                    let ours_unpadded = i64::from(sub.unpadded);
                    let legitimate = their_low <= ours_unpadded + tolerance
                        && ours_unpadded - tolerance <= their_high;
                    findings.note(
                        if legitimate {
                            "BITSTREAM.DataSize[delimitation]"
                        } else {
                            "BITSTREAM.DataSize"
                        },
                        au,
                        format!(
                            "ours {} (unpadded {ours_unpadded}), libav {} (unpadded \
                             {their_low}..={their_high}), delta {delta} over {our_slices} \
                             slices — {}",
                            our_desc.data_size,
                            their_desc.data_size,
                            if legitimate {
                                "within the trailing-zero delimitation class the module docs \
                                 describe, but read it once rather than assume it"
                            } else {
                                "OUTSIDE the legitimate delimitation class: too large to be \
                                 trailing zeros"
                            }
                        ),
                    );
                }
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// This side, in the capture's own format
// ---------------------------------------------------------------------------

/// Write our submissions as the capture format, so a dump of ours and a capture of libav's can
/// be diffed by any tool — and so the parser above is exercised against a writer that shares no
/// code with it.
fn dump(codec: &str, ours: &[OurSubmission]) -> String {
    let mut text = String::new();
    let raw = pf_dxvadec::short_slice_config(match codec {
        "h264" => Codec::H264,
        _ => Codec::H265,
    });
    let _ = writeln!(text, "PFCFG {codec} 0 {raw}");
    for (au, sub) in ours.iter().enumerate() {
        let _ = writeln!(text, "PFPP {codec} {au} {}", to_hex(&sub.pic_params));
        match &sub.qmatrix {
            Some(qm) => {
                let _ = writeln!(text, "PFQM {codec} {au} {}", to_hex(qm));
            }
            None => {
                let _ = writeln!(text, "PFQM {codec} {au} absent");
            }
        }
        for desc in &sub.descriptors {
            let _ = writeln!(
                text,
                "PFBD {codec} {au} {} {} {} {}",
                desc.buffer_type, desc.data_size, desc.num_mbs_in_buffer, desc.data_offset
            );
        }
    }
    text
}

// ===========================================================================
// CPU-provable: no capture, ordinary CI
// ===========================================================================

/// Every hand-declared struct's field table must tile it exactly — no gap anywhere, and nothing
/// left over at the end.
///
/// Two jobs in one. It is what makes the reports above name the right field (a gap would name the
/// wrong one, or none at all). And it is the AUDIT the twelve-vs-ten slice-record defect asked
/// for, in executable form: a struct tiled exactly by its members has no padding, interior OR
/// tail, so its Rust layout is the C declaration under 1-byte packing — which is how `dxva.h`
/// declares all six. The slice records are in the list precisely because they are the pair that
/// got it wrong; the other four were confirmed against libavcodec's runtime `sizeof` as well.
#[test]
fn every_hand_declared_dxva_struct_is_tiled_exactly_by_its_fields() {
    for (what, fields, total) in [
        ("PicParamsH264", H264_FIELDS, size_of::<PicParamsH264>()),
        ("PicParamsHevc", HEVC_FIELDS, size_of::<PicParamsHevc>()),
        ("QmatrixH264", H264_QMATRIX_FIELDS, size_of::<QmatrixH264>()),
        ("QmatrixHevc", HEVC_QMATRIX_FIELDS, size_of::<QmatrixHevc>()),
        (
            "SliceH264Short",
            H264_SLICE_FIELDS,
            size_of::<SliceH264Short>(),
        ),
        (
            "SliceHevcShort",
            HEVC_SLICE_FIELDS,
            size_of::<SliceHevcShort>(),
        ),
        // AV1's two. `PicParamsAv1` is the one struct in this crate whose offsets were
        // MEASURED rather than mirrored — `layout-probe-av1.c` compiled with MSVC against the
        // Windows SDK's own `dxva.h` — and `dxva_av1.rs` pins every one at compile time. What
        // this adds is the other half: that the TABLE above reaches all 912 bytes, so a
        // capture comparison can name every one of them.
        ("PicParamsAv1", AV1_FIELDS, size_of::<PicParamsAv1>()),
        ("TileAv1", AV1_TILE_FIELDS, size_of::<TileAv1>()),
    ] {
        assert_eq!(fields[0].1, 0, "{what}: the first field must start at 0");
        let ranges = field_ranges(fields, total);
        let mut next = 0usize;
        for (name, range) in &ranges {
            assert_eq!(
                range.start, next,
                "{what}: {name} leaves a gap — the struct has no interior padding, so \
                 consecutive offsets must tile it"
            );
            assert!(range.end > range.start, "{what}: {name} is empty");
            next = range.end;
        }
        assert_eq!(next, total, "{what}: the table stops short of the struct");
    }
}

#[test]
fn every_h264_au_submits_four_buffers_in_libavcodecs_order() {
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "AU {au}"
        );
    }
}

/// **No AV1 submission names its decode surface anywhere in the reference store**, and every
/// reference NAME resolves through a slot that holds one.
///
/// This is the defect the Windows parity harness caught on 2026-08-07 and the one nothing on
/// the CPU could see, stated over the SUBMITTED BYTES — which is where a libavcodec capture
/// would see it too, and the reason it belongs in this file as well as in `pic_av1`'s own
/// tests. `plan_to_dxva_av1` released the picture this frame's own `refresh_frame_flags`
/// displaces before assigning the decode target a slot, and `SlotMap::assign` hands back the
/// slot just vacated — so `CurrPicTextureIndex` and one `RefFrameMapTextureIndex` entry were
/// the same surface on 268 of these 274 frames: decode into the picture you predict from.
/// Intel Arc followed the aliased surface and got 245 of 250 delivered frames wrong; NVIDIA
/// tolerated it for 63 frames and then lost one 16x24 luma block at the `order_hint` wrap.
///
/// libavcodec cannot produce this shape and that is the whole argument for calling it a defect
/// rather than a convention: `ff_dxva2_av1_fill_picture_parameters` fills
/// `RefFrameMapTextureIndex` from `h->ref[i]`, the pre-refresh store, and takes
/// `CurrPicTextureIndex` from `h->cur_frame.f`, a frame the reference-frame update has not run
/// on yet. The two cannot be one surface.
///
/// The counts are asserted, not printed. At zero references this test would pass against a
/// conversion that named nothing at all.
#[test]
fn no_av1_submission_names_its_decode_surface_in_the_reference_store() {
    let subs = our_av1_submissions();
    let (mut with_store, mut named_refs) = (0usize, 0usize);
    for (frame, sub) in subs.iter().enumerate() {
        let (curr, store, names) = av1_reference_store(&sub.pic_params);
        assert!(
            store.iter().all(|surface| *surface != curr),
            "frame {frame}: surface {curr} is both CurrPicTextureIndex and a \
             RefFrameMapTextureIndex entry"
        );
        if store.iter().any(|s| *s != UNUSED_INDEX) {
            with_store += 1;
        }
        for (name, slot) in names.iter().enumerate() {
            if *slot == UNUSED_INDEX {
                continue;
            }
            named_refs += 1;
            assert!(
                usize::from(*slot) < store.len(),
                "frame {frame}, reference name {name}: slot {slot} is outside the eight-entry \
                 store — `Index` is an AV1 reference SLOT, not a surface"
            );
            assert_ne!(
                store[usize::from(*slot)],
                UNUSED_INDEX,
                "frame {frame}, reference name {name}: slot {slot} holds no surface, so the \
                 driver would follow `Index` into an empty entry"
            );
        }
    }
    assert_eq!(
        with_store, 273,
        "every frame but the opening key frame carries a populated reference store"
    );
    assert!(
        named_refs > 0,
        "no frame named a reference, so every check above was skipped"
    );
}

/// AV1 submits THREE buffers and never a quantization matrix, on every frame of the vector.
///
/// The codec asymmetry the H.264 and HEVC tests above are about, taken to its third case.
/// H.264 submits the matrix unconditionally, HEVC only under `scaling_list_enabled_flag`, and
/// AV1 has no matrix BUFFER at all: its quantiser matrices are SELECTED by index
/// (`qm_y`/`qm_u`/`qm_v`) out of tables the decoder already holds, and `dxva2_av1_end_frame`
/// passes `NULL, 0` for the qm pair so the generic layer submits nothing. A fourth descriptor
/// here would be a buffer the driver has no `DXVA_Qmatrix_AV1` to read it as.
#[test]
fn every_av1_frame_submits_three_buffers_and_never_a_quantization_matrix() {
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "frame {frame}"
        );
        assert!(sub.qmatrix.is_none(), "frame {frame}");
    }
}

/// No AV1 descriptor carries a macroblock count. AV1 has no macroblocks and `dxva2_av1.c`
/// never touches the field — the same statement `no_hevc_descriptor_ever_carries_a_macroblock_count`
/// makes, and the same defect class review 13 found on the H.264 side in the other direction.
#[test]
fn no_av1_descriptor_ever_carries_a_macroblock_count() {
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        assert_eq!(sub.mb_count, 0, "frame {frame}");
        for desc in &sub.descriptors {
            assert_eq!(
                desc.num_mbs_in_buffer,
                0,
                "frame {frame}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

/// AV1's slice-control buffer is `16 * tile count`, its bitstream descriptor is the packer's
/// PADDED size, and the tile records tile the unpadded window exactly — in order, without gaps
/// and without overlaps.
///
/// The last part is what distinguishes AV1 from the other two codecs here and is the reason
/// this cannot reuse `the_bitstream_descriptor_is_the_packers_padded_size_and_the_slice_records_tile_it_exactly`:
/// a `DXVA_Tile_AV1` addresses a TILE PAYLOAD, which is the bytes after that tile's
/// `tile_size_minus_1` field — so consecutive records are separated by those size fields and do
/// NOT abut, unlike H.264/HEVC slice records which tile their buffer with no gaps. What must
/// hold is weaker and still exact: strictly increasing, non-overlapping, inside the unpadded
/// window, and never starting at a tile-group OBU's first byte (which would hand the driver an
/// OBU header as entropy-coded tile data).
///
/// ⚠ The padding is charged to NO record. H.264 and HEVC add the tail padding to their last
/// slice record's `SliceBytesInBuffer`; `pack_av1` does not, because a tile's size is the
/// tile's, and the descriptor is the only place AV1's padding is accounted at all.
#[test]
fn the_av1_bitstream_descriptor_is_padded_and_the_tile_records_tile_it_without_overlapping() {
    let mut frames_with_padding = 0usize;
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        let bitstream = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_BITSTREAM)
            .unwrap_or_else(|| panic!("frame {frame} submits no bitstream buffer"));
        assert_eq!(
            bitstream.data_size % 128,
            0,
            "frame {frame}: the bitstream descriptor states the PADDED size"
        );
        // ⚠ `1..=128`, not `0..128`. `pack_av1` writes libavcodec's expression verbatim —
        // `BITSTREAM_ALIGN - (cursor % BITSTREAM_ALIGN)` — so data that is ALREADY on the
        // granule gets a whole 128-byte block rather than none (`pack_av1`'s
        // `data_already_on_the_granule_still_gets_a_full_padding_block`). This vector never
        // lands on the granule, which is exactly why the bound has to come from the rule and
        // not from the measurement.
        let padding = bitstream.data_size - sub.unpadded;
        assert!(
            (1..=128).contains(&padding),
            "frame {frame}: {padding} bytes of padding"
        );
        frames_with_padding += 1;

        let slice_control = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
            .unwrap_or_else(|| panic!("frame {frame} submits no slice-control buffer"));
        assert_eq!(
            slice_control.data_size as usize,
            size_of::<TileAv1>() * sub.tiles.len(),
            "frame {frame}: sixteen bytes per TILE"
        );
        assert!(!sub.tiles.is_empty(), "frame {frame}: a frame has tiles");

        let mut previous_end = 0u32;
        for (i, tile) in sub.tiles.iter().enumerate() {
            // `#[repr(packed)]` — copy the fields out before using them.
            let (offset, size) = (tile.data_offset, tile.data_size);
            assert!(size > 0, "frame {frame}, tile {i}: an empty tile payload");
            assert!(
                offset >= previous_end,
                "frame {frame}, tile {i}: starts at {offset}, inside the previous tile which \
                 ends at {previous_end}"
            );
            assert!(
                offset + size <= sub.unpadded,
                "frame {frame}, tile {i}: runs past the bytes the packer wrote"
            );
            previous_end = offset + size;
            assert_eq!(
                tile.anchor_frame, UNUSED_INDEX,
                "frame {frame}, tile {i}: large-scale-tile anchors are libavcodec's 0xFF"
            );
        }
    }
    assert_eq!(
        frames_with_padding, VENDORED_AV1_FRAMES,
        "every frame is padded — the rule is unconditional"
    );
}

#[test]
fn every_h264_bitstream_and_slice_control_descriptor_carries_mb_width_times_mb_height() {
    // Review 13's defect, over the whole vector: `NumMBsInBuffer` was 0 where libavcodec's
    // H.264 path writes `h->mb_width * h->mb_height`, on the exact call this codebase has
    // already seen an Intel driver reject a hand-built variant on. The vendored vector is
    // 320x240 — 20x15 macroblocks.
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert_eq!(sub.mb_count, 20 * 15, "AU {au}");
        for desc in &sub.descriptors {
            let expected = match desc.buffer_type {
                BUFFER_BITSTREAM | BUFFER_SLICE_CONTROL => sub.mb_count,
                _ => 0,
            };
            assert_eq!(
                desc.num_mbs_in_buffer,
                expected,
                "AU {au}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

#[test]
fn every_h264_au_submits_the_quantization_matrix_buffer() {
    // H.264's predicate is UNCONDITIONAL in libavcodec (it passes `&ctx_pic->qm` with
    // `sizeof(qm)` every time), and the PPS's lists are always meaningful because the vendored
    // parser has applied Table 7-2's fallback rules. This is the codec where omitting the
    // buffer would be the defect — the mirror image of HEVC's.
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert!(sub.qmatrix.is_some(), "AU {au}");
        let desc = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
            .unwrap_or_else(|| panic!("AU {au} submits no matrix buffer"));
        assert_eq!(desc.data_size, size_of::<QmatrixH264>() as u32);
        assert_eq!(desc.num_mbs_in_buffer, 0);
    }
}

#[test]
fn the_whole_vendored_hevc_vector_omits_the_quantization_matrix_buffer() {
    // Case 1 of the three the qmatrix predicate has: `scaling_list_enabled_flag` clear. The
    // buffer is not submitted AT ALL — three descriptors, not four with an empty one.
    let ours = our_hevc_submissions();
    for (au, sub) in ours.iter().enumerate() {
        assert!(sub.qmatrix.is_none(), "AU {au}");
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "AU {au}"
        );
        // …and the picture parameters say so, which is the predicate libavcodec reads.
        let flags = u32_at(
            &sub.pic_params,
            offset_of!(PicParamsHevc, dwCodingParamToolFlags),
        );
        assert_eq!(flags & 1, 0, "AU {au}: scaling_list_enabled_flag");
    }
}

#[test]
fn no_hevc_descriptor_ever_carries_a_macroblock_count() {
    // The asymmetry, over the whole vector: libavcodec's HEVC path writes 0 where its H.264
    // path writes mb_width*mb_height, so a CTB count here would be a fresh divergence in the
    // other direction.
    for (au, sub) in our_hevc_submissions().iter().enumerate() {
        for desc in &sub.descriptors {
            assert_eq!(
                desc.num_mbs_in_buffer,
                0,
                "AU {au}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

#[test]
fn every_descriptor_of_both_codecs_starts_at_offset_zero_and_names_a_distinct_buffer() {
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let mut seen = BTreeSet::new();
            for desc in &sub.descriptors {
                assert_eq!(desc.data_offset, 0, "{codec} AU {au}");
                assert!(
                    seen.insert(desc.buffer_type),
                    "{codec} AU {au}: buffer type {} submitted twice",
                    desc.buffer_type
                );
                assert!(desc.data_size > 0, "{codec} AU {au}: an empty buffer");
            }
        }
    }
}

#[test]
fn the_bitstream_descriptor_is_the_packers_padded_size_and_the_slice_records_tile_it_exactly() {
    // The internal consistency a driver reads: every `BSNALunitDataLocation` lands inside the
    // buffer, the records are contiguous from byte 0, and the last one ends EXACTLY at
    // `DataSize` — which is what makes the tail padding charged to it rather than dangling past
    // the end (see pack.rs's module docs).
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let bitstream = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_BITSTREAM)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no bitstream buffer"));
            assert_eq!(bitstream.data_size % 128, 0, "{codec} AU {au}: padded size");
            assert!(
                bitstream.data_size >= sub.unpadded,
                "{codec} AU {au}: {} bytes of slices in a {}-byte buffer",
                sub.unpadded,
                bitstream.data_size
            );
            assert!(
                bitstream.data_size - sub.unpadded <= 128,
                "{codec} AU {au}: {} bytes of padding",
                bitstream.data_size - sub.unpadded
            );
            assert!(!sub.records.is_empty(), "{codec} AU {au}: no slices");
            let mut cursor = 0u32;
            for (i, record) in sub.records.iter().enumerate() {
                assert_eq!(
                    record.location, cursor,
                    "{codec} AU {au}: slice {i} location"
                );
                assert!(
                    record.bytes > 3,
                    "{codec} AU {au}: slice {i} is start code only"
                );
                cursor = record.location + record.bytes;
                assert!(
                    cursor <= bitstream.data_size,
                    "{codec} AU {au}: slice {i} runs past DataSize"
                );
            }
            assert_eq!(
                cursor, bitstream.data_size,
                "{codec} AU {au}: the records must tile the whole buffer, padding included"
            );
        }
    }
}

#[test]
fn the_slice_control_descriptor_is_one_ten_byte_short_format_record_per_slice_for_both_codecs() {
    // Two facts in one, both measured against libavcodec on the RTX 4090 box rather than
    // derived:
    //
    // * **The short record is TEN bytes.** The capture's slice-control `DataSize` is 20 on the
    //   H.264 vector, which is two slices per picture, and 10 on the HEVC vector, which is one
    //   slice segment per picture — two codecs, two slice counts, one record size. `dxva.h`
    //   packs these wire structures to a byte; a `#[repr(C)]` `{u32, u32, u16}` would be twelve
    //   and would displace every record after the first (see `dxva.rs`'s alignment section).
    // * **The `ConfigBitstreamRaw` hazard**: short format is 2 for H.264 and 1 for HEVC — one
    //   number with two spellings — and these are the short records for both. A buffer sized for
    //   one format against a config negotiated for the other is a driver reading a different
    //   struct at every offset.
    assert_eq!(pf_dxvadec::short_slice_config(Codec::H264), 2);
    assert_eq!(pf_dxvadec::short_slice_config(Codec::H265), 1);
    assert_eq!(size_of::<SliceH264Short>(), 10);
    assert_eq!(size_of::<SliceHevcShort>(), 10);
    for (codec, subs, slices_per_picture, capture_data_size) in [
        ("h264", our_h264_submissions(), 2usize, 20u32),
        ("hevc", our_hevc_submissions(), 1, 10),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let control = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no slice-control buffer"));
            assert_eq!(
                control.data_size as usize,
                10 * sub.records.len(),
                "{codec} AU {au}"
            );
            // The vectors' slice counts, so the record size above is anchored to the capture's
            // own number rather than to an arithmetic identity: if our splitter ever produced a
            // different slice count for these streams, the two sides would stop being
            // comparable and 10 would no longer follow from 20 and 10.
            assert_eq!(
                sub.records.len(),
                slices_per_picture,
                "{codec} AU {au}: the vendored vector is {slices_per_picture} slice(s) per picture"
            );
            assert_eq!(
                control.data_size, capture_data_size,
                "{codec} AU {au}: libavcodec's captured slice-control DataSize"
            );
        }
    }
}

#[test]
fn the_tail_padding_is_charged_to_the_last_slice_record_and_to_no_other() {
    // libavcodec's `commit_bitstream_and_slice_buffer` zero-fills
    // `FFMIN(128 - ((current - dxva_data) & 127), end - current)` bytes and then does
    // `slice->SliceBytesInBuffer += padding` — on the FINAL loop iteration's record. So the last
    // record's `SliceBytesInBuffer` counts the padding and every earlier record's does not, and
    // a driver reading the records back must find them tiling the buffer exactly.
    // [`pf_dxvadec::pack`] implements the same rule; this is where it is checked on the real
    // vectors, on the H.264 one because that is the one with more than one slice per picture —
    // the shape a single-slice vector cannot distinguish.
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let bitstream = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_BITSTREAM)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no bitstream buffer"));
            let padding = bitstream.data_size - sub.unpadded;
            assert!(
                (1..=128).contains(&padding),
                "{codec} AU {au}: {padding} bytes of padding"
            );
            let (last, earlier) = sub
                .records
                .split_last()
                .unwrap_or_else(|| panic!("{codec} AU {au}: no slices"));
            // Every earlier record stops exactly where the next slice's start code begins, so
            // none of them carries any of the padding.
            for (i, record) in earlier.iter().enumerate() {
                assert_eq!(
                    record.location + record.bytes,
                    sub.records[i + 1].location,
                    "{codec} AU {au}: record {i} does not end where record {} begins",
                    i + 1
                );
            }
            // The last one runs to the end of the buffer…
            assert_eq!(
                last.location + last.bytes,
                bitstream.data_size,
                "{codec} AU {au}: the last record must reach DataSize"
            );
            // …and stripping the padding off it leaves exactly the slice bytes the packer wrote,
            // which is the statement that the padding is in THAT record and nowhere else.
            assert_eq!(
                sub.records.iter().map(|r| r.bytes).sum::<u32>() - padding,
                sub.unpadded,
                "{codec} AU {au}: the padding is charged more than once, or not at all"
            );
            assert!(
                last.bytes > padding,
                "{codec} AU {au}: the last record is padding only"
            );
        }
    }
}

#[test]
fn the_picture_parameter_buffer_is_the_whole_hand_declared_struct_for_both_codecs() {
    for (codec, subs, size) in [
        ("h264", our_h264_submissions(), size_of::<PicParamsH264>()),
        ("hevc", our_hevc_submissions(), size_of::<PicParamsHevc>()),
        ("av1", our_av1_submissions(), size_of::<PicParamsAv1>()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            assert_eq!(sub.pic_params.len(), size, "{codec} AU {au}");
            assert_eq!(
                sub.descriptors[0].buffer_type, BUFFER_PICTURE_PARAMETERS,
                "{codec} AU {au}"
            );
            assert_eq!(
                sub.descriptors[0].data_size as usize, size,
                "{codec} AU {au}"
            );
        }
    }
}

/// The vector's first HEVC plan with its parameter sets rewritten to a chosen scaling-list
/// shape, converted and packed — the three cases of 7.4.5's activation, at the descriptor level.
///
/// Only the scaling-list fields move; everything else is the parser's own output, which is what
/// makes the "coded nowhere" case meaningful (`pps.scaling_list` then holds the parser's Table
/// 7-5/7-6 default fill, and `sps.scaling_list` the all-zero `ScalingLists::default()` an
/// uncoded SPS is left with).
fn hevc_case(enabled: bool, sps_coded: Option<u8>, pps_coded: Option<u8>) -> OurSubmission {
    use std::rc::Rc;

    let aus = split_into_aus_h265(TEST_25FPS_H265);
    let mut planner = H265Planner::new();
    let mut plan = planner.plan_au(aus[0]).expect("plan");

    let mut sps = (*plan.sps).clone();
    sps.scaling_list_enabled_flag = enabled;
    sps.scaling_list_data_present_flag = sps_coded.is_some();
    if let Some(fill) = sps_coded {
        sps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
        sps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
        sps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
    }
    let mut pps = (*plan.pps).clone();
    pps.scaling_list_data_present_flag = pps_coded.is_some();
    if let Some(fill) = pps_coded {
        pps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
        pps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
        pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
    }
    plan.sps = Rc::new(sps);
    plan.pps = Rc::new(pps);

    let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
    let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let packed = pf_dxvadec::pack(aus[0], &dxva.slice_ranges, &mut mapping).expect("pack");
    let unpadded = pf_dxvadec::packed_size(aus[0], &dxva.slice_ranges).expect("size") as u32;
    OurSubmission {
        pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
        qmatrix: dxva
            .qmatrix
            .as_ref()
            .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
        descriptors: pf_dxvadec::descriptors_h265(&dxva, &packed),
        records: packed.records,
        tiles: Vec::new(),
        unpadded,
        mb_count: 0,
    }
}

#[test]
fn an_hevc_sequence_that_disables_scaling_lists_submits_no_matrix_however_much_is_coded() {
    // Case 1 again, with data coded in BOTH parameter sets: the flag decides, not the data.
    let sub = hevc_case(false, Some(7), Some(9));
    assert!(sub.qmatrix.is_none());
    assert!(!sub
        .descriptors
        .iter()
        .any(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX));
}

#[test]
fn an_hevc_sequence_that_enables_scaling_lists_and_codes_them_submits_the_coded_lists() {
    // Case 2: the buffer travels, sized to the whole struct, carrying the coded data.
    let sub = hevc_case(true, Some(7), Some(9));
    let qm = sub
        .qmatrix
        .as_ref()
        .expect("an enabled sequence submits the matrix");
    assert_eq!(qm.len(), size_of::<QmatrixHevc>());
    let desc = sub
        .descriptors
        .iter()
        .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
        .expect("the matrix buffer is in the set");
    assert_eq!(desc.data_size as usize, size_of::<QmatrixHevc>());
    assert_eq!(desc.num_mbs_in_buffer, 0);
    // The PPS's data wins over the SPS's (7.4.5), which is visible in the bytes themselves.
    assert!(
        qm.iter().all(|&b| b == 9 || b == 17),
        "the PPS's fill of 9 (DC 9 + 8)"
    );
    assert_eq!(
        sub.descriptors
            .iter()
            .map(|d| d.buffer_type)
            .collect::<Vec<_>>(),
        vec![
            BUFFER_PICTURE_PARAMETERS,
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            BUFFER_BITSTREAM,
            BUFFER_SLICE_CONTROL,
        ]
    );
}

#[test]
fn an_hevc_sequence_that_enables_scaling_lists_but_codes_none_submits_the_defaults_not_zeros() {
    // Case 3, and the one that is a live defect if it regresses: `scaling_list_enabled_flag` set
    // with no scaling-list data in either parameter set is a legal, ordinary shape, and 7.4.5
    // says the Table 7-5/7-6 DEFAULTS apply. FFmpeg's parser seeds those defaults; the vendored
    // cros-codecs parser leaves an uncoded SPS all-ZERO — so a conversion that read the SPS
    // here would submit a matrix of zeros while bit 0 of dwCodingParamToolFlags told the driver
    // it was authoritative, and every residual would dequantize to nothing.
    //
    // pic_h265.rs checks the CONTENTS against the spec tables transcribed by hand; this checks
    // the two facts the descriptor level owns — the buffer is submitted, and what it carries is
    // not zeros.
    let sub = hevc_case(true, None, None);
    let qm = sub
        .qmatrix
        .as_ref()
        .expect("an enabled sequence submits the matrix even with nothing coded");
    let desc = sub
        .descriptors
        .iter()
        .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
        .expect("the matrix buffer is in the set");
    assert_eq!(desc.data_size as usize, size_of::<QmatrixHevc>());
    assert!(
        !qm.iter().all(|&b| b == 0),
        "an all-zero matrix dequantizes every residual to nothing"
    );
    // Table 7-5's 4x4 lists are flat 16, and the inferred DC is 8 + 8 = 16.
    let lists0 = offset_of!(QmatrixHevc, ucScalingLists0);
    assert!(qm[lists0..lists0 + 96].iter().all(|&b| b == 16));
    let dc2 = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2);
    assert!(qm[dc2..dc2 + 6].iter().all(|&b| b == 16));
    let dc3 = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3);
    assert!(qm[dc3..dc3 + 2].iter().all(|&b| b == 16));
    // …and every 8x8-and-up list carries a real curve rather than zeros.
    let lists1 = offset_of!(QmatrixHevc, ucScalingLists1);
    let lists3_end = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2);
    assert!(qm[lists1..lists3_end].iter().all(|&b| b != 0));
}

#[test]
fn the_dump_and_the_parser_agree_and_the_comparison_finds_nothing_against_ourselves() {
    // The comparators, exercised on real 250-AU data with a known answer. This is not a
    // tautology dressed as a test: the writer and the parser share no code, so a format drift
    // on either side fails here, and — with the next two tests, which mutate the "capture" and
    // require a NAMED finding — it is what keeps this file from reporting a clean bill of
    // health while comparing nothing.
    let ours = our_h264_submissions();
    let capture = parse_capture(&dump("h264", &ours), "h264");
    preflight(
        &capture,
        ours.len(),
        "h264",
        Some(offset_of!(PicParamsH264, Reserved16Bits)),
    );
    assert_eq!(capture.pic_params.len(), VENDORED_AUS);
    assert_eq!(capture.qmatrix.len(), VENDORED_AUS);
    assert_eq!(capture.descriptors.len(), VENDORED_AUS);
    for findings in [
        compare_h264_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
        compare_qmatrix(
            &ours,
            &capture,
            H264_QMATRIX_FIELDS,
            size_of::<QmatrixH264>(),
        ),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        // Nor may anything be DOCUMENTED away: an allowance that fires on identical bytes is an
        // allowance that would hide a real difference.
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }

    let ours = our_hevc_submissions();
    let capture = parse_capture(&dump("hevc", &ours), "hevc");
    // No Reserved16Bits check for HEVC: the ClearVideo workaround is an H.264-only path.
    preflight(&capture, ours.len(), "hevc", None);
    for findings in [
        compare_hevc_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
        compare_qmatrix(
            &ours,
            &capture,
            HEVC_QMATRIX_FIELDS,
            size_of::<QmatrixHevc>(),
        ),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own HEVC bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }
    // The HEVC matrices are `absent` on this vector, and the parser must carry that fact rather
    // than losing it — the whole of review 13's defect is the difference between the two.
    assert!(capture.qmatrix.values().all(Option::is_none));

    // AV1, on all 274 FRAMES. Nothing here has ever been run against libavcodec's own bytes
    // (module docs say why), so this self-comparison is the only thing standing between
    // `compare_av1_picparams` and a first capture: it proves the 912-byte field table reaches
    // every byte, that `av1_reference_store` reads `CurrPicTextureIndex` and the eight-entry
    // store from the offsets it thinks it does — a wrong one would report a false alias on a
    // correct submission — and that the comparison invents nothing on identical input.
    let ours = our_av1_submissions();
    let capture = parse_capture(&dump("av1", &ours), "av1");
    preflight(&capture, ours.len(), "av1", None);
    assert_eq!(capture.pic_params.len(), VENDORED_AV1_FRAMES);
    for findings in [
        compare_av1_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own AV1 bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }
    // AV1 reports `absent` on every frame — it has no matrix BUFFER at all, unlike HEVC where
    // the same spelling is a per-sequence decision.
    assert!(capture.qmatrix.values().all(Option::is_none));
}

/// A submission holding only what [`compare_descriptors`] reads, for the bitstream-size
/// classifier: `slices` slice records tiling a `padded`-byte buffer whose slice data (before the
/// tail padding) is `unpadded` bytes.
fn descriptor_only_submission(unpadded: u32, padded: u32, slices: usize) -> OurSubmission {
    let each = unpadded / slices as u32;
    let mut records: Vec<SliceRecord> = (0..slices)
        .map(|i| SliceRecord {
            location: i as u32 * each,
            bytes: each,
        })
        .collect();
    // The last record carries the remainder and the padding, exactly as the packer charges it.
    let last = records.last_mut().expect("at least one slice");
    last.bytes = padded - last.location;
    OurSubmission {
        pic_params: vec![0u8; size_of::<PicParamsH264>()],
        qmatrix: None,
        tiles: Vec::new(),
        descriptors: vec![
            BufferDescriptor {
                buffer_type: BUFFER_BITSTREAM,
                data_offset: 0,
                data_size: padded,
                num_mbs_in_buffer: 300,
            },
            BufferDescriptor {
                buffer_type: BUFFER_SLICE_CONTROL,
                data_offset: 0,
                data_size: 10 * slices as u32,
                num_mbs_in_buffer: 300,
            },
        ],
        records,
        unpadded,
        mb_count: 300,
    }
}

/// Rewrite every `PFPP <codec>` line of a capture through `f`, which receives the AU index and
/// the picture-parameter bytes. The instrument the two absorbers below are tested with: a
/// divergence this harness ABSORBS must be reproducible on demand, or its allowance is untested.
fn map_picparams(text: &str, codec: &str, mut f: impl FnMut(usize, &mut Vec<u8>)) -> String {
    let prefix = format!("PFPP {codec} ");
    let mut out = String::new();
    for line in text.lines() {
        match line.strip_prefix(&prefix) {
            Some(rest) => {
                let (au, hex) = rest.split_once(' ').expect("our own dump is well formed");
                let au: usize = au.parse().expect("a decimal AU index");
                let mut bytes = from_hex(hex).expect("our own dump is hex");
                f(au, &mut bytes);
                let _ = writeln!(out, "{prefix}{au} {}", to_hex(&bytes));
            }
            None => {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    out
}

/// Add `delta` to every POC an H.264 picture-parameters buffer carries: the current picture's pair
/// and every IN-USE reference entry's (an unused entry's counts are zero on both sides and must
/// stay that way). This is libavcodec's `prev_poc_msb` offset, synthesised.
fn shift_h264_capture_poc(pp: &mut [u8], delta: i32) {
    let curr = offset_of!(PicParamsH264, CurrFieldOrderCnt);
    let list = offset_of!(PicParamsH264, RefFrameList);
    let focl = offset_of!(PicParamsH264, FieldOrderCntList);
    for field in [curr, curr + 4] {
        let shifted = i32_at(pp, field).wrapping_add(delta);
        pp[field..field + 4].copy_from_slice(&shifted.to_le_bytes());
    }
    for i in 0..16 {
        if pp[list + i] == UNUSED_ENTRY {
            continue;
        }
        for field in [focl + 8 * i, focl + 8 * i + 4] {
            let shifted = i32_at(pp, field).wrapping_add(delta);
            pp[field..field + 4].copy_from_slice(&shifted.to_le_bytes());
        }
    }
}

/// A one-AU capture of `(buffer type, DataSize, NumMBsInBuffer)` descriptors.
fn descriptor_capture(descs: &[(u32, u32, u32)]) -> String {
    let mut text = String::new();
    for (buffer_type, data_size, mbs) in descs {
        let _ = writeln!(text, "PFBD h264 0 {buffer_type} {data_size} {mbs} 0");
    }
    text
}

#[test]
fn libavcodecs_constant_poc_base_is_documented_and_anything_else_about_a_poc_is_a_finding() {
    // The absorber that lets the H.264 comparison pass against the real capture, and the three
    // ways it must NOT absorb. Without this test the POC fields would simply be excluded from the
    // comparison, which is the "green gate that proves nothing" shape this program has been bitten
    // by: an excluded field cannot report a wrong POC either.
    let ours = our_h264_submissions();
    let base = dump("h264", &ours);

    // 1. Every POC offset by FFmpeg's 65536 — what the RTX 4090 capture actually carries.
    let shifted = map_picparams(&base, "h264", |_, pp| shift_h264_capture_poc(pp, 65536));
    let findings = compare_h264_picparams(&ours, &parse_capture(&shifted, "h264"));
    assert!(
        findings.is_empty(),
        "a constant POC base is not a finding, got {:?}",
        findings.fields()
    );
    assert_eq!(
        findings.documented_fields(),
        vec!["FieldOrderCnt[POC base]"]
    );

    // 2. A base that is neither 0 nor 65536 is unexplained, and unexplained is a finding.
    let odd = map_picparams(&base, "h264", |_, pp| shift_h264_capture_poc(pp, 7));
    let findings = compare_h264_picparams(&ours, &parse_capture(&odd, "h264"));
    assert_eq!(findings.fields(), vec!["CurrFieldOrderCnt[0][POC base]"]);
    assert!(findings.documented_fields().is_empty());

    // 3. A base that stops holding is a real POC divergence: 65536 everywhere except one AU,
    //    whose current picture is four counts adrift.
    let drifting = map_picparams(&base, "h264", |au, pp| {
        shift_h264_capture_poc(pp, if au == 10 { 65536 - 4 } else { 65536 })
    });
    let findings = compare_h264_picparams(&ours, &parse_capture(&drifting, "h264"));
    assert_eq!(
        findings.fields(),
        vec![
            "CurrFieldOrderCnt[0][POC]",
            "CurrFieldOrderCnt[1][POC]",
            "RefFrameList[set]",
        ]
    );
    assert_eq!(findings.by_field["CurrFieldOrderCnt[0][POC]"].first_au, 10);
}

#[test]
fn the_hevc_tiles_flag_allowance_is_exactly_bit_ten_with_tiles_disabled_and_nothing_else() {
    // The other absorber, and the reason it is written per-DIFFERENCE rather than per-field: the
    // same word carries eighteen other flags, and a difference in any of them — or in bit 10 while
    // tiles are ENABLED, where the flag stops being inert — must still be a finding.
    let ours = our_hevc_submissions();
    let base = dump("hevc", &ours);
    let at = offset_of!(PicParamsHevc, dwCodingSettingPicturePropertyFlags);
    let rewrite = |text: &str, mask_off: u32, mask_on: u32| {
        map_picparams(text, "hevc", |_, pp| {
            let flags = (u32_at(pp, at) & !mask_off) | mask_on;
            pp[at..at + 4].copy_from_slice(&flags.to_le_bytes());
        })
    };

    // Bit 10 clear on libav's side, tiles disabled on both: the documented divergence, which is
    // what the real capture carries on all 250 AUs.
    let capture = parse_capture(&rewrite(&base, 1 << 10, 0), "hevc");
    let findings = compare_hevc_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "the documented tiles-flag divergence is not a finding, got {:?}",
        findings.fields()
    );
    assert_eq!(
        findings.documented_fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );

    // A different bit of the same word is a finding: bit 11 is
    // pps_loop_filter_across_slices_enabled_flag, which is not inert at all.
    let capture = parse_capture(&rewrite(&base, 1 << 11, 0), "hevc");
    let findings = compare_hevc_picparams(&ours, &capture);
    assert_eq!(
        findings.fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );
    assert!(findings.documented_fields().is_empty());

    // Bit 10 differing while BOTH sides say tiles are enabled: the flag now governs a real tile
    // boundary, so the allowance must not apply.
    let ours_with_tiles: Vec<OurSubmission> = ours
        .iter()
        .map(|sub| {
            let mut pp = sub.pic_params.clone();
            let flags = u32_at(&pp, at) | (1 << 7) | (1 << 10);
            pp[at..at + 4].copy_from_slice(&flags.to_le_bytes());
            OurSubmission {
                pic_params: pp,
                qmatrix: sub.qmatrix.clone(),
                descriptors: sub.descriptors.clone(),
                records: sub.records.clone(),
                tiles: sub.tiles.clone(),
                unpadded: sub.unpadded,
                mb_count: sub.mb_count,
            }
        })
        .collect();
    let capture = parse_capture(
        &rewrite(&dump("hevc", &ours_with_tiles), 1 << 10, 1 << 7),
        "hevc",
    );
    let findings = compare_hevc_picparams(&ours_with_tiles, &capture);
    assert_eq!(
        findings.fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );
    assert!(findings.documented_fields().is_empty());
}

#[test]
fn a_bitstream_size_difference_is_classified_by_the_unpadded_window_it_implies() {
    // The one descriptor field with a legitimate divergence class, and the arithmetic that tells
    // the two apart. Ours: 1026 bytes of slice data over two slices, padded to 1152. Their
    // padding is not captured, but it is 1..=128 bytes, so a captured 1024 means their slice data
    // was 896..=1023 — which reaches to within four bytes per slice of our 1026 (1022 is 4 bytes
    // less over two slices), the trailing-zero delimitation shape. A captured 512 cannot: no
    // padding puts their slice data anywhere near ours.
    let ours = vec![descriptor_only_submission(1026, 1152, 2)];

    let legitimate = descriptor_capture(&[
        (BUFFER_BITSTREAM, 1024, 300),
        (BUFFER_SLICE_CONTROL, 20, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&legitimate, "h264"));
    assert_eq!(findings.fields(), vec!["BITSTREAM.DataSize[delimitation]"]);

    let defect = descriptor_capture(&[
        (BUFFER_BITSTREAM, 512, 300),
        (BUFFER_SLICE_CONTROL, 20, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&defect, "h264"));
    assert_eq!(findings.fields(), vec!["BITSTREAM.DataSize"]);

    // A differing slice count outranks the size: the two sides split the AU differently, which
    // voids the size comparison rather than needing a verdict of its own. Three ten-byte records
    // where ours has two.
    let split = descriptor_capture(&[
        (BUFFER_BITSTREAM, 1024, 300),
        (BUFFER_SLICE_CONTROL, 30, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&split, "h264"));
    assert_eq!(
        findings.fields(),
        vec!["BITSTREAM.DataSize[slice count]", "SLICE_CONTROL.DataSize"]
    );

    // And a NumMBsInBuffer difference is never in the legitimate class.
    let zeroed = descriptor_capture(&[(BUFFER_BITSTREAM, 1152, 0), (BUFFER_SLICE_CONTROL, 20, 0)]);
    let findings = compare_descriptors(&ours, &parse_capture(&zeroed, "h264"));
    assert_eq!(
        findings.fields(),
        vec!["BITSTREAM.NumMBsInBuffer", "SLICE_CONTROL.NumMBsInBuffer"]
    );
}

#[test]
fn a_changed_scalar_field_is_reported_by_its_name() {
    let ours = our_h264_submissions();
    let mut text = dump("h264", &ours);
    // Flip `pic_init_qp_minus26` (offset 172) on AU 7 of the "capture".
    let offset = offset_of!(PicParamsH264, pic_init_qp_minus26);
    text = mutate_capture_byte(&text, 7, offset, 0x5A);
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert_eq!(findings.fields(), vec!["pic_init_qp_minus26"]);
    assert_eq!(findings.by_field["pic_init_qp_minus26"].first_au, 7);

    // …and a differing byte the field table does NOT cover is still reported, by raw offset.
    // The table tiles both structs (asserted above), so this fallback is unreachable in
    // practice; it exists so that a field added to the struct without being added to the table
    // cannot pass unnoticed, and it is exercised here with a deliberately truncated table rather
    // than left as an unproven claim in the module docs.
    let mut findings = Findings::default();
    let mut theirs = ours[0].pic_params.clone();
    theirs[offset] = 0x5A;
    compare_scalars(
        0,
        &ours[0].pic_params,
        &theirs,
        &field_ranges(&H264_FIELDS[..2], 4),
        &[],
        no_allowance,
        &mut findings,
    );
    let expected = format!("<unclassified byte {offset:#06x}>");
    assert_eq!(findings.fields(), vec![expected.as_str()]);
}

#[test]
fn a_reordered_reference_list_is_no_finding_but_a_changed_one_is() {
    // The divergence this harness must NOT report, and the one it must. libavcodec emits its
    // reference array in a different order from ours by construction; a set comparison sees
    // through that. Dropping a reference — or renumbering one side's surfaces inconsistently —
    // must still be caught.
    let ours = our_h264_submissions();
    // An AU with at least two references, so a reversal is observable.
    let (au, entries) = ours
        .iter()
        .enumerate()
        .map(|(au, sub)| (au, h264_ref_entries(&sub.pic_params)))
        .find(|(_, entries)| entries.len() >= 2)
        .expect("the vector must reach two references");

    let reordered = reverse_h264_reference_list(&ours[au].pic_params);
    assert_ne!(
        reordered, ours[au].pic_params,
        "the reversal must change bytes"
    );
    let capture = parse_capture(
        &with_picparams(&dump("h264", &ours), au, &reordered),
        "h264",
    );
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "a reordered reference list is not a finding, got {:?}",
        findings.fields()
    );

    // Now DROP the last reference from that AU: the set differs, and it must be named.
    let mut dropped = ours[au].pic_params.clone();
    let list = offset_of!(PicParamsH264, RefFrameList);
    let last = entries.len() - 1;
    dropped[list + last] = UNUSED_ENTRY;
    let used = offset_of!(PicParamsH264, UsedForReferenceFlags);
    let cleared = u32_at(&dropped, used) & !(0b11 << (2 * last));
    dropped[used..used + 4].copy_from_slice(&cleared.to_le_bytes());
    let capture = parse_capture(&with_picparams(&dump("h264", &ours), au, &dropped), "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.fields().contains(&"RefFrameList[set]"),
        "a dropped reference must be reported, got {:?}",
        findings.fields()
    );
}

#[test]
fn a_wholly_renumbered_surface_set_is_no_finding_and_an_inconsistent_one_is() {
    // The bijection, both ways round. Renumbering EVERY surface index on the capture side is
    // exactly what a different frame pool does, and it must pass; renumbering one AU's alone
    // breaks the mapping under live pictures and must not.
    let ours = our_h264_submissions();
    let base = dump("h264", &ours);

    let renumbered: Vec<Vec<u8>> = ours
        .iter()
        .map(|sub| renumber_h264_surfaces(&sub.pic_params, |slot| slot + 8))
        .collect();
    let mut text = base.clone();
    for (au, pp) in renumbered.iter().enumerate() {
        text = with_picparams(&text, au, pp);
    }
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "a consistently renumbered surface set is a bijection, not a finding, got {:?}",
        findings.fields()
    );

    // One AU renumbered differently from the rest: the pictures it still holds change surface
    // mid-life, which is precisely what a mis-resolved reference looks like.
    let au = ours
        .iter()
        .position(|sub| h264_ref_entries(&sub.pic_params).len() >= 2)
        .expect("two references");
    let mut text = base;
    for (i, pp) in renumbered.iter().enumerate() {
        if i != au {
            text = with_picparams(&text, i, pp);
        }
    }
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings
            .fields()
            .iter()
            .any(|f| f.contains("surface mapping")),
        "an inconsistent surface numbering must be reported, got {:?}",
        findings.fields()
    );
}

#[test]
fn an_omitted_hevc_matrix_buffer_is_reported_as_a_presence_difference() {
    // Review 13's HEVC defect, in the form the harness would have caught it: the capture says
    // `absent`, our side submits one. (Built by rewriting the capture rather than the crate,
    // because the crate no longer has the defect — which is the point.)
    let ours = our_hevc_submissions();
    let mut subs = ours;
    subs[3].qmatrix = Some(vec![0u8; size_of::<QmatrixHevc>()]);
    subs[3].descriptors = vec![
        BufferDescriptor {
            buffer_type: BUFFER_PICTURE_PARAMETERS,
            data_offset: 0,
            data_size: size_of::<PicParamsHevc>() as u32,
            num_mbs_in_buffer: 0,
        },
        BufferDescriptor {
            buffer_type: BUFFER_INVERSE_QUANTIZATION_MATRIX,
            data_offset: 0,
            data_size: size_of::<QmatrixHevc>() as u32,
            num_mbs_in_buffer: 0,
        },
        subs[3].descriptors[1],
        subs[3].descriptors[2],
    ];
    // The capture is the honest one: no matrix on any AU.
    let honest = our_hevc_submissions();
    let capture = parse_capture(&dump("hevc", &honest), "hevc");

    let findings = compare_qmatrix(
        &subs,
        &capture,
        HEVC_QMATRIX_FIELDS,
        size_of::<QmatrixHevc>(),
    );
    assert_eq!(findings.fields(), vec!["<submitted>"]);
    assert_eq!(findings.by_field["<submitted>"].first_au, 3);
    let findings = compare_descriptors(&subs, &capture);
    assert_eq!(findings.fields(), vec!["<buffer set>"]);
}

#[test]
fn a_missing_bitstream_descriptor_is_reported_as_a_missed_patch_site_not_a_defect() {
    // The one capture-side mistake that is likely, because libavcodec fills the bitstream
    // descriptor outside the choke point: three PFBD lines per AU instead of four. The harness
    // must name the patch site rather than accuse our submission of dropping a buffer.
    let ours = our_h264_submissions();
    let text: String = dump("h264", &ours)
        .lines()
        .filter(|line| {
            // Only the BITSTREAM descriptor, which is the FOURTH token: matching on " 6 "
            // anywhere would also delete AU 6's whole descriptor set.
            let fields: Vec<&str> = line.split_whitespace().collect();
            !(fields.first() == Some(&"PFBD") && fields.get(3) == Some(&"6"))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let capture = parse_capture(&text, "h264");
    let findings = compare_descriptors(&ours, &capture);
    assert_eq!(findings.fields(), vec!["<buffer set>"]);
    assert!(
        findings.by_field["<buffer set>"]
            .detail
            .contains("commit_bitstream_and_slice_buffer"),
        "the report must name the patch site, got {:?}",
        findings.by_field["<buffer set>"].detail
    );
}

#[test]
fn an_unreadable_or_short_capture_is_refused_rather_than_partly_compared() {
    let ours = our_h264_submissions();
    let good = dump("h264", &ours);

    // A line whose format drifted.
    let broken = good.replace("PFPP h264 5 ", "PFPP h264 5 zz");
    let capture = parse_capture(&broken, "h264");
    assert_eq!(capture.unreadable.len(), 1);

    // A capture of the wrong stream length.
    let short: String = good
        .lines()
        .filter(|line| !line.starts_with("PFPP h264 24 "))
        .map(|line| format!("{line}\n"))
        .collect();
    let capture = parse_capture(&short, "h264");
    assert_eq!(capture.pic_params.len(), VENDORED_AUS - 1);
    assert!(!capture.pic_params.contains_key(&24));

    // Another codec's lines are not this codec's.
    assert!(parse_capture(&good, "hevc").pic_params.is_empty());

    // A capture logged against a codec context rather than NULL carries FFmpeg's own
    // `[h264 @ 0x…] ` prefix, and must still read cleanly.
    let prefixed: String = good
        .lines()
        .map(|line| format!("[h264 @ 0x7ff1c380a200] {line}\n"))
        .collect();
    let capture = parse_capture(&prefixed, "h264");
    assert!(capture.unreadable.is_empty());
    assert_eq!(capture.pic_params.len(), VENDORED_AUS);
    assert_eq!(capture.descriptors.len(), VENDORED_AUS);
}

// ---------------------------------------------------------------------------
// Capture-side mutators, for the tests above
// ---------------------------------------------------------------------------

/// Replace AU `au`'s `PFPP` line with `pp`.
fn with_picparams(text: &str, au: usize, pp: &[u8]) -> String {
    let prefix = format!("PFPP h264 {au} ");
    let hevc = format!("PFPP hevc {au} ");
    text.lines()
        .map(|line| {
            if line.starts_with(&prefix) {
                format!("{prefix}{}\n", to_hex(pp))
            } else if line.starts_with(&hevc) {
                format!("{hevc}{}\n", to_hex(pp))
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

/// Set one byte of AU `au`'s captured picture parameters.
fn mutate_capture_byte(text: &str, au: usize, offset: usize, value: u8) -> String {
    let prefix = format!("PFPP h264 {au} ");
    let mut out = String::new();
    for line in text.lines() {
        if let Some(hex) = line.strip_prefix(&prefix) {
            let mut bytes = from_hex(hex).expect("our own dump is hex");
            bytes[offset] = value;
            let _ = writeln!(out, "{prefix}{}", to_hex(&bytes));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

/// Reverse the in-use entries of an H.264 reference array, carrying each entry's keys and flag
/// bits with it — the reordering libavcodec's own `short_ref`-then-`long_ref` walk produces,
/// synthesised so the set comparison can be tested without a capture.
fn reverse_h264_reference_list(pp: &[u8]) -> Vec<u8> {
    let mut out = pp.to_vec();
    let list = offset_of!(PicParamsH264, RefFrameList);
    let poc = offset_of!(PicParamsH264, FieldOrderCntList);
    let nums = offset_of!(PicParamsH264, FrameNumList);
    let used_at = offset_of!(PicParamsH264, UsedForReferenceFlags);
    let missing_at = offset_of!(PicParamsH264, NonExistingFrameFlags);
    let entries = h264_ref_entries(pp);
    let slots: Vec<u8> = (0..entries.len()).map(|i| pp[list + i]).collect();
    let used = u32_at(pp, used_at);
    let missing = u16_at(pp, missing_at);
    let mut new_used = used & !((1u32 << (2 * entries.len())) - 1);
    let mut new_missing = missing & !((1u16 << entries.len()) - 1);
    for (i, source) in (0..entries.len()).rev().enumerate() {
        out[list + i] = slots[source];
        out[nums + 2 * i..nums + 2 * i + 2]
            .copy_from_slice(&pp[nums + 2 * source..nums + 2 * source + 2]);
        out[poc + 8 * i..poc + 8 * i + 8]
            .copy_from_slice(&pp[poc + 8 * source..poc + 8 * source + 8]);
        new_used |= (used >> (2 * source) & 0b11) << (2 * i);
        new_missing |= (missing >> source & 1) << i;
    }
    out[used_at..used_at + 4].copy_from_slice(&new_used.to_le_bytes());
    out[missing_at..missing_at + 2].copy_from_slice(&new_missing.to_le_bytes());
    out
}

/// Rewrite every surface index of an H.264 picture-parameters buffer through `f`.
fn renumber_h264_surfaces(pp: &[u8], f: impl Fn(u8) -> u8) -> Vec<u8> {
    let mut out = pp.to_vec();
    let curr = offset_of!(PicParamsH264, CurrPic);
    out[curr] = (out[curr] & 0x80) | (f(out[curr] & 0x7F) & 0x7F);
    let list = offset_of!(PicParamsH264, RefFrameList);
    for i in 0..16 {
        if out[list + i] != UNUSED_ENTRY {
            out[list + i] = (out[list + i] & 0x80) | (f(out[list + i] & 0x7F) & 0x7F);
        }
    }
    out
}

// ===========================================================================
// Capture-dependent: #[ignore]d, and saying why
// ===========================================================================

/// Emit this crate's whole submission — both codecs — in the capture's own format, so the two
/// files can be diffed by any tool without a capture at all.
#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_AV1=<file> (see the module docs)"]
fn our_av1_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_AV1", "av1")
        .expect("PF_LIBAV_CAPTURE_AV1=<file> names a capture (see the module docs)");
    let ours = our_av1_submissions();
    // No `Reserved16Bits` preflight: both libavcodec workarounds are H.264-only
    // (`dxva2_h264.c`), and `DXVA_PicParams_AV1` has no such field to test.
    preflight(&capture, ours.len(), "av1", None);
    compare_av1_picparams(&ours, &capture).verdict("AV1 picture parameters", ours.len());
}

#[test]
#[ignore = "writes a dump: PF_DXVA_DUMP=<path>"]
fn dump_our_submission_in_the_captures_own_format() {
    let path = std::env::var("PF_DXVA_DUMP").expect("PF_DXVA_DUMP=<path> names the output file");
    let mut text = dump("h264", &our_h264_submissions());
    text.push_str(&dump("hevc", &our_hevc_submissions()));
    // AV1 too, and it is the codec that needs this most: no libavcodec AV1 capture has
    // ever been taken (module docs say why), so for that codec this dump is the only
    // way to read what the driver is being handed at all.
    text.push_str(&dump("av1", &our_av1_submissions()));
    std::fs::write(&path, text).expect("write the dump");
    println!("wrote {path}");
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264=<file> (see the module docs)"]
fn our_h264_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264")
        .expect("PF_LIBAV_CAPTURE_H264=<file> names a capture (see the module docs)");
    let ours = our_h264_submissions();
    preflight(
        &capture,
        ours.len(),
        "h264",
        Some(offset_of!(PicParamsH264, Reserved16Bits)),
    );
    compare_h264_picparams(&ours, &capture).verdict("H.264 picture parameters", ours.len());
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_HEVC=<file> (see the module docs)"]
fn our_hevc_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc")
        .expect("PF_LIBAV_CAPTURE_HEVC=<file> names a capture (see the module docs)");
    let ours = our_hevc_submissions();
    preflight(&capture, ours.len(), "hevc", None);
    compare_hevc_picparams(&ours, &capture).verdict("HEVC picture parameters", ours.len());
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264/PF_LIBAV_CAPTURE_HEVC (module docs)"]
fn our_buffer_descriptors_match_libavcodecs() {
    let h264 = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264");
    let hevc = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc");
    assert!(
        h264.is_some() || hevc.is_some(),
        "PF_LIBAV_CAPTURE_H264=<file> and/or PF_LIBAV_CAPTURE_HEVC=<file> name a capture"
    );
    if let Some(capture) = h264 {
        let ours = our_h264_submissions();
        preflight(
            &capture,
            ours.len(),
            "h264",
            Some(offset_of!(PicParamsH264, Reserved16Bits)),
        );
        compare_descriptors(&ours, &capture).verdict("H.264 buffer descriptors", ours.len());
    }
    if let Some(capture) = hevc {
        let ours = our_hevc_submissions();
        preflight(&capture, ours.len(), "hevc", None);
        compare_descriptors(&ours, &capture).verdict("HEVC buffer descriptors", ours.len());
    }
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264/PF_LIBAV_CAPTURE_HEVC (module docs)"]
fn our_quantization_matrices_match_libavcodecs() {
    let h264 = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264");
    let hevc = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc");
    assert!(
        h264.is_some() || hevc.is_some(),
        "PF_LIBAV_CAPTURE_H264=<file> and/or PF_LIBAV_CAPTURE_HEVC=<file> name a capture"
    );
    if let Some(capture) = h264 {
        let ours = our_h264_submissions();
        preflight(
            &capture,
            ours.len(),
            "h264",
            Some(offset_of!(PicParamsH264, Reserved16Bits)),
        );
        compare_qmatrix(
            &ours,
            &capture,
            H264_QMATRIX_FIELDS,
            size_of::<QmatrixH264>(),
        )
        .verdict("H.264 quantization matrices", ours.len());
    }
    if let Some(capture) = hevc {
        let ours = our_hevc_submissions();
        preflight(&capture, ours.len(), "hevc", None);
        compare_qmatrix(
            &ours,
            &capture,
            HEVC_QMATRIX_FIELDS,
            size_of::<QmatrixHevc>(),
        )
        .verdict("HEVC quantization matrices", ours.len());
    }
}
