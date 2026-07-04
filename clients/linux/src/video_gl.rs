//! VAAPI dmabuf → RGBA GL texture converter — the Steam Deck's hardware-decode presenter.
//!
//! The direct path hands the decoder's NV12 dmabuf (fds + AMD tiled modifier) to
//! `GdkDmabufTexture` and lets GTK import + color-convert it. On the Deck that renders
//! corrupt/gray/washed-out: since Mesa 25.1 radeonsi exports VCN decode surfaces TILED, and
//! GTK's tiled-NV12 import mishandles the layout (the Flatpak runtime's Mesa drives both
//! sides). Moonlight-qt and mpv are clean on the same box because they never let a toolkit
//! near the YUV: they import the dmabuf into their own EGL context and convert with their
//! own shader. This module is that architecture for the GTK client:
//!
//!   VAAPI frame → per-plane `EGLImage`s (R8 luma + GR88 chroma, modifier passed through)
//!   → our YUV→RGB shader (matrix + range from the stream's real CICP signaling)
//!   → an RGBA texture in a `GdkGLContext`-shared context → `GdkGLTexture` (fence-synced).
//!
//! GTK then composites a plain RGBA texture — no YUV format negotiation, no modifier
//! handling, no compositor CSC. Same-Mesa export/import is the exact proven-working path.
//! Everything runs on the GTK main thread (the converter is driven by the frame consumer);
//! one 800p–4K NV12→RGB pass is sub-millisecond GPU work.
//!
//! Failure at any step (GLX-backed GDK context, missing EGL extensions, import rejection)
//! is surfaced as an error — the caller falls back to software decode, never to the broken
//! direct path.

use crate::video::{ColorDesc, DmabufFrame};
use anyhow::{anyhow, bail, Context as _, Result};
use gtk::{gdk, prelude::*};
use khronos_egl as egl;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

// --- EGL_EXT_image_dma_buf_import(+_modifiers) constants (khronos-egl exposes none) ------
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
// eglCreateImageKHR takes 32-bit EGLint attribs (the core-1.5 eglCreateImage variant is the
// one with pointer-sized EGLAttrib) — using the wrong width feeds the driver garbage pairs.
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NONE: i32 = 0x3038;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// `fourcc('N','V','1','2')` — the only decoder output today (8-bit 4:2:0). P010 joins when
/// the Linux host grows 10-bit.
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
const DRM_FORMAT_R8: u32 = 0x2020_3852;
const DRM_FORMAT_GR88: u32 = 0x3838_5247;

// --- The slice of GL we use (loaded via eglGetProcAddress — Mesa/NVIDIA both implement
// --- EGL_KHR_get_all_proc_addresses, so core functions resolve too) ----------------------
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_RGBA8: u32 = 0x8058;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TRIANGLES: u32 = 0x0004;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;

macro_rules! gl_fns {
    ($($name:ident : fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct GlFns { $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)* }
        impl GlFns {
            #[allow(non_snake_case)]
            fn load(egl: &Egl) -> Result<GlFns> {
                $(
                    // eglGetProcAddress returns a plain fn pointer; the signature is fixed
                    // by the GL spec for each name.
                    let $name = egl
                        .get_proc_address(concat!("gl", stringify!($name)))
                        .ok_or_else(|| anyhow!(concat!("gl", stringify!($name), " unresolvable")))?;
                )*
                // SAFETY: each pointer came from eglGetProcAddress for exactly that GL entry
                // point; the transmute only fixes the signature the spec defines for it.
                unsafe {
                    Ok(GlFns { $($name: std::mem::transmute::<extern "system" fn(), unsafe extern "C" fn($($arg),*) $(-> $ret)?>($name),)* })
                }
            }
        }
    };
}

