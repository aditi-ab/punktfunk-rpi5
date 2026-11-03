//! Raspberry Pi HEVC decode through FFmpeg's V4L2 Request API.
//!
//! The Pi decoder produces Broadcom SAND DMA-BUFs. V3DV cannot import that modifier
//! through Vulkan, so the Raspberry Pi FFmpeg fork's NEON transfer converts SAND to
//! planar 8/10-bit samples. Ten-bit samples are rounded to eight bits for the
//! existing I420 Vulkan upload path; stream colour metadata stays attached.

use crate::video::{CpuPlanarFrame, DecodeBatch, DecodeMetadata, DecodedImage, TaggedImage};
use crate::video_color::ColorDesc;
use anyhow::{bail, Result};
use ffmpeg_sys_next as ffi;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;
use std::slice;
use std::time::{Duration, Instant};

pub const DECODER_PIN: &str = "v4l2-request";
// FFmpeg 6 exposed this as AV_FRAME_FLAG_KEY. Keep the ABI-stable bit value
// locally so the client still compiles against older development headers used
// for non-Pi test builds.
const FRAME_FLAG_KEY: i32 = 1 << 1;
const MAX_PENDING: usize = 8;
const IDLE_DRAIN_AFTER: Duration = Duration::from_millis(50);

fn idle_drain_due(last_input: Option<Instant>, now: Instant) -> bool {
    last_input.is_some_and(|at| now.saturating_duration_since(at) >= IDLE_DRAIN_AFTER)
}

struct InputLedger {
    pending: VecDeque<(i64, DecodeMetadata)>,
    next: i64,
    accepted: usize,
    yielded: bool,
    delay: usize,
}

impl InputLedger {
    fn new(delay: usize) -> Self {
        Self {
            pending: VecDeque::with_capacity(MAX_PENDING),
            next: 1,
            accepted: 0,
            yielded: false,
            delay,
        }
    }

    fn token(&self) -> Result<i64> {
        anyhow::ensure!(
            self.pending.len() < MAX_PENDING && self.next < i64::MAX,
            "decode metadata queue exhausted"
        );
        Ok(self.next)
    }

    fn accept(&mut self, metadata: DecodeMetadata) {
        self.pending.push_back((self.next, metadata));
        self.next += 1;
        self.accepted = self.accepted.saturating_add(1);
    }

    fn take(&mut self, token: i64) -> Result<DecodeMetadata> {
        let index = self
            .pending
            .iter()
            .position(|(id, _)| *id == token)
            .ok_or_else(|| anyhow::anyhow!("decoded frame has no accepted input token"))?;
        self.yielded = true;
        Ok(self.pending.remove(index).unwrap().1)
    }

    fn priming(&self) -> bool {
        !self.yielded && self.accepted > 0 && self.accepted <= self.delay
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.accepted = 0;
        self.yielded = false;
    }
}

#[derive(Default)]
struct DecodeSample {
    phases: [Duration; 4], // Codec, SAND transfer, owned pixel copy, whole call.
    frames: u64,
}

struct DecodeTiming {
    started: Instant,
    calls: u64,
    frames: u64,
    sums: [Duration; 4],
    peaks: [Duration; 4],
}

impl DecodeTiming {
    fn new(started: Instant) -> Self {
        Self {
            started,
            calls: 0,
            frames: 0,
            sums: [Duration::ZERO; 4],
            peaks: [Duration::ZERO; 4],
        }
    }

    fn add(&mut self, sample: DecodeSample, now: Instant) -> Option<Self> {
        self.calls += 1;
        self.frames += sample.frames;
        for (i, duration) in sample.phases.into_iter().enumerate() {
            self.sums[i] += duration;
            self.peaks[i] = self.peaks[i].max(duration);
        }
        (now.duration_since(self.started) >= Duration::from_secs(1))
            .then(|| std::mem::replace(self, Self::new(now)))
    }

    fn mean_us(&self, phase: usize) -> f64 {
        self.sums[phase].as_secs_f64() * 1_000_000.0 / self.calls as f64
    }
}

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

struct PackedTransfer<'a> {
    frame: *mut ffi::AVFrame,
    _bytes: &'a mut [u8],
}

