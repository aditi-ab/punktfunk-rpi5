//! NVENC hardware encoder (Windows, D3D11 input) — zero-copy capture→encode on the GPU.
//!
//! Drives the raw NVENC API via `nvidia_video_codec_sdk::{sys, ENCODE_API}` (the safe `Encoder`
//! wrapper is CUDA-only). Opens an encode session bound to the **same** `ID3D11Device` as the DXGI
//! capturer (the device is carried on `FramePayload::D3d11`), registers a small pool of encoder-owned
//! BGRA textures once, and per frame `CopyResource`s the captured texture into a pooled one and
//! `encode_picture`s it. Mirrors the Linux NVENC config: CBR + ultra-low-latency, infinite GOP,
//! P-frames only, forced-IDR for RFI, in-band SPS/PPS each keyframe.
//!
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise) — compiles GPU-less, but
//! `open`/`submit` only succeed on a GPU box. The software encoder (`super::sw`) is the fallback.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use nvidia_video_codec_sdk::ENCODE_API as API;

const POOL: usize = 4;

fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
    }
}

struct PooledTex {
    tex: ID3D11Texture2D,
    reg: nv::NV_ENC_REGISTERED_PTR,
    map: nv::NV_ENC_INPUT_PTR,
}

pub struct NvencD3d11Encoder {
    ctx: Option<ID3D11DeviceContext>,
    encoder: *mut c_void,
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    pool: Vec<PooledTex>,
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, usize, u64)>,
    frame_idx: i64,
    force_kf: bool,
    inited: bool,
}

// Raw NVENC handle + COM ptrs; confined to the single encode thread (like the Linux encoder).
unsafe impl Send for NvencD3d11Encoder {}

impl NvencD3d11Encoder {
    pub fn open(
        codec: Codec,
        _format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        Ok(Self {
            ctx: None,
            encoder: ptr::null_mut(),
            codec_guid: codec_guid(codec),
            width,
            height,
            fps,
            bitrate_bps,
            buffer_fmt: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            pool: Vec::new(),
            next: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            inited: false,
        })
    }

    /// Lazily create the session on the first frame's D3D11 device (so capture + encode share it).
    fn init_session(&mut self, device: &ID3D11Device) -> Result<()> {
        unsafe {
            self.ctx = Some(device.GetImmediateContext().context("D3D11 immediate context")?);

            // 1. open the session bound to the D3D11 device.
            let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
                device: device.as_raw(),
                apiVersion: nv::NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc: *mut c_void = ptr::null_mut();
            (API.open_encode_session_ex)(&mut params, &mut enc)
                .result_without_string()
                .map_err(|e| anyhow!("NVENC open_encode_session_ex: {e:?} (no NVIDIA GPU?)"))?;
            self.encoder = enc;

            // 2. seed the P1 + ultra-low-latency preset config.
            let mut preset = nv::NV_ENC_PRESET_CONFIG {
                version: nv::NV_ENC_PRESET_CONFIG_VER,
                presetCfg: nv::NV_ENC_CONFIG {
                    version: nv::NV_ENC_CONFIG_VER,
                    ..Default::default()
                },
                ..Default::default()
            };
            (API.get_encode_preset_config_ex)(
                enc,
                self.codec_guid,
                nv::NV_ENC_PRESET_P1_GUID,
                nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset,
            )
            .result_without_string()
            .map_err(|e| anyhow!("get_encode_preset_config_ex: {e:?}"))?;
            let mut cfg = preset.presetCfg;

            // 3. mirror the Linux RC config: CBR, infinite GOP, P-only, ~1-frame VBV.
            cfg.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
            cfg.frameIntervalP = 1;
            cfg.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            let bps = self.bitrate_bps.min(u32::MAX as u64) as u32;
            cfg.rcParams.averageBitRate = bps;
            cfg.rcParams.maxBitRate = bps;
            let vbv = (self.bitrate_bps as f64 / self.fps.max(1) as f64) as u32;
            cfg.rcParams.vbvBufferSize = vbv;
            cfg.rcParams.vbvInitialDelay = vbv;

            // 4. initialize the encoder.
            let mut init = nv::NV_ENC_INITIALIZE_PARAMS {
                version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
                encodeGUID: self.codec_guid,
                presetGUID: nv::NV_ENC_PRESET_P1_GUID,
                tuningInfo: nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeWidth: self.width,
                encodeHeight: self.height,
                darWidth: self.width,
                darHeight: self.height,
                frameRateNum: self.fps,
                frameRateDen: 1,
                enablePTD: 1,
                encodeConfig: &mut cfg,
                ..Default::default()
            };
            (API.initialize_encoder)(enc, &mut init)
                .result_without_string()
                .map_err(|e| anyhow!("initialize_encoder: {e:?}"))?;

            // 5. encoder-owned BGRA texture pool, registered once, + one bitstream per slot.
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            for _ in 0..POOL {
                let mut tex: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(nvenc pool)")?;
                let tex = tex.context("null pool texture")?;
                let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType:
                        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                    width: self.width,
                    height: self.height,
                    pitch: 0,
                    resourceToRegister: tex.as_raw(),
                    bufferFormat: self.buffer_fmt,
                    bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                (API.register_resource)(enc, &mut rr)
                    .result_without_string()
                    .map_err(|e| anyhow!("register_resource: {e:?}"))?;
                self.pool.push(PooledTex {
                    tex,
                    reg: rr.registeredResource,
                    map: ptr::null_mut(),
                });
                let mut cb = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                (API.create_bitstream_buffer)(enc, &mut cb)
                    .result_without_string()
                    .map_err(|e| anyhow!("create_bitstream_buffer: {e:?}"))?;
                self.bitstreams.push(cb.bitstreamBuffer);
            }
            self.inited = true;
            tracing::info!(
                "NVENC D3D11 session: {}x{}@{} {} Mbps {:?}",
                self.width,
                self.height,
                self.fps,
                bps / 1_000_000,
                self.codec_guid
            );
            Ok(())
        }
    }
}

