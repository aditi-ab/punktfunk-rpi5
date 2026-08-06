//! Native D3D11VA decode — M5 of the native-decode program: `ID3D11VideoDecoder` driven
//! straight from pf-bitstream's per-AU plans, with no libavcodec anywhere in the path.
//!
//! It is the DXVA counterpart of `video_vk_native` and it replaces exactly one half of
//! [`crate::video_d3d11`]: what WRITES the decode surface. The other half — the fixed-function
//! `ID3D11VideoProcessor` blitting NV12/P010 into a ring of shareable RGBA textures the
//! presenter imports by NT handle ([`HandoffRing`]) — is shared code, byte for byte, because
//! it is the field-proven half (the NVIDIA NV12-import TDR that forced RGB, the Intel green
//! bar that forced the stream source rect, the key-0 keyed-mutex protocol). This rung
//! therefore is NOT zero-copy, and deliberately so: that constraint governs the Vulkan path,
//! where the decoded image IS the presented image.
//!
//! # Admission
//!
//! Explicit pin only — `PUNKTFUNK_DECODER=native-d3d11va`. It is NOT in the automatic ladder
//! and must not be until it has hardware evidence: M2's native Vulkan rung was admitted to
//! `auto` only after WP-D closed bit-exact against libavcodec on three drivers plus a
//! 92-minute soak, and this rung has decoded nothing yet. A refusal or an init failure logs
//! and falls through to the standard ladder, so the pin can never cost a session its decoder.
//!
//! # The decode pool — the part that has already failed once
//!
//! [`crate::video_d3d11`]'s module docs record it plainly: a **hand-built decode pool
//! validated on NVIDIA was rejected by Intel at the first `SubmitDecoderBuffers`**, which is
//! why the FFmpeg rung leaves the pool to libavcodec. A native decoder has no such luxury —
//! it must own its pool — so this is the highest-risk code in the milestone, and the answer
//! is not to invent a pool but to reproduce libavcodec's exactly. What that path does, from
//! `ff_dxva2_common_frame_params` and `d3d11va_frames_init`:
//!
//! * **ONE `ID3D11Texture2D` with `ArraySize = pool size`**, not N individual textures. The
//!   array slice is the DXVA surface index, which is what makes `DXVA_PicEntry::Index7Bits`
//!   and the DPB slot the same number.
//! * **`BindFlags = D3D11_BIND_DECODER`, and nothing else.** Not `SHADER_RESOURCE`, not
//!   `RENDER_TARGET`: a decode pool that also claims a sampling bind flag is precisely the
//!   sort of request a driver may honour on one vendor and reject on another. The hand-off's
//!   `CreateVideoProcessorInputView` needs no bind flag at all.
//! * **`MiscFlags = 0`** — no sharing. The shareable textures are the RGBA ring's, on the
//!   other side of the video processor.
//! * **Dimensions aligned to the codec's granule** (16 for H.264, 128 for HEVC —
//!   [`pf_dxvadec::align_surface`]), so the surface is TALLER than the frame. That padding is
//!   the green bar the hand-off's stream source rect already excludes. The alignment applies
//!   to the TEXTURE only: `D3D11_VIDEO_DECODER_DESC` gets the CODED size, exactly as
//!   `d3d11va_create_decoder` passes `avctx->coded_width/coded_height` while
//!   `ff_dxva2_common_frame_params` allocates at `FFALIGN(coded, surface_alignment)`.
//! * **`Usage = D3D11_USAGE_DEFAULT`, `MipLevels = 1`, `SampleDesc.Count = 1`**, format NV12
//!   or P010 per profile.
//!
//! Everything else about pool sizing is [`pf_dxvadec::pool_size`], which is unit-tested; the
//! driver's own `ConfigMinRenderTargetBuffCount` is honoured there.
//!
//! # What is decided here vs decided in pf-dxvadec
//!
//! Nothing in this file can be tested by any gate this program runs — it is `cfg(windows)`,
//! so neither the macOS host nor the Linux container compiles it, and the Windows box only
//! `cargo check`s. Every decision that could be a pure function therefore lives in
//! [`pf_dxvadec`] with unit tests: the DXVA buffer layouts, the profile table, the
//! decoder-config choice, the surface alignment, the pool size, the bitstream packing rules,
//! and the whole plan → picparams/qmatrix/slice-control conversion. What is left here is
//! enumeration, allocation and submission — the parts that genuinely need a device.

use anyhow::{anyhow, bail, Context as _, Result};
use pf_dxvadec::{Codec, DxvaProfile};
use windows::core::{Interface, GUID};
use windows::Win32::d3d11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDecoder,
    ID3D11VideoDecoderOutputView, ID3D11VideoDevice, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VDOV_DIMENSION_TEXTURE2D, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
    D3D11_VIDEO_DECODER_BUFFER_DESC, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
    D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
    D3D11_VIDEO_DECODER_BUFFER_TYPE, D3D11_VIDEO_DECODER_CONFIG, D3D11_VIDEO_DECODER_DESC,
    D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC,
};
use windows::Win32::dxgi::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use crate::video::{ColorDesc, DecodeHealth, StreamFormat};
use crate::video_d3d11::{create_device, D3d11Frame, HandoffRing, HandoffSource};

/// `D3D11_BIND_DECODER` — the decode pool's ONLY bind flag (module docs).
const BIND_DECODER: u32 = 0x200;

/// `DecoderBeginFrame` answers `E_PENDING` while the hardware is still busy with an earlier
/// picture. libavcodec's `ff_dxva2_common_end_frame` retries up to 50 times, sleeping
/// `av_usleep(2000)` between attempts — a hundred milliseconds in total, and these two
/// constants are that budget rather than a smaller one of our own.
///
/// A shorter budget looks safer and is not: `E_PENDING` means the hardware is BUSY, not
/// wedged, and a 4K decoder that needs longer than the budget gets an `Err` — which ticks
/// the ladder's demotion streak for the offence of being busy. The retry loop only ever runs
/// while the decoder is working, so the wait is bounded by the work in flight; a genuinely
/// wedged decoder still surfaces, a tenth of a second later.
const BEGIN_FRAME_RETRIES: u32 = 50;
const BEGIN_FRAME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(2);
/// `E_PENDING`.
const E_PENDING: i32 = 0x8000_000A_u32 as i32;

/// The environment value that pins this rung.
pub(crate) const DECODER_PIN: &str = "native-d3d11va";

/// One codec's planning state. The negotiated codec picks it once, at construction — the
/// same shape `video_vk_native`'s `Codec` has, and for the same reason: everything below
/// the plan is codec-agnostic, so forking the session/pool/submission machinery per codec
/// would fork the part that is hardest to get right.
enum Planner {
    H264(Box<pf_dxvadec::H264Planner>),
    H265(Box<pf_dxvadec::H265Planner>),
}

