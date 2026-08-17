# Vendored: cros-codecs (parser layer only)

- **Upstream:** <https://android.googlesource.com/platform/system/cros-codecs/> (the
  authoritative AOSP tree). Snapshot taken from the read-only GitHub mirror
  <https://github.com/chromeos/cros-codecs>, branch `main`,
  commit **`5ff6d693ffae0b36935b8fc13092c733b4c2646f`**, fetched 2026-08-05.
- **License:** BSD-3-Clause (`LICENSE`, copied verbatim). Attribution headers retained
  in every source file.
- **Why vendored, not a crates.io dependency:** the GitHub repo is a read-only mirror
  and the crates.io release lags it; a pinned, reviewed snapshot is the supply-chain
  posture punktfunk already uses elsewhere (`clients/android/native/vendor/ndk`,
  `punktfunk-host/vendor/usbip-sim`). Decision of record:
  punktfunk-planning `design/client-native-decode.md` §8.1.

## What was taken

`src/codec/{h264,h265,av1,vp9}` (parsers, DPBs, picture types, NALU/OBU machinery,
their `test_data` vectors — they double as punktfunk's conformance corpus),
`src/bitstream_utils.rs`, `LICENSE`. Upstream designed the `codec` module for exactly
this extraction — its module doc: "There shall be no dependencies from other modules of
this crate to this module, so that it can be turned into a crate of its own if needed
in the future."

## What was left behind

- `decoder/`, `encoder/`, `backend/`, `c2_wrapper/`, `video_frame`, `image_processing`,
  `utils` — the Linux-only halves (libva/v4l2/gbm/nix). punktfunk's `pf-bitstream` +
  `pf-vkdecode` occupy that layer.
- `codec/vp8` — VP9 has no dependency on it (verified) and no punktfunk host will ever
  emit VP8.

## Deviations from pristine upstream

1. `src/lib.rs` — rewritten: keeps only the module decls and `Resolution` /
   `ResolutionRoundMode` (the sole root items `codec` references), both copied verbatim;
   adds crate-level `#![allow(clippy::all, mismatched_lifetime_syntaxes)]` — vendored
   code is not held to the workspace lint bar (CI's `-D warnings` legs would fail on
   upstream style otherwise).
2. `src/codec.rs` — one line removed (`pub mod vp8;`).
3. `Cargo.toml` — rewritten: `log` is the only dependency the vendored subset needs,
   plus `env_logger`/`serde_json` dev-dependencies for upstream's in-tree tests.
4. `cargo fmt` normalization under the workspace's rustfmt config (mechanical only).
5. **Zero-unsafe, enforced**: `#![forbid(unsafe_code)]` added to lib.rs. Upstream's codec
   module had exactly one production `unsafe` (h264/dpb.rs `build_ref_pic_lists`: ref→index
   via pointer `offset_from`) — replaced with a safe `position(ptr::eq)` over the ≤16-entry
   DPB — and three test-only `mem::zeroed()` asserts, replaced with `Default::default()`
   (`PredWeightTable` derives `Default`; all-integer struct, identical value). The layer
   facing untrusted bytes is now compiler-verified free of unsafe — the property that
   motivates replacing libavcodec's C parsers in the first place.
6. `src/codec/h264/picture.rs` — `PictureData::new_from_slice`: `display_resolution`
   computed as `visible_rect.max` instead of `max - min`. `Sps::visible_rectangle()`
   returns the crop offset in `min` and the visible *size* in `max` (see its
   definition: `max.x = width - crop_left - crop_right`); upstream's subtraction
   double-counts the left/top crop and, worse, panics on u32 underflow for a
   large-but-parser-valid `frame_crop_left_offset` (e.g. 100 crop units on a 320-wide
   SPS). Found by pf-bitstream's conformance-window tests; upstream never hits it
   because real encoders crop right/bottom only. **Reported upstream 2026-08-06:
   <https://github.com/chromeos/cros-codecs/issues/99>.**

7. `src/codec/h265/parser.rs` — `parse_slice_header`: reject
   `num_long_term_sps + num_long_term_pics > 16` before the long-term RPS loop.
   Upstream bounds the pair only by `MAX_LONG_TERM_REF_PIC_SETS` (32) combined, while
   every long-term array in `SliceHeader` (`poc_lsb_lt`, `used_by_curr_pic_lt`,
   `delta_poc_msb_present_flag`, `delta_poc_msb_cycle_lt`, `lt_idx_sps`) is `[_; 16]`
   — a hostile slice header with 17+ entries panics the parser with an
   index-out-of-bounds (bounds checks stay on in release). Found by pf-bitstream's
   H.265 planner review; regression-tested there
   (`a_hostile_long_term_count_is_a_parse_error_not_a_panic`). **Reported upstream
   2026-08-06: <https://github.com/chromeos/cros-codecs/issues/100>.**

8. `src/bitstream_utils.rs` — `BitReader::read_bits` accepts 32 bits, and the 31-bit
   limit moves to `read_bits_signed` where its reason lives. Upstream capped the unsigned
   read at 31 "because that would break the read_bits_signed() function" — true of the
   signed path's `i32` accumulator, but it denies the unsigned path a width AV1 requires
   in five places: `timing_info`'s `num_units_in_display_tick` and `time_scale`,
   `decoder_model_info`'s `num_units_in_decoding_tick` (all `f(32)`), and the
   variable-width buffer-delay and `buffer_removal_time` fields, whose lengths are read
   from the stream and reach 32. A sequence header with `timing_info_present_flag` set
   was therefore unparseable — a legal stream that AMD's AMF encoder emits and NVENC does
   not, so **every AV1 session on an AMD host failed**: `AV1 parse: more than 31 (32) bits
   were requested` on the first access unit, then `No sequence header parsed yet` for
   every one after. Upstream's own `BitWriter::write_f` already accepts 32, so the crate
   could emit a header it could not read back.

   Three edits, all required together — the guard alone is not the fix:
   - the trailing mask is `u32::MAX` at 32 (`1u32 << 32` overflows: a debug panic, and in
     release a mask of zero, i.e. a silent `0` return);
   - the byte cursor is advanced before the accumulation loop when it sits at zero
     remaining bits, which otherwise shifts by the full width and ORs the spent byte in.
     At ≤31 bits the mask discarded those bits, so it was invisible; at 32 it cannot;
   - `read_bits_signed` carries its own `> 31` guard, so widening the unsigned path does
     not silently widen the signed one into an overflow.

   Also fixed in the same function: the sign extension `-1 ^ ((1 << num_bits) - 1)`
   overflows at `num_bits == 31` (`1i32 << 31` is `i32::MIN`; subtracting one from it
   panics in debug) — a latent panic at a width the guard admits and upstream's comment
   considered safe. Rewritten as `-1i32 << num_bits`, equal for every accepted width.
   Regression-tested here (`read_thirty_two_bits_*`, `signed_reads_stop_at_thirty_one_bits`,
   `widths_below_thirty_two_are_unchanged_across_a_spent_byte`) and end-to-end as an AV1
   synthesize/parse round trip (`sequence_header_obu_round_trips_timing_info`), which
   reproduces the field error string exactly when the guard is reverted. **Report upstream
   — not yet filed.**

   Not changed: `read_bits_signed(0)` still underflows on `num_bits - 1`. Unreachable —
   AV1 has its own `read_su`, and the H.26x `se(v)` callers all pass positive widths — so
   it is left to upstream rather than widened into this deviation.

Re-sync procedure: fetch the AOSP tree, re-apply this trim, diff `codec/` +
`bitstream_utils.rs` (expect near-zero conflicts), update the commit pin above.