unsafe extern "C" fn borrowed_pixels(_opaque: *mut c_void, _data: *mut u8) {}

impl<'a> PackedTransfer<'a> {
    // SAFETY: frame is exclusively borrowed until this guard drops; FFmpeg's
    // synchronous transfer does not retain the destination buffer reference.
    unsafe fn new(
        frame: *mut ffi::AVFrame,
        bytes: &'a mut [u8],
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let (offsets, total) = CpuPlanarFrame::packed_layout(width, height)?;
        anyhow::ensure!(
            bytes.len() == total,
            "packed transfer buffer length mismatch"
        );
        anyhow::ensure!(
            width <= i32::MAX as u32 && height <= i32::MAX as u32,
            "packed transfer dimensions exceed FFmpeg limits"
        );
        let data = bytes.as_mut_ptr();
        let guard = Self {
            frame,
            _bytes: bytes,
        };
        unsafe {
            ffi::av_frame_unref(frame);
            (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*frame).width = width as i32;
            (*frame).height = height as i32;
            (*frame).buf[0] =
                ffi::av_buffer_create(data, total, Some(borrowed_pixels), ptr::null_mut(), 0);
            anyhow::ensure!(
                !(*frame).buf[0].is_null(),
                "allocate packed transfer buffer reference"
            );
            for (i, offset) in offsets.into_iter().enumerate() {
                (*frame).data[i] = data.add(offset);
                (*frame).linesize[i] = if i == 0 { width } else { width.div_ceil(2) } as i32;
            }
        }
        Ok(guard)
    }
}

impl Drop for PackedTransfer<'_> {
    fn drop(&mut self) {
        // The Vec owns the pixels. Clear all FFmpeg references before its borrow ends.
        unsafe { ffi::av_frame_unref(self.frame) };
    }
}

