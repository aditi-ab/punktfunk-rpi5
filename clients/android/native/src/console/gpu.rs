//! Skia over the host's EGL context: one `DirectContext` for the host's lifetime, and a
//! `Surface` wrapping framebuffer 0 of whatever window surface is current, re-wrapped when
//! the window's size changes.

use super::egl::EglContext;
use anyhow::{anyhow, Result};
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{ColorType, Surface};

/// GL_RGBA8 — the sized internal format of the RGBA8888 EGL config's default framebuffer.
const GL_RGBA8: u32 = 0x8058;

pub(super) struct Gpu {
    pub(super) context: DirectContext,
}

impl Gpu {
    /// The Skia context over the (already current) EGL context. `cache_bytes` is the resource
    /// budget the console asked for (posters, glyph atlases — see `ConsoleOptions`).
    pub(super) fn new(_egl: &EglContext, cache_bytes: usize) -> Result<Gpu> {
        // Skia's native GL interface on Android assembles itself over `eglGetProcAddress`.
        let interface = gpu::gl::Interface::new_native()
            .ok_or_else(|| anyhow!("Skia: no native GL interface (is a GLES context current?)"))?;
        let mut context = gpu::direct_contexts::make_gl(interface, None)
            .ok_or_else(|| anyhow!("Skia: DirectContext over GLES failed"))?;
        context.set_resource_cache_limit(cache_bytes);
        log::info!(
            "console: Skia GL DirectContext, {} MB resource budget",
            cache_bytes >> 20
        );
        Ok(Gpu { context })
    }

    /// A Skia surface over the current window surface's default framebuffer.
    pub(super) fn wrap_window(
        &mut self,
        egl: &EglContext,
        width: u32,
        height: u32,
    ) -> Result<Surface> {
        let fb = gpu::gl::FramebufferInfo {
            fboid: 0,
            format: GL_RGBA8,
            protected: gpu::Protected::No,
        };
        let samples = usize::try_from(egl.samples).unwrap_or(0);
        let stencil = usize::try_from(egl.stencil_bits).unwrap_or(0);
        let target = gpu::backend_render_targets::make_gl(
            (width as i32, height as i32),
            if samples > 1 { Some(samples) } else { None },
            stencil,
            fb,
        );
        gpu::surfaces::wrap_backend_render_target(
            &mut self.context,
            &target,
            SurfaceOrigin::BottomLeft,
            ColorType::RGBA8888,
            None,
            None,
        )
        .ok_or_else(|| anyhow!("Skia: wrap FBO 0 as a surface ({width}×{height})"))
    }
}
