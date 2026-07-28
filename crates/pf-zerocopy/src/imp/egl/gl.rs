//! GL plumbing for the EGL zero-copy blit (plan §W4, carved out of the EGL facade): the GL enum
//! constants, the `#[link]`'d libGL / libgbm entry points, the fullscreen-triangle shader sources
//! (BGRA swizzle + the NV12 / YUV444 BT.709 convert passes), and the shader/program compile
//! helpers. The de-tiling blit passes and the EGLDisplay importer that drive this all live in
//! [`super`].

#![allow(non_upper_case_globals)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{bail, ensure, Result};
use std::os::raw::{c_int, c_void};

pub(crate) const GL_TEXTURE_2D: u32 = 0x0DE1;
pub(crate) const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub(crate) const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub(crate) const GL_LINEAR: c_int = 0x2601;
pub(crate) const GL_NEAREST: c_int = 0x2600;
pub(crate) const GL_RGBA8: u32 = 0x8058;
// Single/dual-channel 8-bit formats for the NV12 convert targets: R8 luma (full-res),
// RG8 interleaved chroma (half-res). The `_RED`/`_RG` enums are the matching client formats.
pub(crate) const GL_R8: u32 = 0x8229;
pub(crate) const GL_RG8: u32 = 0x822B;
// Client pixel format/type for texture uploads (self-test only): RGBA bytes.
pub(crate) const GL_RGBA: u32 = 0x1908;
pub(crate) const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub(crate) const GL_FRAMEBUFFER: u32 = 0x8D40;
pub(crate) const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
pub(crate) const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
pub(crate) const GL_TEXTURE0: u32 = 0x84C0;
pub(crate) const GL_TRIANGLES: u32 = 0x0004;
pub(crate) const GL_VERTEX_SHADER: u32 = 0x8B31;
pub(crate) const GL_FRAGMENT_SHADER: u32 = 0x8B30;
pub(crate) const GL_COMPILE_STATUS: u32 = 0x8B81;
pub(crate) const GL_LINK_STATUS: u32 = 0x8B82;

// libglvnd's libGL dispatches these to the NVIDIA driver based on the current EGL/GL context.
#[link(name = "GL")]
extern "C" {
    pub(crate) fn glGenTextures(n: c_int, textures: *mut u32);
    pub(crate) fn glBindTexture(target: u32, texture: u32);
    pub(crate) fn glTexParameteri(target: u32, pname: u32, param: c_int);
    pub(crate) fn glDeleteTextures(n: c_int, textures: *const u32);
    pub(crate) fn glTexStorage2D(
        target: u32,
        levels: c_int,
        internalformat: u32,
        width: c_int,
        height: c_int,
    );
    pub(crate) fn glGetError() -> u32;
    pub(crate) fn glGenFramebuffers(n: c_int, framebuffers: *mut u32);
    pub(crate) fn glDeleteFramebuffers(n: c_int, framebuffers: *const u32);
    pub(crate) fn glBindFramebuffer(target: u32, framebuffer: u32);
    pub(crate) fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: c_int,
    );
    pub(crate) fn glCheckFramebufferStatus(target: u32) -> u32;
    pub(crate) fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int);
    pub(crate) fn glGenVertexArrays(n: c_int, arrays: *mut u32);
    pub(crate) fn glDeleteVertexArrays(n: c_int, arrays: *const u32);
    pub(crate) fn glBindVertexArray(array: u32);
    pub(crate) fn glDrawArrays(mode: u32, first: c_int, count: c_int);
    pub(crate) fn glActiveTexture(texture: u32);
    pub(crate) fn glUseProgram(program: u32);
    pub(crate) fn glFlush();
    pub(crate) fn glCreateShader(shader_type: u32) -> u32;
    pub(crate) fn glShaderSource(
        shader: u32,
        count: c_int,
        string: *const *const i8,
        length: *const c_int,
    );
    pub(crate) fn glCompileShader(shader: u32);
    pub(crate) fn glGetShaderiv(shader: u32, pname: u32, params: *mut c_int);
    pub(crate) fn glDeleteShader(shader: u32);
    pub(crate) fn glCreateProgram() -> u32;
    pub(crate) fn glAttachShader(program: u32, shader: u32);
    pub(crate) fn glLinkProgram(program: u32);
    pub(crate) fn glGetProgramiv(program: u32, pname: u32, params: *mut c_int);
    pub(crate) fn glGetUniformLocation(program: u32, name: *const i8) -> c_int;
    pub(crate) fn glUniform1i(location: c_int, v0: c_int);
    pub(crate) fn glDeleteProgram(program: u32);
    pub(crate) fn glTexSubImage2D(
        target: u32,
        level: c_int,
        xoffset: c_int,
        yoffset: c_int,
        width: c_int,
        height: c_int,
        format: u32,
        type_: u32,
        pixels: *const c_void,
    );
}

#[link(name = "gbm")]
extern "C" {
    pub(crate) fn gbm_create_device(fd: c_int) -> *mut c_void;
    pub(crate) fn gbm_device_destroy(device: *mut c_void);
}

/// `glEGLImageTargetTexture2DOES(target, EGLImage)` — loaded via `eglGetProcAddress`.
pub(crate) type EglImageTargetFn = unsafe extern "system" fn(u32, *mut c_void);