fn direct_transfer_dimensions(width: i32, height: i32, crop: [usize; 4]) -> bool {
    width > 0 && height > 0 && width % 2 == 0 && height % 2 == 0 && crop == [0; 4]
}

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
    timing: Option<DecodeTiming>,
    direct_transfer: bool,
    inputs: InputLedger,
    last_input: Option<Instant>,
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
                bail!("allocate HEVC decoder context");
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device.0);
            (*ctx).get_format = Some(pick_drm_prime);
            let threads = if std::env::var("PUNKTFUNK_RPI_DECODE_THREADS").as_deref() == Ok("1") {
                1
            } else {
                2
            };
            if threads == 1 {
                (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            } else {
                (*ctx).flags &= !(ffi::AV_CODEC_FLAG_LOW_DELAY as i32);
            }
            (*ctx).thread_count = threads;
            (*ctx).thread_type = ffi::FF_THREAD_FRAME as i32;
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
                bail!("allocate FFmpeg packet/frame storage");
            }
            (*planar).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            let delay = if (*ctx).active_thread_type & ffi::FF_THREAD_FRAME as i32 != 0 {
                1
            } else {
                0
            };
            tracing::info!(
                threads = (*ctx).thread_count,
                delay,
                "V4L2 Request decode pipeline configured"
            );
            Ok(Self {
                ctx,
                _hw_device: hw_device,
                packet,
                frame,
                planar,
                timing: None,
                direct_transfer: std::env::var("PUNKTFUNK_RPI_DIRECT_TRANSFER").as_deref()
                    != Ok("0"),
                inputs: InputLedger::new(delay),
                last_input: None,
            })
        }
    }

    pub fn name(&self) -> &'static str {
        DECODER_PIN
    }

    pub fn decode(&mut self, au: &[u8]) -> Result<Option<CpuPlanarFrame>> {
        let batch = self.decode_tagged_mode(Some((au, DecodeMetadata::default())), true)?;
        anyhow::ensure!(
            batch.images.len() <= 1,
            "untagged decode returned multiple pictures"
        );
        Ok(batch
            .images
            .into_iter()
            .next()
            .map(|output| match output.image {
                DecodedImage::V4l2Planar(frame) => frame,
                _ => unreachable!(),
            }))
    }

    pub fn decode_tagged(&mut self, input: Option<(&[u8], DecodeMetadata)>) -> Result<DecodeBatch> {
        self.decode_tagged_mode(input, false)
    }

    fn decode_tagged_mode(
        &mut self,
        input: Option<(&[u8], DecodeMetadata)>,
        synchronous: bool,
    ) -> Result<DecodeBatch> {
        if input.is_none()
            && (self.inputs.pending.is_empty() || !idle_drain_due(self.last_input, Instant::now()))
        {
            return Ok(DecodeBatch::default());
        }
        let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
        let mut sample = started.map(|_| DecodeSample::default());
        if started.is_none() {
            self.timing = None;
        }
        let result = self.decode_tagged_inner(input, sample.as_mut(), synchronous);
        if result.is_err() {
            unsafe {
                ffi::av_packet_unref(self.packet);
                ffi::av_frame_unref(self.frame);
                ffi::avcodec_flush_buffers(self.ctx);
            }
            self.inputs.reset();
            self.last_input = None;
        }
        if let (Some(at), Some(mut sample)) = (started, sample) {
            let now = Instant::now();
            sample.phases[3] = now.duration_since(at);
            let timing = self.timing.get_or_insert_with(|| DecodeTiming::new(at));
            if let Some(window) = timing.add(sample, now) {
                tracing::debug!(
                    calls = window.calls,
                    frames = window.frames,
                    window_ms = now.duration_since(window.started).as_millis() as u64,
                    codec_mean_us = window.mean_us(0),
                    codec_max_us = window.peaks[0].as_micros() as u64,
                    transfer_mean_us = window.mean_us(1),
                    transfer_max_us = window.peaks[1].as_micros() as u64,
                    copy_mean_us = window.mean_us(2),
                    copy_max_us = window.peaks[2].as_micros() as u64,
                    total_mean_us = window.mean_us(3),
                    total_max_us = window.peaks[3].as_micros() as u64,
                    "V4L2 Request decode timing per call"
                );
            }
        }
        result
    }

    fn decode_tagged_inner(
        &mut self,
        input: Option<(&[u8], DecodeMetadata)>,
        mut timing: Option<&mut DecodeSample>,
        synchronous: bool,
    ) -> Result<DecodeBatch> {
        let mut batch = DecodeBatch::default();
        unsafe {
            if let Some((au, metadata)) = input {
                let token = self.inputs.token()?;
                let size = i32::try_from(au.len())
                    .map_err(|_| anyhow::anyhow!("decode packet exceeds FFmpeg size limit"))?;
                let status = ffi::av_new_packet(self.packet, size);
                if status < 0 {
                    return Err(av_error("allocate decode packet", status));
                }
                ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
                (*self.packet).pts = token;
                (*self.packet).dts = token;
                let mut accepted = false;
                for attempt in 0..2 {
                    let at = timing.as_ref().map(|_| Instant::now());
                    let status = ffi::avcodec_send_packet(self.ctx, self.packet);
                    if let (Some(at), Some(sample)) = (at, timing.as_mut()) {
                        sample.phases[0] += at.elapsed();
                    }
                    if status == -libc::EAGAIN && attempt == 0 {
                        self.receive_tagged(&mut batch, 0, timing.as_deref_mut())?;
                        anyhow::ensure!(
                            !batch.images.is_empty(),
                            "decode send and receive both need input"
                        );
                        continue;
                    }
                    if status < 0 {
                        return Err(av_error("submit decode packet", status));
                    }
                    self.inputs.accept(metadata);
                    self.last_input = Some(Instant::now());
                    accepted = true;
                    break;
                }
                ffi::av_packet_unref(self.packet);
                anyhow::ensure!(accepted, "decode packet was not accepted");
            }
            let flags = if input.is_none() || synchronous {
                ffi::AV_CODEC_RECEIVE_FRAME_FLAG_SYNCHRONOUS as u32
            } else {
                0
            };
            self.receive_tagged(&mut batch, flags, timing)?;
        }
        anyhow::ensure!(
            input.is_some() || !batch.images.is_empty() || self.inputs.pending.is_empty(),
            "idle decode drain returned no pending picture"
        );
        batch.priming = input.is_some() && batch.images.is_empty() && self.inputs.priming();
        Ok(batch)
    }

    unsafe fn receive_tagged(
        &mut self,
        batch: &mut DecodeBatch,
        flags: u32,
        mut timing: Option<&mut DecodeSample>,
    ) -> Result<()> {
        unsafe {
            loop {
                let at = timing.as_ref().map(|_| Instant::now());
                let status = ffi::avcodec_receive_frame_flags(self.ctx, self.frame, flags);
                if let (Some(at), Some(sample)) = (at, timing.as_mut()) {
                    sample.phases[0] += at.elapsed();
                }
                if status == -libc::EAGAIN {
                    return Ok(());
                }
                if status < 0 {
                    return Err(av_error("receive decoded frame", status));
                }
                let decoded = (|| {
                    anyhow::ensure!(
                        batch.images.len() < MAX_PENDING,
                        "decode output batch exceeded limit"
                    );
                    let metadata = self.inputs.take((*self.frame).pts)?;
                    let image = DecodedImage::V4l2Planar(self.planar_frame(timing.as_deref_mut())?);
                    Ok(TaggedImage {
                        image,
                        metadata,
                        delayed: self.inputs.delay > 0,
                    })
                })();
                ffi::av_frame_unref(self.frame);
                batch.images.push(decoded?);
                if let Some(sample) = timing.as_mut() {
                    sample.frames += 1;
                }
            }
        }
    }

    unsafe fn planar_frame(
        &mut self,
        mut timing: Option<&mut DecodeSample>,
    ) -> Result<CpuPlanarFrame> {
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32 {
                bail!("V4L2 Request returned a non-DRM frame");
            }
            if (*self.frame).hw_frames_ctx.is_null() {
                bail!("missing hardware frame context for SAND transfer");
            }
            let mut formats = ptr::null_mut();
            let status = ffi::av_hwframe_transfer_get_formats(
                (*self.frame).hw_frames_ctx,
                ffi::AVHWFrameTransferDirection::AV_HWFRAME_TRANSFER_DIRECTION_FROM,
                &mut formats,
                0,
            );
            if status < 0 {
                return Err(av_error("query SAND transfer formats", status));
            }
            let mut format = ffi::AVPixelFormat::AV_PIX_FMT_NONE;
            let mut candidate = formats;
            while !candidate.is_null() && *candidate != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
                if matches!(
                    *candidate,
                    ffi::AVPixelFormat::AV_PIX_FMT_YUV420P
                        | ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE
                ) {
                    format = *candidate;
                    break;
                }
                candidate = candidate.add(1);
            }
            ffi::av_free(formats.cast());
            if format == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
                bail!("no supported planar SAND transfer format");
            }
            let ten_bit = format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE;
            if self.direct_transfer
                && !ten_bit
                && direct_transfer_dimensions(
                    (*self.frame).width,
                    (*self.frame).height,
                    [
                        (*self.frame).crop_left,
                        (*self.frame).crop_top,
                        (*self.frame).crop_right,
                        (*self.frame).crop_bottom,
                    ],
                )
            {
                match self.packed_frame(timing.as_deref_mut()) {
                    Ok(frame) => return Ok(frame),
                    Err(error) => {
                        self.direct_transfer = false;
                        tracing::warn!(%error, "packed SAND transfer disabled; using planar copy");
                    }
                }
            }
            if (*self.planar).buf[0].is_null()
                || (*self.planar).width != (*self.frame).width
                || (*self.planar).height != (*self.frame).height
                || (*self.planar).format != format as i32
            {
                ffi::av_frame_unref(self.planar);
                (*self.planar).format = format as i32;
                (*self.planar).width = (*self.frame).width;
                (*self.planar).height = (*self.frame).height;
                let status = ffi::av_frame_get_buffer(self.planar, 64);
                if status < 0 {
                    return Err(av_error("allocating the planar transfer frame", status));
                }
                tracing::info!(
                    input_bits = if ten_bit { 10 } else { 8 },
                    output_bits = 8,
                    width = (*self.planar).width,
                    height = (*self.planar).height,
                    "SAND planar transfer configured"
                );
            }
            let status = ffi::av_frame_make_writable(self.planar);
            if status < 0 {
                return Err(av_error(
                    "making the planar transfer frame writable",
                    status,
                ));
            }
            let transfer_started = timing.as_ref().map(|_| Instant::now());
            let status = ffi::av_hwframe_transfer_data(self.planar, self.frame, 0);
            if let (Some(at), Some(sample)) = (transfer_started, timing.as_mut()) {
                sample.phases[1] += at.elapsed();
            }
            if status < 0 {
                return Err(av_error("transfer SAND output to planar samples", status));
            }

            let copy_started = timing.as_ref().map(|_| Instant::now());
            let width = (*self.planar).width as u32;
            let height = (*self.planar).height as u32;
            let (chroma_width, chroma_height) = CpuPlanarFrame::chroma_dims(width, height);
            let dimensions = [
                (width, height),
                (chroma_width, chroma_height),
                (chroma_width, chroma_height),
            ];
            let mut planes: [&[u8]; 3] = [&[]; 3];
            let mut strides = [0usize; 3];
            for index in 0..3 {
                if (*self.planar).data[index].is_null() || (*self.planar).linesize[index] <= 0 {
                    bail!("planar transfer returned an invalid plane {index}");
                }
                let stride = (*self.planar).linesize[index] as usize;
                let rows = dimensions[index].1 as usize;
                let row_bytes = dimensions[index].0 as usize * if ten_bit { 2 } else { 1 };
                if rows == 0 || row_bytes == 0 || stride < row_bytes {
                    bail!("invalid planar transfer dimensions or stride");
                }
                strides[index] = stride;
                planes[index] = slice::from_raw_parts(
                    (*self.planar).data[index],
                    (rows - 1) * stride + row_bytes,
                );
            }
            let color = ColorDesc {
                primaries: (*self.frame).color_primaries as u8,
                transfer: (*self.frame).color_trc as u8,
                matrix: (*self.frame).colorspace as u8,
                full_range: (*self.frame).color_range == ffi::AVColorRange::AVCOL_RANGE_JPEG,
            };
            let keyframe = (*self.frame).flags & FRAME_FLAG_KEY != 0
                || (*self.frame).pict_type == ffi::AVPictureType::AV_PICTURE_TYPE_I;
            let recovery = punktfunk_core::reanchor::LocalRecovery::NONE;
            let copied = if ten_bit {
                let (offsets, total) = CpuPlanarFrame::packed_layout(width, height)?;
                let mut data = vec![0; total];
                for index in 0..3 {
                    let end = offsets.get(index + 1).copied().unwrap_or(total);
                    downshift_10bit_into(
                        planes[index],
                        strides[index],
                        dimensions[index].0 as usize,
                        dimensions[index].1 as usize,
                        &mut data[offsets[index]..end],
                    )?;
                }
                CpuPlanarFrame::from_packed(
                    width, height, data, color, keyframe, recovery, "software",
                )
            } else {
                CpuPlanarFrame::from_i420(width, height, planes, strides, color, keyframe, recovery)
            };
            if let (Some(at), Some(sample)) = (copy_started, timing.as_mut()) {
                sample.phases[2] += at.elapsed();
            }
            copied
        }
    }

    unsafe fn packed_frame(
        &mut self,
        mut timing: Option<&mut DecodeSample>,
    ) -> Result<CpuPlanarFrame> {
        unsafe {
            let width = (*self.frame).width as u32;
            let height = (*self.frame).height as u32;
            let (_, total) = CpuPlanarFrame::packed_layout(width, height)?;
            let copy_started = timing.as_ref().map(|_| Instant::now());
            let mut data = vec![0; total];
            let destination = PackedTransfer::new(self.planar, &mut data, width, height)?;
            if let (Some(at), Some(sample)) = (copy_started, timing.as_mut()) {
                sample.phases[2] += at.elapsed();
            }
            let transfer_started = timing.as_ref().map(|_| Instant::now());
            let status = ffi::av_hwframe_transfer_data(destination.frame, self.frame, 0);
            if let (Some(at), Some(sample)) = (transfer_started, timing.as_mut()) {
                sample.phases[1] += at.elapsed();
            }
            if status < 0 {
                return Err(av_error("transfer SAND output to packed samples", status));
            }
            anyhow::ensure!(
                (*destination.frame).width == width as i32
                    && (*destination.frame).height == height as i32
                    && (*destination.frame).format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32,
                "packed transfer changed picture layout"
            );
            let (offsets, _) = CpuPlanarFrame::packed_layout(width, height)?;
            let base = destination._bytes.as_ptr();
            for (i, offset) in offsets.into_iter().enumerate() {
                anyhow::ensure!(
                    (*destination.frame).data[i].cast_const() == base.add(offset)
                        && (*destination.frame).linesize[i]
                            == if i == 0 { width } else { width / 2 } as i32,
                    "packed transfer changed plane {i} layout"
                );
            }
            drop(destination);
            CpuPlanarFrame::from_packed(
                width,
                height,
                data,
                ColorDesc {
                    primaries: (*self.frame).color_primaries as u8,
                    transfer: (*self.frame).color_trc as u8,
                    matrix: (*self.frame).colorspace as u8,
                    full_range: (*self.frame).color_range == ffi::AVColorRange::AVCOL_RANGE_JPEG,
                },
                (*self.frame).flags & FRAME_FLAG_KEY != 0
                    || (*self.frame).pict_type == ffi::AVPictureType::AV_PICTURE_TYPE_I,
                punktfunk_core::reanchor::LocalRecovery::NONE,
                "software",
            )
        }
    }
}

