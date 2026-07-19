//! EGL side of the zero-copy path: open a headless EGLDisplay on the NVIDIA GPU (GBM platform on
//! the render node) and import a PipeWire dmabuf as an `EGLImage` with `EGL_LINUX_DMA_BUF_EXT`.
//! The DRM format **modifier** is mandatory on NVIDIA (its buffers are tiled; importing without
//! the modifier yields a corrupt image or `EGL_BAD_MATCH`).
//!
//! Desktop NVIDIA can't register a dmabuf `EGLImage` with CUDA directly — `cuGraphicsEGLRegisterImage`
//! is Tegra-only and `cuGraphicsGLRegisterImage` rejects EGLImage-backed textures (their internal
//! format is opaque). So we follow OBS/Sunshine: bind the `EGLImage` to a GL texture
//! (`glEGLImageTargetTexture2DOES`), render it through a fullscreen-triangle shader into a plain
//! immutable `GL_RGBA8` texture (de-tiling and swizzling to the BGRx the encoder wants), then
//! register *that* texture with CUDA ([`MappedTexture`]) and copy it device-to-device into an
//! owned [`DeviceBuffer`] so the dmabuf can be returned to the compositor immediately.

#![allow(non_upper_case_globals)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::cuda::{self, DeviceBuffer};
use anyhow::{ensure, Context as _, Result};
use khronos_egl as egl;
use std::os::raw::{c_int, c_void};

// EGL_EXT_image_dma_buf_import / _modifiers + platform enums (not defined by khronos-egl).
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

#[path = "egl/gl.rs"]
mod gl;
use gl::*;

/// Per-size GL machinery to blit a dmabuf EGLImage into a CUDA-registrable `GL_RGBA8` texture.
struct GlBlit {
    program: u32,
    vao: u32,
    fbo: u32,
    /// CUDA-registrable destination (immutable GL_RGBA8).
    dst_tex: u32,
    /// Source texture re-targeted to each frame's EGLImage.
    src_tex: u32,
    width: u32,
    height: u32,
    /// `dst_tex` registered with CUDA once (not per frame); mapped+copied each frame.
    registered: cuda::RegisteredTexture,
    /// Recycled CUDA device buffers (the imported frames handed to the encoder).
    pool: cuda::BufferPool,
}

