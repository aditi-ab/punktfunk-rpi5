//! Zero-copy capture→encode (plan §9): the PipeWire dmabuf is imported into CUDA via EGL and
//! handed straight to NVENC, eliminating the per-frame CPU copies (at 5K the CPU-copy path
//! moves ~3.5 GB/s). On NVENC opt in with `PUNKTFUNK_ZEROCOPY=1` (the CPU-copy path stays that
//! backend's default and the runtime fallback: foreign-allocator / no-dmabuf / import failure).
//! On the VAAPI (AMD/Intel) backend zero-copy is the **default** — its LINEAR-dmabuf passthrough
//! replaces a triple CPU touch (mmap de-pad + swscale CSC + surface upload) — with a one-shot
//! downgrade to the CPU path if the compositor never accepts the dmabuf offer.
//!
//! Pieces: [`cuda`] (driver-API FFI + the shared `CUcontext` + device buffers), [`egl`] (the
//! headless EGLDisplay + dmabuf→`EGLImage`→CUDA import). The encoder's CUDA-frame path lives in
//! `encode/linux.rs`; the dmabuf negotiation lives in `capture/linux.rs`.

pub mod cuda;
pub mod egl;
pub mod vulkan;

use std::sync::atomic::{AtomicBool, Ordering};

pub use cuda::DeviceBuffer;
pub use egl::{DmabufPlane, EglImporter};

/// Whether a `PUNKTFUNK_*` flag is truthy (`1`/`true`/`yes`/`on`), or `None` when unset.
fn flag_opt(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

/// Whether a `PUNKTFUNK_*` flag is truthy (`1`/`true`/`yes`/`on`); unset ⇒ false.
fn flag(name: &str) -> bool {
    flag_opt(name).unwrap_or(false)
}

/// One-shot downgrade latch: a VAAPI-passthrough capture whose dmabuf-only offer never negotiated
/// (the compositor can't allocate a LINEAR BGRx dmabuf) flips this, so the encode loop's pipeline
/// rebuild lands on the CPU offer instead of failing the same negotiation forever. Only consulted
/// when `PUNKTFUNK_ZEROCOPY` is unset — an explicit `=1` keeps forcing the dmabuf offer.
static VAAPI_DMABUF_FAILED: AtomicBool = AtomicBool::new(false);

/// Record that the VAAPI LINEAR-dmabuf offer failed negotiation (see [`VAAPI_DMABUF_FAILED`]).
pub fn note_vaapi_dmabuf_failed() {
    VAAPI_DMABUF_FAILED.store(true, Ordering::Relaxed);
}

/// True when `PUNKTFUNK_ZEROCOPY` is explicitly truthy — the operator forced the dmabuf offer, so
/// a failed negotiation keeps erroring loudly instead of silently downgrading to the CPU path.
pub fn vaapi_dmabuf_forced() -> bool {
    flag_opt("PUNKTFUNK_ZEROCOPY") == Some(true)
}

/// Whether the zero-copy path is on. `PUNKTFUNK_ZEROCOPY` decides when set (truthy = on, else
/// off). Unset defaults **on for the VAAPI (AMD/Intel) backend** — the stock AMD/Intel install
/// gets the GPU dmabuf path, not three full-frame CPU touches — unless a failed negotiation
/// downgraded it ([`note_vaapi_dmabuf_failed`]); and **off for NVENC**, whose EGL→CUDA import
/// stays opt-in (Mutter+NVIDIA has known dmabuf-capture races; see `PUNKTFUNK_FORCE_SHM`).
pub fn enabled() -> bool {
    match flag_opt("PUNKTFUNK_ZEROCOPY") {
        Some(v) => v,
        None => {
            crate::encode::linux_zero_copy_is_vaapi()
                && !VAAPI_DMABUF_FAILED.load(Ordering::Relaxed)
        }
    }
}

/// Whether the tiled-GL zero-copy path converts to NV12 on the GPU and feeds NVENC native YUV —
/// deleting NVENC's internal RGB→YUV CSC, which otherwise runs on the SM/3D engine the game
/// saturates (Tier 2A). **Default ON** (validated color-correct on the RTX 5070 Ti via
/// `nv12-selftest` + live decode on dev + Bazzite/KWin boxes; latency- and CPU-neutral idle,
/// frees SM headroom under load — the same default the Windows host ships). `PUNKTFUNK_NV12=0`
/// restores the RGB/BGRx feed. LINEAR (gamescope/Vulkan-bridge) captures are unaffected either way.
pub fn nv12_enabled() -> bool {
    flag_opt("PUNKTFUNK_NV12").unwrap_or(true)
}

/// DRM FourCC for a packed 32-bit format name (little-endian, e.g. `b"XR24"`).
const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// Map a SPA/our [`crate::capture::PixelFormat`] to the DRM FourCC EGL expects for import.
/// SPA byte order `BGRx` ⇒ DRM `XRGB8888` (memory B,G,R,X), etc.
pub fn drm_fourcc(format: crate::capture::PixelFormat) -> Option<u32> {
    use crate::capture::PixelFormat::*;
    Some(match format {
        Bgrx => fourcc(b"XR24"), // DRM_FORMAT_XRGB8888
        Bgra => fourcc(b"AR24"), // DRM_FORMAT_ARGB8888
        Rgbx => fourcc(b"XB24"), // DRM_FORMAT_XBGR8888
        Rgba => fourcc(b"AB24"), // DRM_FORMAT_ABGR8888
        // 24-bit packed RGB/BGR have no straightforward dmabuf import here; use the CPU path.
        // Rgb10a2/Nv12/P010 are the Windows HDR / video-processor formats — never produced on Linux.
        Rgb | Bgr | Rgb10a2 | Nv12 | P010 => return None,
    })
}

/// Standalone probe (the `zerocopy-probe` subcommand): initialize the EGL importer + CUDA
/// context and report. De-risks the FFI/linking/GPU-access without needing a capture session.
pub fn probe() -> anyhow::Result<()> {
    let _importer = EglImporter::new()?;
    let ctx = cuda::context()?;
    tracing::info!(cuda_ctx = ?ctx, "zero-copy probe OK — EGL display + CUDA context initialized");
    Ok(())
}

/// Reference BT.709 LIMITED-range conversion of one full-range RGB pixel (`u8`) to (Y, U, V) in
/// `f64`, matching the GPU shaders in [`egl`]. Y in [16,235], U/V in [16,240].
fn bt709_limited(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b);
    let u = 128.0 + 224.0 * (-0.1146 * r - 0.3854 * g + 0.5000 * b);
    let v = 128.0 + 224.0 * (0.5000 * r - 0.4542 * g - 0.0458 * b);
    (y, u, v)
}