fn downshift_10bit_into(
    source: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    target: &mut [u8],
) -> Result<()> {
    let row_bytes = width
        .checked_mul(2)
        .ok_or_else(|| anyhow::anyhow!("ten-bit row size overflow"))?;
    let required = height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride))
        .and_then(|size| size.checked_add(row_bytes));
    anyhow::ensure!(
        width > 0
            && stride >= row_bytes
            && required.is_some_and(|size| size <= source.len())
            && width.checked_mul(height) == Some(target.len()),
        "invalid ten-bit plane layout"
    );
    for row in 0..height {
        let source = &source[row * stride..row * stride + row_bytes];
        let target = &mut target[row * width..(row + 1) * width];
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        // SAFETY: AArch64 supplies NEON; complete eight-sample chunks fit both slices.
        let done = unsafe { downshift_10bit_neon(source, target) };
        #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
        let done = 0;
        for (sample, output) in source[done * 2..].chunks_exact(2).zip(&mut target[done..]) {
            let value = u16::from_le_bytes([sample[0], sample[1]]);
            *output = ((u32::from(value) + 2) >> 2).min(255) as u8;
        }
    }
    Ok(())
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn downshift_10bit_neon(source: &[u8], target: &mut [u8]) -> usize {
    use std::arch::aarch64::{vld1q_u8, vqrshrn_n_u16, vreinterpretq_u16_u8, vst1_u8};
    let chunks = (source.len() / 16).min(target.len() / 8);
    for chunk in 0..chunks {
        // SAFETY: each chunk reads 16 valid bytes and writes eight. Byte loads
        // accept unaligned planes; rounded saturating narrowing matches the scalar path.
        unsafe {
            let samples = vreinterpretq_u16_u8(vld1q_u8(source.as_ptr().add(chunk * 16)));
            vst1_u8(
                target.as_mut_ptr().add(chunk * 8),
                vqrshrn_n_u16::<2>(samples),
            );
        }
    }
    chunks * 8
}

