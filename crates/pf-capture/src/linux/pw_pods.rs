//! The PipeWire `EnumFormat` / `Buffers` / `Meta` param pods the capture stream offers, and the
//! `Pod` serializer they all end in.
//!
//! Split out of `linux/pipewire.rs` (sweep Phase 5.2) because it is the crate's WIRE surface: what
//! these builders put in a pod is what the compositor intersects against, and a missing property is
//! not a compile error but a link that silently stalls in `negotiating`. Nothing here touches the
//! stream, the buffers or the frames — every function is a pure `facts -> Vec<u8>`.

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use spa::param::video::VideoFormat;

pub(super) fn serialize_pod(obj: pw::spa::pod::Object) -> Result<Vec<u8>> {
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pod")?
    .0
    .into_inner())
}

/// Build a LINEAR/modifier DMA-BUF `EnumFormat` pod. Packed BGRx is the existing import path;
/// NV12 is gamescope's producer-side RGB→YUV path (opt-in during bring-up).
pub(super) fn build_dmabuf_format(
    format: VideoFormat,
    modifiers: &[u64],
    preferred: Option<(u32, u32, u32)>,
) -> Result<Vec<u8>> {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    if format == VideoFormat::NV12 {
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorMatrix,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709,
            )),
        });
        obj.properties.push(pw::spa::pod::Property {
            key: pw::spa::sys::SPA_FORMAT_VIDEO_colorRange,
            flags: pw::spa::pod::PropertyFlags::MANDATORY,
            value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
                pw::spa::sys::SPA_VIDEO_COLOR_RANGE_16_235,
            )),
        });
    }
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Long(
            pw::spa::utils::Choice(
                pw::spa::utils::ChoiceFlags::empty(),
                pw::spa::utils::ChoiceEnum::Enum {
                    default: modifiers[0] as i64,
                    alternatives: modifiers.iter().map(|&m| m as i64).collect(),
                },
            ),
        )),
    });
    serialize_pod(obj)
}

/// Build one GNOME 50+ HDR format pod: `format` (xRGB_210LE / xBGR_210LE) as a LINEAR-only
/// dmabuf with **MANDATORY** BT.2020 primaries + SMPTE ST.2084 (PQ) transfer-function props —
/// the exact colorimetry Mutter's monitor stream advertises while the mirrored monitor is in
/// HDR mode (its HDR pods carry the same props MANDATORY, so both sides must speak them for
/// the intersection to exist; an SDR or pre-50 producer can never match this pod).
///
/// LINEAR-only because every 10-bit consumer we have reads the buffer without a de-tile pass:
/// the CPU path mmaps it, and the VAAPI passthrough imports it into a VA surface. The tiled
/// EGL de-tile blit renders into an 8-bit `GL_RGBA8` texture — it would silently crush the
/// depth — so tiled modifiers are deliberately NOT advertised (a zero-copy 10-bit de-tile is
/// the follow-up). SHM is excluded entirely: Mutter's SHM record path paints 8-bit ARGB32
/// regardless of the negotiated format.
/// `SPA_VIDEO_TRANSFER_SMPTE2084` (PQ) — spelled out rather than taken from `pw::spa::sys`
/// because libspa only grew the constant with the BT2020_10/SMPTE2084/ARIB_STD_B67 block, and
/// the distro builders (Ubuntu 24.04 noble for the .deb) ship headers predating it — bindgen
/// then emits no such constant and the host fails to compile there, even though the code never
/// runs on those systems (the HDR path needs GNOME 50+).
///
/// 14 is the enum's position in `spa/param/video/color.h` and is wire ABI, not a private
/// detail: SPA mirrors GStreamer's `GstVideoTransferFunction`, where that block was added
/// together, so the value is identical on every libspa that has the symbol at all. On one that
/// doesn't, PipeWire simply fails to intersect this format offer and the session negotiates
/// SDR — the same outcome as not offering HDR.
const SPA_VIDEO_TRANSFER_SMPTE2084: u32 = 14;