/// NV12 colour self-test (the `nv12-selftest` subcommand): stand up the EGL/GL + CUDA stack, upload
/// a known synthetic RGBA pattern, run the real NV12 convert shaders on the GPU, read the Y and UV
/// planes back, and compare against a Rust BT.709 limited-range reference. Validates colour
/// correctness on the GPU **without a display** (the project's green-screen bugs came from exactly
/// this kind of plane/layout error). PASS if max abs error Y ≤ 2, U/V ≤ 3.
pub fn nv12_selftest() -> anyhow::Result<()> {
    use anyhow::bail;

    // 64x64, even dims. A 4x4 grid of 16x16 flat-colour blocks (so each 2x2 chroma footprint is
    // uniform → exact chroma comparison) covering the primaries + gray/black/white, then the rest
    // is a diagonal gradient (every pixel changes — a Y-channel stress that also exercises the
    // chroma averaging; the gradient blocks are compared on Y only).
    const W: u32 = 64;
    const H: u32 = 64;
    const BLK: u32 = 16;
    // (name, r, g, b) for the labelled blocks in row-major grid order; the rest fall to gradient.
    let named: [(&str, u8, u8, u8); 8] = [
        ("red", 255, 0, 0),
        ("green", 0, 255, 0),
        ("blue", 0, 0, 255),
        ("white", 255, 255, 255),
        ("black", 0, 0, 0),
        ("gray128", 128, 128, 128),
        ("yellow", 255, 255, 0),
        ("cyan", 0, 255, 255),
    ];

    // Build the RGBA pattern + a parallel record of each pixel's (r,g,b) and whether it sits in a
    // flat block (chroma-comparable) or the gradient (Y-only).
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    let mut flat = vec![false; (W * H) as usize];
    let grid_cols = W / BLK; // 4
    let pixel_rgb = |x: u32, y: u32| -> (u8, u8, u8, bool) {
        let bx = x / BLK;
        let by = y / BLK;
        let idx = (by * grid_cols + bx) as usize;
        if idx < named.len() {
            let (_, r, g, b) = named[idx];
            (r, g, b, true)
        } else {
            // Diagonal gradient — distinct per pixel.
            let r = ((x * 4) & 0xff) as u8;
            let g = ((y * 4) & 0xff) as u8;
            let b = (((x + y) * 2) & 0xff) as u8;
            (r, g, b, false)
        }
    };
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, is_flat) = pixel_rgb(x, y);
            let i = ((y * W + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
            flat[(y * W + x) as usize] = is_flat;
        }
    }

    // GPU convert.
    let mut importer = EglImporter::new()?;
    let nv12 = importer.convert_rgba_for_test(&rgba, W, H)?;
    let (uv_ptr, uv_pitch) = nv12
        .uv
        .ok_or_else(|| anyhow::anyhow!("self-test buffer is not NV12"))?;
    // Read both planes back to host (tightly packed).
    let y_host = cuda::read_plane_to_host(nv12.ptr, nv12.pitch, W as usize, H as usize)?;
    let uv_host = cuda::read_plane_to_host(uv_ptr, uv_pitch, (W as usize / 2) * 2, H as usize / 2)?;

    // Compare Y over every pixel.
    let mut max_y_err = 0.0f64;
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, _) = pixel_rgb(x, y);
            let (ref_y, _, _) = bt709_limited(r, g, b);
            let got = y_host[(y * W + x) as usize] as f64;
            max_y_err = max_y_err.max((got - ref_y).abs());
        }
    }

    // Compare U/V over flat blocks only (each 2x2 footprint is a single colour → exact reference).
    // Chroma is W/2 × H/2 samples, interleaved [U,V] per sample.
    let cw = W / 2;
    let ch = H / 2;
    let mut max_u_err = 0.0f64;
    let mut max_v_err = 0.0f64;
    for cy in 0..ch {
        for cx in 0..cw {
            // The 2x2 source footprint of this chroma sample.
            let (sx, sy) = (cx * 2, cy * 2);
            // Only compare where all 4 source pixels are flat (uniform colour).
            let all_flat =
                (0..2).all(|dy| (0..2).all(|dx| flat[((sy + dy) * W + (sx + dx)) as usize]));
            if !all_flat {
                continue;
            }
            let (r, g, b, _) = pixel_rgb(sx, sy);
            let (_, ref_u, ref_v) = bt709_limited(r, g, b);
            let base = ((cy * cw + cx) * 2) as usize;
            let got_u = uv_host[base] as f64;
            let got_v = uv_host[base + 1] as f64;
            max_u_err = max_u_err.max((got_u - ref_u).abs());
            max_v_err = max_v_err.max((got_v - ref_v).abs());
        }
    }

    // Per-primary actual-vs-expected (block centre for chroma).
    println!("NV12 self-test ({W}x{H}, BT.709 limited range)");
    println!(
        "  {:<8} {:>14} {:>14} {:>14}",
        "color", "Y exp/got", "U exp/got", "V exp/got"
    );
    for (idx, (name, r, g, b)) in named.iter().enumerate() {
        let bx = (idx as u32 % grid_cols) * BLK + BLK / 2;
        let by = (idx as u32 / grid_cols) * BLK + BLK / 2;
        let (ey, eu, ev) = bt709_limited(*r, *g, *b);
        let gy = y_host[(by * W + bx) as usize] as f64;
        let (ccx, ccy) = (bx / 2, by / 2);
        let cbase = ((ccy * cw + ccx) * 2) as usize;
        let gu = uv_host[cbase] as f64;
        let gv = uv_host[cbase + 1] as f64;
        println!(
            "  {:<8} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0}",
            name, ey, gy, eu, gu, ev, gv
        );
    }
    println!(
        "  max abs error:  Y={max_y_err:.2} (≤2)   U={max_u_err:.2} (≤3)   V={max_v_err:.2} (≤3)"
    );

    if max_y_err <= 2.0 && max_u_err <= 3.0 && max_v_err <= 3.0 {
        println!("PASS");
        Ok(())
    } else {
        println!("FAIL");
        bail!("NV12 self-test FAILED (Y={max_y_err:.2} U={max_u_err:.2} V={max_v_err:.2})");
    }
}