#[cfg(test)]
mod tests {
    use super::{
        direct_transfer_dimensions, downshift_10bit_into, idle_drain_due, DecodeSample,
        DecodeTiming, InputLedger, PackedTransfer, MAX_PENDING,
    };
    use crate::video::DecodeMetadata;
    use ffmpeg_sys_next as ffi;
    use std::time::{Duration, Instant};

    #[test]
    fn delayed_outputs_match_accepted_inputs_not_the_latest_packet() {
        let mut inputs = InputLedger::new(1);
        let first = DecodeMetadata {
            pts_ns: 100,
            received_ns: 110,
            flags: 7,
            recovery_generation: 3,
        };
        let second = DecodeMetadata {
            pts_ns: 200,
            received_ns: 220,
            flags: 8,
            recovery_generation: 4,
        };
        let token = inputs.token().unwrap();
        assert_eq!(inputs.token().unwrap(), token);
        inputs.accept(first);
        assert!(inputs.priming());
        let next = inputs.token().unwrap();
        inputs.accept(second);
        assert!(!inputs.priming());
        assert_eq!(inputs.take(next).unwrap(), second);
        assert_eq!(inputs.take(token).unwrap(), first);
        assert!(inputs.take(token).is_err());
        assert!(!inputs.priming());
    }

