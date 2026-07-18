//! PyroWave host encoder (Windows) — **separate-plane zero-copy D3D11→Vulkan** via pyrowave's own
//! compat device (design/pyrowave-windows-host-zerocopy.md). The opt-in wired-LAN intra-only wavelet
//! codec, the Windows twin of `enc/linux/pyrowave.rs`.
//!
//! Shape (deliberately minimal — no `ash`, no hand-rolled external-memory import): pyrowave owns its
//! OWN Vulkan device, selected by the render GPU's vendor/device-id
//! (`pyrowave_create_device_by_compat`). The capturer's CSC produces TWO SEPARATE D3D11 plane
//! textures — a full-res `R8` **Y** + a half-res `R8G8` **CbCr** (BT.709 limited, matching the Linux
//! `rgb2yuv.comp` layout the wavelet clients decode) — each shared to that device as an NT handle
//! (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT`) via `pyrowave_image_create`. Separate
//! single/two-component textures import reliably on NVIDIA at any size, unlike a single planar NV12
//! texture (the vendored interop test: "only very specific resource sizes"). A shared
//! D3D11/D3D12 fence — signalled by the capturer *after* the convert — is imported as a Vulkan
//! timeline semaphore (`pyrowave_sync_object_create`) so the wavelet read is ordered after the
//! D3D11 convert. `pyrowave_encoder_encode_gpu_synchronous` performs the acquire (waiting the fence
//! value), the encode, and the release in ONE pyrowave-owned submission, referencing the external
//! image with `VK_QUEUE_FAMILY_EXTERNAL`. The dangerous cross-API import (incl. the NVIDIA
//! video-layout workaround) stays entirely inside validated pyrowave/Granite. Every AU is a
//! keyframe; the AU/wire-chunk framing is the shared [`crate::pyrowave_wire`] helper (byte-identical
//! to Linux).
//!
//! The capture side (a BGRA→YUV CSC into two shareable plane textures + a shared fence, gated on the
//! pyrowave session flag) lives in `pf-capture` (`windows/idd_push.rs`); the CbCr plane + fence ride
//! the frame on [`pf_frame::dxgi::D3d11Frame::pyro`], the Y plane on `D3d11Frame::texture`.
// Every `unsafe` block in this module carries a `// SAFETY:` proof (the crate root enforces it).