/// The two 10-bit PQ formats an HDR session offers, **in negotiation order**. The order is not a
/// style choice — on NVIDIA it is the difference between correct colour and red/blue swapped.
///
/// `xBGR_210LE` (DRM `XBGR2101010`, Vulkan `A2B10G10R10_UNORM_PACK32`) comes FIRST because the
/// first compatible consumer pod wins, and it is the only one gamescope fills correctly on every
/// vendor:
///
/// * `A2R10G10B10_UNORM_PACK32` **linear-tiled storage** is an optional Vulkan feature that
///   NVIDIA does not implement. gamescope's capture textures are mappable, hence linear, so on
///   NVIDIA its composite `imageStore` into that image lands in XBGR order — the bytes come out
///   byte-reversed while the buffer is still LABELLED `XRGB2101010`.
/// * The host believes the label: `xRGB_210LE → PixelFormat::X2Rgb10 →`
///   `NV_ENC_BUFFER_FORMAT_ARGB10`. Every mapping in that chain is individually correct, which is
///   exactly why the bug is invisible from this side — the *content* is what's wrong.
/// * Upstream gamescope hit the same wall and fixed it with `vulkan_get_rgb10_capture_format()`,
///   which probes `linearTilingFeatures` for STORAGE+SAMPLED and falls back to `XBGR2101010`.
///   That landed AFTER 3.16.25, so the pinned `punktfunk-gamescope` (3.16.25-7-g60561e2 +pfhdr4)
///   predates it and cannot self-correct — hence fixing the preference host-side, where it ships
///   in the host binary with no gamescope rebuild.
///
/// Preferring xBGR costs nothing anywhere else: `A2B10G10R10_UNORM_PACK32` is the universally
/// supported packed-10 format (it is the standard HDR10 swapchain format), it is what upstream
/// falls back to, and `X2Bgr10` has a first-class encoder path (NVENC `ABGR10`, VAAPI
/// `X2BGR10LE`). `xRGB_210LE` stays as the second pod so a producer that somehow offers only it
/// can still negotiate HDR rather than falling off to the SDR downgrade.
///
/// ⚠ The real fix belongs upstream in the patch set: `spa_format_to_drm()` should offer only the
/// format `vulkan_get_rgb10_capture_format()` reports. Until the gamescope pin moves past that
/// commit, THIS ORDER is what keeps NVIDIA HDR sessions correct — do not "tidy" it.
pub(super) const HDR_FORMAT_ORDER: [VideoFormat; 2] =
    [VideoFormat::xBGR_210LE, VideoFormat::xRGB_210LE];

pub(super) fn build_hdr_dmabuf_format(
    format: VideoFormat,
    preferred: Option<(u32, u32, u32)>,
) -> Result<Vec<u8>> {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Long(0), // DRM_FORMAT_MOD_LINEAR
    });
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_transferFunction,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_VIDEO_TRANSFER_SMPTE2084)),
    });
    obj.properties.push(pw::spa::pod::Property {
        key: pw::spa::sys::SPA_FORMAT_VIDEO_colorPrimaries,
        flags: pw::spa::pod::PropertyFlags::MANDATORY,
        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(
            pw::spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020,
        )),
    });
    serialize_pod(obj)
}

/// The default (shm/CPU-path) format offer: raw video in any encoder-mappable layout, any
/// size, any framerate (0/1 = variable allowed — gamescope fixates exactly that).
pub(super) fn build_default_format_obj(preferred: Option<(u32, u32, u32)>) -> pw::spa::pod::Object {
    let (dw, dh, dhz) = preferred.unwrap_or((1920, 1080, 60));
    pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        // Offer the layouts the encoder can map to an NVENC input format. wlroots
        // commonly fixates packed RGB (3 bpp); other compositors offer 4 bpp. Only
        // these are requested, so negotiation fails loudly rather than handing us a
        // format we'd misinterpret.
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGB,
            VideoFormat::RGB,
            VideoFormat::BGR,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::BGRA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: dw,
                height: dh
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: dhz, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 240, denom: 1 }
        ),
    )
}

/// Build a Buffers param for the CPU path accepting anything mappable: MemPtr, MemFd, and
/// DmaBuf. The DmaBuf bit matters for producers like gamescope whose format intersection
/// lands on their modifier-bearing (LINEAR) pod: they then offer *only* DmaBuf buffers, and
/// without this bit the buffer-type intersection is empty and the link silently stalls in
/// "negotiating". A LINEAR dmabuf is mmap-able by MAP_BUFFERS, so the CPU de-pad copy works.
pub(super) fn build_mappable_buffers() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Int(
                (1i32 << pw::spa::sys::SPA_DATA_MemPtr)
                    | (1i32 << pw::spa::sys::SPA_DATA_MemFd)
                    | (1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
            ),
        }],
    })
}

