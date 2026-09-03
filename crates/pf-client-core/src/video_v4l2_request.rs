//! Raspberry Pi HEVC decode through FFmpeg's V4L2 Request API.
//!
//! The Pi decoder produces Broadcom SAND DMA-BUFs. V3DV cannot import that modifier
//! through Vulkan, so the Raspberry Pi FFmpeg fork's NEON transfer converts SAND to
//! tightly packed I420 for Punktfunk's existing planar Vulkan upload path.

use crate::video::CpuPlanarFrame;
use crate::video_color::ColorDesc;
use anyhow::{bail, Result};
use ffmpeg_sys_next as ffi;
use std::ffi::CStr;
use std::ptr;
use std::slice;

pub const DECODER_PIN: &str = "v4l2-request";

struct AvBuffer(*mut ffi::AVBufferRef);

impl AvBuffer {
    unsafe fn from_raw(value: *mut ffi::AVBufferRef) -> Result<Self> {
        if value.is_null() {
            bail!("FFmpeg returned a null hardware device");
        }
        Ok(Self(value))
    }
}

impl Drop for AvBuffer {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

unsafe impl Send for AvBuffer {}

unsafe extern "C" fn pick_drm_prime(
    _ctx: *mut ffi::AVCodecContext,
    mut formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        while *formats != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *formats == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
                return *formats;
            }
            formats = formats.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct V4l2RequestDecoder {
    ctx: *mut ffi::AVCodecContext,
    _hw_device: AvBuffer,
    packet: *mut ffi::AVPacket,
    frame: *mut ffi::AVFrame,
    planar: *mut ffi::AVFrame,
}

unsafe impl Send for V4l2RequestDecoder {}

impl V4l2RequestDecoder {
    pub fn new(wire_codec: u8) -> Result<Self> {
        if wire_codec != punktfunk_core::quic::CODEC_HEVC {
            bail!("V4L2 Request is available only for HEVC on Raspberry Pi 5");
        }
        unsafe {
            let mut raw_device = ptr::null_mut();
            let status = ffi::av_hwdevice_ctx_create(
                &mut raw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if status < 0 {
                return Err(av_error("DRM hardware device creation", status));
            }
            let hw_device = AvBuffer::from_raw(raw_device)?;
            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                bail!("the bundled FFmpeg has no HEVC decoder");
            }
            let mut ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("could not allocate the HEVC decoder context");
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device.0);
            (*ctx).get_format = Some(pick_drm_prime);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1;
            (*ctx).extra_hw_frames = 4;
            let status = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if status < 0 {
                ffi::avcodec_free_context(&mut ctx);
                return Err(av_error("opening the V4L2 Request HEVC decoder", status));
            }
            let packet = ffi::av_packet_alloc();
            let frame = ffi::av_frame_alloc();
            let planar = ffi::av_frame_alloc();
            if packet.is_null() || frame.is_null() || planar.is_null() {
                let mut packet = packet;
                let mut frame = frame;
                let mut planar = planar;
                ffi::av_packet_free(&mut packet);
                ffi::av_frame_free(&mut frame);
                ffi::av_frame_free(&mut planar);
                ffi::avcodec_free_context(&mut ctx);
                bail!("could not allocate FFmpeg packet/frame storage");
            }
            (*planar).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            Ok(Self { ctx, _hw_device: hw_device, packet, frame, planar })
        }
    }

    pub fn name(&self) -> &'static str {
        DECODER_PIN
    }

    pub fn decode(&mut self, au: &[u8]) -> Result<Option<CpuPlanarFrame>> {
        unsafe {
            let status = ffi::av_new_packet(self.packet, au.len() as i32);
            if status < 0 {
                return Err(av_error("av_new_packet", status));
            }
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
            let status = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if status < 0 {
                return Err(av_error("avcodec_send_packet", status));
            }
            let mut output = None;
            loop {
                let status = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if status == -libc::EAGAIN {
                    break;
                }
                if status < 0 {
                    return Err(av_error("avcodec_receive_frame", status));
                }
                output = Some(self.planar_frame()?);
                ffi::av_frame_unref(self.frame);
            }
            Ok(output)
        }
    }

    unsafe fn planar_frame(&mut self) -> Result<CpuPlanarFrame> {
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32 {
                bail!("V4L2 Request returned a non-DRM frame");
            }
            if (*self.planar).buf[0].is_null()
                || (*self.planar).width != (*self.frame).width
                || (*self.planar).height != (*self.frame).height
            {
                ffi::av_frame_unref(self.planar);
                (*self.planar).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
                (*self.planar).width = (*self.frame).width;
                (*self.planar).height = (*self.frame).height;
                let status = ffi::av_frame_get_buffer(self.planar, 64);
                if status < 0 {
                    return Err(av_error("allocating the planar transfer frame", status));
                }
            }
            let status = ffi::av_frame_make_writable(self.planar);
            if status < 0 {
                return Err(av_error("making the planar transfer frame writable", status));
            }
            let status = ffi::av_hwframe_transfer_data(self.planar, self.frame, 0);
            if status < 0 {
                return Err(av_error("transferring SAND output to planar I420", status));
            }

            let width = (*self.planar).width as u32;
            let height = (*self.planar).height as u32;
            let (chroma_width, chroma_height) = CpuPlanarFrame::chroma_dims(width, height);
            let dimensions = [(width, height), (chroma_width, chroma_height), (chroma_width, chroma_height)];
            let mut planes: [&[u8]; 3] = [&[]; 3];
            let mut strides = [0usize; 3];
            for index in 0..3 {
                if (*self.planar).data[index].is_null() || (*self.planar).linesize[index] <= 0 {
                    bail!("planar transfer returned an invalid plane {index}");
                }
                let stride = (*self.planar).linesize[index] as usize;
                let rows = dimensions[index].1 as usize;
                let row_bytes = dimensions[index].0 as usize;
                strides[index] = stride;
                planes[index] = slice::from_raw_parts(
                    (*self.planar).data[index],
                    (rows - 1) * stride + row_bytes,
                );
            }
            let flags = (*self.frame).flags;
            CpuPlanarFrame::from_i420(
                width,
                height,
                planes,
                strides,
                ColorDesc {
                    primaries: (*self.frame).color_primaries as u8,
                    transfer: (*self.frame).color_trc as u8,
                    matrix: (*self.frame).colorspace as u8,
                    full_range: (*self.frame).color_range
                        == ffi::AVColorRange::AVCOL_RANGE_JPEG,
                },
                flags & ffi::AV_FRAME_FLAG_KEY != 0
                    || (*self.frame).pict_type == ffi::AVPictureType::AV_PICTURE_TYPE_I,
                punktfunk_core::reanchor::LocalRecovery::NONE,
            )
        }
    }
}

impl Drop for V4l2RequestDecoder {
    fn drop(&mut self) {
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_frame_free(&mut self.planar);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}

fn av_error(operation: &str, status: i32) -> anyhow::Error {
    let mut buffer = [0u8; 128];
    let message = unsafe {
        if ffi::av_strerror(status, buffer.as_mut_ptr(), buffer.len()) == 0 {
            CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
        } else {
            format!("FFmpeg error {status}")
        }
    };
    anyhow::anyhow!("{operation}: {message}")
}
