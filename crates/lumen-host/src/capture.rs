//! Frame capture (plan §7). On Linux: a PipeWire ScreenCast portal stream. M0 uses the
//! CPU-copy fallback (the portal delivers a CPU buffer; the encoder uploads it to the GPU
//! internally). Zero-copy dmabuf→NVENC import is deferred (plan §9 risk).

use anyhow::Result;

/// Packed pixel layout of a [`CapturedFrame`]. The ScreenCast portal negotiates the
/// format; on wlroots it is commonly packed `RGB` (3 bytes/pixel). The encoder maps these
/// to an NVENC-accepted input format (`rgb0`/`bgr0`/`rgba`/`bgra`), expanding 3→4 bytes
/// where needed — no host-side colour conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// `[B,G,R,x]`, 4 bpp.
    Bgrx,
    /// `[R,G,B,x]`, 4 bpp.
    Rgbx,
    /// `[B,G,R,A]`, 4 bpp.
    Bgra,
    /// `[R,G,B,A]`, 4 bpp.
    Rgba,
    /// `[R,G,B]`, 3 bpp.
    Rgb,
    /// `[B,G,R]`, 3 bpp.
    Bgr,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb | PixelFormat::Bgr => 3,
            _ => 4,
        }
    }
}

/// A captured frame. For zero-copy the real type would wrap a dmabuf fd + modifier; the
/// CPU buffer is the M0 fallback path (plan §9 risk: per-GPU dmabuf import quirks).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pts_ns: u64,
    /// Pixel layout of `cpu_bytes`.
    pub format: PixelFormat,
    /// Tightly-packed pixels in `format`, `width * height * format.bytes_per_pixel()`
    /// bytes (no row padding).
    pub cpu_bytes: Vec<u8>,
}

/// Produces frames from a captured output. Lives on its own thread, feeding the encoder
/// over a bounded drop-oldest channel (never block the compositor).
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Non-blocking: the freshest frame available since the last call, or `None` if none has
    /// arrived (the caller reuses its last frame to hold a steady output rate). The default
    /// just produces a frame each call — fine for instant synthetic sources; the portal
    /// overrides it to drain its channel without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }
}

/// A deterministic moving test pattern (BGRx). Lets M0 exercise the encode → file →
/// `lumen_core` path with no live capture session, and produces obviously non-static
/// content (a sweeping bar + animated gradient) so the encoded output is verifiable.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl SyntheticCapturer {
    const BPP: usize = 4; // emits BGRx

    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(width > 0 && height > 0 && fps > 0);
        let buf = vec![0u8; width as usize * height as usize * Self::BPP];
        SyntheticCapturer {
            width,
            height,
            fps,
            frame_idx: 0,
            buf,
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bpp = Self::BPP;
        let t = self.frame_idx;
        // A vertical bar sweeps left→right once every ~2s; the background is a gradient
        // whose phase advances each frame, so every pixel changes frame-to-frame.
        let bar_x = ((t * w as u64) / (self.fps as u64 * 2)) % w as u64;
        let phase = (t % 256) as usize;
        for y in 0..h {
            let row = y * w * bpp;
            for x in 0..w {
                let i = row + x * bpp;
                let on_bar = (x as u64).abs_diff(bar_x) < 8;
                // BGRx byte order: [B, G, R, x]
                self.buf[i] = if on_bar {
                    255
                } else {
                    ((x + phase) & 0xff) as u8
                };
                self.buf[i + 1] = if on_bar {
                    255
                } else {
                    ((y + phase) & 0xff) as u8
                };
                self.buf[i + 2] = if on_bar { 255 } else { ((x + y) & 0xff) as u8 };
                self.buf[i + 3] = 0;
            }
        }
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps as u64;
        self.frame_idx += 1;
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Bgrx,
            cpu_bytes: self.buf.clone(),
        })
    }
}

/// A cheap moving test pattern (BGRx) for the streaming path: a pulsing field + a white band
/// sweeping down, generated with whole-buffer `fill`s so it stays real-time even at 5K.
pub struct FastSyntheticCapturer {
    width: u32,
    height: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl FastSyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0);
        FastSyntheticCapturer {
            width,
            height,
            frame_idx: 0,
            buf: vec![0u8; width as usize * height as usize * 4],
        }
    }
}

impl Capturer for FastSyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let (w, h) = (self.width as usize, self.height as usize);
        let row = w * 4;
        let shade = (self.frame_idx % 256) as u8;
        self.buf.fill(shade);
        let band_h = (h / 20).max(1);
        let band_y = (self.frame_idx as usize * 6) % h;
        for y in band_y..(band_y + band_h).min(h) {
            self.buf[y * row..(y + 1) * row].fill(0xff);
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            cpu_bytes: self.buf.clone(),
        })
    }
}

/// Open a live capturer for a client-sized monitor via the xdg ScreenCast portal
/// (`ashpd`) → PipeWire (`pipewire`). Implemented in the `linux` submodule.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::open().map(|c| Box::new(c) as Box<dyn Capturer>)
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

#[cfg(target_os = "linux")]
mod linux;