/// What one planned AU produced, reduced to the codec-agnostic facts submission needs.
struct Submission {
    /// The DXVA picture-parameters buffer, as bytes.
    pic_params: Vec<u8>,
    /// The DXVA inverse-quantization-matrix buffer, as bytes — `None` when the buffer must
    /// NOT be submitted (HEVC with `scaling_list_enabled_flag` clear, which is every
    /// punktfunk HEVC stream; see `pf_dxvadec::DecodePlanDxvaH265::qmatrix`).
    qmatrix: Option<Vec<u8>>,
    /// `NumMBsInBuffer` for the bitstream and slice-control descriptors: the coded picture
    /// in macroblocks on the H.264 path, 0 on the HEVC one. Both are libavcodec's values
    /// (`commit_bitstream_and_slice_buffer` in `dxva2_h264.c` and `dxva2_hevc.c`).
    mb_count: u32,
    /// Slice NALU ranges within the AU, for the bitstream packer.
    slice_ranges: Vec<std::ops::Range<usize>>,
    /// The surface (array slice) the picture decodes into.
    setup_slot: u8,
    /// Which codec's slice-control record the packer's locations become.
    codec: Codec,
    /// The picture's colour signalling and keyframe-ness, for the hand-off.
    colour: ColorDesc,
    keyframe: bool,
    /// Display size (the conformance-window crop), which is what the hand-off blits.
    width: u32,
    height: u32,
    /// The plan carried an integrity warning: a reference the DPB no longer held, a
    /// `frame_num` gap, a NALU walk that stopped early. The picture would be decoded from a
    /// substitute, so it is never submitted — see [`NativeD3d11Decoder::decode`].
    concealed: bool,
}

/// Everything about the stream that a decode session is BUILT FROM — the session's identity,
/// read off the SPS the planner just activated rather than off the negotiated format.
///
/// Every field here decides an object that cannot be changed after creation: the coded size
/// and the DPB depth size the decoder, the pool and the slot map; the chroma format and the
/// luma bit depth pick the profile GUID and with it the surfaces' `DXGI_FORMAT`. A change in
/// any of them is a renegotiation, and the session is rebuilt WHOLE — a half-rebuilt session
/// hands out surface indices its pool does not have, or decodes 10-bit samples into 8-bit
/// surfaces.
///
/// That last one is not hypothetical: `colour_of`'s docs record that the Windows host flips
/// an HDR desktop to PQ/BT.2020 in-band with a new SPS mid-stream. An SPS that also moved the
/// luma depth 8 → 10 at an unchanged coded size would, if this struct held only the size and
/// the depth, leave an `HEVC_VLD_MAIN` decoder writing into an NV12 pool while the picture
/// parameters told the driver the samples are ten bits wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamShape {
    coded_width: u32,
    coded_height: u32,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
}

impl StreamShape {
    fn bit_depth(&self) -> u8 {
        8 + self.bit_depth_luma_minus8
    }
}

/// The live decoder plus everything sized to the stream it was built for. Rebuilt whole on a
/// renegotiation (any [`StreamShape`] change), because every one of these is derived from the
/// SPS and a half-rebuilt decoder is the shape of a corrupt reference.
struct Session {
    decoder: ID3D11VideoDecoder,
    /// The decode pool: ONE texture array (module docs), kept alive for the session and
    /// handed to the video processor as the blit source.
    pool: ID3D11Texture2D,
    /// One output view per array slice — `DecoderBeginFrame`'s target.
    views: Vec<ID3D11VideoDecoderOutputView>,
    slots: pf_dxvadec::SlotMap,
    /// The SPS facts this session was built from; anything else is a rebuild.
    shape: StreamShape,
    /// The profile [`StreamShape::chroma_format_idc`] and the luma depth chose — which is
    /// not necessarily the one the NEGOTIATED format chose at construction.
    profile: DxvaProfile,
}

pub(crate) struct NativeD3d11Decoder {
    /// Kept for pool creation on a renegotiation.
    device: ID3D11Device,
    /// Kept so the session's teardown/rebuild happens on a live context; the hand-off holds
    /// its own clone for the blit.
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// The decoder, its surface pool and its slot map, sized to the stream. Declared BEFORE
    /// `handoff` so it drops first: Rust drops fields in declaration order, and the ring must
    /// outlive the decode surfaces whose contents it converted — the same ordering the FFmpeg
    /// rung gets by freeing its codec context in `Drop` before its `handoff` field falls.
    session: Option<Session>,
    /// The shared `VideoProcessorBlt` → shareable-RGBA hand-off.
    handoff: HandoffRing,
    planner: Planner,
    codec: Codec,
    /// `StatusReportFeedbackNumber`, monotonic from 1 — 0 is what a driver reads out of a
    /// buffer nobody wrote, so it is never a legitimate tag.
    status_id: u32,
    health: DecodeHealth,
    want_recovery: bool,
}

// SAFETY: every field is either owned plain data or a reference-counted COM interface with
// interlocked counts, so moving the whole struct to another thread and releasing it there is
// sound. D3D11's immediate context is not thread-SAFE but it is thread-AGNOSTIC: it requires
// serialised use, which `&mut self` on every method gives, not use from one fixed thread. The
// presenter never touches these objects — it reaches the shared textures through their NT
// handles on its own device. Moved, never shared; deliberately NOT `Sync`. (Identical
// argument to `D3d11vaDecoder`'s, and for the identical reason.)
unsafe impl Send for NativeD3d11Decoder {}

impl NativeD3d11Decoder {
    /// Build the decoder on the presenter's adapter.
    ///
    /// Everything that can fail as a REFUSAL fails here, before a single AU: the codec, the
    /// negotiated picture shape, the adapter's profile list, and the decoder config. That is
    /// the ladder's cheap exit — a construction failure falls through to the next rung with a
    /// clean stream, where a first-AU failure would burn the opening IDR and only exit
    /// through an error-streak demotion.
    ///
    /// The DECODER itself is not created here: its `D3D11_VIDEO_DECODER_DESC` needs the coded
    /// picture size, which only the in-band SPS knows. The negotiated [`StreamFormat`] is
    /// enough to pick a profile, and that profile is enough to prove the adapter can decode
    /// this session at all — but it is NOT the profile the session is built with. That one is
    /// derived per session from the SPS ([`StreamShape`]), because the negotiated format and
    /// the in-band one can disagree, and when they do the SPS is the one that decodes.
    pub(crate) fn new(
        codec: Codec,
        stream: StreamFormat,
        luid: Option<[u8; 8]>,
        hdr10_out: bool,
    ) -> Result<NativeD3d11Decoder> {
        let profile = pf_dxvadec::profile_for(codec, stream.chroma_format_idc, stream.bit_depth)
            .ok_or_else(|| {
                anyhow!(
                    "no DXVA profile for {codec:?} chroma_format_idc {} at {} bits",
                    stream.chroma_format_idc,
                    stream.bit_depth
                )
            })?;
        let (device, context) = create_device(luid)?;
        let handoff = HandoffRing::new(device.clone(), context.clone(), hdr10_out)?;
        let video_device = handoff.video_device().clone();
        let video_context: ID3D11VideoContext = context
            .cast()
            .context("context lacks ID3D11VideoContext (created without VIDEO_SUPPORT)")?;
        profile_supported(&video_device, profile)?;
        let planner = match codec {
            Codec::H264 => Planner::H264(Box::new(pf_dxvadec::H264Planner::new())),
            Codec::H265 => Planner::H265(Box::new(pf_dxvadec::H265Planner::new())),
        };
        tracing::info!(
            ?codec,
            negotiated_profile = profile.name,
            chroma = stream.chroma_format_idc,
            bits = stream.bit_depth,
            "native D3D11VA decoder built (pf-dxvadec, pinned)"
        );
        Ok(NativeD3d11Decoder {
            device,
            context,
            video_device,
            video_context,
            session: None,
            handoff,
            planner,
            codec,
            status_id: 0,
            // No per-operation status query exists in D3D11VA the way Vulkan Video's
            // `RESULT_STATUS_ONLY` does — `ID3D11VideoContext` exposes no per-picture status
            // read at all — so `failed` can only ever be 0 here and the flag says so
            // honestly. A report that cannot tell "clean" from "unmeasured" is the founding
            // failure of this program; claiming query support we do not have would recreate
            // it exactly.
            health: DecodeHealth {
                status_queries: false,
                ..DecodeHealth::default()
            },
            want_recovery: false,
        })
    }