    #[test]
    fn metadata_and_priming_are_bounded_and_reset_discards_old_tokens() {
        let mut inputs = InputLedger::new(1);
        assert!(!inputs.priming());
        for _ in 0..MAX_PENDING {
            assert!(inputs.token().is_ok());
            inputs.accept(DecodeMetadata::default());
        }
        assert!(inputs.token().is_err());
        assert!(!inputs.priming());
        inputs.reset();
        assert!(inputs.pending.is_empty());
        assert!(!inputs.priming());
        assert!(inputs.take(1).is_err());
        assert_eq!(inputs.token().unwrap(), MAX_PENDING as i64 + 1);
        inputs.accept(DecodeMetadata::default());
        assert!(inputs.priming());
        let mut single = InputLedger::new(0);
        single.accept(DecodeMetadata::default());
        assert!(!single.priming());
    }

    #[test]
    fn idle_drain_preserves_regular_cadence_but_releases_an_isolated_frame() {
        let start = Instant::now();
        assert!(!idle_drain_due(None, start));
        for period in [8, 17] {
            let mut last = start;
            for _ in 0..120 {
                let arrival = last + Duration::from_millis(period);
                assert!(!idle_drain_due(Some(last), arrival));
                last = arrival;
            }
            assert!(!idle_drain_due(
                Some(last),
                last + Duration::from_millis(49)
            ));
            assert!(idle_drain_due(Some(last), last + Duration::from_millis(50)));
            assert!(idle_drain_due(Some(last), last + Duration::from_millis(70)));
        }
    }