impl GlBlit {
    unsafe fn new(width: u32, height: u32) -> Result<GlBlit> {
        let program = compile_program()?;
        let mut vao = 0u32;
        glGenVertexArrays(1, &mut vao); // core profile needs a bound VAO for glDrawArrays
        let mut fbo = 0u32;
        glGenFramebuffers(1, &mut fbo);

        let mut dst_tex = 0u32;
        glGenTextures(1, &mut dst_tex);
        glBindTexture(GL_TEXTURE_2D, dst_tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width as c_int, height as c_int);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

        let mut src_tex = 0u32;
        glGenTextures(1, &mut src_tex);
        glBindTexture(GL_TEXTURE_2D, src_tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glBindTexture(GL_TEXTURE_2D, 0);

        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glFramebufferTexture2D(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            dst_tex,
            0,
        );
        let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        ensure!(
            status == GL_FRAMEBUFFER_COMPLETE,
            "blit FBO incomplete ({status:#x})"
        );
        // Register the (immutable, reused) destination texture with CUDA once, and stand up the
        // device-buffer pool — both per-resolution, not per-frame. Requires the CUDA context to be
        // current (the caller makes it current before constructing the blit).
        let registered = cuda::RegisteredTexture::register_gl(dst_tex)?;
        let pool = cuda::BufferPool::new(width, height)?;
        Ok(GlBlit {
            program,
            vao,
            fbo,
            dst_tex,
            src_tex,
            width,
            height,
            registered,
            pool,
        })
    }

    /// Bind `image` to the source texture and render it into `dst_tex`.
    ///
    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        let _ = glGetError();
        egl_image_target(GL_TEXTURE_2D, image);
        let e = glGetError();
        glBindTexture(GL_TEXTURE_2D, 0);
        ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");

        glBindFramebuffer(GL_FRAMEBUFFER, self.fbo);
        glViewport(0, 0, self.width as c_int, self.height as c_int);
        glUseProgram(self.program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        glBindVertexArray(self.vao);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glBindVertexArray(0);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glFlush(); // submit GL work before CUDA maps the texture
        Ok(())
    }
}

impl Drop for GlBlit {
    fn drop(&mut self) {
        // Unregister the CUDA graphics resource BEFORE deleting the GL texture it wraps (see
        // `Nv12Blit::drop` — same ordering hazard). Previously `GlBlit` had no `Drop` at all, so
        // its GL objects leaked on every size change and on importer teardown.
        self.registered.release();
        // SAFETY: these GL names were all created by THIS `GlBlit` in `GlBlit::new` on the current
        // GL context, still current here (the owning `EglImporter` drops on its single capture
        // thread and never releases the context). Each `glDelete*` gets a count of 1 and a `&u32`
        // to one live field; the symbols dispatch through libGL to the driver for the current
        // context. Each name is deleted exactly once, after its CUDA registration was released.
        unsafe {
            glDeleteTextures(1, &self.dst_tex);
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(1, &self.fbo);
            glDeleteVertexArrays(1, &self.vao);
            glDeleteProgram(self.program);
        }
    }
}

/// Per-size GL machinery to convert a dmabuf EGLImage into an NV12 (BT.709 limited-range) pair —
/// the [`GlBlit`] analogue for the `PUNKTFUNK_NV12` path. Two passes share `src_tex`: a full-res Y
/// pass into a CUDA-registrable `GL_R8` texture and a half-res UV pass into a `GL_RG8` texture.
/// Feeding NVENC native NV12 deletes its internal RGB→YUV CSC (which otherwise runs on the SM that a
/// saturating game pins at 100%); the convert here replaces the BGRx swizzle [`GlBlit`] did, at ~the
/// same 3D cost.
struct Nv12Blit {
    y_program: u32,
    uv_program: u32,
    vao: u32,
    y_fbo: u32,
    uv_fbo: u32,
    /// CUDA-registrable luma target (immutable `GL_R8`, W×H).
    y_tex: u32,
    /// CUDA-registrable chroma target (immutable `GL_RG8`, W/2 × H/2).
    uv_tex: u32,
    /// Source texture re-targeted to each frame's EGLImage. `GL_LINEAR` so the UV pass averages 2×2.
    src_tex: u32,
    width: u32,
    height: u32,
    y_registered: cuda::RegisteredTexture,
    uv_registered: cuda::RegisteredTexture,
    /// Recycled NV12 device buffers (two-plane) handed to the encoder.
    pool: cuda::BufferPool,
    /// Self-test only: whether `src_tex` has had immutable RGBA8 storage allocated for the upload
    /// path (the live path retargets `src_tex` via EGLImage instead, never allocating storage).
    test_src_storage: bool,
}

impl Nv12Blit {
    unsafe fn new(width: u32, height: u32) -> Result<Nv12Blit> {
        ensure!(
            width % 2 == 0 && height % 2 == 0,
            "NV12 convert needs even dimensions (got {width}x{height})"
        );
        let y_program = compile_program_with(FRAG_Y_SRC)?;
        let uv_program = compile_program_with(FRAG_UV_SRC)?;
        let mut vao = 0u32;
        glGenVertexArrays(1, &mut vao);
        let mut fbos = [0u32; 2];
        glGenFramebuffers(2, fbos.as_mut_ptr());
        let (y_fbo, uv_fbo) = (fbos[0], fbos[1]);

        // Luma target: GL_R8 at full resolution.
        let mut y_tex = 0u32;
        glGenTextures(1, &mut y_tex);
        glBindTexture(GL_TEXTURE_2D, y_tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, width as c_int, height as c_int);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

        // Chroma target: GL_RG8 at half resolution (R=U, G=V).
        let mut uv_tex = 0u32;
        glGenTextures(1, &mut uv_tex);
        glBindTexture(GL_TEXTURE_2D, uv_tex);
        glTexStorage2D(
            GL_TEXTURE_2D,
            1,
            GL_RG8,
            (width / 2) as c_int,
            (height / 2) as c_int,
        );
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

        // Source: GL_LINEAR so the half-res UV pass averages the 2×2 chroma footprint.
        let mut src_tex = 0u32;
        glGenTextures(1, &mut src_tex);
        glBindTexture(GL_TEXTURE_2D, src_tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glBindTexture(GL_TEXTURE_2D, 0);

        for (fbo, tex) in [(y_fbo, y_tex), (uv_fbo, uv_tex)] {
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            ensure!(
                status == GL_FRAMEBUFFER_COMPLETE,
                "NV12 blit FBO incomplete ({status:#x}) — GL_R8/GL_RG8 not renderable?"
            );
        }
        // Register both convert targets with CUDA once (per-resolution), + the NV12 two-plane pool.
        let y_registered = cuda::RegisteredTexture::register_gl(y_tex)?;
        let uv_registered = cuda::RegisteredTexture::register_gl(uv_tex)?;
        let pool = cuda::BufferPool::new_nv12(width, height)?;
        Ok(Nv12Blit {
            y_program,
            uv_program,
            vao,
            y_fbo,
            uv_fbo,
            y_tex,
            uv_tex,
            src_tex,
            width,
            height,
            y_registered,
            uv_registered,
            pool,
            test_src_storage: false,
        })
    }

    /// Bind `image` to the source texture and run both convert passes into `y_tex`/`uv_tex`.
    ///
    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        let _ = glGetError();
        egl_image_target(GL_TEXTURE_2D, image);
        let e = glGetError();
        glBindTexture(GL_TEXTURE_2D, 0);
        ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");
        self.run_passes()
    }

