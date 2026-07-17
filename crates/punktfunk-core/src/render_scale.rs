//! Render-resolution scaling — the pure geometry behind the clients' render-scale setting.
//!
//! Client-side supersampling: a client asks the host to render/encode at `chosen resolution ×
//! scale` (a normal larger/smaller [`Mode`](crate::Mode) — the host does no scaling), then its
//! presenter downscales the larger decoded frame to the display (`> 1` = supersampling for
//! sharpness) or upscales a smaller one (`< 1` = a lighter host GPU / thinner link). This module is
//! where the multiplier becomes a host-valid mode dimension: multiply, preserve the aspect ratio,
//! floor to even (the host's `validate_dimensions` rejects odd sizes), and clamp to the codec's
//! per-axis ceiling so a connect can't request a size the encoder will reject.
//!
//! It is the Rust twin of the Apple client's `PunktfunkShared/RenderScale.swift` and is shared by
//! the native (Linux/`clients/session`, Windows) clients and the Android JNI path — all of which
//! reach `punktfunk-core`. Kept dependency-free + side-effect-free so it is unit-tested here.

/// Minimum supported multiplier (renders under native, upscaled on present).
pub const MIN_SCALE: f64 = 0.5;
/// Maximum supported multiplier (supersamples, clamped to the codec ceiling per axis).
pub const MAX_SCALE: f64 = 4.0;

/// The multipliers a picker offers. `1.0` (Native) is the default; the rest are the round stops
/// users reason about. Shared so every client's list stays identical.
pub const PRESETS: [f64; 9] = [0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// The encoder/host per-axis ceiling for a codec preference string (the clients' `codec` setting:
/// `"auto"`/`"hevc"`/`"h264"`/`"av1"`/`"pyrowave"`). H.264 tops out at 4096 px/axis; everything else
/// (incl. "auto", which negotiates HEVC/AV1 in practice) at 8192 — the same walls the host enforces
/// in `pf-encode`'s `codec.rs::max_dimension`.
pub fn max_dimension(codec: &str) -> u32 {
    if codec == "h264" {
        4096
    } else {
        8192
    }
}

/// Clamp a raw stored multiplier into `[MIN_SCALE, MAX_SCALE]`, treating a missing / non-positive /
/// NaN value as `1.0` (Native).
pub fn sanitize(raw: f64) -> f64 {
    if raw.is_nan() || raw <= 0.0 {
        return 1.0;
    }
    raw.clamp(MIN_SCALE, MAX_SCALE)
}

/// Apply `scale` to a base pixel size: preserve aspect, even-floor each axis, and clamp uniformly so
/// neither axis exceeds `max_dim` (the larger axis lands on the cap, the ratio is kept). Also floors
/// each axis at 320×200 (the host never accepts smaller). The result is a directly host-valid
/// [`Mode`](crate::Mode) width/height.
pub fn apply(base_w: u32, base_h: u32, scale: f64, max_dim: u32) -> (u32, u32) {
    let scale = sanitize(scale);
    let mut w = base_w.max(1) as f64 * scale;
    let mut h = base_h.max(1) as f64 * scale;
    // Uniform down-clamp if either axis blew past the ceiling — keep the aspect ratio intact.
    let cap = max_dim as f64;
    let over = (w / cap).max(h / cap);
    if over > 1.0 {
        w /= over;
        h /= over;
    }
    (even_floor(w, 320), even_floor(h, 200))
}

/// Floor a dimension to an even integer, not below `minimum` (also even-floored).
fn even_floor(value: f64, minimum: u32) -> u32 {
    let v = (value.floor() as i64).max(minimum as i64).max(0) as u32;
    v / 2 * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_and_defaults() {
        assert_eq!(sanitize(0.0), 1.0);
        assert_eq!(sanitize(-3.0), 1.0);
        assert_eq!(sanitize(f64::NAN), 1.0);
        assert_eq!(sanitize(0.1), 0.5);
        assert_eq!(sanitize(9.0), 4.0);
        assert_eq!(sanitize(1.5), 1.5);
    }

    #[test]
    fn max_dimension_is_codec_aware() {
        assert_eq!(max_dimension("h264"), 4096);
        assert_eq!(max_dimension("hevc"), 8192);
        assert_eq!(max_dimension("av1"), 8192);
        assert_eq!(max_dimension("auto"), 8192);
    }

    #[test]
    fn native_is_identity() {
        assert_eq!(apply(1920, 1080, 1.0, 8192), (1920, 1080));
    }

    #[test]
    fn supersample_doubles() {
        assert_eq!(apply(1920, 1080, 2.0, 8192), (3840, 2160));
    }

    #[test]
    fn under_render_halves() {
        assert_eq!(apply(1920, 1080, 0.5, 8192), (960, 540));
    }

    #[test]
    fn results_are_even() {
        // 1366×768 × 1.5 = 2049×1152 → even-floored to 2048×1152.
        let (w, h) = apply(1366, 768, 1.5, 8192);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert_eq!((w, h), (2048, 1152));
    }

    #[test]
    fn over_ceiling_clamps_uniformly() {
        // 4K × 4 = 15360×8640; both exceed 8192 → width lands on cap, 16:9 kept (8192×4608).
        let (w, h) = apply(3840, 2160, 4.0, 8192);
        assert!(w <= 8192 && h <= 8192);
        assert_eq!((w, h), (8192, 4608));
    }

    #[test]
    fn h264_ceiling_is_tighter() {
        // 1080p × 4 = 7680×4320; under H.264's 4096 wall → 4096×2304.
        assert_eq!(apply(1920, 1080, 4.0, 4096), (4096, 2304));
    }

    #[test]
    fn minimum_floor_honoured() {
        let (w, h) = apply(400, 300, 0.5, 8192);
        assert!(w >= 320 && h >= 200);
    }
}