    #[test]
    fn direct_transfer_requires_even_uncropped_dimensions() {
        assert!(direct_transfer_dimensions(130, 4, [0; 4]));
        assert!(direct_transfer_dimensions(3840, 2160, [0; 4]));
        for (w, h) in [(0, 4), (-2, 4), (130, 0), (129, 4), (130, 3)] {
            assert!(!direct_transfer_dimensions(w, h, [0; 4]));
        }
        for i in 0..4 {
            let mut crop = [0; 4];
            crop[i] = 2;
            assert!(!direct_transfer_dimensions(130, 4, crop));
        }
    }

    #[test]
    fn packed_transfer_releases_references_on_success_and_error() {
        unsafe {
            let mut frame = ffi::av_frame_alloc();
            assert!(!frame.is_null());
            let mut data = vec![0; 130 * 4 * 3 / 2];
            let address = data.as_ptr();
            {
                let destination = PackedTransfer::new(frame, &mut data, 130, 4).unwrap();
                assert_eq!(ffi::av_buffer_get_ref_count((*frame).buf[0]), 1);
                for (i, length) in [520, 130, 130].into_iter().enumerate() {
                    std::ptr::write_bytes((*destination.frame).data[i], (i + 1) as u8, length);
                }
            }
            assert!((*frame).buf[0].is_null());
            assert!((*frame).data[0].is_null());
            assert_eq!(data.as_ptr(), address);
            assert!(data[..520].iter().all(|&v| v == 1));
            assert!(data[520..650].iter().all(|&v| v == 2));
            assert!(data[650..].iter().all(|&v| v == 3));
            let fail = |bytes: &mut [u8]| -> anyhow::Result<()> {
                let _destination = PackedTransfer::new(frame, bytes, 130, 4)?;
                anyhow::bail!("injected transfer failure");
            };
            assert!(fail(&mut data).is_err());
            assert!((*frame).buf[0].is_null());
            assert!((*frame).data[0].is_null());
            data.fill(7);
            assert_eq!(data[779], 7);
            assert!(PackedTransfer::new(frame, &mut data[..779], 130, 4).is_err());
            ffi::av_frame_free(&mut frame);
        }
    }