gl_fns! {
    GenTextures: fn(i32, *mut u32);
    DeleteTextures: fn(i32, *const u32);
    BindTexture: fn(u32, u32);
    TexParameteri: fn(u32, u32, i32);
    TexImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    ActiveTexture: fn(u32);
    EGLImageTargetTexture2DOES: fn(u32, *const c_void);
    GenFramebuffers: fn(i32, *mut u32);
    DeleteFramebuffers: fn(i32, *const u32);
    BindFramebuffer: fn(u32, u32);
    FramebufferTexture2D: fn(u32, u32, u32, u32, i32);
    CheckFramebufferStatus: fn(u32) -> u32;
    Viewport: fn(i32, i32, i32, i32);
    CreateShader: fn(u32) -> u32;
    ShaderSource: fn(u32, i32, *const *const u8, *const i32);
    CompileShader: fn(u32);
    GetShaderiv: fn(u32, u32, *mut i32);
    GetShaderInfoLog: fn(u32, i32, *mut i32, *mut u8);
    DeleteShader: fn(u32);
    CreateProgram: fn() -> u32;
    AttachShader: fn(u32, u32);
    LinkProgram: fn(u32);
    GetProgramiv: fn(u32, u32, *mut i32);
    UseProgram: fn(u32);
    GetUniformLocation: fn(u32, *const u8) -> i32;
    Uniform1i: fn(i32, i32);
    Uniform3fv: fn(i32, i32, *const f32);
    UniformMatrix3fv: fn(i32, i32, u8, *const f32);
    GenVertexArrays: fn(i32, *mut u32);
    DeleteVertexArrays: fn(i32, *const u32);
    DeleteProgram: fn(u32);
    BindVertexArray: fn(u32);
    DrawArrays: fn(u32, i32, i32);
    FenceSync: fn(u32, u32) -> *const c_void;
    DeleteSync: fn(*const c_void);
    Flush: fn();
    GetError: fn() -> u32;
}

type Egl = egl::DynamicInstance<egl::EGL1_4>;
type EglCreateImageKhr = unsafe extern "C" fn(
    *mut c_void, // EGLDisplay
    *mut c_void, // EGLContext (EGL_NO_CONTEXT for dmabuf)
    egl::Enum,
    *mut c_void, // EGLClientBuffer (null for dmabuf)
    *const i32,  // EGLint attrib list (KHR variant — NOT pointer-sized EGLAttrib)
) -> *const c_void;
type EglDestroyImageKhr = unsafe extern "C" fn(*mut c_void, *const c_void) -> egl::Boolean;