    /// The rung's name, for the logs a field report leans on.
    pub(crate) fn name(&self) -> &'static str {
        DECODER_PIN
    }

    /// This session's decode integrity — see [`DecodeHealth`].
    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    /// Drain the "this stream needs a keyframe" request raised by concealment.
    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.want_recovery)
    }

    /// Plan, convert and submit one access unit.
    ///
    /// The three answers, and why they differ:
    /// * `Ok(Some(frame))` — a picture, converted into the hand-off ring.
    /// * `Ok(None)` — nothing to show, and NOT an error: an AU whose plan needed concealment
    ///   (its picture is not fit to present, so it is dropped and recovery is requested), or
    ///   an HEVC RASL picture skipped after an open-GOP join (the spec's own answer, 8.1.3
    ///   NOTE). Making either an `Err` would tick the demotion streak on exactly the lossy
    ///   links and open-GOP joins this rung exists to handle.
    /// * `Err` — the decoder could not run. Streak-eligible, counted as `refused`.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<D3d11Frame>> {
        let submission = match self.plan(au) {
            Ok(Some(submission)) => submission,
            // A skipped RASL picture: no plan, no error, nothing to show. It costs no
            // health entry either — the decoder was never fed.
            Ok(None) => return Ok(None),
            Err(e) => {
                self.health.note(false, true, 0);
                tracing::warn!(error = %format!("{e:#}"), "native D3D11VA refused the access unit");
                return Err(e);
            }
        };
        if submission.concealed {
            // The plan needed a substitute for something lost. Fold it, ask for recovery,
            // and do NOT submit: a concealed picture is not fit to present, and submitting
            // it would put a wrong reference in the DPB for every AU after it.
            self.health.note(true, false, 0);
            self.want_recovery = true;
            return Ok(None);
        }
        let frame = match self.submit(au, &submission) {
            Ok(frame) => frame,
            Err(e) => {
                self.health.note(false, true, 0);
                tracing::warn!(error = %format!("{e:#}"), "native D3D11VA submission failed");
                return Err(e);
            }
        };
        self.health.note(false, false, 0);
        Ok(Some(frame))
    }

    /// Plan one AU and convert it, (re)building the session when the stream's shape moved.
    ///
    /// `Ok(None)` is the RASL skip and nothing else.
    fn plan(&mut self, au: &[u8]) -> Result<Option<Submission>> {
        self.status_id = self.status_id.wrapping_add(1).max(1);
        let status_id = self.status_id;
        match &mut self.planner {
            Planner::H264(planner) => {
                let plan = planner.plan_au(au).map_err(|e| anyhow!("plan: {e}"))?;
                let concealed = plan.warnings.iter().any(pf_dxvadec::is_integrity_warning);
                let session = ensure_session(
                    &mut self.session,
                    &self.device,
                    &self.video_device,
                    self.codec,
                    StreamShape {
                        coded_width: plan.picture.coded_width,
                        coded_height: plan.picture.coded_height,
                        max_dpb_frames: plan.picture.max_dpb_frames,
                        chroma_format_idc: plan.picture.chroma_format_idc,
                        bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
                        bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
                    },
                )?;
                let dxva = pf_dxvadec::plan_to_dxva(&plan, &mut session.slots, status_id)
                    .map_err(|e| anyhow!("plan → DXVA: {e}"))?;
                Ok(Some(Submission {
                    pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                    // H.264 always carries the matrices: libavcodec's `dxva2_h264_end_frame`
                    // submits the buffer unconditionally, and the PPS's lists are always
                    // meaningful (the parser has applied Table 7-2's fallback rules).
                    qmatrix: Some(pf_dxvadec::as_bytes(&dxva.qmatrix).to_vec()),
                    mb_count: dxva.mb_count,
                    slice_ranges: dxva.slice_ranges,
                    setup_slot: dxva.setup_slot,
                    codec: Codec::H264,
                    colour: colour_of(plan.picture.colour),
                    keyframe: plan.picture.is_idr,
                    width: plan.picture.display_crop.width,
                    height: plan.picture.display_crop.height,
                    concealed,
                }))
            }
            Planner::H265(planner) => {
                let plan = match planner.plan_au(au) {
                    Ok(plan) => plan,
                    // An HEVC stream joined at a CRA carries leading pictures whose
                    // references precede the join; the spec's answer is to decode and output
                    // nothing for them. Never an error — mapping it to one would make every
                    // open-GOP join beg the host for a keyframe it has no reason to send.
                    Err(pf_dxvadec::PlanErrorH265::RaslSkipped { poc }) => {
                        tracing::debug!(poc, "RASL picture skipped after an open-GOP join");
                        return Ok(None);
                    }
                    Err(e) => bail!("plan: {e}"),
                };
                let concealed = plan
                    .warnings
                    .iter()
                    .any(pf_dxvadec::is_integrity_warning_h265);
                let session = ensure_session(
                    &mut self.session,
                    &self.device,
                    &self.video_device,
                    self.codec,
                    StreamShape {
                        coded_width: plan.picture.coded_width,
                        coded_height: plan.picture.coded_height,
                        max_dpb_frames: plan.picture.max_dpb_frames,
                        chroma_format_idc: plan.picture.chroma_format_idc,
                        bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
                        bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
                    },
                )?;
                let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, &mut session.slots, status_id)
                    .map_err(|e| anyhow!("plan → DXVA: {e}"))?;
                Ok(Some(Submission {
                    pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                    // `None` unless the sequence enables scaling lists — the buffer is then
                    // not submitted at all, which is libavcodec's own condition.
                    qmatrix: dxva
                        .qmatrix
                        .as_ref()
                        .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
                    // libavcodec's HEVC path leaves `NumMBsInBuffer` 0: HEVC has no
                    // macroblocks, and the field has no CTB spelling.
                    mb_count: 0,
                    slice_ranges: dxva.slice_ranges,
                    setup_slot: dxva.setup_slot,
                    codec: Codec::H265,
                    colour: colour_of(plan.picture.colour),
                    keyframe: plan.picture.is_irap,
                    width: plan.picture.display_crop.width,
                    height: plan.picture.display_crop.height,
                    concealed,
                }))
            }
        }
    }

    /// `DecoderBeginFrame` → four buffers → `SubmitDecoderBuffers` → `DecoderEndFrame`, then
    /// the shared hand-off.
    ///
    /// Buffer order matches libavcodec's exactly (picture parameters, quantization matrices,
    /// bitstream, slice control): a driver is entitled to care, and matching the path every
    /// Windows player exercises costs nothing.
    fn submit(&mut self, au: &[u8], sub: &Submission) -> Result<D3d11Frame> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("no decode session (plan should have built one)"))?;
        let view = session
            .views
            .get(usize::from(sub.setup_slot))
            .ok_or_else(|| anyhow!("setup surface {} is outside the pool", sub.setup_slot))?;

        begin_frame(&self.video_context, &session.decoder, view)?;
        // From here the decoder is INSIDE a frame; every exit must end it, or the next AU's
        // `DecoderBeginFrame` fails and the session is wedged. `end_frame` is therefore
        // called on both paths rather than only on success.
        let result = self.fill_and_submit(au, sub, session);
        // SAFETY: a COM call on the live video context, ending the frame this method began on
        // the live decoder. Its own failure is reported only when nothing worse happened.
        let ended = unsafe { self.video_context.DecoderEndFrame(&session.decoder) };
        result?;
        ended.ok().context("DecoderEndFrame")?;

        // The hand-off. `pool` is the decode texture array and `setup_slot` its slice — the
        // very shape libavcodec's `data[0]`/`data[1]` describe, which is why this is the same
        // call the FFmpeg rung makes.
        let pool = session.pool.clone();
        self.handoff.present(HandoffSource {
            texture: &pool,
            array_slice: u32::from(sub.setup_slot),
            width: sub.width,
            height: sub.height,
            color: sub.colour,
            keyframe: sub.keyframe,
            decoder: DECODER_PIN,
        })
    }

    /// The four decoder buffers, filled and submitted. Split out so the caller can guarantee
    /// `DecoderEndFrame` on every path.
    fn fill_and_submit(&self, au: &[u8], sub: &Submission, session: &Session) -> Result<()> {
        let mut descs: Vec<D3D11_VIDEO_DECODER_BUFFER_DESC> = Vec::with_capacity(4);

        let pp_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            |dst| {
                copy_into(dst, &sub.pic_params)?;
                Ok(sub.pic_params.len())
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            pp_size,
            0,
        ));

        // The quantization matrices, when the stream has any. An HEVC sequence with scaling
        // lists disabled submits NO such buffer — libavcodec's condition exactly — because
        // the picture parameters have already told the driver to ignore the matrix, and a
        // driver that honours what it was handed anyway would dequantize against it.
        if let Some(qmatrix) = &sub.qmatrix {
            let qm_size = write_buffer(
                &self.video_context,
                &session.decoder,
                D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                |dst| {
                    copy_into(dst, qmatrix)?;
                    Ok(qmatrix.len())
                },
            )?;
            descs.push(buffer_desc(
                D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                qm_size,
                0,
            ));
        }

        // The bitstream buffer is packed IN PLACE in the driver's mapping — no staging copy —
        // and hands back the slice locations the control buffer below is built from. That
        // ordering is why the two cannot be one step.
        let mut packed = None;
        let bs_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            |dst| {
                let p = pf_dxvadec::pack(au, &sub.slice_ranges, dst)
                    .map_err(|e| anyhow!("bitstream pack: {e}"))?;
                let size = p.data_size as usize;
                packed = Some(p);
                Ok(size)
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            bs_size,
            sub.mb_count,
        ));
        let packed = packed.expect("the writer above ran or returned an error");

        let sc_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            |dst| match sub.codec {
                Codec::H264 => {
                    let records = pf_dxvadec::slice_control(&packed.records);
                    let bytes = pf_dxvadec::slice_bytes(&records);
                    copy_into(dst, bytes)?;
                    Ok(bytes.len())
                }
                Codec::H265 => {
                    let records = pf_dxvadec::slice_control_h265(&packed.records);
                    let bytes = pf_dxvadec::slice_bytes(&records);
                    copy_into(dst, bytes)?;
                    Ok(bytes.len())
                }
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            sc_size,
            sub.mb_count,
        ));

        // SAFETY: a COM call on the live video context with the live decoder and a slice of
        // fully-initialized descriptors that outlives the call. Every buffer named by a
        // descriptor was released back to the driver by `write_buffer` before this runs,
        // which is what makes them submittable.
        unsafe {
            self.video_context
                .SubmitDecoderBuffers(&session.decoder, &descs)
        }
        .ok()
        .context("SubmitDecoderBuffers")
    }
}