    #[test]
    fn timing_counts_empty_calls_and_resets_after_each_window() {
        let at = Instant::now();
        let mut timing = DecodeTiming::new(at);
        let sample = |frames, us| DecodeSample {
            phases: [Duration::from_micros(us); 4],
            frames,
        };
        assert!(timing.add(sample(0, 10), at).is_none());
        let report = timing
            .add(sample(2, 30), at + Duration::from_secs(1))
            .unwrap();
        assert_eq!((report.calls, report.frames), (2, 2));
        assert_eq!(report.mean_us(0), 20.0);
        assert_eq!(report.peaks[0], Duration::from_micros(30));
        assert_eq!((timing.calls, timing.frames), (0, 0));
        assert!(timing
            .add(sample(1, 5), at + Duration::from_millis(1500))
            .is_none());
        let report = timing
            .add(sample(1, 7), at + Duration::from_secs(2))
            .unwrap();
        assert_eq!(report.mean_us(3), 6.0);
        assert_eq!(report.peaks[3], Duration::from_micros(7));
    }

    #[test]
    fn ten_bit_downshift_matches_every_u16_value() {
        let input: Vec<u8> = (0..=u16::MAX).flat_map(u16::to_le_bytes).collect();
        let mut output = vec![0; 65536];
        downshift_10bit_into(&input, input.len(), output.len(), 1, &mut output).unwrap();
        for (value, actual) in output.into_iter().enumerate() {
            assert_eq!(
                actual,
                (((value as u32 + 2) >> 2).min(255)) as u8,
                "sample {value}"
            );
        }
    }

    #[test]
    fn ten_bit_downshift_handles_unaligned_padded_rows_and_vector_tails() {
        for width in [1, 3, 7, 8, 9, 15, 17, 130] {
            let height = 3;
            let stride = width * 2 + 3;
            let mut input = vec![0xaa; 1 + (height - 1) * stride + width * 2];
            let mut expected = Vec::new();
            for row in 0..height {
                for column in 0..width {
                    let value = ((row * width + column) * 911 + 31) as u16;
                    let at = 1 + row * stride + column * 2;
                    input[at..at + 2].copy_from_slice(&value.to_le_bytes());
                    expected.push(((u32::from(value) + 2) >> 2).min(255) as u8);
                }
            }
            let mut output = vec![0x55; 2 + width * height];
            downshift_10bit_into(
                &input[1..],
                stride,
                width,
                height,
                &mut output[1..1 + width * height],
            )
            .unwrap();
            assert_eq!(&output[1..1 + width * height], expected.as_slice());
            assert_eq!((output[0], output[1 + width * height]), (0x55, 0x55));
        }
    }

    #[test]
    fn ten_bit_downshift_rejects_short_planes_and_invalid_geometry() {
        let mut output = [0; 8];
        for (stride, width, height) in [
            (7, 4, 2),
            (8, 0, 2),
            (8, 4, 0),
            (8, usize::MAX, 2),
            (usize::MAX, 4, 3),
        ] {
            assert!(downshift_10bit_into(&[0; 16], stride, width, height, &mut output).is_err());
        }
        assert!(downshift_10bit_into(&[0; 15], 8, 4, 2, &mut output).is_err());
        assert!(downshift_10bit_into(&[0; 16], 8, 4, 2, &mut output[..7]).is_err());
    }

    #[test]
    fn ten_bit_transfer_preserves_range_rounding_and_row_padding() {
        let samples = [0u16, 64, 512, 940, 1023, 65535, 1, 2, 3, 4, 1022];
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let mut output = vec![0; 10];
        downshift_10bit_into(&bytes, 12, 5, 2, &mut output).unwrap();
        assert_eq!(output, [0, 16, 128, 235, 255, 0, 1, 1, 1, 255]);
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
    let mut buffer = [0 as c_char; 128];
    let message = unsafe {
        if ffi::av_strerror(status, buffer.as_mut_ptr(), buffer.len()) == 0 {
            CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            format!("FFmpeg error {status}")
        }
    };
    anyhow::anyhow!("{operation}: {message}")
}
