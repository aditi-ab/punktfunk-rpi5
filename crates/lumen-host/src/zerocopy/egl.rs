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

use super::cuda::{self, DeviceBuffer};
use anyhow::{bail, ensure, Context as _, Result};
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

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_LINEAR: c_int = 0x2601;
const GL_NEAREST: c_int = 0x2600;
const GL_RGBA8: u32 = 0x8058;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TRIANGLES: u32 = 0x0004;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;

// libglvnd's libGL dispatches these to the NVIDIA driver based on the current EGL/GL context.
#[link(name = "GL")]
extern "C" {
    fn glGenTextures(n: c_int, textures: *mut u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexParameteri(target: u32, pname: u32, param: c_int);
    fn glDeleteTextures(n: c_int, textures: *const u32);
    fn glTexStorage2D(target: u32, levels: c_int, internalformat: u32, width: c_int, height: c_int);
    fn glGetError() -> u32;
    fn glGenFramebuffers(n: c_int, framebuffers: *mut u32);
    fn glBindFramebuffer(target: u32, framebuffer: u32);
    fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: c_int,
    );
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int);
    fn glGenVertexArrays(n: c_int, arrays: *mut u32);
    fn glBindVertexArray(array: u32);
    fn glDrawArrays(mode: u32, first: c_int, count: c_int);
    fn glActiveTexture(texture: u32);
    fn glUseProgram(program: u32);
    fn glFlush();
    fn glCreateShader(shader_type: u32) -> u32;
    fn glShaderSource(shader: u32, count: c_int, string: *const *const i8, length: *const c_int);
    fn glCompileShader(shader: u32);
    fn glGetShaderiv(shader: u32, pname: u32, params: *mut c_int);
    fn glDeleteShader(shader: u32);
    fn glCreateProgram() -> u32;
    fn glAttachShader(program: u32, shader: u32);
    fn glLinkProgram(program: u32);
    fn glGetProgramiv(program: u32, pname: u32, params: *mut c_int);
    fn glGetUniformLocation(program: u32, name: *const i8) -> c_int;
    fn glUniform1i(location: c_int, v0: c_int);
}

#[link(name = "gbm")]
extern "C" {
    fn gbm_create_device(fd: c_int) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
}

/// `glEGLImageTargetTexture2DOES(target, EGLImage)` — loaded via `eglGetProcAddress`.
type EglImageTargetFn = unsafe extern "system" fn(u32, *mut c_void);

// Fullscreen-triangle blit: sample the dmabuf EGLImage texture and write it (swizzled to BGRA,
// to match the BGRx the encoder expects) into a normal GL_RGBA8 texture that CUDA *can* register.
const VERT_SRC: &[u8] = b"#version 330 core\nout vec2 v_tex;\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));v_tex=p;gl_Position=vec4(p*2.0-1.0,0.0,1.0);}\n";
const FRAG_SRC: &[u8] = b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){o_color=texture(image,v_tex).bgra;}\n";

unsafe fn compile_shader(kind: u32, src: &[u8]) -> Result<u32> {
    let sh = glCreateShader(kind);
    ensure!(sh != 0, "glCreateShader failed");
    let ptr = src.as_ptr() as *const i8;
    let len = src.len() as c_int;
    glShaderSource(sh, 1, &ptr, &len);
    glCompileShader(sh);
    let mut ok: c_int = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &mut ok);
    if ok == 0 {
        glDeleteShader(sh);
        bail!("GL shader compile failed");
    }
    Ok(sh)
}

unsafe fn compile_program() -> Result<u32> {
    let vs = compile_shader(GL_VERTEX_SHADER, VERT_SRC)?;
    let fs = compile_shader(GL_FRAGMENT_SHADER, FRAG_SRC)?;
    let prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
    let mut ok: c_int = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &mut ok);
    ensure!(ok != 0, "GL program link failed");
    glUseProgram(prog);
    let loc = glGetUniformLocation(prog, c"image".as_ptr());
    if loc >= 0 {
        glUniform1i(loc, 0); // sampler -> texture unit 0
    }
    glUseProgram(0);
    Ok(prog)
}

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
    /// LINEAR-dmabuf path (gamescope): CUDA external-memory imports cached per buffer fd (the
    /// producer's buffer pool keeps fds stable for the stream's life) + the destination pool.
    linear: std::collections::HashMap<i32, cuda::ExternalDmabuf>,
    linear_pool: Option<cuda::BufferPool>,
    gbm: *mut c_void,
    render_fd: c_int,
}