    /// Run the two convert passes from whatever is currently in `src_tex` (caller populated it).
    /// Shared by [`run`](Self::run) (EGLImage source) and the self-test (uploaded RGBA source).
    ///
    /// # Safety: the GL context is current on this thread.
    unsafe fn run_passes(&self) -> Result<()> {
        glActiveTexture(GL_TEXTURE0);
        glBindVertexArray(self.vao);
        // Y pass: full-res into the R8 target.
        glBindFramebuffer(GL_FRAMEBUFFER, self.y_fbo);
        glViewport(0, 0, self.width as c_int, self.height as c_int);
        glUseProgram(self.y_program);
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        // UV pass: half-res into the RG8 target (GL_LINEAR averages the 2×2).
        glBindFramebuffer(GL_FRAMEBUFFER, self.uv_fbo);
        glViewport(0, 0, (self.width / 2) as c_int, (self.height / 2) as c_int);
        glUseProgram(self.uv_program);
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        glDrawArrays(GL_TRIANGLES, 0, 3);

        glBindVertexArray(0);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glFlush(); // submit GL work before CUDA maps the textures
        Ok(())
    }
}

impl Drop for Nv12Blit {
    fn drop(&mut self) {
        // Unregister the CUDA graphics resources BEFORE deleting the GL textures they wrap.
        // `Drop::drop` runs before the fields' own drops, so without this the `glDeleteTextures`
        // below would destroy `y_tex`/`uv_tex` while still CUDA-registered — leaving the driver a
        // registration onto freed GL state (the stale-driver-state class that crashed this path).
        self.y_registered.release();
        self.uv_registered.release();
        // SAFETY: these GL names (textures/FBOs/VAO/programs) were all created by THIS `Nv12Blit`
        // in `Nv12Blit::new` on the current GL context, which is still current because the owning
        // `EglImporter` is dropped on its single capture thread (fields drop before
        // `EglImporter::drop`, which never releases the context). `glDelete*` takes a count + a
        // pointer to that many names: `&self.y_tex`/`&self.vao` are `&u32` to one live field (n=1);
        // `[self.y_fbo, self.uv_fbo].as_ptr()` points at a 2-element temporary that lives for the
        // whole `glDeleteFramebuffers` call (n=2 matches). The symbols dispatch through libGL
        // (libglvnd) to the driver for the current context. Each name is deleted exactly once,
        // after its CUDA registration was released above.
        unsafe {
            glDeleteTextures(1, &self.y_tex);
            glDeleteTextures(1, &self.uv_tex);
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(2, [self.y_fbo, self.uv_fbo].as_ptr());
            glDeleteVertexArrays(1, &self.vao);
            glDeleteProgram(self.y_program);
            glDeleteProgram(self.uv_program);
        }
    }
}

/// Per-size GL machinery to convert a dmabuf EGLImage into planar **YUV444** (BT.709; studio or
/// full range per `PUNKTFUNK_444_FULLRANGE`) — the [`Nv12Blit`] analogue for a 4:4:4 session.
/// Three full-res passes share `src_tex`, each into its own CUDA-registrable `GL_R8` texture
/// (Y/U/V — no subsampling, so no half-res pass and no siting question). The pooled destination
/// is ONE stacked allocation (`BufferPool::new_yuv444`), which keeps the worker↔host wire
/// single-plane. This is what lets a 4:4:4 NVENC session stay zero-copy instead of falling to
/// the CPU swscale path.
struct Yuv444Blit {
    programs: [u32; 3],
    vao: u32,
    fbos: [u32; 3],
    /// CUDA-registrable full-res `GL_R8` targets: Y, U, V.
    texs: [u32; 3],
    /// Source texture re-targeted to each frame's EGLImage.
    src_tex: u32,
    width: u32,
    height: u32,
    registered: [cuda::RegisteredTexture; 3],
    /// Recycled stacked-YUV444 device buffers handed to the encoder.
    pool: cuda::BufferPool,
}

impl Yuv444Blit {
    unsafe fn new(width: u32, height: u32) -> Result<Yuv444Blit> {
        ensure!(
            width % 2 == 0 && height % 2 == 0,
            "YUV444 convert needs even dimensions (got {width}x{height})"
        );
        let full_range = std::env::var("PUNKTFUNK_444_FULLRANGE").is_ok_and(|v| v.trim() == "1");
        let (y_src, u_src, v_src) = yuv444_frag_sources(full_range);
        let programs = [
            compile_program_with(&y_src)?,
            compile_program_with(&u_src)?,
            compile_program_with(&v_src)?,
        ];
        let mut vao = 0u32;
        glGenVertexArrays(1, &mut vao);
        let mut fbos = [0u32; 3];
        glGenFramebuffers(3, fbos.as_mut_ptr());
        let mut texs = [0u32; 3];
        glGenTextures(3, texs.as_mut_ptr());
        for &tex in &texs {
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, width as c_int, height as c_int);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        }
        // Source: LINEAR is exact at the 1:1 mapping every pass uses (texel centres), matching
        // the Nv12Blit source setup.
        let mut src_tex = 0u32;
        glGenTextures(1, &mut src_tex);
        glBindTexture(GL_TEXTURE_2D, src_tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glBindTexture(GL_TEXTURE_2D, 0);
        for (&fbo, &tex) in fbos.iter().zip(&texs) {
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            ensure!(
                status == GL_FRAMEBUFFER_COMPLETE,
                "YUV444 blit FBO incomplete ({status:#x}) — GL_R8 not renderable?"
            );
        }
        let registered = [
            cuda::RegisteredTexture::register_gl(texs[0])?,
            cuda::RegisteredTexture::register_gl(texs[1])?,
            cuda::RegisteredTexture::register_gl(texs[2])?,
        ];
        let pool = cuda::BufferPool::new_yuv444(width, height)?;
        if full_range {
            tracing::info!("YUV444 zero-copy convert: FULL range (PUNKTFUNK_444_FULLRANGE=1)");
        }
        Ok(Yuv444Blit {
            programs,
            vao,
            fbos,
            texs,
            src_tex,
            width,
            height,
            registered,
            pool,
        })
    }

    /// Bind `image` to the source texture and run the three plane passes.
    ///
    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        glBindTexture(GL_TEXTURE_2D, self.src_tex);
        let _ = glGetError();
        egl_image_target(GL_TEXTURE_2D, image);
        let e = glGetError();
        glBindTexture(GL_TEXTURE_2D, 0);
        ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");
        glActiveTexture(GL_TEXTURE0);
        glBindVertexArray(self.vao);
        for (&fbo, &program) in self.fbos.iter().zip(&self.programs) {
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glViewport(0, 0, self.width as c_int, self.height as c_int);
            glUseProgram(program);
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            glDrawArrays(GL_TRIANGLES, 0, 3);
        }
        glBindVertexArray(0);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glFlush(); // submit GL work before CUDA maps the textures
        Ok(())
    }
}

