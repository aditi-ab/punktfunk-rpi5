//! EGL side of the zero-copy path: open a headless EGLDisplay on the NVIDIA GPU (via the GBM
//! platform on the render node) and import a PipeWire dmabuf as an `EGLImage` with
//! `EGL_LINUX_DMA_BUF_EXT`. The DRM format **modifier** is mandatory on NVIDIA (its buffers are
//! tiled; importing without the modifier yields a corrupt image or `EGL_BAD_MATCH`). The image
//! is then handed to CUDA (`cuGraphicsEGLRegisterImage`) and copied device-to-device into an
//! owned buffer so the dmabuf can be returned to the compositor immediately.

#![allow(non_upper_case_globals)]

use super::cuda::{self, DeviceBuffer, MappedImage};
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

#[link(name = "gbm")]
extern "C" {
    fn gbm_create_device(fd: c_int) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
}

/// One dmabuf plane as delivered by PipeWire (single-plane for BGRx).
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// Headless EGL display + GBM device used to import dmabufs. Lives on the capture thread.
pub struct EglImporter {
    egl: Egl,
    display: egl::Display,
    no_ctx: egl::Context,
    gbm: *mut c_void,
    render_fd: c_int,
}

// The EGL/GBM handles are confined to the capture thread; the struct is moved there once.
unsafe impl Send for EglImporter {}

impl EglImporter {
    /// Open the render node, create a GBM device, and a headless EGLDisplay on it. Also forces
    /// the shared CUDA context to exist (so a later `import` only touches the hot path).
    pub fn new() -> Result<EglImporter> {
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

        // Create the shared CUDA context up front so import() is pure hot path.
        cuda::context().context("create CUDA context")?;

        let no_ctx = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        tracing::info!("zero-copy EGL importer ready (GBM platform, dma_buf_import + modifiers)");
        Ok(EglImporter {
            egl,
            display,
            no_ctx,
            gbm,
            render_fd,
        })
    }

    /// Import one dmabuf and copy it device-to-device into a fresh owned CUDA buffer.
    /// `fourcc` is the DRM FourCC, `modifier` the 64-bit DRM format modifier from PipeWire.
    pub fn import(
        &self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
    ) -> Result<DeviceBuffer> {
        let attrs: [egl::Attrib; 19] = [
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
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (modifier & 0xFFFF_FFFF) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (modifier >> 32) as egl::Attrib,
            egl::ATTRIB_NONE,
            0,
            0,
        ];
        let client = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                self.no_ctx,
                EGL_LINUX_DMA_BUF_EXT,
                client,
                &attrs[..17], // up to and including ATTRIB_NONE
            )
            .context("eglCreateImage(EGL_LINUX_DMA_BUF_EXT) — modifier mismatch?")?;

        // CUDA: register + map + copy out, then drop the registration and the EGL image.
        let result = (|| -> Result<DeviceBuffer> {
            cuda::make_current()?;
            // SAFETY: `image` is a valid EGLImage we just created; context is current.
            let mapped = unsafe { MappedImage::register(image.as_ptr()) }?;
            let dst = DeviceBuffer::alloc(width, height)?;
            mapped.copy_to(&dst)?;
            Ok(dst)
        })();

        let _ = self.egl.destroy_image(self.display, image);
        result
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