/// pf-bitstream's H.273 code points as the presenter's [`ColorDesc`].
///
/// Per picture, never latched at session start: the Windows host switches an HDR desktop to
/// PQ/BT.2020 IN-BAND with a new SPS mid-stream, and a backend that captured the first AU's
/// colour would paint HDR frames washed out. (pf-bitstream applies E.2.1's "unspecified"
/// inference where the VUI is silent, so these are always meaningful code points.)
fn colour_of(colour: pf_dxvadec::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: colour.colour_primaries,
        transfer: colour.transfer_characteristics,
        matrix: colour.matrix_coefficients,
        full_range: colour.video_full_range,
    }
}

/// Does the adapter expose this decode profile, for this surface format?
///
/// Checked at construction rather than at the first AU, for the same reason the FFmpeg rung
/// checks it there: an unsupported profile discovered mid-stream costs the opening IDR and
/// exits only through a demotion streak.
fn profile_supported(video: &ID3D11VideoDevice, profile: DxvaProfile) -> Result<()> {
    let wanted = GUID::from_u128(profile.guid);
    // SAFETY: COM calls on the live video device; the count bounds the loop and each profile
    // is returned by value.
    let profiles: Vec<GUID> = unsafe {
        let n = video.GetVideoDecoderProfileCount();
        (0..n)
            .filter_map(|i| video.GetVideoDecoderProfile(i).ok())
            .collect()
    };
    if !profiles.contains(&wanted) {
        bail!("adapter exposes no {} decode profile", profile.name);
    }
    // SAFETY: same live device; the arguments are a borrowed local GUID and a plain format
    // enum.
    let ok = unsafe { video.CheckVideoDecoderFormat(&wanted, profile.dxgi_format as DXGI_FORMAT) }
        .map(|b| b.as_bool())
        .unwrap_or(false);
    if !ok {
        bail!(
            "adapter's {} profile cannot decode into DXGI format {}",
            profile.name,
            profile.dxgi_format
        );
    }
    Ok(())
}

/// Build the session if there is none, or rebuild it when the stream's shape moved.
///
/// The shape is read off the SPS the planner just activated, never off the negotiated format:
/// the decoder object, the surface pool, the slot map AND the profile are all derived from it
/// (see [`StreamShape`]), and a partially-rebuilt session hands out surface indices the pool
/// does not have — or decodes at a sample width its surfaces cannot hold. Rebuilding whole is
/// the only correct answer, and it is what the plan → DXVA conversion's `CapacityMismatch`
/// refusal exists to force for the DPB-depth leg.
fn ensure_session<'a>(
    slot: &'a mut Option<Session>,
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    codec: Codec,
    shape: StreamShape,
) -> Result<&'a mut Session> {
    let matches = slot.as_ref().is_some_and(|s| s.shape == shape);
    if !matches {
        if let Some(old) = slot.as_ref() {
            // The old profile is worth a line of its own: a rebuild that also changes it is
            // the in-band 8-bit → 10-bit flip, and a field report showing the decoder
            // following the stream there is the difference between "HDR looked wrong" and a
            // diagnosis.
            tracing::info!(
                was = ?old.shape,
                was_profile = old.profile.name,
                now = ?shape,
                "stream renegotiated — rebuilding the native D3D11VA decode session"
            );
        }
        // Dropped BEFORE the replacement is built so the old pool's VRAM is released first —
        // a 4K pool is on the order of a hundred megabytes and holding two while the new one
        // allocates is how a rebuild fails on a small card.
        *slot = None;
        *slot = Some(Session::build(device, video_device, codec, shape)?);
    }
    Ok(slot.as_mut().expect("built or already matching"))
}