// The EGL handles are confined to the capture thread; the struct is moved there once.
unsafe impl Send for EglImporter {}

impl EglImporter {
    /// Open a headless EGLDisplay on the NVIDIA EGL device. Also forces the shared CUDA context
    /// to exist (so a later `import` only touches the hot path).
    pub fn new() -> Result<EglImporter> {
        // GBM platform on the NVIDIA render node: this ties the EGLDisplay (and its GL contexts)
        // to the same DRM device CUDA-GL interop associates with, which the EGL device platform
        // did not (cuGraphicsGLRegisterImage rejected device-platform GL textures).
        let path = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let render_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        ensure!(render_fd >= 0, "open /dev/dri/renderD128 for GBM");
        let gbm = unsafe { gbm_create_device(render_fd) };
        if gbm.is_null() {
            unsafe { libc::close(render_fd) };
            anyhow::bail!("gbm_create_device failed");
        }

        let egl: Egl =
            unsafe { Egl::load_required() }.context("load libEGL (EGL 1.5 dynamic instance)")?;
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
        let egl_image_target: EglImageTargetFn = unsafe {
            std::mem::transmute(
                egl.get_proc_address("glEGLImageTargetTexture2DOES")
                    .context("glEGLImageTargetTexture2DOES unavailable")?,
            )
        };

        // Create the shared CUDA context up front so import() is pure hot path.
        cuda::context().context("create CUDA context")?;

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
            linear: std::collections::HashMap::new(),
            linear_pool: None,
            gbm,
            render_fd,
        })
    }

    /// Import a LINEAR dmabuf via CUDA external memory (no EGL/GL involved — NVIDIA's EGL can't
    /// sample LINEAR, but the bytes are directly addressable once imported). The import is
    /// cached per fd; per frame this is one device→device copy into a pooled buffer.
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
        let fd = plane.fd;
        let ext = match self.linear.entry(fd) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // Size from the fd itself (the chunk's size field is unreliable for dmabufs).
                let size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
                anyhow::ensure!(size > 0, "lseek(dmabuf) failed");
                e.insert(cuda::ExternalDmabuf::import(fd, size as u64)?)
            }
        };
        let dst = self.linear_pool.as_ref().unwrap().get()?;
        cuda::copy_pitched_to_buffer(ext.ptr + plane.offset as u64, plane.stride as usize, &dst)?;
        Ok(dst)
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
        let query: QueryFn = unsafe { std::mem::transmute(sym) };
        let dpy = self.display.as_ptr();
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

        // EGLImage → (sampled by a shader) → GL_RGBA8 texture → register *that* with CUDA → map
        // → array → copy out. Registering the EGLImage texture directly fails (its layout isn't a
        // CUDA-registrable format); the RGBA8 render target is.
        let result = self.blit_and_copy(image.as_ptr(), width, height);
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
            self.blit = Some(unsafe { GlBlit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.blit.as_mut().unwrap();
        // SAFETY: GL + CUDA contexts current on this thread; `image` is a valid EGLImage.
        unsafe { blit.run(egl_image_target, image)? };
        // Persistent registration (mapped per frame) + a pooled buffer — no per-frame
        // cuGraphicsGLRegisterImage / cuMemAllocPitch.
        let dst = blit.pool.get()?;
        blit.registered.copy_mapped_to(&dst)?;
        Ok(dst)
    }
}

impl Drop for EglImporter {
    fn drop(&mut self) {
        if !self.gbm.is_null() {
            unsafe { gbm_device_destroy(self.gbm) };
        }
        if self.render_fd >= 0 {
            unsafe { libc::close(self.render_fd) };
        }
    }
}
