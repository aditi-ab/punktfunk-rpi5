# Vendored `ndk` 0.9.0 (patched)

Verbatim copy of the published `ndk` 0.9.0 crate (https://crates.io/crates/ndk,
MIT OR Apache-2.0, © the rust-mobile contributors), wired in via `[patch.crates-io]`
in the workspace root.

**The only change** is in `src/media/media_codec.rs`: `MediaCodec::as_ptr` is made
`pub` (upstream keeps it private) so the Android client can register
`AMediaCodec_setOnFrameRenderedCallback` through `ndk-sys` — the render-timestamp
callback behind the HUD's `display` stage (punktfunk-planning: `stats-unification.md`), which the
wrapper doesn't expose. Grep for `punktfunk vendored patch` to find it.

Drop this vendor copy when upstream exposes the raw pointer or a frame-rendered
callback binding (tracked against https://github.com/rust-mobile/ndk).