impl Session {
    fn build(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        codec: Codec,
        shape: StreamShape,
    ) -> Result<Session> {
        // A single `DXGI_FORMAT` carries one sample width for both planes, so a stream whose
        // chroma is coded deeper than its luma has no surface this backend can allocate.
        // Refused rather than approximated: the ladder answers with the FFmpeg rung.
        if shape.bit_depth_chroma_minus8 != shape.bit_depth_luma_minus8 {
            bail!(
                "luma is {}-bit and chroma is {}-bit; no DXGI decode format carries both",
                shape.bit_depth(),
                8 + shape.bit_depth_chroma_minus8
            );
        }
        // Derived HERE, from the SPS, rather than latched from the negotiated format at
        // construction — the two can disagree, and this is the one that decodes.
        let profile = pf_dxvadec::profile_for(codec, shape.chroma_format_idc, shape.bit_depth())
            .ok_or_else(|| {
                anyhow!(
                    "no DXVA profile for {codec:?} chroma_format_idc {} at {} bits",
                    shape.chroma_format_idc,
                    shape.bit_depth()
                )
            })?;
        profile_supported(video_device, profile)?;
        let guid = GUID::from_u128(profile.guid);
        // `DXGI_FORMAT` is a plain type alias in this windows-rs rev, so the profile's raw
        // code point IS the format; the cast is the alias, not a conversion.
        let format = profile.dxgi_format as DXGI_FORMAT;
        let coded_width = shape.coded_width;
        let coded_height = shape.coded_height;
        // The SURFACES are aligned to the codec's granule; the DECODER is told the CODED
        // size. That is libavcodec's split — `d3d11va_create_decoder` passes
        // `avctx->coded_width/coded_height` into `D3D11_VIDEO_DECODER_DESC` while
        // `ff_dxva2_common_frame_params` allocates the texture at `FFALIGN(coded,
        // surface_alignment)` — and the two are not interchangeable: a driver may reject an
        // over-large `SampleHeight`, or hand back a different config list for it.
        let aligned_width = pf_dxvadec::align_surface(coded_width, codec);
        let aligned_height = pf_dxvadec::align_surface(coded_height, codec);
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: guid,
            SampleWidth: coded_width,
            SampleHeight: coded_height,
            OutputFormat: format,
        };

        // Enumerate the driver's configs and pick a short-format one (pf-dxvadec's
        // `pick_config` is the whole decision, and it is unit-tested). The driver's own
        // struct is handed back to `CreateVideoDecoder` untouched: re-synthesising it from
        // the three fields selection reads would drop the dozen `Config*` members a driver
        // may care about.
        // SAFETY: COM calls on the live video device with a borrowed local descriptor; the
        // count bounds the loop and each config is written into a local that outlives its
        // call.
        let configs: Vec<D3D11_VIDEO_DECODER_CONFIG> = unsafe {
            let count = video_device
                .GetVideoDecoderConfigCount(&desc)
                .context("GetVideoDecoderConfigCount")?;
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let mut config = D3D11_VIDEO_DECODER_CONFIG::default();
                if video_device
                    .GetVideoDecoderConfig(&desc, i, &mut config)
                    .ok()
                    .is_ok()
                {
                    out.push(config);
                }
            }
            out
        };
        let facts: Vec<pf_dxvadec::ConfigFacts> = configs
            .iter()
            .map(|c| pf_dxvadec::ConfigFacts {
                bitstream_raw: c.ConfigBitstreamRaw,
                no_encryption: c.guidConfigBitstreamEncryption == GUID::zeroed(),
                min_render_target_buffers: c.ConfigMinRenderTargetBuffCount,
            })
            .collect();
        let index = pf_dxvadec::pick_config(codec, &facts).ok_or_else(|| {
            anyhow!(
                "{} offers no short-format ({}) decoder config among {} — the FFmpeg rung \
                 implements the long format, this one does not",
                profile.name,
                pf_dxvadec::short_slice_config(codec),
                facts.len()
            )
        })?;
        let config = configs[index];

        // SAFETY: a COM call on the live video device over two borrowed local descriptors;
        // the returned decoder is owned by this `Session`.
        let decoder = unsafe { video_device.CreateVideoDecoder(&desc, &config) }
            .context("CreateVideoDecoder")?;

        let slots = pf_dxvadec::SlotMap::new(shape.max_dpb_frames);
        let pool_size =
            pf_dxvadec::pool_size(slots.capacity(), facts[index].min_render_target_buffers);

        // THE decode pool — one texture array, `D3D11_BIND_DECODER` only, no share flags.
        // See the module docs for why every one of these fields is what it is.
        let pool_desc = D3D11_TEXTURE2D_DESC {
            Width: aligned_width,
            Height: aligned_height,
            MipLevels: 1,
            ArraySize: pool_size,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: BIND_DECODER,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut pool = None;
        // SAFETY: a `?`-checked `CreateTexture2D` on the live device, over a fully-initialized
        // stack descriptor and a live `Option` out-param.
        unsafe { device.CreateTexture2D(&pool_desc, None, Some(&mut pool)) }
            .ok()
            .context("create the D3D11VA decode surface pool")?;
        let pool: ID3D11Texture2D = pool.expect("CreateTexture2D succeeded");

        // One output view per array slice. The view is what `DecoderBeginFrame` targets, and
        // its `ArraySlice` is the DXVA surface index — so `views[i]` decodes into surface i,
        // which is DPB slot i.
        let mut views = Vec::with_capacity(pool_size as usize);
        for slice in 0..pool_size {
            let mut view_desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
                DecodeProfile: guid,
                ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            view_desc.Anonymous.Texture2D.ArraySlice = slice;
            let mut view = None;
            // SAFETY: COM calls on the live video device with the pool texture just created
            // and a borrowed local descriptor; the out-param is checked before use.
            unsafe {
                video_device.CreateVideoDecoderOutputView(&pool, &view_desc, Some(&mut view))
            }
            .ok()
            .context("CreateVideoDecoderOutputView")?;
            views.push(view.expect("output view created"));
        }

        tracing::info!(
            profile = profile.name,
            coded_width,
            coded_height,
            aligned_width,
            aligned_height,
            bit_depth = shape.bit_depth(),
            chroma_format_idc = shape.chroma_format_idc,
            pool_size,
            dpb_slots = slots.capacity(),
            config_bitstream_raw = config.ConfigBitstreamRaw,
            "native D3D11VA decode session built"
        );
        Ok(Session {
            decoder,
            pool,
            views,
            slots,
            shape,
            profile,
        })
    }
}