/// Build a Buffers param for a TRUE SHM path: MemPtr + MemFd only, NO DmaBuf. Forces the
/// producer to download into mappable memory (Mutter's `glReadPixels`), which orders against its
/// render — so the frame is complete and current by construction. This is the only race-free
/// capture of Mutter's virtual monitor on NVIDIA: the compositor renders straight into the buffer
/// pool, NVIDIA attaches no implicit dmabuf fence (verified: `EXPORT_SYNC_FILE` waited=false) and
/// can't produce an explicit sync_fd, so any dmabuf read (zero-copy OR mmap) races the render and
/// flashes the buffer's previous frame. Excluding DmaBuf is what makes the difference vs.
/// `build_mappable_buffers` (which still let Mutter hand dmabufs).
pub(super) fn build_shm_only_buffers() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pw::spa::pod::Property {
            key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: pw::spa::pod::PropertyFlags::empty(),
            value: pw::spa::pod::Value::Int(
                (1i32 << pw::spa::sys::SPA_DATA_MemPtr) | (1i32 << pw::spa::sys::SPA_DATA_MemFd),
            ),
        }],
    })
}

/// PW5 stage 2: the buffer-pool depth we ASK for on the zero-copy path, as a Choice range.
///
/// The zero-copy path hands the SPA buffer back to the producer at `.process` return, while the
/// encode thread still holds a dup of its dmabuf fd and has not yet imported, let alone read, the
/// contents. Nothing bounds that window — see the `queue_raw_buffer` comment in `pipewire.rs` — so
/// the only thing that keeps capture untorn is the producer round-robining a pool deeper than our
/// import+encode latency. Until PW5 stage 1 nobody had ever counted what that pool was; we never
/// even asked for a size (`build_dmabuf_buffers` set `dataType` and nothing else).
///
/// A **range**, deliberately, not a fixed count: SPA intersects the consumer's and producer's
/// Buffers params, so a fixed 8 against a producer that can only afford 4 empties the intersection
/// and the link silently stalls in "negotiating" — the exact failure mode the cursor-meta `size`
/// property already cost this codebase once (see `build_cursor_meta_param`). With a range the
/// producer clamps into it and negotiation still succeeds.
///
/// The numbers: `min` stays at 2 so nothing that works today stops working; `default` 8 is ~133 ms
/// of buffer at 60 Hz and ~33 ms at 240 Hz, comfortably past the ~3-4 ms capture→fence latency
/// measured in PW3/PW4 even with a second frame in flight; `max` 16 is a ceiling, not a request
/// (a 4K 4:4:4 buffer is ~25 MB, so 16 is ~400 MB of compositor allocation and worth capping).
/// **What the producer actually picks is logged by the stage-1 census — trust that line, not
/// these constants.**
const POOL_MIN: i32 = 2;
const POOL_DEFAULT: i32 = 8;
const POOL_MAX: i32 = 16;

/// Build a Buffers param requesting dmabuf-only buffers, with pool headroom (see [`POOL_DEFAULT`]).
pub(super) fn build_dmabuf_buffers() -> Result<Vec<u8>> {
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: pw::spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Int(1i32 << pw::spa::sys::SPA_DATA_DmaBuf),
            },
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                    pw::spa::utils::Choice(
                        pw::spa::utils::ChoiceFlags::empty(),
                        pw::spa::utils::ChoiceEnum::Range {
                            default: POOL_DEFAULT,
                            min: POOL_MIN,
                            max: POOL_MAX,
                        },
                    ),
                )),
            },
        ],
    })
}