use crate::pyrowave_wire;
use crate::{EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pyrowave_sys as pw;
use std::collections::VecDeque;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIResource1;
use windows::Win32::System::Threading::GetCurrentProcess;

/// Headroom over the per-frame rate budget for the packetized bitstream (block headers + meta).
const BS_SLACK: usize = 256 * 1024;
/// Bound the per-texture image-import cache. The IDD out-ring is a small fixed set (OUT_RING=3);
/// this only ever grows past it if the capturer recreates its out-ring within one encoder's life
/// (a desktop-switch device recreate), in which case the stale imports are evicted + destroyed.
const IMPORT_CACHE_CAP: usize = 8;

// --- Vulkan enum values not surfaced by pyrowave-sys' bindgen (only enums *reachable* from the
// pyrowave C API are generated; these plain #define / flags-typedef values are stable spec
// constants). bindgen renders every reachable Vulkan enum as a `u32` type alias, so these u32
// literals assign straight into the generated struct fields. ---
// The usage the validated interop helper (`create_pyrowave_image_from_d3d11`) requests.
const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: u32 = 0x0000_0001;
const VK_IMAGE_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
const VK_IMAGE_USAGE_SAMPLED_BIT: u32 = 0x0000_0004;
/// `VK_QUEUE_FAMILY_EXTERNAL` (`~0u32 - 1`): the image is owned by an external (D3D11) queue family;
/// pyrowave's acquire/release transitions ownership in/out across the interop boundary.
const VK_QUEUE_FAMILY_EXTERNAL: u32 = 0xFFFF_FFFE;

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

fn budget_for(bitrate_bps: u64, fps: u32) -> usize {
    ((bitrate_bps / (8 * fps.max(1) as u64)) as usize).max(64 * 1024)
}

pub struct PyroWaveEncoder {
    // pyrowave owns the whole Vulkan device (create_device_by_compat) — no ash on this side.
    pw_dev: pw::pyrowave_device,
    pw_enc: pw::pyrowave_encoder,
    // The imported shared fence (a Vulkan timeline semaphore aliasing the capturer's D3D11 fence).
    // Null until the capturer delivers the fence handle on the first frame (or after a rebuild).
    sync: pw::pyrowave_sync_object,
    // Imported plane textures, cached by the out-ring texture's raw pointer (stable per ring slot):
    // the full-res R8 Y plane and the half-res R8G8 CbCr plane, imported SEPARATELY (a single planar
    // NV12 import is unreliable on NVIDIA at arbitrary sizes).
    y_images: Vec<(isize, pw::pyrowave_image)>,
    cbcr_images: Vec<(isize, pw::pyrowave_image)>,

    width: u32,
    height: u32,
    fps: u32,
    /// Per-frame bitstream budget (hard CBR): `bitrate / (8 * fps)`.
    frame_budget: usize,
    /// Datagram-aligned mode (plan §4.4): packetize at this boundary. `None` = one dense packet/AU.
    wire_chunk: Option<usize>,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: used only from the single encode thread; the pyrowave handles are owned and only touched
// from that thread, and pyrowave only submits GPU work inside the API calls we make (mirrors the
// Linux `PyroWaveEncoder`'s `unsafe impl Send`). The D3D11 texture pointers travel as plain `isize`
// cache keys, never dereferenced here.
unsafe impl Send for PyroWaveEncoder {}

impl PyroWaveEncoder {
    pub fn open(width: u32, height: u32, fps: u32, bitrate_bps: u64) -> Result<Self> {
        if width % 2 != 0 || height % 2 != 0 {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        let fps = fps.max(1);
        // Select pyrowave's device by the SELECTED render adapter's vendor/device-id — NOT by LUID:
        // in Session 0 (the host service context) the Vulkan ICD reports `deviceLUIDValid = false`,
        // so a by-LUID match would find nothing, while the vendor/device-id match + the external
        // import both work (design doc Stage 0; `pyrowave_c.cpp` guards LUID use behind validity).
        let (vid, pid) = pf_gpu::selected_gpu()
            .map(|s| (s.info.vendor_id, s.info.device_id))
            .unwrap_or((0, 0));
        // SAFETY: `create_device_by_compat` builds pyrowave's own instance/device from the
        // vendor/device-id (null uuids/luid = "don't constrain by those"); the out-param is a live
        // local. `confirm_interop_support` / `encoder_create` take that just-created non-null
        // device; on any failure we destroy what we created before returning. All pointers are
        // freshly created and owned by the returned struct (or freed on the error path).
        unsafe {
            let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
            pw_check(
                pw::pyrowave_create_device_by_compat(
                    vid,
                    pid,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut pw_dev,
                ),
                "create_device_by_compat",
            )
            .with_context(|| {
                format!(
                    "open a PyroWave Vulkan device for GPU {vid:04x}:{pid:04x} (render adapter)"
                )
            })?;

            // The make-or-break gate (design doc Risk 1): confirm this device can do the
            // external-memory interop the zero-copy import needs. In a service context where the
            // import is unavailable this fails HERE (clean HEVC renegotiation) instead of at the
            // first frame's import.
            if !pw::pyrowave_device_confirm_interop_support(pw_dev) {
                pw::pyrowave_device_destroy(pw_dev);
                bail!(
                    "the PyroWave Vulkan device does not confirm external-memory interop support \
                     (D3D11→Vulkan zero-copy import unavailable on this GPU / in this session \
                     context) — the session should renegotiate to HEVC"
                );
            }

            let einfo = pw::pyrowave_encoder_create_info {
                device: pw_dev,
                width: width as i32,
                height: height as i32,
                chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            };
            let mut pw_enc: pw::pyrowave_encoder = std::ptr::null_mut();
            if let Err(e) = pw_check(
                pw::pyrowave_encoder_create(&einfo, &mut pw_enc),
                "encoder_create",
            ) {
                pw::pyrowave_device_destroy(pw_dev);
                return Err(e);
            }

            let frame_budget = budget_for(bitrate_bps.max(1_000_000), fps);
            tracing::info!(
                gpu = format!("{vid:04x}:{pid:04x}"),
                mode = %format!("{width}x{height}@{fps}"),
                budget_kib = frame_budget / 1024,
                "PyroWave encoder open (Windows NV12 zero-copy, intra-only wavelet, BT.709 limited 4:2:0)"
            );

            Ok(Self {
                pw_dev,
                pw_enc,
                sync: std::ptr::null_mut(),
                y_images: Vec::new(),
                cbcr_images: Vec::new(),
                width,
                height,
                fps,
                frame_budget,
                wire_chunk: None,
                bitstream: Vec::new(),
                pending: VecDeque::new(),
            })
        }
    }

    /// Import one capturer plane D3D11 texture (`R8_UNORM` Y or `R8G8_UNORM` CbCr) into pyrowave's
    /// Vulkan device. Creates a fresh shared NT handle from the texture (the capturer marked the ring
    /// `SHARED | SHARED_NTHANDLE`); `pyrowave_image_create` takes ownership of the handle and closes
    /// it on import. Single/two-component textures import reliably on NVIDIA at any size — unlike a
    /// planar NV12 — so no MUTABLE_FORMAT / planar-layout workaround is involved.
    ///
    /// # Safety
    /// `texture` must be a live `ID3D11Texture2D` of format `vk_format`, sized `w`×`h`, created
    /// shareable, on the same physical GPU as `pw_dev`. The returned `pyrowave_image` is owned by the
    /// caller (destroyed in `Drop`/eviction). Takes `pw_dev` by value (not `&self`) so the cache
    /// closures don't double-borrow the encoder.
    unsafe fn import_plane(
        pw_dev: pw::pyrowave_device,
        texture: &ID3D11Texture2D,
        vk_format: pw::VkFormat,
        w: u32,
        h: u32,
    ) -> Result<pw::pyrowave_image> {
        // The shared NT handle (mirrors the interop test's `create_pyrowave_image_from_d3d11`).
        let res: IDXGIResource1 = texture
            .cast()
            .context("ID3D11Texture2D -> IDXGIResource1 (plane not created shareable?)")?;
        // GENERIC_ALL (0x1000_0000) — the access the interop test hands the shared handle.
        let handle: HANDLE = res
            .CreateSharedHandle(None, 0x1000_0000, PCWSTR::null())
            .context("IDXGIResource1::CreateSharedHandle(plane texture)")?;

        // Zero-init then set the fields we need (pNext/queue-family/initialLayout stay 0 = null /
        // UNDEFINED) — robust against however bindgen renders `Default` for the raw-pointer fields.
        let mut ici: pw::VkImageCreateInfo = std::mem::zeroed();
        ici.sType = pw::VkStructureType_VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
        ici.imageType = pw::VkImageType_VK_IMAGE_TYPE_2D;
        ici.format = vk_format;
        ici.extent = pw::VkExtent3D {
            width: w,
            height: h,
            depth: 1,
        };
        ici.mipLevels = 1;
        ici.arrayLayers = 1;
        ici.samples = pw::VkSampleCountFlagBits_VK_SAMPLE_COUNT_1_BIT;
        ici.tiling = pw::VkImageTiling_VK_IMAGE_TILING_OPTIMAL;
        ici.usage = VK_IMAGE_USAGE_SAMPLED_BIT
            | VK_IMAGE_USAGE_TRANSFER_SRC_BIT
            | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
        ici.sharingMode = pw::VkSharingMode_VK_SHARING_MODE_EXCLUSIVE;
        let info = pw::pyrowave_image_create_info {
            device: pw_dev,
            external_handle: handle.0 as usize as pw::pyrowave_os_handle,
            handle_type:
                pw::VkExternalMemoryHandleTypeFlagBits_VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT,
            image_create_info: &ici,
        };
        let mut image: pw::pyrowave_image = std::ptr::null_mut();
        if let Err(e) = pw_check(pw::pyrowave_image_create(&info, &mut image), "image_create") {
            // pyrowave only closes the handle on a SUCCESSFUL import — close it ourselves on failure.
            let _ = CloseHandle(handle);
            return Err(e);
        }
        Ok(image)
    }

    /// Import (cache) a plane texture by its stable per-slot pointer, evicting the oldest when the
    /// cache is over cap (the out-ring is small + fixed; growth only happens on a mid-life ring
    /// recreate). Returns the cached-or-fresh `pyrowave_image`.
    ///
    /// # Safety
    /// Same contract as [`import_plane`].
    unsafe fn cached_plane(
        cache: &mut Vec<(isize, pw::pyrowave_image)>,
        make: impl FnOnce() -> Result<pw::pyrowave_image>,
        key: isize,
    ) -> Result<pw::pyrowave_image> {
        if let Some((_, img)) = cache.iter().find(|(k, _)| *k == key) {
            return Ok(*img);
        }
        let img = make()?;
        if cache.len() >= IMPORT_CACHE_CAP {
            let (_, old) = cache.remove(0);
            pw::pyrowave_image_destroy(old);
        }
        cache.push((key, img));
        Ok(img)
    }

    /// Import the capturer's shared fence as a Vulkan timeline semaphore. Called only when this
    /// encoder has no timeline yet (the first frame, or a fresh encoder after a mode-switch rebuild).
    /// pyrowave takes ownership of the handle and CLOSES it on import, so we hand it a private
    /// **duplicate** of the capturer's persistent handle — leaving the original valid for the next
    /// rebuild's re-import (the capturer passes the same handle on every frame).
    ///
    /// # Safety
    /// `handle` must be the capturer's live shared D3D11/D3D12 fence NT handle on `self.pw_dev`'s GPU.
    unsafe fn import_fence(&mut self, handle: isize) -> Result<()> {
        let mut dup = HANDLE::default();
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(handle as *mut core::ffi::c_void),
            GetCurrentProcess(),
            &mut dup,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
        .context("DuplicateHandle(shared fence for pyrowave import)")?;
        let info = pw::pyrowave_sync_object_create_info {
            device: self.pw_dev,
            external_handle: dup.0 as usize as pw::pyrowave_os_handle,
            // D3D11 fence == D3D12 fence on Windows 10+; must be imported as TIMELINE.
            handle_type:
                pw::VkExternalSemaphoreHandleTypeFlagBits_VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_D3D12_FENCE_BIT,
            semaphore_type: pw::VkSemaphoreType_VK_SEMAPHORE_TYPE_TIMELINE,
            import_flags: 0,
        };
        let mut sync: pw::pyrowave_sync_object = std::ptr::null_mut();
        if let Err(e) = pw_check(
            pw::pyrowave_sync_object_create(&info, &mut sync),
            "sync_object_create",
        ) {
            // pyrowave only closes the handle on a SUCCESSFUL import — close the dup on failure.
            let _ = CloseHandle(dup);
            return Err(e);
        }
        self.sync = sync;
        Ok(())
    }

    /// One frame, synchronously: import (cache) the two plane textures + fence → encode (pyrowave
    /// owns the submission: acquire waits the capturer's fence value, references both images as
    /// `QUEUE_FAMILY_EXTERNAL`, release hands them back) → packetize into an `EncodedFrame`.
    ///
    /// # Safety
    /// Runs on the single encode thread; all pyrowave calls take handles this struct owns.
    unsafe fn encode_frame(&mut self, frame: &CapturedFrame) -> Result<()> {
        let FramePayload::D3d11(d3d) = &frame.payload else {
            bail!("pyrowave (Windows) needs a D3D11 frame (the capturer must be in pyrowave mode)")
        };
        let share = d3d.pyro.as_ref().context(
            "pyrowave (Windows): the frame carries no PyroWave payload — the capturer was not opened \
             in pyrowave mode (session_plan::output_format must set OutputFormat::pyrowave)",
        )?;

        // Import the fence whenever this encoder has no timeline yet — the first frame, OR a fresh
        // encoder after a client mode-switch rebuild (the capturer passes the persistent handle on
        // every frame precisely so a rebuilt encoder can re-import it).
        if self.sync.is_null() {
            let h = share
                .fence_handle
                .context("pyrowave (Windows): frame carried no shared fence handle")?;
            self.import_fence(h)?;
        }

        // Import (cache) the two SEPARATE plane textures by their stable per-slot pointers: the
        // full-res R8 Y on `d3d.texture`, the half-res R8G8 CbCr on `share.cbcr`. `pw_dev` is a Copy
        // handle so the cache closures don't borrow `self` alongside `&mut self.*_images`.
        let (w, h) = (self.width, self.height);
        let pw_dev = self.pw_dev;
        let y_img = {
            let key = d3d.texture.as_raw() as isize;
            let tex = &d3d.texture;
            Self::cached_plane(
                &mut self.y_images,
                || Self::import_plane(pw_dev, tex, pw::VkFormat_VK_FORMAT_R8_UNORM, w, h),
                key,
            )?
        };
        let cbcr_img = {
            let key = share.cbcr.as_raw() as isize;
            let tex = &share.cbcr;
            Self::cached_plane(
                &mut self.cbcr_images,
                || Self::import_plane(pw_dev, tex, pw::VkFormat_VK_FORMAT_R8G8_UNORM, w / 2, h / 2),
                key,
            )?
        };

        // Plane views built BY HAND exactly like the Linux encoder (`enc/linux/pyrowave.rs`): Y from
        // the R8 image (full-res, IDENTITY), Cb/Cr from the R8G8 image (half-res) with R/G swizzle to
        // synthesize the two chroma planes from the interleaved CbCr — the documented NV12-style
        // hand-off. All GENERAL layout (pyrowave's GPU-buffer contract accepts it without transitions).
        let y_vk = pw::pyrowave_image_get_handle(y_img);
        let cbcr_vk = pw::pyrowave_image_get_handle(cbcr_img);
        let plane = |image, pw_w, pw_h, fmt, swizzle| pw::pyrowave_image_view {
            image,
            width: pw_w,
            height: pw_h,
            image_format: fmt,
            view_format: fmt,
            mip_level: 0,
            layer: 0,
            aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
            swizzle,
            layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
        };
        let r8 = pw::VkFormat_VK_FORMAT_R8_UNORM;
        let rg8 = pw::VkFormat_VK_FORMAT_R8G8_UNORM;
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [
                plane(
                    y_vk,
                    w,
                    h,
                    r8,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                ),
                plane(
                    cbcr_vk,
                    w / 2,
                    h / 2,
                    rg8,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                ),
                plane(
                    cbcr_vk,
                    w / 2,
                    h / 2,
                    rg8,
                    pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_G,
                ),
            ],
        };

        // Acquire the two external images (owned by the D3D11 queue family), waiting the capturer's
        // fence value so the wavelet read is ordered after the D3D11 CSC; release hands them back.
        // pyrowave owns the submission (no explicit command buffer).
        let refs = [
            pw::pyrowave_gpu_external_reference {
                image: y_img,
                queue_family_index: VK_QUEUE_FAMILY_EXTERNAL,
            },
            pw::pyrowave_gpu_external_reference {
                image: cbcr_img,
                queue_family_index: VK_QUEUE_FAMILY_EXTERNAL,
            },
        ];
        let acquire = pw::pyrowave_gpu_sync_operation {
            images: refs.as_ptr(),
            num_images: refs.len(),
            sync: pw::pyrowave_sync_point {
                semaphore: pw::pyrowave_sync_object_get_semaphore(self.sync),
                value: share.fence_value,
            },
        };
        let release = pw::pyrowave_gpu_sync_operation {
            images: refs.as_ptr(),
            num_images: refs.len(),
            // No release signal needed (null semaphore): encode is synchronous and the out-ring depth
            // guarantees the slot is not reused before the next synchronous encode completes (the same
            // contract the NVENC path relies on).
            sync: std::mem::zeroed(),
        };
        let rc = pw::pyrowave_rate_control {
            maximum_bitstream_size: self.frame_budget,
        };
        pw_check(
            pw::pyrowave_encoder_encode_gpu_synchronous(
                self.pw_enc,
                &acquire,
                &release,
                &buffers,
                &rc,
            ),
            "encode_gpu_synchronous",
        )?;

        // ---- packetize (shared framing helper — byte-identical to the Linux encoder) ----
        let cap = self.frame_budget + BS_SLACK;
        self.bitstream.resize(cap, 0);
        let boundary = pyrowave_wire::packet_boundary(self.wire_chunk, cap);
        let mut n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_compute_num_packets(self.pw_enc, boundary, &mut n),
            "compute_num_packets",
        )?;
        if n == 0 || (self.wire_chunk.is_none() && n != 1) {
            bail!("pyrowave: unexpected packet count {n} at boundary {boundary}");
        }
        let mut packets = vec![pw::pyrowave_packet { offset: 0, size: 0 }; n];
        let mut out_n: usize = 0;
        pw_check(
            pw::pyrowave_encoder_packetize(
                self.pw_enc,
                packets.as_mut_ptr(),
                boundary,
                &mut out_n,
                self.bitstream.as_mut_ptr() as *mut std::ffi::c_void,
                cap,
            ),
            "packetize",
        )?;
        packets.truncate(out_n.max(1));
        // Correct pyrowave's zeroed sequence-header VUI: it signals ycbcr_range=FULL, but our CSC
        // emits BT.709 LIMITED — patch the bit HONEST so VUI-honoring clients don't wash out blacks.
        if let Some(p) = packets.first() {
            pyrowave_wire::mark_limited_range(&mut self.bitstream, p.offset);
        }
        let pkts: Vec<(usize, usize)> = packets.iter().map(|p| (p.offset, p.size)).collect();
        let au = pyrowave_wire::build_au(&pkts, &self.bitstream, self.wire_chunk);
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: frame.pts_ns,
            // Every frame is independently decodable — the codec's whole recovery story.
            keyframe: true,
            recovery_anchor: false,
            chunk_aligned: self.wire_chunk.is_some(),
        });
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // SAFETY: single-threaded encoder; `encode_frame` records/submits on handles this struct
        // owns and pyrowave waits its own fence before packetize returns.
        unsafe { self.encode_frame(frame) }
    }

    fn caps(&self) -> EncoderCaps {
        // All defaults: no RFI (every frame is intra), no HDR (8-bit SDR codec), 4:2:0 only.
        EncoderCaps::default()
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.pop_front())
    }

    fn reset(&mut self) -> bool {
        // Cheap in-place rebuild: recreate only the pyrowave encoder object (no rate-control /
        // reference state to preserve). The device, imported textures and fence survive.
        // SAFETY: encode is synchronous (no work in flight); the device outlives the swapped encoder.
        unsafe {
            pw::pyrowave_encoder_destroy(self.pw_enc);
            let einfo = pw::pyrowave_encoder_create_info {
                device: self.pw_dev,
                width: self.width as i32,
                height: self.height as i32,
                chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            };
            let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
            let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
            if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                tracing::error!(result = ?r, "pyrowave: encoder rebuild failed");
                return false;
            }
            self.pw_enc = enc;
        }
        self.pending.clear();
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Rate control is a plain per-frame byte budget — an in-place retarget is free (no IDR,
        // nothing in flight). Phase 3 pins the session rate and bypasses ABR; this faithfully
        // applies whatever the caller asks until then.
        self.frame_budget = budget_for(bps.max(1_000_000), self.fps);
        tracing::debug!(
            mbps = bps / 1_000_000,
            budget_kib = self.frame_budget / 1024,
            "pyrowave: per-frame rate budget retargeted in place"
        );
        true
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        // Sanity floor: a boundary below one block header + payload word is meaningless.
        if shard_payload >= 64 {
            self.wire_chunk = Some(shard_payload);
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization on (partial-frame loss mode)"
            );
        }
    }

    fn flush(&mut self) -> Result<()> {
        // Synchronous per-frame encode: nothing buffered beyond `pending`.
        Ok(())
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // SAFETY: owned handles, destroyed exactly once; pyrowave objects (encoder, images, sync) go
        // before the device they borrow (per pyrowave.h).
        unsafe {
            pw::pyrowave_encoder_destroy(self.pw_enc);
            for (_, img) in self.y_images.drain(..).chain(self.cbcr_images.drain(..)) {
                pw::pyrowave_image_destroy(img);
            }
            if !self.sync.is_null() {
                pw::pyrowave_sync_object_destroy(self.sync);
            }
            pw::pyrowave_device_destroy(self.pw_dev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::dxgi::{D3d11Frame, PyroFrameShare};
    use pf_frame::PixelFormat;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4,
        ID3D11Fence, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_WRITE, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
    };

    /// Decode a dense PyroWave AU with upstream's own decoder → YUV420P plane means (the golden
    /// oracle, mirroring the Linux `decode_plane_means`).
    ///
    /// # Safety
    /// `au` must be a complete dense PyroWave AU for a `w`×`h` 4:2:0 frame.
    unsafe fn decode_plane_means(w: u32, h: u32, au: &[u8]) -> (f64, f64, f64) {
        let mut dev: pw::pyrowave_device = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_create_default_device(&mut dev),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        let dinfo = pw::pyrowave_decoder_create_info {
            device: dev,
            width: w as i32,
            height: h as i32,
            chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            fragment_path: false,
        };
        let mut dec: pw::pyrowave_decoder = std::ptr::null_mut();
        assert_eq!(
            pw::pyrowave_decoder_create(&dinfo, &mut dec),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        assert_eq!(
            pw::pyrowave_decoder_push_packet(dec, au.as_ptr() as *const _, au.len()),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        assert!(pw::pyrowave_decoder_decode_is_ready(dec, false));
        let mut y = vec![0u8; (w * h) as usize];
        let mut cb = vec![0u8; (w * h / 4) as usize];
        let mut cr = vec![0u8; (w * h / 4) as usize];
        let mut buf: pw::pyrowave_cpu_buffer = std::mem::zeroed();
        buf.format = pw::pyrowave_cpu_buffer_format_PYROWAVE_CPU_BUFFER_FORMAT_YUV420P;
        buf.width = w as i32;
        buf.height = h as i32;
        buf.data = [
            y.as_mut_ptr() as *mut _,
            cb.as_mut_ptr() as *mut _,
            cr.as_mut_ptr() as *mut _,
        ];
        buf.row_stride_in_bytes = [w as usize, (w / 2) as usize, (w / 2) as usize];
        buf.plane_size_in_bytes = [y.len(), cb.len(), cr.len()];
        assert_eq!(
            pw::pyrowave_decoder_decode_cpu_buffer_synchronous(dec, &buf),
            pw::pyrowave_result_PYROWAVE_SUCCESS
        );
        pw::pyrowave_decoder_destroy(dec);
        pw::pyrowave_device_destroy(dev);
        let mean = |v: &[u8]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
        (mean(&y), mean(&cb), mean(&cr))
    }

    /// Create a shareable `format` plane texture (`bpp` bytes/texel), fill each texel with `bytes`
    /// via a CPU staging copy, and return it. Mirrors the capturer's SHARED|SHARED_NTHANDLE +
    /// RENDER_TARGET out-ring textures.
    ///
    /// # Safety
    /// `bytes.len() == bpp`; runs on a live D3D11 device/context.
    unsafe fn make_plane(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        bpp: usize,
        bytes: &[u8],
    ) -> ID3D11Texture2D {
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0)
                as u32,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .expect("CreateTexture2D(plane default)");
        let tex = tex.unwrap();
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .expect("CreateTexture2D(plane staging)");
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .expect("Map(plane staging)");
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *mut u8;
        for row in 0..(h as usize) {
            let r = base.add(row * pitch);
            for x in 0..(w as usize) {
                for (b, &v) in bytes.iter().enumerate() {
                    *r.add(x * bpp + b) = v;
                }
            }
        }
        context.Unmap(&staging, 0);
        context.CopyResource(&tex, &staging);
        tex
    }

    /// End-to-end zero-copy smoke: distinct solid Y/Cb/Cr filled into SEPARATE shareable plane
    /// textures (full-res R8 Y + half-res R8G8 CbCr) → shared to pyrowave's own Vulkan device (the
    /// SESSION-0-relevant `create_device_by_compat` + `D3D11_TEXTURE_BIT` import + shared-fence path)
    /// → encode → upstream-decode. Returns the decoded plane means. A flat gray can't detect a plane
    /// swap / spatial error, so this fills Y≠Cb≠Cr.
    ///
    /// # Safety
    /// Runs on a real D3D11 + Vulkan-1.3 GPU; all COM/FFI handles are locally owned.
    unsafe fn run_case(w: u32, h: u32) -> (f64, f64, f64) {
        // A fresh D3D11 device on the default hardware adapter.
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .expect("D3D11CreateDevice");
        let device = device.unwrap();
        let context = context.unwrap();

        // Full-res R8 Y (=100) + half-res R8G8 CbCr (=180,60) — the exact layout the encoder ingests.
        let y_tex = make_plane(&device, &context, w, h, DXGI_FORMAT_R8_UNORM, 1, &[100]);
        let cbcr_tex = make_plane(
            &device,
            &context,
            w / 2,
            h / 2,
            DXGI_FORMAT_R8G8_UNORM,
            2,
            &[180, 60],
        );

        // Shared fence signalled after the fills (mirrors the capturer's convert→signal ordering).
        let dev5: ID3D11Device5 = device.cast().expect("ID3D11Device5");
        let mut fence: Option<ID3D11Fence> = None;
        dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
            .expect("CreateFence");
        let fence = fence.unwrap();
        let fence_handle = fence
            .CreateSharedHandle(None, 0x1000_0000, windows::core::PCWSTR::null())
            .expect("fence CreateSharedHandle");
        let ctx4: ID3D11DeviceContext4 = context.cast().expect("ID3D11DeviceContext4");
        ctx4.Signal(&fence, 1).expect("Signal");
        context.Flush();

        // Encode the shared textures through the real backend.
        let mut enc = PyroWaveEncoder::open(w, h, 60, 100_000_000).expect("PyroWaveEncoder::open");
        let frame = CapturedFrame {
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: y_tex,
                device: device.clone(),
                pyro: Some(PyroFrameShare {
                    cbcr: cbcr_tex,
                    fence_handle: Some(fence_handle.0 as isize),
                    fence_value: 1,
                }),
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("one AU per frame");
        assert!(au.keyframe, "every pyrowave AU is a keyframe");
        assert!(!au.data.is_empty(), "AU is non-empty");
        // The dense AU starts with the 8-byte BitstreamSequenceHeader; the range VUI must read
        // LIMITED (bit 30 = byte 7 bit 6 = 0x40) — `mark_limited_range` corrects pyrowave's zeroed
        // default so VUI-honoring clients (Apple) don't wash out blacks.
        assert_eq!(
            au.data[7] & 0x40,
            0x40,
            "sequence header must signal ycbcr_range=LIMITED"
        );
        decode_plane_means(w, h, &au.data)
    }

    /// The Windows NV12 zero-copy path end-to-end on a real GPU. `#[ignore]`d (needs D3D11 + a
    /// Vulkan-1.3 device); build anywhere, run on the GPU host:
    ///   cargo test -p pf-encode --features pyrowave --no-run
    ///   <bin> --ignored --nocapture pyrowave_win_smoke
    /// Runs both a known-good square size and real streaming sizes to characterize the documented
    /// NVIDIA NV12 D3D11→Vulkan import size sensitivity (design doc Risk 4 / the interop-test note).
    #[test]
    #[ignore = "needs a real D3D11 + Vulkan-1.3 GPU (run on the Windows host, not the build box)"]
    fn pyrowave_win_smoke() {
        for (w, h) in [(1024u32, 1024u32), (1280, 720), (1920, 1080), (2560, 1440)] {
            // SAFETY: single-threaded test; `run_case` owns every COM/FFI handle it touches.
            let (ym, cbm, crm) = unsafe { run_case(w, h) };
            eprintln!(
                "{w}x{h}: decoded means Y={ym:.1} Cb={cbm:.1} Cr={crm:.1} (expect 100/180/60)"
            );
            assert!(
                (ym - 100.0).abs() < 6.0 && (cbm - 180.0).abs() < 6.0 && (crm - 60.0).abs() < 6.0,
                "{w}x{h}: NV12 round-trip means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) drifted from \
                 the filled 100/180/60 — chroma plane mapping wrong (swap? wrong plane?)"
            );
        }
    }
}
