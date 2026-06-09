//! EGL side of the zero-copy path: open a headless EGLDisplay on the NVIDIA EGL device and
//! import a PipeWire dmabuf as an `EGLImage` with `EGL_LINUX_DMA_BUF_EXT`. The DRM format
//! **modifier** is mandatory on NVIDIA (its buffers are tiled; importing without the modifier
//! yields a corrupt image or `EGL_BAD_MATCH`). The image is handed to CUDA
//! (`cuGraphicsEGLRegisterImage`) and copied device-to-device into an owned buffer so the
//! dmabuf can be returned to the compositor immediately.
//!
//! NOTE (WIP): the negotiation + EGL import are verified end-to-end against KWin (a tiled
//! dmabuf reaches `eglCreateImage` successfully), but `cuGraphicsEGLRegisterImage` currently
//! returns `CUDA_ERROR_INVALID_VALUE` on the dmabuf-imported `EGLImage`. The likely fix is to
//! bind the `EGLImage` to a GL texture (`glEGLImageTargetTexture2DOES`) and register *that* via
//! `cuGraphicsGLRegisterImage` (OBS/Sunshine's path), which needs a GL context.

#![allow(non_upper_case_globals)]

use super::cuda::{self, DeviceBuffer, MappedImage};
use anyhow::{ensure, Context as _, Result};
use khronos_egl as egl;
use std::os::raw::c_void;

// EGL_EXT_image_dma_buf_import / _modifiers + platform enums (not defined by khronos-egl).
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_PLATFORM_DEVICE_EXT: egl::Enum = 0x313F;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

/// One dmabuf plane as delivered by PipeWire (single-plane for BGRx).
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// Headless EGLDisplay (NVIDIA device platform) used to import dmabufs. Lives on the capture
/// thread. The device platform — not GBM — is what NVIDIA's CUDA-EGL interop registers against.
pub struct EglImporter {
    egl: Egl,
    display: egl::Display,
    no_ctx: egl::Context,
}

// The EGL handles are confined to the capture thread; the struct is moved there once.
unsafe impl Send for EglImporter {}

impl EglImporter {
    /// Open a headless EGLDisplay on the NVIDIA EGL device. Also forces the shared CUDA context
    /// to exist (so a later `import` only touches the hot path).
    pub fn new() -> Result<EglImporter> {
        let egl: Egl =
            unsafe { Egl::load_required() }.context("load libEGL (EGL 1.5 dynamic instance)")?;

        // Enumerate EGL devices and use the first (the NVIDIA GPU on a single-GPU box).
        type QueryDevicesFn = unsafe extern "system" fn(
            max_devices: i32,
            devices: *mut *mut c_void,
            num_devices: *mut i32,
        ) -> u32;
        let query_devices: QueryDevicesFn = unsafe {
            std::mem::transmute(
                egl.get_proc_address("eglQueryDevicesEXT")
                    .context("eglQueryDevicesEXT unavailable")?,
            )
        };
        let device = unsafe {
            let mut count: i32 = 0;
            ensure!(
                query_devices(0, std::ptr::null_mut(), &mut count) != 0 && count > 0,
                "no EGL devices found"
            );
            let mut devices = vec![std::ptr::null_mut::<c_void>(); count as usize];
            ensure!(
                query_devices(count, devices.as_mut_ptr(), &mut count) != 0,
                "eglQueryDevicesEXT enumeration failed"
            );
            devices[0]
        };

        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_DEVICE_EXT,
                device as egl::NativeDisplayType,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("eglGetPlatformDisplay(DEVICE) on the NVIDIA EGL device")?;
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
        tracing::info!(
            "zero-copy EGL importer ready (EGL device platform, dma_buf_import + modifiers)"
        );
        Ok(EglImporter {
            egl,
            display,
            no_ctx,
        })
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
        &self,
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
