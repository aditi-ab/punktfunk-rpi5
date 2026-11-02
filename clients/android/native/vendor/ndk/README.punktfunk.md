# Vendored `ndk` 0.9.0 (patched)

Verbatim copy of the published `ndk` 0.9.0 crate (https://crates.io/crates/ndk,
MIT OR Apache-2.0, © the rust-mobile contributors), wired in via `[patch.crates-io]`
in the workspace root.

**The only changes** make two raw-pointer accessors `pub` (upstream keeps them private), so
the Android client can bind entry points the wrapper doesn't expose:

- `src/media/media_codec.rs`: `MediaCodec::as_ptr`, for
  `AMediaCodec_setOnFrameRenderedCallback` — the render-timestamp callback behind the HUD's
  `display` stage (punktfunk-planning: `stats-unification.md`).
- `src/audio.rs`: `AudioStreamBuilder::as_ptr`, for `AAudioStreamBuilder_setChannelMask` — the
  speaker layout of a 5.1/7.1 stream.

Grep for `punktfunk vendored patch` to find them. Drop this vendor copy when upstream exposes
the raw pointers or both bindings (tracked against https://github.com/rust-mobile/ndk).