impl Drop for Yuv444Blit {
    fn drop(&mut self) {
        // Unregister the CUDA graphics resources BEFORE deleting the GL textures they wrap
        // (same teardown-order hazard as `Nv12Blit::drop`).
        for r in &mut self.registered {
            r.release();
        }
        // SAFETY: these GL names were all created by THIS `Yuv444Blit` in `new` on the current GL
        // context (still current — the owning `EglImporter` drops on its single capture thread).
        // Each `glDelete*` takes a count + a pointer to that many names; the arrays are live
        // fields (or a live temporary for the whole call). Each name is deleted exactly once,
        // after its CUDA registration was released above.
        unsafe {
            glDeleteTextures(3, self.texs.as_ptr());
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(3, self.fbos.as_ptr());
            glDeleteVertexArrays(1, &self.vao);
            for &p in &self.programs {
                glDeleteProgram(p);
            }
        }
    }
}

/// Which GPU conversion `import_inner` runs on the de-tiled EGLImage — mirrors the three tiled
/// [`super::proto::ImportKind`] entry points.
#[derive(Clone, Copy)]
enum Convert {
    /// BGRx swizzle only ([`GlBlit`]).
    Rgb,
    /// RGB → NV12, BT.709 limited ([`Nv12Blit`]).
    Nv12,
    /// RGB → planar YUV444, BT.709 ([`Yuv444Blit`]) — the 4:4:4 zero-copy path.
    Yuv444,
}

/// One dmabuf plane as delivered by PipeWire (single-plane for BGRx).
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// Headless EGLDisplay (NVIDIA device platform) + a surfaceless desktop-GL context used to
/// import dmabufs and bridge them to CUDA via a GL texture. Lives on the capture thread (the GL
/// context is made current there once).
pub struct EglImporter {
    egl: Egl,
    display: egl::Display,
    no_ctx: egl::Context,
    /// Surfaceless GL context (current on the capture thread) for the EGLImage→texture bind.
    _gl_ctx: egl::Context,
    egl_image_target: EglImageTargetFn,
    /// Lazily-created GL blit machinery (recreated if the frame size changes).
    blit: Option<GlBlit>,
    /// Lazily-created NV12 convert machinery (`PUNKTFUNK_NV12` path; recreated on size change).
    nv12_blit: Option<Nv12Blit>,
    /// Lazily-created planar-YUV444 convert machinery (4:4:4 sessions; recreated on size change).
    yuv444_blit: Option<Yuv444Blit>,
    /// LINEAR-dmabuf path (gamescope): a Vulkan bridge (dmabuf → exportable OPAQUE_FD → CUDA),
    /// created lazily on the first LINEAR frame, + the destination pool.
    vk: Option<super::vulkan::VkBridge>,
    linear_pool: Option<cuda::BufferPool>,
    /// NV12 twin of [`linear_pool`](Self::linear_pool) for the bridge's compute-CSC output
    /// (T2.5b) — separate pools because a session may fall back RGB mid-stream.
    linear_nv12_pool: Option<cuda::BufferPool>,
    gbm: *mut c_void,
    render_fd: c_int,
}

// SAFETY: `EglImporter` owns thread-affine handles — an EGLDisplay/contexts made current on one
// thread, a loaded GL proc pointer, a `gbm_device*`, a raw fd, and CUDA-registered GL textures —
// none safe to touch concurrently. It is constructed inside `pipewire_thread` on the dedicated
// `punktfunk-pipewire` thread, and every method (`import*`, `supported_modifiers`, `Drop`) runs on
// that same thread; it is never accessed through a shared `&` from another thread. `Send` asserts
// only that transferring *ownership* is sound (needed so the importer can live in the PipeWire
// stream's user-data, whose API imposes a `Send` bound) — the live handles are never used
// off-thread. `Sync` is deliberately NOT implied.
unsafe impl Send for EglImporter {}