/// Request the compositor attach `SPA_META_Cursor` to each buffer, so the pointer travels as
/// metadata (position + an occasional bitmap) instead of being burned into the frame. Paired
/// with the portal's `CursorMode::Metadata`; producers that don't support it simply don't
/// attach it (harmless). Size is a range up to a 1024×1024 bitmap — see the note on `max` below for
/// why this is not the "bigger than any real cursor" 256² it used to be.
pub(super) fn build_cursor_meta_param() -> Result<Vec<u8>> {
    fn meta_size(w: u32, h: u32) -> i32 {
        (std::mem::size_of::<spa::sys::spa_meta_cursor>()
            + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
            + (w as usize * h as usize * 4)) as i32
    }
    serialize_pod(pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: pw::spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_type,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_Cursor)),
            },
            pw::spa::pod::Property {
                key: pw::spa::sys::SPA_PARAM_META_size,
                flags: pw::spa::pod::PropertyFlags::empty(),
                value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                    pw::spa::utils::Choice(
                        pw::spa::utils::ChoiceFlags::empty(),
                        // The max must cover the producer's offer or the Meta param silently
                        // fails to negotiate and NO buffer ever carries the meta region:
                        // Mutter offers a FIXED `SPA_POD_Int(CURSOR_META_SIZE(384, 384))`
                        // (meta-screen-cast-stream-src.c, GNOME 50) — a 256² max made the
                        // intersection empty, which cost the whole Linux cursor channel
                        // on-glass. 1024² is headroom, not an allocation: the negotiated
                        // region follows the producer's value.
                        pw::spa::utils::ChoiceEnum::Range {
                            default: meta_size(64, 64),
                            min: meta_size(1, 1),
                            max: meta_size(1024, 1024),
                        },
                    ),
                )),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `SPA_PARAM_BUFFERS_dataType` bitmask a serialized Buffers pod carries.
    ///
    /// A deliberately literal SPA reader rather than a heuristic scan: an object property is
    /// `{ key: u32, flags: u32, value: spa_pod }` and a `spa_pod` is `{ size: u32, type: u32, body }`,
    /// so the `i32` sits exactly 16 bytes past the key — and the intervening `size` word is itself
    /// `4`, which is why "find the first plausible-looking int" reads the wrong field.
    fn buffers_data_type(pod: &[u8]) -> i32 {
        let key = spa::sys::SPA_PARAM_BUFFERS_dataType.to_ne_bytes();
        let at = pod
            .windows(4)
            .position(|w| w == key)
            .expect("dataType key present in the Buffers pod");
        let word = |off: usize| u32::from_ne_bytes(pod[off..off + 4].try_into().unwrap());
        assert_eq!(word(at + 8), 4, "dataType's value pod should be 4 bytes");
        assert_eq!(
            word(at + 12),
            spa::sys::SPA_TYPE_Int,
            "dataType's value pod should be an Int"
        );
        i32::from_ne_bytes(pod[at + 16..at + 20].try_into().unwrap())
    }

    const MEM_PTR: i32 = 1 << spa::sys::SPA_DATA_MemPtr;
    const MEM_FD: i32 = 1 << spa::sys::SPA_DATA_MemFd;
    const DMABUF: i32 = 1 << spa::sys::SPA_DATA_DmaBuf;

    /// The three Buffers pods differ ONLY in this bitmask, and each bit is load-bearing:
    /// `build_mappable_buffers` must include DmaBuf or gamescope's modifier-bearing pod wins the
    /// format intersection and the BUFFER intersection is then empty (a link stuck in
    /// "negotiating"); `build_shm_only_buffers` must EXCLUDE it or Mutter hands dmabufs and the
    /// race-free download path is not race-free; `build_dmabuf_buffers` must exclude the mappable
    /// types or an HDR session can be handed a MemFd buffer, which Mutter paints 8-bit ARGB32
    /// regardless of the negotiated 10-bit format.
    #[test]
    fn each_buffers_pod_requests_exactly_its_own_data_types() {
        assert_eq!(
            buffers_data_type(&build_mappable_buffers().unwrap()),
            MEM_PTR | MEM_FD | DMABUF,
            "the CPU path must accept mappable dmabufs too"
        );
        assert_eq!(
            buffers_data_type(&build_shm_only_buffers().unwrap()),
            MEM_PTR | MEM_FD,
            "PUNKTFUNK_FORCE_SHM must exclude DmaBuf"
        );
        assert_eq!(
            buffers_data_type(&build_dmabuf_buffers().unwrap()),
            DMABUF,
            "the zero-copy/HDR path must exclude SHM"
        );
    }

    /// Every pod builder must produce a pod libspa will accept back — a serializer that silently
    /// emitted a malformed object would fail only at negotiation, on a live compositor.
    #[test]
    fn every_pod_round_trips_through_pod_from_bytes() {
        let mut pods: Vec<(&str, Vec<u8>)> = vec![
            ("mappable buffers", build_mappable_buffers().unwrap()),
            ("shm-only buffers", build_shm_only_buffers().unwrap()),
            ("dmabuf buffers", build_dmabuf_buffers().unwrap()),
            ("cursor meta", build_cursor_meta_param().unwrap()),
            (
                "default format",
                serialize_pod(build_default_format_obj(None)).unwrap(),
            ),
            (
                "dmabuf BGRx",
                build_dmabuf_format(VideoFormat::BGRx, &[0, 1, 2], Some((1920, 1080, 60))).unwrap(),
            ),
            (
                "dmabuf NV12",
                build_dmabuf_format(VideoFormat::NV12, &[0], Some((1280, 720, 60))).unwrap(),
            ),
            (
                "hdr xRGB",
                build_hdr_dmabuf_format(VideoFormat::xRGB_210LE, None).unwrap(),
            ),
            (
                "hdr xBGR",
                build_hdr_dmabuf_format(VideoFormat::xBGR_210LE, Some((3840, 2160, 120))).unwrap(),
            ),
        ];
        for (name, bytes) in &mut pods {
            assert!(!bytes.is_empty(), "{name} serialized to nothing");
            assert_eq!(bytes.len() % 8, 0, "{name} is not 8-byte aligned/padded");
            assert!(
                spa::pod::Pod::from_bytes(bytes).is_some(),
                "{name} did not parse back as a pod"
            );
        }
    }

    /// The HDR pods carry BOTH colorimetry properties MANDATORY — Mutter's HDR pods do the same, so
    /// the intersection only exists if we speak them. Dropping either would negotiate an SDR-labelled
    /// 10-bit stream (or nothing at all).
    #[test]
    fn the_hdr_pods_carry_mandatory_pq_and_bt2020() {
        for fmt in [VideoFormat::xRGB_210LE, VideoFormat::xBGR_210LE] {
            let pod = build_hdr_dmabuf_format(fmt, None).unwrap();
            for (name, key) in [
                (
                    "transferFunction",
                    spa::sys::SPA_FORMAT_VIDEO_transferFunction,
                ),
                ("colorPrimaries", spa::sys::SPA_FORMAT_VIDEO_colorPrimaries),
                ("modifier", spa::sys::SPA_FORMAT_VIDEO_modifier),
            ] {
                assert!(
                    pod.windows(4).any(|w| w == key.to_ne_bytes()),
                    "{fmt:?} pod is missing {name}"
                );
            }
            // The PQ id and BT.2020 id must both appear as values.
            assert!(
                pod.windows(4)
                    .any(|w| w == SPA_VIDEO_TRANSFER_SMPTE2084.to_ne_bytes()),
                "{fmt:?} pod does not carry the PQ transfer id"
            );
            assert!(
                pod.windows(4)
                    .any(|w| w == spa::sys::SPA_VIDEO_COLOR_PRIMARIES_BT2020.to_ne_bytes()),
                "{fmt:?} pod does not carry BT.2020 primaries"
            );
        }
    }

    /// An NV12 offer pins BT.709 limited so gamescope's producer-side RGB→YUV shader matches OUR
    /// bitstream colorimetry; the packed-RGB offer must NOT carry those (it is not YUV).
    #[test]
    fn only_the_nv12_offer_pins_the_colour_matrix() {
        let nv12 = build_dmabuf_format(VideoFormat::NV12, &[0], None).unwrap();
        let bgrx = build_dmabuf_format(VideoFormat::BGRx, &[0], None).unwrap();
        for (name, key) in [
            ("colorMatrix", spa::sys::SPA_FORMAT_VIDEO_colorMatrix),
            ("colorRange", spa::sys::SPA_FORMAT_VIDEO_colorRange),
        ] {
            assert!(
                nv12.windows(4).any(|w| w == key.to_ne_bytes()),
                "NV12 offer is missing {name}"
            );
            assert!(
                !bgrx.windows(4).any(|w| w == key.to_ne_bytes()),
                "packed-RGB offer should not pin {name}"
            );
        }
    }

    /// Pin our hand-written PQ transfer id against the real libspa binding. We can't take the
    /// constant from `pw::spa::sys` directly (older distro headers don't export it — see
    /// [`super::SPA_VIDEO_TRANSFER_SMPTE2084`]), so assert the two agree wherever the symbol
    /// DOES exist. Any libspa that renumbers the enum fails this instead of silently tagging
    /// the HDR offer with the wrong transfer function.
    ///
    /// Only builds where tests are compiled — the .deb/.rpm builders run plain `cargo build`,
    /// so this never reintroduces the compile failure it exists to prevent.
    #[test]
    fn pq_transfer_id_matches_libspa() {
        assert_eq!(
            super::SPA_VIDEO_TRANSFER_SMPTE2084,
            super::pw::spa::sys::SPA_VIDEO_TRANSFER_SMPTE2084,
            "libspa renumbered spa_video_transfer_function — update the hardcoded PQ id"
        );
    }

    /// PW5 stage 2: the pool request must be a **Choice Range**, never a fixed Int.
    ///
    /// This is the whole safety argument for asking at all: SPA intersects the two sides' Buffers
    /// params, so a fixed count a producer cannot afford empties the intersection and the link
    /// stalls in "negotiating" with no error anywhere — the same trap that cost this codebase the
    /// entire Linux cursor channel once (see `build_cursor_meta_param`). Asserting the pod shape
    /// is what keeps a later "simplify" from turning the range back into a number.
    #[test]
    fn the_dmabuf_pool_request_is_a_range_not_a_fixed_count() {
        let pod = build_dmabuf_buffers().unwrap();
        let key = spa::sys::SPA_PARAM_BUFFERS_buffers.to_ne_bytes();
        let at = pod
            .windows(4)
            .position(|w| w == key)
            .expect("the dmabuf Buffers pod must carry a buffers count");
        let word = |off: usize| u32::from_ne_bytes(pod[off..off + 4].try_into().unwrap());
        // Property = { key, flags, value_pod }; value_pod = { size, type, body }. A Choice body
        // is { type: u32, flags: u32, child_size: u32, child_type: u32, values… }.
        assert_eq!(
            word(at + 12),
            spa::sys::SPA_TYPE_Choice,
            "the buffers count must be a Choice, not a bare Int — a fixed count can fail \
             negotiation outright"
        );
        assert_eq!(
            word(at + 16),
            spa::sys::SPA_CHOICE_Range,
            "the Choice must be a Range (default, min, max)"
        );
        assert_eq!(word(at + 24), 4, "Choice child pods are 4-byte Ints");
        assert_eq!(word(at + 28), spa::sys::SPA_TYPE_Int, "…of type Int");
        let vals: Vec<i32> = (0..3)
            .map(|i| i32::from_ne_bytes(pod[at + 32 + i * 4..at + 36 + i * 4].try_into().unwrap()))
            .collect();
        assert_eq!(
            vals,
            vec![POOL_DEFAULT, POOL_MIN, POOL_MAX],
            "Range values are serialized default-first"
        );
        // The minimum must not exceed what producers already serve, or the ask becomes a demand.
        const { assert!(POOL_MIN <= 2) };
    }

    /// xBGR_210LE must be offered FIRST, and this is a correctness test, not a style one.
    ///
    /// The first compatible consumer pod wins the negotiation. Leading with `xRGB_210LE` makes an
    /// NVIDIA gamescope session land on `XRGB2101010`, whose linear-tiled `A2R10G10B10` storage
    /// NVIDIA does not support — gamescope's composite `imageStore` writes XBGR bytes under an
    /// XRGB label and the whole stream comes out with red and blue swapped. Every format mapping
    /// on the host side is individually correct, so nothing downstream can detect it.
    ///
    /// Field-confirmed 2026-08-09 on the RTX 5070 Ti Bazzite host with 0.26.0. See the
    /// [`HDR_FORMAT_ORDER`] docs for the upstream fix this predates.
    #[test]
    fn hdr_offers_xbgr_before_xrgb() {
        assert_eq!(
            HDR_FORMAT_ORDER[0],
            VideoFormat::xBGR_210LE,
            "xBGR_210LE must be offered first — leading with xRGB_210LE swaps red and blue on \
             every NVIDIA gamescope HDR session"
        );
        assert_eq!(
            HDR_FORMAT_ORDER[1],
            VideoFormat::xRGB_210LE,
            "xRGB_210LE stays as the fallback pod so a producer offering only it can still \
             negotiate HDR instead of dropping to the SDR downgrade"
        );
        // Both must still build: the order is a preference, never a removal.
        for fmt in HDR_FORMAT_ORDER {
            assert!(
                !build_hdr_dmabuf_format(fmt, None).unwrap().is_empty(),
                "{fmt:?} must still produce a format pod"
            );
        }
    }
}