/// `DecoderBeginFrame` with the `E_PENDING` retry loop — the hardware is still busy with an
/// earlier picture, which is a wait, not a failure.
fn begin_frame(
    context: &ID3D11VideoContext,
    decoder: &ID3D11VideoDecoder,
    view: &ID3D11VideoDecoderOutputView,
) -> Result<()> {
    for attempt in 0..BEGIN_FRAME_RETRIES {
        // SAFETY: a COM call on the live video context with the live decoder and output view;
        // the content-key arguments are the "no protected content" pair (size 0, null).
        let hr = unsafe { context.DecoderBeginFrame(decoder, view, 0, None) };
        if hr.0 == E_PENDING {
            // libavcodec's own back-off, to the microsecond — see the constants.
            std::thread::sleep(BEGIN_FRAME_BACKOFF);
            continue;
        }
        return hr
            .ok()
            .with_context(|| format!("DecoderBeginFrame (after {attempt} pending retries)"));
    }
    bail!("DecoderBeginFrame stayed E_PENDING for {BEGIN_FRAME_RETRIES} attempts")
}

/// Map one decoder buffer, let `write` fill it, and release it back to the driver.
///
/// The release is unconditional: a buffer left mapped wedges every later `GetDecoderBuffer`
/// of the same type, so a writer's error must not be allowed to skip it. Returns the number
/// of bytes the writer used, for the buffer's `DataSize`.
fn write_buffer(
    context: &ID3D11VideoContext,
    decoder: &ID3D11VideoDecoder,
    kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    write: impl FnOnce(&mut [u8]) -> Result<usize>,
) -> Result<usize> {
    let mut size = 0u32;
    let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: a COM call on the live video context and decoder; both out-params are locals
    // that outlive the call, and neither is read before the HRESULT is checked.
    unsafe { context.GetDecoderBuffer(decoder, kind, &mut size, &mut ptr) }
        .ok()
        .with_context(|| format!("GetDecoderBuffer({kind:?})"))?;
    if ptr.is_null() {
        // Nothing was mapped, so nothing must be released.
        bail!("GetDecoderBuffer({kind:?}) returned a null mapping");
    }
    // SAFETY: `GetDecoderBuffer` succeeded and reported a non-null pointer to a mapping of
    // `size` bytes that the driver keeps valid until the matching `ReleaseDecoderBuffer`
    // below — which runs before this borrow can escape, because the slice is confined to
    // `write`'s call. Write-only, so uninitialized driver memory is never read; `u8` has no
    // alignment requirement, and a decoder buffer never approaches `isize::MAX`.
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), size as usize) };
    let written = write(dst);
    // SAFETY: releases exactly the buffer mapped above, on the same live context and decoder.
    let released = unsafe { context.ReleaseDecoderBuffer(decoder, kind) };
    let written = written.with_context(|| format!("filling the {kind:?} decoder buffer"))?;
    released
        .ok()
        .with_context(|| format!("ReleaseDecoderBuffer({kind:?})"))?;
    Ok(written)
}

/// A submission descriptor for one filled buffer.
///
/// `mb_count` is `NumMBsInBuffer`, and it is NOT uniformly 0. libavcodec's H.264 path
/// computes `h->mb_width * h->mb_height` and writes it on both the BITSTREAM and the
/// SLICE_CONTROL descriptor (`commit_bitstream_and_slice_buffer`, for both slice formats,
/// the second through `ff_dxva2_commit_buffer`'s `mb_count` argument); its HEVC path writes
/// 0 on the same two. Picture parameters and quantization matrices take 0 in both codecs.
///
/// The value is arguably redundant in VLD mode — the driver has the same two numbers in the
/// picture parameters — but this module's whole method is to reproduce libavcodec exactly,
/// on the evidence that a hand-built variant was rejected by Intel at the first
/// `SubmitDecoderBuffers`, and this is a field libav fills on precisely that call.
fn buffer_desc(
    kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    size: usize,
    mb_count: u32,
) -> D3D11_VIDEO_DECODER_BUFFER_DESC {
    D3D11_VIDEO_DECODER_BUFFER_DESC {
        BufferType: kind,
        DataSize: size as u32,
        NumMBsInBuffer: mb_count,
        ..Default::default()
    }
}