impl EglImporter {
    /// Open a headless EGLDisplay on the NVIDIA EGL device. Also forces the shared CUDA context
    /// to exist (so a later `import` only touches the hot path).
    pub fn new() -> Result<EglImporter> {
        // GBM platform on the NVIDIA render node: this ties the EGLDisplay (and its GL contexts)
        // to the same DRM device CUDA-GL interop associates with, which the EGL device platform
        // did not (cuGraphicsGLRegisterImage rejected device-platform GL textures).
        let path = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        // SAFETY: `path` is a live local `CString` (built from a string with no interior NUL, so it
        // is NUL-terminated); `path.as_ptr()` is a valid pointer to that buffer which outlives this
        // synchronous `open`. `open` only reads the path and returns a new fd (or -1); it neither
        // retains the pointer nor writes through it, so there is no aliasing or lifetime hazard.
        let render_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        ensure!(render_fd >= 0, "open /dev/dri/renderD128 for GBM");
        // SAFETY: `render_fd` is the live DRM render-node fd just returned by `open` and checked
        // `>= 0`. `gbm_create_device` (libgbm, linked above) builds a `gbm_device` over that fd and
        // returns a `*mut gbm_device` (or null); it borrows but does not take ownership of the fd,
        // which `EglImporter` keeps open and closes only in `Drop` after `gbm_device_destroy`. No
        // Rust-owned memory is passed, so there is nothing to alias.
        let gbm = unsafe { gbm_create_device(render_fd) };
        if gbm.is_null() {
            // SAFETY: reached only when `gbm_create_device` failed (null) — the fd was not consumed
            // and no `EglImporter` exists yet to close it again, so this `close` runs exactly once on
            // the live `render_fd`, releasing it before the error return. No double-close.
            unsafe { libc::close(render_fd) };
            anyhow::bail!("gbm_create_device failed");
        }

        // SAFETY: `Egl::load_required` dlopens the system libEGL and binds its entry points,
        // trusting that libEGL (libglvnd) is a genuine EGL 1.5 implementation whose core symbols
        // match the ABI the `khronos_egl` `EGL1_5` bindings declare. No Rust memory is passed; the
        // returned instance is afterwards used only through the safe `khronos_egl` wrappers.
        let egl: Egl =
            unsafe { Egl::load_required() }.context("load libEGL (EGL 1.5 dynamic instance)")?;
        // SAFETY: `gbm` is the non-null `gbm_device*` created just above (checked), and
        // `EGL_PLATFORM_GBM_KHR` is exactly the platform enum that pairs with a GBM device as the
        // native-display handle, so the `gbm as NativeDisplayType` cast hands EGL a valid native
        // display for the requested platform. `&[egl::ATTRIB_NONE]` is a properly terminated, empty
        // attribute array borrowed for this synchronous call; EGL only reads it and returns an
        // `EGLDisplay`, retaining no pointer into Rust memory.
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm as egl::NativeDisplayType,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("eglGetPlatformDisplay(GBM) on the NVIDIA render node")?;
        egl.initialize(display).context("eglInitialize")?;

        let exts = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .context("query EGL extensions")?
            .to_string_lossy()
            .into_owned();
        ensure!(
            exts.contains("EGL_EXT_image_dma_buf_import"),
            "EGL lacks EGL_EXT_image_dma_buf_import"
        );
        ensure!(
            exts.contains("EGL_EXT_image_dma_buf_import_modifiers"),
            "EGL lacks EGL_EXT_image_dma_buf_import_modifiers (needed for NVIDIA tiled dmabufs)"
        );

        // A surfaceless desktop-GL context so we can bind the dmabuf EGLImage to a GL texture
        // (cuGraphicsEGLRegisterImage is Tegra-only; desktop CUDA interop goes through GL).
        egl.bind_api(egl::OPENGL_API)
            .context("eglBindAPI(OpenGL)")?;
        // The default EGL_SURFACE_TYPE in eglChooseConfig is WINDOW_BIT, which a headless device
        // display has none of — request a pbuffer-capable config (we run surfaceless anyway).
        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_BIT,
                    egl::NONE,
                ],
            )
            .context("eglChooseConfig")?
            .context("no EGL config for OpenGL")?;
        let gl_ctx = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .context("eglCreateContext(OpenGL)")?;
        egl.make_current(display, None, None, Some(gl_ctx))
            .context("eglMakeCurrent surfaceless (needs EGL_KHR_surfaceless_context)")?;
        // SAFETY: the GL context was made current on this thread just above, which `eglGetProcAddress`
        // requires to return a usable pointer. The non-null (`?`-checked) pointer it returns for
        // "glEGLImageTargetTexture2DOES" is the driver's implementation of that GL-OES entry point,
        // whose real ABI is `void(GLenum, GLeglImageOES)` = `(u32, *mut c_void)` `extern "system"`.
        // `EglImageTargetFn` is declared with exactly that signature, so the transmute only retypes a
        // same-size, same-ABI thin function pointer (no value/representation change). The function is
        // present because `EGL_EXT_image_dma_buf_import` was asserted on this display above.
        let egl_image_target: EglImageTargetFn = unsafe {
            std::mem::transmute(
                egl.get_proc_address("glEGLImageTargetTexture2DOES")
                    .context("glEGLImageTargetTexture2DOES unavailable")?,
            )
        };

        // Create the shared CUDA context up front so import() is pure hot path.
        cuda::context().context("create CUDA context")?;

        // SAFETY: `egl::NO_CONTEXT` is EGL's defined sentinel (a null handle) for "no context";
        // `Context::from_ptr` only stores the handle (it never dereferences it), so wrapping the
        // null sentinel is sound and yields exactly the `EGL_NO_CONTEXT` value that
        // `eglCreateImage(EGL_LINUX_DMA_BUF_EXT)` requires as its context argument later.
        let no_ctx = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        tracing::info!(
            "zero-copy EGL importer ready (GBM platform + GL texture interop, dma_buf_import + modifiers)"
        );
        Ok(EglImporter {
            egl,
            display,
            no_ctx,
            _gl_ctx: gl_ctx,
            egl_image_target,
            blit: None,
            nv12_blit: None,
            yuv444_blit: None,
            vk: None,
            linear_pool: None,
            linear_nv12_pool: None,
            gbm,
            render_fd,
        })
    }

    /// Import a LINEAR dmabuf via the Vulkan bridge (no EGL/GL involved — NVIDIA's EGL can't
    /// sample LINEAR, and the CUDA driver rejects raw dmabuf fds; Vulkan imports the dmabuf,
    /// GPU-copies into an exportable allocation, and CUDA reads that). See [`super::vulkan`].
    pub fn import_linear(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.linear_pool.as_ref().map(|p| (p.width(), p.height())) != Some((width, height)) {
            self.linear_pool = Some(cuda::BufferPool::new(width, height)?);
        }
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        self.vk.as_mut().unwrap().import_linear(
            plane.fd,
            plane.offset,
            plane.stride,
            height,
            self.linear_pool.as_ref().unwrap(),
        )
    }

    /// Like [`import_linear`](Self::import_linear), but the bridge's compute CSC converts to a
    /// two-plane **NV12** buffer (latency plan T2.5b) — the gamescope/LINEAR analogue of
    /// [`import_nv12`](Self::import_nv12), so NVENC encodes native YUV on the dedicated-session
    /// path too instead of paying its internal RGB→YUV CSC on the contended SM.
    pub fn import_linear_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        // Even dimensions only: the UV copy walks `height.div_ceil(2)` chroma rows (the correct NV12
        // count), but the pooled UV plane is sized at `height/2` rows — for an odd height those
        // disagree by one row and the copy writes a full `uv_pitch` past the allocation (OOB device
        // write / CUDA_ERROR_ILLEGAL_ADDRESS that poisons the shared context). Reject here, matching
        // the guards `Nv12Blit::new`/`Yuv444Blit::new` already carry.
        anyhow::ensure!(
            width % 2 == 0 && height % 2 == 0,
            "LINEAR NV12 needs even dimensions (got {width}x{height})"
        );
        cuda::make_current()?;
        if self
            .linear_nv12_pool
            .as_ref()
            .map(|p| (p.width(), p.height()))
            != Some((width, height))
        {
            self.linear_nv12_pool = Some(cuda::BufferPool::new_nv12(width, height)?);
        }
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        self.vk.as_mut().unwrap().import_linear_nv12(
            plane.fd,
            plane.offset,
            plane.stride,
            width,
            height,
            self.linear_nv12_pool.as_ref().unwrap(),
        )
    }

    /// Drop the Vulkan bridge's cached per-fd import (see [`super::vulkan::VkBridge::forget_fd`]).
    /// No-op when the bridge hasn't been built (tiled-only captures).
    pub fn forget_linear_fd(&mut self, fd: i32) {
        if let Some(vk) = self.vk.as_mut() {
            vk.forget_fd(fd);
        }
    }

    /// Tear down the whole LINEAR-path import cache (the Vulkan bridge and every per-fd source
    /// buffer in it). Called when the PipeWire stream renegotiates — the buffer pool the cache
    /// keyed on is gone, and a recycled fd number must never resolve to a stale import. The
    /// bridge lazily rebuilds on the next LINEAR frame (renegotiations are rare).
    pub fn clear_linear_cache(&mut self) {
        self.vk = None;
    }

    /// The DRM format modifiers the NVIDIA EGL stack can import for `fourcc`, via
    /// `eglQueryDmaBufModifiersEXT`. We advertise these to PipeWire so the compositor allocates
    /// a dmabuf in a layout we can import. Empty on failure (caller falls back).
    pub fn supported_modifiers(&self, fourcc: u32) -> Vec<u64> {
        type QueryFn = unsafe extern "system" fn(
            dpy: *mut c_void,
            format: i32,
            max_modifiers: i32,
            modifiers: *mut u64,
            external_only: *mut u32,
            num_modifiers: *mut i32,
        ) -> u32;
        let Some(sym) = self.egl.get_proc_address("eglQueryDmaBufModifiersEXT") else {
            return Vec::new();
        };
        // SAFETY: `sym` is the non-null pointer `eglGetProcAddress("eglQueryDmaBufModifiersEXT")`
        // returned (the `let-else` already bailed on `None`) — the driver's implementation of that
        // EGL extension entry point. `QueryFn` is declared with that function's exact documented ABI
        // (`EGLDisplay, EGLint, EGLint, EGLuint64* , EGLBoolean*, EGLint* -> EGLBoolean`), all
        // `extern "system"`, so the transmute only retypes a same-size, same-ABI thin fn pointer.
        let query: QueryFn = unsafe { std::mem::transmute(sym) };
        let dpy = self.display.as_ptr();
        // SAFETY: `dpy` is this importer's live, initialized `EGLDisplay`; `query` is the proc loaded
        // just above. The first call passes null out-arrays with `max_modifiers == 0`, which the
        // extension defines as "write only the count" — it writes solely through `&mut count` (a live
        // local `i32`). For the second call, `mods`/`ext` are freshly allocated `Vec`s of exactly
        // `count` elements and `max_modifiers == count`, so the driver writes at most `count`
        // `u64`/`u32` entries (in bounds) plus the actual count through `&mut n` (a live local). All
        // four Rust addresses outlive these synchronous calls and alias nothing else. `truncate` only
        // shrinks, so even a misbehaving `n > count` cannot read out of bounds.
        unsafe {
            let mut count: i32 = 0;
            if query(
                dpy,
                fourcc as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
            ) == 0
                || count <= 0
            {
                return Vec::new();
            }
            let mut mods = vec![0u64; count as usize];
            let mut ext = vec![0u32; count as usize];
            let mut n: i32 = 0;
            if query(
                dpy,
                fourcc as i32,
                count,
                mods.as_mut_ptr(),
                ext.as_mut_ptr(),
                &mut n,
            ) == 0
            {
                return Vec::new();
            }
            mods.truncate(n.max(0) as usize);
            mods
        }
    }

    /// Import one dmabuf and copy it device-to-device into a fresh owned CUDA buffer. `fourcc`
    /// is the DRM FourCC; `modifier` is the explicit 64-bit DRM format modifier when one was
    /// negotiated, or `None` to import with the buffer's implicit modifier (base
    /// `EGL_EXT_image_dma_buf_import`, which the NVIDIA driver resolves for its own buffers).
    pub fn import(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Rgb)
    }

    /// Like [`import`](Self::import), but de-tiles **and converts** the dmabuf to NV12 (BT.709
    /// limited range) on the GPU — the `PUNKTFUNK_NV12` path — so NVENC can encode native YUV with
    /// no internal RGB→YUV CSC. The returned [`DeviceBuffer`] carries both NV12 planes
    /// (`DeviceBuffer::is_nv12`). Only the tiled EGL/GL path supports this (LINEAR/Vulkan stays RGB).
    pub fn import_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Nv12)
    }

    /// Like [`import_nv12`](Self::import_nv12), but converts to planar **YUV444** (full chroma —
    /// a 4:4:4 session) into one stacked 3-plane [`DeviceBuffer`] (`DeviceBuffer::yuv444`). Only
    /// the tiled EGL/GL path supports this.
    pub fn import_yuv444(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Yuv444)
    }

    fn import_inner(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        convert: Convert,
    ) -> Result<DeviceBuffer> {
        let mut attrs: Vec<egl::Attrib> = vec![
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.stride as egl::Attrib,
        ];
        if let Some(m) = modifier {
            attrs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (m & 0xFFFF_FFFF) as egl::Attrib,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (m >> 32) as egl::Attrib,
            ]);
        }
        attrs.push(egl::ATTRIB_NONE);
        // SAFETY: `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)` mandates a NULL `EGLClientBuffer`
        // (the source is described entirely by the attribute list built above), so wrapping
        // `null_mut()` is the required value. `from_ptr` only stores the pointer without
        // dereferencing it, so constructing it from null is sound.
        let client = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                self.no_ctx,
                EGL_LINUX_DMA_BUF_EXT,
                client,
                &attrs,
            )
            .context("eglCreateImage(EGL_LINUX_DMA_BUF_EXT) — modifier mismatch?")?;

        // EGLImage → (sampled by a shader) → GL_RGBA8 texture (or NV12 R8+RG8 pair, or the three
        // YUV444 R8 planes) → register *that* with CUDA → map → array → copy out. Registering the
        // EGLImage texture directly fails (its layout isn't a CUDA-registrable format); the
        // render targets are.
        let result = match convert {
            Convert::Nv12 => self.blit_and_copy_nv12(image.as_ptr(), width, height),
            Convert::Yuv444 => self.blit_and_copy_yuv444(image.as_ptr(), width, height),
            Convert::Rgb => self.blit_and_copy(image.as_ptr(), width, height),
        };
        let _ = self.egl.destroy_image(self.display, image);
        result
    }

    /// Render the dmabuf `image` into the registrable RGBA8 texture and copy it to an owned CUDA
    /// buffer. (Re)creates the per-size GL blit machinery as needed.
    fn blit_and_copy(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `GlBlit::new` requires the GL context current on the calling thread and a
            // current CUDA context. Both hold: this runs on the capture thread where
            // `EglImporter::new` made the GL context current and never released it, and
            // `cuda::make_current()?` ran at the top of this function. `width`/`height` are plain
            // `Copy` frame dimensions.
            self.blit = Some(unsafe { GlBlit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.blit.as_mut().unwrap();
        // SAFETY: `GlBlit::run` requires a current GL context and a valid `EGLImage`. The GL context
        // is current on this capture thread (made current in `EglImporter::new`, never released) and
        // `cuda::make_current()` ran above; `egl_image_target` is the `glEGLImageTargetTexture2DOES`
        // pointer loaded in `new`; `image` is the raw handle of the live `EGLImage` that
        // `import_inner` created with `eglCreateImage` and destroys only AFTER this call returns, so
        // it stays valid for the whole synchronous `run`.
        unsafe { blit.run(egl_image_target, image)? };
        // Persistent registration (mapped per frame) + a pooled buffer — no per-frame
        // cuGraphicsGLRegisterImage / cuMemAllocPitch.
        let dst = blit.pool.get()?;
        blit.registered.copy_mapped_to(&dst)?;
        Ok(dst)
    }

    /// Convert the dmabuf `image` to NV12 (Y in an R8 texture, UV in an RG8 texture) and copy both
    /// planes into a pooled NV12 [`DeviceBuffer`]. (Re)creates the per-size convert machinery as
    /// needed. The `PUNKTFUNK_NV12` analogue of [`blit_and_copy`].
    fn blit_and_copy_nv12(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.nv12_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Nv12Blit::new` requires the GL context current on the calling thread and a
            // current CUDA context. Both hold: this runs on the capture thread where
            // `EglImporter::new` made the GL context current and never released it, and
            // `cuda::make_current()?` ran at the top of this function. `width`/`height` are plain
            // `Copy` frame dimensions.
            self.nv12_blit = Some(unsafe { Nv12Blit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.nv12_blit.as_mut().unwrap();
        // SAFETY: `Nv12Blit::run` requires a current GL context and a valid `EGLImage`. The GL
        // context is current on this capture thread (made current in `EglImporter::new`, never
        // released) and `cuda::make_current()` ran above; `egl_image_target` is the
        // `glEGLImageTargetTexture2DOES` pointer loaded in `new`; `image` is the raw handle of the
        // live `EGLImage` that `import_inner` created with `eglCreateImage` and destroys only AFTER
        // this call returns, so it stays valid for the whole synchronous `run`.
        unsafe { blit.run(egl_image_target, image)? };
        let dst = blit.pool.get()?;
        cuda::copy_mapped_nv12(&mut blit.y_registered, &mut blit.uv_registered, &dst)?;
        Ok(dst)
    }

    /// Convert the dmabuf `image` to planar YUV444 (three full-res `R8` textures) and copy the
    /// planes into a pooled stacked [`DeviceBuffer`]. (Re)creates the per-size convert machinery
    /// as needed — the 4:4:4 analogue of [`blit_and_copy_nv12`](Self::blit_and_copy_nv12).
    fn blit_and_copy_yuv444(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.yuv444_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Yuv444Blit::new` requires the GL context current on the calling thread and
            // a current CUDA context. Both hold: this runs on the capture thread where
            // `EglImporter::new` made the GL context current and never released it, and
            // `cuda::make_current()?` ran at the top of this function. `width`/`height` are plain
            // `Copy` frame dimensions.
            self.yuv444_blit = Some(unsafe { Yuv444Blit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.yuv444_blit.as_mut().unwrap();
        // SAFETY: `Yuv444Blit::run` requires a current GL context and a valid `EGLImage`. The GL
        // context is current on this capture thread (made current in `EglImporter::new`, never
        // released) and `cuda::make_current()` ran above; `egl_image_target` is the
        // `glEGLImageTargetTexture2DOES` pointer loaded in `new`; `image` is the raw handle of the
        // live `EGLImage` that `import_inner` created with `eglCreateImage` and destroys only
        // AFTER this call returns, so it stays valid for the whole synchronous `run`.
        unsafe { blit.run(egl_image_target, image)? };
        let dst = blit.pool.get()?;
        let [y, u, v] = &mut blit.registered;
        cuda::copy_mapped_yuv444(y, u, v, &dst)?;
        Ok(dst)
    }

    /// Self-test entry: upload a packed `width`×`height` RGBA8 host pattern into a GL texture, run
    /// the NV12 convert passes on the GPU, and copy both planes into a pooled NV12 [`DeviceBuffer`].
    /// Exercises the exact shaders + CUDA copy the live path uses, but sourced from an uploaded
    /// texture instead of a dmabuf EGLImage (no compositor needed). `rgba` is tightly packed, 4 B/px.
    pub fn convert_rgba_for_test(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        anyhow::ensure!(
            rgba.len() == width as usize * height as usize * 4,
            "test RGBA buffer {} bytes != {}x{}x4",
            rgba.len(),
            width,
            height
        );
        cuda::make_current()?;
        if self.nv12_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Nv12Blit::new` requires the GL context current on the calling thread and a
            // current CUDA context. Both hold: this self-test path runs on the thread that owns this
            // `EglImporter` with its GL context current, and `cuda::make_current()?` ran just above.
            // `width`/`height` are plain `Copy` scalars.
            self.nv12_blit = Some(unsafe { Nv12Blit::new(width, height)? });
        }
        let blit = self.nv12_blit.as_mut().unwrap();
        // SAFETY: runs on the thread that owns this `EglImporter` with its GL context current.
        // `blit.src_tex` is a texture this `Nv12Blit` owns; `glTexStorage2D` allocates immutable
        // RGBA8 storage exactly once (guarded by `test_src_storage`) sized `width×height`.
        // `glTexSubImage2D` then uploads exactly `width×height` RGBA8 texels, reading `width*height*4`
        // bytes from `rgba.as_ptr()`; the caller already asserted `rgba.len() == width*height*4`, rows
        // are `width*4` bytes (a multiple of the default 4-byte unpack alignment, so no row-padding
        // over-read), and `rgba` is a live borrow that outlives this synchronous upload. `run_passes`
        // then needs only the current GL context (no further Rust pointers). All GL names are this
        // blit's own, alias no other live object, and nothing is retained past the calls.
        unsafe {
            // Upload the host RGBA into `src_tex` (an immutable GL_RGBA8 backing must exist first;
            // the live path never allocates it — it retargets `src_tex` via EGLImage instead).
            glBindTexture(GL_TEXTURE_2D, blit.src_tex);
            if !blit.test_src_storage {
                glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width as c_int, height as c_int);
                blit.test_src_storage = true;
            }
            let _ = glGetError();
            glTexSubImage2D(
                GL_TEXTURE_2D,
                0,
                0,
                0,
                width as c_int,
                height as c_int,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                rgba.as_ptr() as *const c_void,
            );
            let e = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            ensure!(e == 0, "glTexSubImage2D(test source) failed ({e:#x})");
            blit.run_passes()?;
        }
        let dst = blit.pool.get()?;
        cuda::copy_mapped_nv12(&mut blit.y_registered, &mut blit.uv_registered, &dst)?;
        Ok(dst)
    }
}

impl Drop for EglImporter {
    fn drop(&mut self) {
        if !self.gbm.is_null() {
            // SAFETY: `self.gbm` is the non-null `gbm_device*` from `gbm_create_device` in `new`
            // (checked non-null here), owned exclusively by this `EglImporter` and destroyed exactly
            // once (in `Drop`). It is freed BEFORE `render_fd` is closed below — the correct order,
            // since the device borrowed that fd for its lifetime.
            unsafe { gbm_device_destroy(self.gbm) };
        }
        if self.render_fd >= 0 {
            // SAFETY: `self.render_fd` is the fd `open` returned in `new` (checked `>= 0`), owned
            // exclusively by this `EglImporter`; this `close` runs exactly once, after the gbm device
            // that borrowed it has been destroyed. No double-close or use-after-close.
            unsafe { libc::close(self.render_fd) };
        }
    }
}