/// The YUV→RGB conversion for a stream's CICP signaling: `rgb = mat * (yuv + off)`, with the
/// limited/full-range expansion folded in. `mat` is column-major (GL convention). Pure —
/// unit-tested against the reference white/black points.
pub fn yuv_to_rgb(desc: ColorDesc) -> ([f32; 9], [f32; 3]) {
    // BT.601 (5/6), BT.2020 (9/10); everything else — incl. unspecified — is the host's
    // BT.709 SDR default (mirrors the software path's swscale coefficient choice).
    let (kr, kb) = match desc.matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let (sy, oy, sc) = if desc.full_range {
        (1.0f32, 0.0f32, 1.0f32)
    } else {
        (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
    };
    let (kr, kb, kg) = (kr as f32, kb as f32, kg as f32);
    // Column-major: columns are the Y, U, V contributions to (R, G, B).
    let mat = [
        sy,
        sy,
        sy, // Y column
        0.0,
        -2.0 * (1.0 - kb) * kb / kg * sc,
        2.0 * (1.0 - kb) * sc, // U column
        2.0 * (1.0 - kr) * sc,
        -2.0 * (1.0 - kr) * kr / kg * sc,
        0.0, // V column
    ];
    (mat, [oy, -0.5, -0.5])
}

/// An output texture GTK has released, waiting to be recycled (or its fence deleted). GL
/// objects can only be touched with our context current, so releases park here and
/// [`GlConverter::convert`] drains them.
struct Retired {
    tex: u32,
    sync: usize, // GLsync as usize — the release closure must be Send
    size: (u32, u32),
}

pub struct GlConverter {
    ctx: gdk::GLContext,
    egl: Egl,
    egl_display: *mut c_void,
    create_image: EglCreateImageKhr,
    destroy_image: EglDestroyImageKhr,
    gl: GlFns,
    program: u32,
    vao: u32,
    fbo: u32,
    u_mat: i32,
    u_off: i32,
    /// Uniforms match this signaling; a change (mid-stream SDR↔HDR) re-uploads them.
    uniforms_for: Option<ColorDesc>,
    /// Free output textures + fences returned by GTK's release funcs (shared with the
    /// `Send` release closures; drained/recycled at each convert).
    retired: Arc<Mutex<Vec<Retired>>>,
}

impl GlConverter {
    /// Build against the widget's display. Must run on the GTK main thread; fails cleanly
    /// on a GLX-backed GDK context or missing EGL dmabuf-import extensions (the caller
    /// falls back to software decode).
    pub fn new(widget: &impl IsA<gtk::Widget>) -> Result<GlConverter> {
        let display = widget.display();
        let ctx = display.create_gl_context().context("create GdkGLContext")?;
        ctx.realize().context("realize GdkGLContext")?;
        ctx.make_current();

        // SAFETY (whole block): the GdkGLContext is current on this thread, so EGL/GL
        // queries and object creation target it; pointers are only used while it lives.
        unsafe {
            let egl = Egl::load_required().context("dlopen libEGL")?;
            let egl_display = egl
                .get_current_display()
                .ok_or_else(|| anyhow!("GDK context is not EGL-backed (GLX?)"))?;
            let exts = egl
                .query_string(Some(egl_display), egl::EXTENSIONS)
                .context("EGL_EXTENSIONS")?
                .to_string_lossy()
                .into_owned();
            for need in ["EGL_EXT_image_dma_buf_import", "EGL_KHR_image_base"] {
                if !exts.contains(need) {
                    bail!("EGL lacks {need}");
                }
            }
            // Tiled surfaces carry an explicit modifier — without the _modifiers extension
            // the import would silently assume implied/linear and sample garbage.
            if !exts.contains("EGL_EXT_image_dma_buf_import_modifiers") {
                bail!("EGL lacks EGL_EXT_image_dma_buf_import_modifiers");
            }
            let create_image: EglCreateImageKhr =
                std::mem::transmute::<extern "system" fn(), EglCreateImageKhr>(
                    egl.get_proc_address("eglCreateImageKHR")
                        .ok_or_else(|| anyhow!("no eglCreateImageKHR"))?,
                );
            let destroy_image: EglDestroyImageKhr =
                std::mem::transmute::<extern "system" fn(), EglDestroyImageKhr>(
                    egl.get_proc_address("eglDestroyImageKHR")
                        .ok_or_else(|| anyhow!("no eglDestroyImageKHR"))?,
                );
            let gl = GlFns::load(&egl)?;

            let es = ctx.api().contains(gdk::GLAPI::GLES);
            let program = build_program(&gl, es)?;
            (gl.UseProgram)(program);
            let u_mat = (gl.GetUniformLocation)(program, c"u_mat".as_ptr() as *const u8);
            let u_off = (gl.GetUniformLocation)(program, c"u_off".as_ptr() as *const u8);
            let u_y = (gl.GetUniformLocation)(program, c"u_y".as_ptr() as *const u8);
            let u_c = (gl.GetUniformLocation)(program, c"u_c".as_ptr() as *const u8);
            (gl.Uniform1i)(u_y, 0);
            (gl.Uniform1i)(u_c, 1);
            let mut vao = 0u32;
            (gl.GenVertexArrays)(1, &mut vao);
            let mut fbo = 0u32;
            (gl.GenFramebuffers)(1, &mut fbo);

            tracing::info!(
                gles = es,
                "GL presenter ready — VAAPI dmabufs convert in-process (own EGL import + shader)"
            );
            Ok(GlConverter {
                ctx,
                egl,
                egl_display: egl_display.as_ptr(),
                create_image,
                destroy_image,
                gl,
                program,
                vao,
                fbo,
                u_mat,
                u_off,
                uniforms_for: None,
                retired: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    /// Convert one decoded frame into an RGBA `GdkTexture`. The source surface (guard) is
    /// held until GTK releases the output texture — the GPU read is long finished by then.
    /// `color_state` tags the output (full-range RGB, transfer left baked — same semantics
    /// as the software path's tagged `GdkMemoryTexture`); `None` = untagged sRGB.
    pub fn convert(
        &mut self,
        frame: DmabufFrame,
        color_state: Option<&gdk::ColorState>,
    ) -> Result<gdk::Texture> {
        if frame.fourcc != DRM_FORMAT_NV12 {
            bail!("GL presenter handles NV12 only (got {:#x})", frame.fourcc);
        }
        if frame.planes.len() < 2 {
            bail!("NV12 needs 2 planes (got {})", frame.planes.len());
        }
        self.ctx.make_current();
        let gl = &self.gl;

        // SAFETY (whole body): our context is current; every GL/EGL object created here is
        // either destroyed before return or owned by the pool/release machinery.
        unsafe {
            // Recycle what GTK released since last frame (GL objects need the context, so
            // the release closures only park entries — this is where they die/revive).
            let size = (frame.width, frame.height);
            let mut out_tex = 0u32;
            {
                let mut retired = self.retired.lock().unwrap();
                retired.retain_mut(|r| {
                    if r.sync != 0 {
                        (gl.DeleteSync)(r.sync as *const c_void);
                        r.sync = 0;
                    }
                    if out_tex == 0 && r.size == size {
                        out_tex = r.tex;
                        false
                    } else if r.size != size {
                        (gl.DeleteTextures)(1, &r.tex); // stale size (mode change)
                        false
                    } else {
                        true // spare same-size texture for a later frame
                    }
                });
            }
            if out_tex == 0 {
                (gl.GenTextures)(1, &mut out_tex);
                (gl.BindTexture)(GL_TEXTURE_2D, out_tex);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
                (gl.TexImage2D)(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8 as i32,
                    frame.width as i32,
                    frame.height as i32,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    std::ptr::null(),
                );
            }

            // Import both planes with the surface's modifier — exactly the layer-wise
            // import Moonlight/mpv drive on this hardware.
            let y = &frame.planes[0];
            let c = &frame.planes[1];
            let img_y =
                self.plane_image(frame.width, frame.height, DRM_FORMAT_R8, y, frame.modifier)?;
            let img_c = match self.plane_image(
                frame.width.div_ceil(2),
                frame.height.div_ceil(2),
                DRM_FORMAT_GR88,
                c,
                frame.modifier,
            ) {
                Ok(img) => img,
                Err(e) => {
                    (self.destroy_image)(self.egl_display, img_y);
                    return Err(e);
                }
            };

            let mut planes = [0u32; 2];
            (gl.GenTextures)(2, planes.as_mut_ptr());
            for (tex, img) in planes.iter().zip([img_y, img_c]) {
                (gl.BindTexture)(GL_TEXTURE_2D, *tex);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
                (gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
                (gl.EGLImageTargetTexture2DOES)(GL_TEXTURE_2D, img);
            }

            (gl.UseProgram)(self.program);
            if self.uniforms_for != Some(frame.color) {
                let (mat, off) = yuv_to_rgb(frame.color);
                (gl.UniformMatrix3fv)(self.u_mat, 1, 0, mat.as_ptr());
                (gl.Uniform3fv)(self.u_off, 1, off.as_ptr());
                self.uniforms_for = Some(frame.color);
            }
            (gl.BindFramebuffer)(GL_FRAMEBUFFER, self.fbo);
            (gl.FramebufferTexture2D)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                out_tex,
                0,
            );
            let status = (gl.CheckFramebufferStatus)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                (gl.BindFramebuffer)(GL_FRAMEBUFFER, 0);
                (gl.DeleteTextures)(2, planes.as_ptr());
                (self.destroy_image)(self.egl_display, img_y);
                (self.destroy_image)(self.egl_display, img_c);
                (gl.DeleteTextures)(1, &out_tex);
                bail!("FBO incomplete ({status:#x})");
            }
            (gl.Viewport)(0, 0, frame.width as i32, frame.height as i32);
            (gl.BindVertexArray)(self.vao);
            (gl.ActiveTexture)(GL_TEXTURE0);
            (gl.BindTexture)(GL_TEXTURE_2D, planes[0]);
            (gl.ActiveTexture)(GL_TEXTURE0 + 1);
            (gl.BindTexture)(GL_TEXTURE_2D, planes[1]);
            (gl.DrawArrays)(GL_TRIANGLES, 0, 3);
            (gl.BindFramebuffer)(GL_FRAMEBUFFER, 0);

            let sync = (gl.FenceSync)(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
            (gl.Flush)();
            // The draw is queued: plane textures + images can go now (the driver keeps the
            // underlying buffers alive until the queued commands execute).
            (gl.DeleteTextures)(2, planes.as_ptr());
            (self.destroy_image)(self.egl_display, img_y);
            (self.destroy_image)(self.egl_display, img_c);

            let err = (gl.GetError)();
            if err != 0 {
                (gl.DeleteTextures)(1, &out_tex);
                bail!("GL error {err:#x} during convert");
            }

            let mut b = gdk::GLTextureBuilder::new()
                .set_context(Some(&self.ctx))
                .set_id(out_tex)
                .set_width(frame.width as i32)
                .set_height(frame.height as i32)
                .set_format(gdk::MemoryFormat::R8g8b8a8)
                .set_sync(Some(sync));
            if let Some(state) = color_state {
                b = b.set_color_state(state);
            }
            let retired = self.retired.clone();
            let guard = frame.guard;
            let sync_bits = sync as usize; // GLsync as usize — the closure must be Send
            let texture = b.build_with_release_func(move || {
                drop(guard); // the decoder surface outlived every GPU read of it
                retired.lock().unwrap().push(Retired {
                    tex: out_tex,
                    sync: sync_bits,
                    size,
                });
            });
            Ok(texture)
        }
    }

    /// One single-plane `EGLImage` over a dmabuf plane (R8 luma / GR88 chroma), modifier
    /// passed explicitly.
    ///
    /// # Safety
    /// `self.ctx` must be current; the fd stays owned by the caller (EGL dups internally).
    unsafe fn plane_image(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        plane: &crate::video::DmabufPlane,
        modifier: u64,
    ) -> Result<*const c_void> {
        let mut attribs = vec![
            EGL_WIDTH,
            width as i32,
            EGL_HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.stride as i32,
        ];
        if modifier != DRM_FORMAT_MOD_INVALID && modifier != 0 {
            attribs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (modifier & 0xffff_ffff) as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (modifier >> 32) as u32 as i32,
            ]);
        }
        attribs.push(EGL_NONE);
        // SAFETY: attribs is a valid EGL_NONE-terminated list; display/context are live.
        let img = unsafe {
            (self.create_image)(
                self.egl_display,
                std::ptr::null_mut(), // EGL_NO_CONTEXT — dmabuf import
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if img.is_null() {
            bail!(
                "eglCreateImageKHR rejected plane ({}x{} {:#x} mod {:#018x}): {:?}",
                width,
                height,
                fourcc,
                modifier,
                self.egl.get_error()
            );
        }
        Ok(img)
    }
}

impl Drop for GlConverter {
    /// Delete our objects from the shared context group (the context lives in GDK's share
    /// group — per-session leftovers would pile up across sessions). Textures GTK still
    /// holds at this moment release into `retired` afterwards, where nobody drains them:
    /// those names leak, but it's ≤ the pool depth once per session, not per frame.
    fn drop(&mut self) {
        self.ctx.make_current();
        let gl = &self.gl;
        // SAFETY: context current; only objects this converter created are deleted.
        unsafe {
            for r in self.retired.lock().unwrap().drain(..) {
                if r.sync != 0 {
                    (gl.DeleteSync)(r.sync as *const c_void);
                }
                (gl.DeleteTextures)(1, &r.tex);
            }
            (gl.DeleteFramebuffers)(1, &self.fbo);
            (gl.DeleteVertexArrays)(1, &self.vao);
            (gl.DeleteProgram)(self.program);
        }
    }
}

/// Compile the fullscreen-triangle NV12→RGB program (GLSL 300 es / 330 core per the GDK
/// context's API). `gl_VertexID` drives the geometry — no buffers at all.
///
/// # Safety
/// A GL context must be current; `gl` must belong to it.
unsafe fn build_program(gl: &GlFns, es: bool) -> Result<u32> {
    let header = if es {
        "#version 300 es\nprecision highp float;\n"
    } else {
        "#version 330 core\n"
    };
    let vs_src = format!(
        "{header}
out vec2 v_uv;
void main() {{
    vec2 p = vec2(float((gl_VertexID & 1) << 2) - 1.0, float((gl_VertexID & 2) << 1) - 1.0);
    v_uv = p * 0.5 + 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}}"
    );
    let fs_src = format!(
        "{header}
in vec2 v_uv;
out vec4 frag;
uniform sampler2D u_y;
uniform sampler2D u_c;
uniform mat3 u_mat;
uniform vec3 u_off;
void main() {{
    vec3 yuv = vec3(texture(u_y, v_uv).r, texture(u_c, v_uv).rg);
    frag = vec4(clamp(u_mat * (yuv + u_off), 0.0, 1.0), 1.0);
}}"
    );
    // SAFETY: caller holds a current context; sources are valid UTF-8 with explicit lengths.
    unsafe {
        let compile = |kind: u32, src: &str| -> Result<u32> {
            let sh = (gl.CreateShader)(kind);
            let ptr = src.as_ptr();
            let len = src.len() as i32;
            (gl.ShaderSource)(sh, 1, &ptr, &len);
            (gl.CompileShader)(sh);
            let mut ok = 0i32;
            (gl.GetShaderiv)(sh, GL_COMPILE_STATUS, &mut ok);
            if ok == 0 {
                let mut log = vec![0u8; 1024];
                let mut n = 0i32;
                (gl.GetShaderInfoLog)(sh, 1024, &mut n, log.as_mut_ptr());
                (gl.DeleteShader)(sh);
                bail!(
                    "shader compile: {}",
                    String::from_utf8_lossy(&log[..n.max(0) as usize])
                );
            }
            Ok(sh)
        };
        let vs = compile(GL_VERTEX_SHADER, &vs_src)?;
        let fs = match compile(GL_FRAGMENT_SHADER, &fs_src) {
            Ok(fs) => fs,
            Err(e) => {
                (gl.DeleteShader)(vs);
                return Err(e);
            }
        };
        let prog = (gl.CreateProgram)();
        (gl.AttachShader)(prog, vs);
        (gl.AttachShader)(prog, fs);
        (gl.LinkProgram)(prog);
        (gl.DeleteShader)(vs);
        (gl.DeleteShader)(fs);
        let mut ok = 0i32;
        (gl.GetProgramiv)(prog, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            bail!("program link failed");
        }
        Ok(prog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(matrix: u8, full_range: bool) -> ColorDesc {
        ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix,
            full_range,
        }
    }

    fn apply(mat: &[f32; 9], off: &[f32; 3], yuv: [f32; 3]) -> [f32; 3] {
        let v = [yuv[0] + off[0], yuv[1] + off[1], yuv[2] + off[2]];
        // Column-major: out[r] = Σ mat[col*3 + r] * v[col]
        core::array::from_fn(|r| (0..3).map(|c| mat[c * 3 + r] * v[c]).sum())
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0.
    #[test]
    fn bt709_limited_white_black() {
        let (mat, off) = yuv_to_rgb(desc(1, false));
        let white = apply(&mat, &off, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let black = apply(&mat, &off, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.005, "white {white:?}");
            assert!(b.abs() < 0.005, "black {black:?}");
        }
    }

    /// Full-range identity points: Y=1 → white, Y=0 → black, and a 601-vs-709 red spot
    /// check (pure V excursion produces R = 2(1−Kr)·0.5).
    #[test]
    fn full_range_and_red_excursion() {
        let (mat, off) = yuv_to_rgb(desc(5, true));
        let white = apply(&mat, &off, [1.0, 0.5, 0.5]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let red = apply(&mat, &off, [0.0, 0.5, 1.0]);
        assert!((red[0] - 2.0 * (1.0 - 0.299) * 0.5).abs() < 1e-4, "{red:?}");
        // 709 differs from 601 in the same spot — guards the matrix-code dispatch.
        let (mat709, off709) = yuv_to_rgb(desc(1, true));
        let red709 = apply(&mat709, &off709, [0.0, 0.5, 1.0]);
        assert!(
            (red709[0] - 2.0 * (1.0 - 0.2126) * 0.5).abs() < 1e-4,
            "{red709:?}"
        );
        assert!((red[0] - red709[0]).abs() > 0.05);
    }
}