/// Copy `src` into the driver's mapping, refusing rather than truncating.
fn copy_into(dst: &mut [u8], src: &[u8]) -> Result<()> {
    if src.len() > dst.len() {
        bail!(
            "a {}-byte DXVA buffer does not fit the driver's {}-byte mapping",
            src.len(),
            dst.len()
        );
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(())
}

#[cfg(test)]
mod parity {
    //! Frame-hash parity for this rung — the evidence M5 shipped without.
    //!
    //! `#[ignore]`d: it needs a real D3D11 video device. Run it on a Windows box with
    //!
    //! ```text
    //! cargo test -p pf-client-core --lib video_d3d11_native -- --ignored --nocapture
    //! ```
    //!
    //! and pin a GPU on a multi-adapter box with `PF_DXVA_ADAPTER=<substring of the
    //! adapter description>` — .173 enumerates its AMD iGPU first, not the 4090, so an
    //! unpinned run there reports the iGPU and that is a fact worth printing rather
    //! than assuming.
    //!
    //! # What it proves, and against what
    //!
    //! The same thing `pf-vkdecode`'s `gpu_parity` proves for the Vulkan rung, against
    //! the same reference: H.264 and H.265 decoding are exactly specified, so a
    //! conformant decoder must reproduce libavcodec's SOFTWARE output bit for bit. The
    //! goldens are therefore libavcodec's, not the FFmpeg D3D11VA rung's — ground truth
    //! rather than a peer implementation, and the identical yardstick M3 was held to,
    //! which makes the two rungs' verdicts directly comparable. It reads back the
    //! DECODE surface, before the `VideoProcessorBlt`, so what is hashed is what this
    //! rung is responsible for: the shared hand-off is the field-proven half.
    //!
    //! # Why the harness reorders and the rung does not
    //!
    //! This rung presents every picture the instant it decodes: `submit` blits
    //! `setup_slot` and returns. It never consults `AuPlan::dpb.outputs`, which is
    //! where display order lives — the native Vulkan rung keeps a display-order queue
    //! for exactly that reason, and libavcodec's D3D11VA rung reorders internally.
    //!
    //! For punktfunk's own streams the two orders coincide (hosts emit zero-reorder
    //! low-delay output with no B pictures), which is why this has never shown. Both
    //! vendored conformance vectors DO reorder, though — the H.265 one's first B
    //! picture at AU 3 is what localised the RPS slot defect — so a harness that hashed
    //! in decode order would report a permutation against display-order goldens and
    //! read like a decoder fault.
    //!
    //! So the harness hashes each decoded surface against the `PicId` the planner
    //! assigned it, then emits those hashes in the planner's own output order. The
    //! reordering is the TEST's, done by the same planner the rung already trusts, and
    //! the divergence is recorded here rather than papered over: a stream that actually
    //! reordered would present out of order through this rung today.
    //!
    //! # The crop
    //!
    //! The decode pool is aligned to the codec's granule and is therefore TALLER than
    //! the picture, so the chroma plane starts at `RowPitch * texture_height`, not
    //! `RowPitch * display_height` — reading it at the display height is the 1088-row
    //! smear this project has already paid for once.

    use std::collections::HashMap;

    use pf_dxvadec::H264Planner;
    use pf_dxvadec::H265Planner;
    use sha2::Digest;
    use windows::Win32::d3d11::ID3D11Resource;
    use windows::Win32::d3d11::D3D11_CPU_ACCESS_READ;
    use windows::Win32::d3d11::D3D11_MAPPED_SUBRESOURCE;
    use windows::Win32::d3d11::D3D11_MAP_READ;
    use windows::Win32::d3d11::D3D11_USAGE_STAGING;
    use windows::Win32::dxgi::CreateDXGIFactory1;
    use windows::Win32::dxgi::IDXGIFactory1;
    use windows::Win32::dxgi::DXGI_ADAPTER_DESC1;

    use super::*;

    /// The vendored H.264 vector — the same file, at the same relative path, that
    /// `pf-vkdecode`'s GPU legs decode. 250 access units, two slice NALUs per picture.
    const TEST_25FPS_H264: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// The vendored H.265 twin: 250 access units, Main 8-bit 4:2:0, one slice each.
    const TEST_25FPS_H265: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );

    /// libavcodec's per-display-frame NV12 hashes. Deliberately the SAME files the
    /// Vulkan rung is held to, read across the crate boundary rather than copied: two
    /// rungs measured against two copies of a golden set is two measurements, and the
    /// point of this file is that they are one.
    const GOLDENS_H264: &str = include_str!("../../pf-vkdecode/tests/data/test-25fps.nv12.sha256");
    const GOLDENS_H265: &str =
        include_str!("../../pf-vkdecode/tests/data/test-25fps-h265.nv12.sha256");

    /// Both vendored vectors are 250 display frames.
    const FRAME_COUNT: usize = 250;

    /// The golden file's hash lines (comments and blanks skipped).
    fn golden_hashes(file: &'static str) -> Vec<&'static str> {
        file.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use std::fmt::Write as _;
        sha2::Sha256::digest(data)
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// Byte offsets of every Annex-B NAL header in `stream`, in order.
    ///
    /// Emulation prevention guarantees `00 00 01` cannot appear inside a NAL payload,
    /// so scanning for it finds start codes and nothing else; the header begins on the
    /// byte after. Hand-rolled rather than borrowed from the parser because
    /// `pf-client-core` does not depend on the vendored crate — and kept honest by the
    /// access-unit count both legs assert, which no plausible splitter bug survives.
    fn nal_headers(stream: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0x00, 0x00, 0x01] {
                out.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Split `stream` into access units, given a per-NAL `(is_slice, starts_a_picture)`
    /// rule. A new AU begins at a non-VCL NALU following slices, or at a slice that
    /// declares itself the first of a picture when the current AU already has slices —
    /// the same rule pf-bitstream applies, spelled once for both codecs.
    fn split_aus(stream: &[u8], classify: impl Fn(&[u8], usize) -> (bool, bool)) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut au_start = 0usize;
        let mut au_has_slice = false;
        for header in nal_headers(stream) {
            let (is_slice, first_in_picture) = classify(stream, header);
            // The start code owning this header: three bytes, plus the optional
            // leading zero byte of the four-byte form.
            let mut start = header - 3;
            if start > 0 && stream[start - 1] == 0x00 {
                start -= 1;
            }
            if au_has_slice && (!is_slice || first_in_picture) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// H.264: one-byte NAL header, `nal_unit_type` in the low 5 bits (1 = non-IDR
    /// slice, 5 = IDR slice), and `first_mb_in_slice == 0` is the top bit of the byte
    /// after it.
    fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = matches!(s[h] & 0x1f, 1 | 5);
            let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// H.265: TWO-byte NAL header, `nal_unit_type` in bits 1..7 of the first byte and
    /// "is a slice" the numeric range `< 32`, so `first_slice_segment_in_pic_flag` is
    /// the top bit of the byte at `+2` where H.264 reads `+1`.
    fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = (s[h] >> 1) & 0x3f < 32;
            let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// The decode order and the display order of a vector's pictures, as `PicId`s.
    ///
    /// Both come from a planner run ALONGSIDE the decoder's own, over the same access
    /// units: the planner is deterministic, so the ids it hands this walk are the ids
    /// it hands the rung, and no production code has to grow a test accessor.
    struct Order {
        /// One id per access unit, in submission order.
        decode: Vec<u64>,
        /// The same ids in the planner's output (bumping) order, flush included.
        display: Vec<u64>,
    }

    fn order_h264(aus: &[&[u8]]) -> Order {
        let mut planner = H264Planner::new();
        let mut order = Order {
            decode: Vec::new(),
            display: Vec::new(),
        };
        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: this rung hands the blit a size and no origin, so a \
                 non-zero conformance-window offset would be cropped from the wrong \
                 corner — by the rung, not just by this harness"
            );
            order.decode.push(
                plan.dpb.stored.unwrap_or_else(|| {
                    panic!("AU {index}: every picture of this vector is stored")
                }),
            );
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    fn order_h265(aus: &[&[u8]]) -> Order {
        let mut planner = H265Planner::new();
        let mut order = Order {
            decode: Vec::new(),
            display: Vec::new(),
        };
        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: a non-zero conformance-window offset is cropped from the \
                 wrong corner by this rung"
            );
            order.decode.push(
                plan.dpb.stored.unwrap_or_else(|| {
                    panic!("AU {index}: every picture of this vector is stored")
                }),
            );
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    /// The LUID of the adapter whose description contains `PF_DXVA_ADAPTER`, and the
    /// descriptions of everything enumerated (printed, so a run always says which GPU
    /// answered rather than leaving it to be inferred).
    fn pinned_adapter() -> Option<[u8; 8]> {
        let want = std::env::var("PF_DXVA_ADAPTER").ok();
        // SAFETY: DXGI factory creation takes no pointer and returns an owned factory
        // or an error; the `Ok` binding is what proves one came back.
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            eprintln!("adapters: CreateDXGIFactory1 failed");
            return None;
        };
        let mut chosen = None;
        for i in 0.. {
            // SAFETY: a COM call on the live factory; `Ok` proves an adapter came back.
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            // SAFETY: `DXGI_ADAPTER_DESC1` is plain-old-data, so all-zeroes is valid.
            let mut desc: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
            // SAFETY: a COM call on the adapter just enumerated, filling the zeroed
            // local through the out-param; checked before the descriptor is read.
            if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]);
            let mut luid = [0u8; 8];
            luid[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
            luid[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
            let hit = want
                .as_deref()
                .is_some_and(|w| name.to_lowercase().contains(&w.to_lowercase()));
            eprintln!(
                "adapter {i}: {name}{}",
                if hit { "  <= pinned" } else { "" }
            );
            if hit && chosen.is_none() {
                chosen = Some(luid);
            }
        }
        if want.is_some() && chosen.is_none() {
            panic!("PF_DXVA_ADAPTER matched no adapter (see the list above)");
        }
        chosen
    }

    /// GPU→CPU readback of one decode-pool slice, cropped to `display` and packed
    /// tightly as NV12/P010 — byte-for-byte the layout the goldens hash.
    struct Readback {
        ctx: ID3D11DeviceContext,
        staging: Option<ID3D11Texture2D>,
    }

    impl Readback {
        fn read(
            &mut self,
            device: &ID3D11Device,
            pool: &ID3D11Texture2D,
            slice: u32,
            display: (u32, u32),
        ) -> Vec<u8> {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: `GetDesc` fills a plain-old-data descriptor through an out-param
            // on a live texture and returns nothing to check.
            unsafe { pool.GetDesc(&mut desc) };

            if self.staging.is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ as u32,
                    MiscFlags: 0,
                };
                let mut t: Option<ID3D11Texture2D> = None;
                // SAFETY: one `?`-checked call on the live device over a fully
                // initialised stack descriptor and a live `Option` out-param.
                unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut t)) }
                    .ok()
                    .expect("create the readback staging texture");
                self.staging = t;
            }
            let staging = self.staging.clone().expect("staging texture");

            let (width, height) = display;
            assert!(
                width <= desc.Width && height <= desc.Height,
                "the display region {width}x{height} does not fit the {}x{} pool surface",
                desc.Width,
                desc.Height
            );
            let ten_bit = desc.Format == pf_dxvadec::DXGI_FORMAT_P010;
            let bytes_per_sample = if ten_bit { 2 } else { 1 };
            let row_bytes = width as usize * bytes_per_sample;

            // SAFETY: `src` and `dst` are the same device's textures of identical
            // format and dimensions, so the single-subresource copy on the immediate
            // context is valid; `slice` is the array slice the decoder just wrote and
            // `MipLevels == 1` makes it the subresource index. `Map(D3D11_MAP_READ)`
            // on a STAGING texture blocks until that copy has retired and yields
            // `pData` valid for the whole resource: for NV12/P010 the luma plane is
            // `desc.Height` rows at `RowPitch` and the chroma plane follows at byte
            // offset `RowPitch * desc.Height`, so `total` below is exactly the mapped
            // extent and every sub-slice read is inside it. `Unmap` pairs the `Map`.
            let out = unsafe {
                let src: ID3D11Resource = pool.cast().expect("pool -> resource");
                let dst: ID3D11Resource = staging.cast().expect("staging -> resource");
                self.ctx
                    .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, slice, None);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                    .ok()
                    .expect("Map the readback staging texture");
                let pitch = map.RowPitch as usize;
                let aligned_h = desc.Height as usize;
                let total = pitch * (aligned_h + aligned_h.div_ceil(2));
                let mapped = std::slice::from_raw_parts(map.pData as *const u8, total);
                // The chroma plane starts at the ALIGNED height, never the display
                // height — the pool surface is taller than the picture.
                let chroma_off = pitch * aligned_h;
                let mut out = Vec::with_capacity(row_bytes * (height as usize).div_ceil(2) * 3);
                for y in 0..height as usize {
                    out.extend_from_slice(&mapped[y * pitch..y * pitch + row_bytes]);
                }
                for y in 0..(height as usize).div_ceil(2) {
                    let row = chroma_off + y * pitch;
                    out.extend_from_slice(&mapped[row..row + row_bytes]);
                }
                self.ctx.Unmap(&staging, 0);
                out
            };
            out
        }
    }

    /// Decode `aus` through a real `NativeD3d11Decoder`, hash every picture, and
    /// compare the planner's display order against libavcodec's goldens.
    fn parity_run(codec: Codec, aus: &[&[u8]], order: &Order, goldens: &[&str], label: &str) {
        assert_eq!(
            aus.len(),
            FRAME_COUNT,
            "{label}: the vector must split into {FRAME_COUNT} access units — a \
             different count means this file's splitter disagrees with pf-bitstream's, \
             and nothing below it is meaningful"
        );
        assert_eq!(
            order.display.len(),
            goldens.len(),
            "{label}: the planner outputs {} pictures, the goldens carry {}",
            order.display.len(),
            goldens.len()
        );

        let luid = pinned_adapter();
        let mut decoder = NativeD3d11Decoder::new(codec, StreamFormat::SDR_420_8, luid, false)
            .unwrap_or_else(|e| panic!("{label}: the box must host this profile — {e:#}"));
        let mut readback = Readback {
            ctx: decoder.context.clone(),
            staging: None,
        };

        let mut by_id: HashMap<u64, String> = HashMap::new();
        for (index, au) in aus.iter().enumerate() {
            let sub = decoder
                .plan(au)
                .unwrap_or_else(|e| panic!("AU {index}: plan failed — {e:#}"))
                .unwrap_or_else(|| panic!("AU {index}: this vector has no skipped pictures"));
            assert!(
                !sub.concealed,
                "AU {index}: a clean vector must need no concealment"
            );
            let display = (sub.width, sub.height);
            let slice = u32::from(sub.setup_slot);
            decoder
                .submit(au, &sub)
                .unwrap_or_else(|e| panic!("AU {index}: submit failed — {e:#}"));
            let session = decoder.session.as_ref().expect("submit built a session");
            let pool = session.pool.clone();
            let bytes = readback.read(&decoder.device, &pool, slice, display);
            by_id.insert(order.decode[index], sha256_hex(&bytes));
        }

        let mut mismatches = 0usize;
        for (n, (id, golden)) in order.display.iter().zip(goldens.iter()).enumerate() {
            let got = by_id
                .get(id)
                .unwrap_or_else(|| panic!("display frame {n} names PicId {id}, never decoded"));
            if got != golden {
                if mismatches < 10 {
                    eprintln!("{label}: display frame {n} (PicId {id}): {got} != {golden}");
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{label}: {mismatches}/{} frames diverge from libavcodec (first 10 above; \
             frame 0 is intra-only — if IT mismatches suspect the readback geometry \
             (pitch/crop/plane offset) rather than the decode)",
            goldens.len()
        );
        eprintln!(
            "{label}: {} frames bit-identical to libavcodec software decode",
            goldens.len()
        );
    }

    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h264_aus(TEST_25FPS_H264);
        let order = order_h264(&aus);
        parity_run(
            Codec::H264,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H264),
            "H.264",
        );
    }

    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(TEST_25FPS_H265);
        let order = order_h265(&aus);
        parity_run(
            Codec::H265,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H265),
            "H.265",
        );
    }

    // ---------------------------------------------------------------------
    // CPU guards — NOT `#[ignore]`d, so ordinary CI notices when this file's
    // splitter or the goldens drift away from pf-bitstream.
    // ---------------------------------------------------------------------

    #[test]
    fn the_local_splitter_agrees_with_the_planner_on_both_vectors() {
        let h264 = split_h264_aus(TEST_25FPS_H264);
        assert_eq!(h264.len(), FRAME_COUNT, "H.264 vector access units");
        let order = order_h264(&h264);
        assert_eq!(order.decode.len(), FRAME_COUNT);
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_H264).len(),
            "the H.264 planner's output count must match the golden count"
        );

        let h265 = split_h265_aus(TEST_25FPS_H265);
        assert_eq!(h265.len(), FRAME_COUNT, "H.265 vector access units");
        let order = order_h265(&h265);
        assert_eq!(order.decode.len(), FRAME_COUNT);
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_H265).len(),
            "the H.265 planner's output count must match the golden count"
        );
    }

    #[test]
    fn both_vendored_vectors_really_do_reorder() {
        // The module docs claim the harness must reorder because these vectors do. If
        // that ever stops being true the claim is stale, and hashing in decode order
        // would be the simpler harness — so assert the reason, not just the behaviour.
        for (name, order) in [
            ("H.264", order_h264(&split_h264_aus(TEST_25FPS_H264))),
            ("H.265", order_h265(&split_h265_aus(TEST_25FPS_H265))),
        ] {
            assert_ne!(
                order.decode, order.display,
                "{name}: this vector no longer reorders — the harness's PicId \
                 indirection is now unnecessary and its docs are wrong"
            );
        }
    }
}
