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

Re-sync procedure: fetch the AOSP tree, re-apply this trim, diff `codec/` +
`bitstream_utils.rs` (expect near-zero conflicts), update the commit pin above.