impl Encoder for NvencD3d11Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!("NVENC D3D11 encoder needs a GPU texture frame (use the software encoder for CPU frames)")
            }
        };
        if !self.inited {
            let device = frame.device.clone();
            self.init_session(&device)?;
        }
        let slot = self.next % POOL;
        self.next += 1;
        unsafe {
            let ctx = self.ctx.as_ref().context("no D3D11 context")?;
            ctx.CopyResource(&self.pool[slot].tex, &frame.texture);

            let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
                version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: self.pool[slot].reg,
                ..Default::default()
            };
            (API.map_input_resource)(self.encoder, &mut mp)
                .result_without_string()
                .map_err(|e| anyhow!("map_input_resource: {e:?}"))?;
            self.pool[slot].map = mp.mappedResource;

            let pts = self.frame_idx as u64;
            self.frame_idx += 1;
            let flags = if std::mem::take(&mut self.force_kf) {
                nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            };
            let mut pic = nv::NV_ENC_PIC_PARAMS {
                version: nv::NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: 0,
                inputBuffer: mp.mappedResource,
                bufferFmt: mp.mappedBufferFmt,
                outputBitstream: self.bitstreams[slot],
                pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: pts,
                encodePicFlags: flags as u32,
                ..Default::default()
            };
            (API.encode_picture)(self.encoder, &mut pic)
                .result_without_string()
                .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
            self.pending
                .push_back((self.bitstreams[slot], slot, captured.pts_ns));
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some((bs, slot, pts_ns)) = self.pending.pop_front() else {
            return Ok(None);
        };
        unsafe {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: bs,
                ..Default::default()
            };
            (API.lock_bitstream)(self.encoder, &mut lock)
                .result_without_string()
                .map_err(|e| anyhow!("lock_bitstream: {e:?}"))?;
            let data = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let keyframe = matches!(
                lock.pictureType,
                nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            (API.unlock_bitstream)(self.encoder, bs)
                .result_without_string()
                .map_err(|e| anyhow!("unlock_bitstream: {e:?}"))?;
            if !self.pool[slot].map.is_null() {
                let _ = (API.unmap_input_resource)(self.encoder, self.pool[slot].map);
                self.pool[slot].map = ptr::null_mut();
            }
            Ok(Some(EncodedFrame {
                data,
                pts_ns,
                keyframe,
            }))
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + frameIntervalP=1: each submit yields its AU; no internal queue to drain.
    }
}

impl Drop for NvencD3d11Encoder {
    fn drop(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        unsafe {
            for p in &self.pool {
                if !p.map.is_null() {
                    let _ = (API.unmap_input_resource)(self.encoder, p.map);
                }
                let _ = (API.unregister_resource)(self.encoder, p.reg);
            }
            for &bs in &self.bitstreams {
                let _ = (API.destroy_bitstream_buffer)(self.encoder, bs);
            }
            let _ = (API.destroy_encoder)(self.encoder);
        }
    }
}