// Fullscreen-triangle blit: sample the dmabuf EGLImage texture and write it (swizzled to BGRA,
// to match the BGRx the encoder expects) into a normal GL_RGBA8 texture that CUDA *can* register.
pub(crate) const VERT_SRC: &[u8] = b"#version 330 core\nout vec2 v_tex;\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));v_tex=p;gl_Position=vec4(p*2.0-1.0,0.0,1.0);}\n";
pub(crate) const FRAG_SRC: &[u8] = b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){o_color=texture(image,v_tex).bgra;}\n";

// NV12 BT.709 LIMITED-range convert from full-range RGB in [0,1]. Two passes share `VERT_SRC` and
// the same source texture (the de-tiled dmabuf):
//   Y pass  → GL_R8 luma, full-res:   Y = (16 + 219·(0.2126R+0.7152G+0.0722B))/255
//   UV pass → GL_RG8 chroma, half-res (GL_LINEAR averages the 2×2 footprint):
//     U = (128 + 224·(-0.1146R-0.3854G+0.5000B))/255  → R channel
//     V = (128 + 224·( 0.5000R-0.4542G-0.0458B))/255  → G channel
// RG8's (R=U, G=V) byte order matches NV12's interleaved [U,V]. All outputs clamped to [0,1].
// Matches the Windows VideoConverter (BT.709, limited/studio range) so the two hosts look identical.
pub(crate) const FRAG_Y_SRC: &[u8] = b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){vec3 c=texture(image,v_tex).rgb;float Y=(16.0+219.0*(0.2126*c.r+0.7152*c.g+0.0722*c.b))/255.0;o_color=vec4(clamp(Y,0.0,1.0),0.0,0.0,1.0);}\n";
pub(crate) const FRAG_UV_SRC: &[u8] = b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){vec3 c=texture(image,v_tex).rgb;float U=(128.0+224.0*(-0.1146*c.r-0.3854*c.g+0.5000*c.b))/255.0;float V=(128.0+224.0*(0.5000*c.r-0.4542*c.g-0.0458*c.b))/255.0;o_color=vec4(clamp(U,0.0,1.0),clamp(V,0.0,1.0),0.0,1.0);}\n";

/// The three planar-YUV444 convert shaders (full-res `R8` target each) — the [`Yuv444Blit`]
/// analogue of `FRAG_Y_SRC`/`FRAG_UV_SRC` with NO subsampling (4:4:4 keeps every chroma sample).
/// Same BT.709 coefficients; `full_range` flips the quantization from studio (16+219 / 128±112)
/// to the full 0..255 swing — the encoder flips the VUI (`PUNKTFUNK_444_FULLRANGE`, read by both
/// processes from the same inherited environment) in lockstep, so pixels and signaling agree.
pub(crate) fn yuv444_frag_sources(full_range: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (y_scale, y_off, c_scale) = if full_range {
        ("255.0", "0.0", "255.0")
    } else {
        ("219.0", "16.0", "224.0")
    };
    let head = "#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){vec3 c=texture(image,v_tex).rgb;";
    let y = format!(
        "{head}float Y=({y_off}+{y_scale}*(0.2126*c.r+0.7152*c.g+0.0722*c.b))/255.0;o_color=vec4(clamp(Y,0.0,1.0),0.0,0.0,1.0);}}\n"
    );
    let u = format!(
        "{head}float U=(128.0+{c_scale}*(-0.1146*c.r-0.3854*c.g+0.5000*c.b))/255.0;o_color=vec4(clamp(U,0.0,1.0),0.0,0.0,1.0);}}\n"
    );
    let v = format!(
        "{head}float V=(128.0+{c_scale}*(0.5000*c.r-0.4542*c.g-0.0458*c.b))/255.0;o_color=vec4(clamp(V,0.0,1.0),0.0,0.0,1.0);}}\n"
    );
    (y.into_bytes(), u.into_bytes(), v.into_bytes())
}

pub(crate) unsafe fn compile_shader(kind: u32, src: &[u8]) -> Result<u32> {
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

/// Compile+link the fullscreen-triangle program with fragment source `frag` and bind its `image`
/// sampler to texture unit 0.
pub(crate) unsafe fn compile_program_with(frag: &[u8]) -> Result<u32> {
    // Callers retry per frame, so every error path below must delete what it created — a leak
    // here is unbounded, not one-shot.
    let vs = compile_shader(GL_VERTEX_SHADER, VERT_SRC)?;
    let fs = match compile_shader(GL_FRAGMENT_SHADER, frag) {
        Ok(fs) => fs,
        Err(e) => {
            glDeleteShader(vs);
            return Err(e);
        }
    };
    let prog = glCreateProgram();
    if prog == 0 {
        glDeleteShader(vs);
        glDeleteShader(fs);
        bail!("glCreateProgram failed");
    }
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glDeleteShader(vs);
    glDeleteShader(fs);
    let mut ok: c_int = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &mut ok);
    if ok == 0 {
        glDeleteProgram(prog);
        bail!("GL program link failed");
    }
    glUseProgram(prog);
    let loc = glGetUniformLocation(prog, c"image".as_ptr());
    if loc >= 0 {
        glUniform1i(loc, 0); // sampler -> texture unit 0
    }
    glUseProgram(0);
    Ok(prog)
}

pub(crate) unsafe fn compile_program() -> Result<u32> {
    compile_program_with(FRAG_SRC)
}
