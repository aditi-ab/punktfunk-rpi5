//! The CPU rung — the ladder's LAST one, and (M8) the first one with no FFmpeg in it.
//!
//! * **H.264 → openh264** (BSD-2). Already a workspace dependency: the host's GPU-less
//!   encoder is the same library ([`pf-encode`'s `enc/sw.rs`]), so the licence posture and
//!   the statically-bundled build were settled before this rung existed.
//! * **AV1 → rav1d** (BSD-2) — dav1d, ported to Rust. Picking it over the `dav1d` FFI
//!   crate is a packaging decision, argued in `Cargo.toml`; picking it over *nothing* is
//!   the plan's ("dav1d SW is the safety net"). Two properties come free with it:
//!   there is no `avcodec_find_decoder(AV1)` to hand us libdav1d behind a
//!   `hw_device_ctx` it silently ignores, and no C decoder in the process at all.
//! * **HEVC → dropped.** No permissively licensed software HEVC decoder exists (libde265
//!   is LGPL, which defeats the point of the excision). This rung REFUSES an HEVC
//!   session with a typed [`NoSoftwareRung`], which is what the session layer turns into
//!   a reconnect that advertises HEVC-less decode caps — see
//!   [`crate::video::last_rung_verdict`]. Narrowing instead (limping on at 5 fps, or
//!   freezing) is the failure mode this whole program exists to end.
//!
//! **Output is PLANES, not RGBA.** The decoder hands the presenter tightly-packed I420
//! and the presenter's existing planar CSC shader does the colour, which deletes two
//! things at once: swscale's per-frame YUV→RGBA pass, and swscale's BT.601 default —
//! the footgun the old `convert_rgba` carried ~30 lines of correction code for.
//!
//! **Colour comes from pf-bitstream, not from the decoder.** openh264 reports no VUI at
//! all and rav1d reports its own sequence header, so a rung that trusted its decoder
//! would have two colour implementations to keep in step with the four hardware rungs'
//! one. Instead the H.264 leg plans every AU with [`H264Planner`] — the SAME planner
//! `pf-vkdecode`/`pf-dxvadec`/`pf-vaadec` submit from — and reads
//! `plan.picture.colour`. The signalled matrix/range therefore cannot differ between the
//! software rung and the hardware rungs, because it is literally the same code reading
//! the same SPS. The H.264 leg takes the recovery point SEI from the same plan, through
//! `pf-vkdecode`'s own [`RecoveryWatch`], so an intra-refresh session re-anchors here on
//! the same rule the native rung uses.
//!
//! **The picture envelope is checked BEFORE the decoder sees the AU, on both legs.**
//! 8-bit 4:2:0 only: openh264 has no wider support at all and rav1d is compiled
//! `bitdepth_8` here. H.264 reads it off the SPS the planner activated; AV1 reads it off
//! the sequence header with `dav1d_parse_sequence_header`. Both raise the SAME typed
//! [`NoSoftwareRung`] so the session reconnects. Letting the DECODER answer instead is
//! what the M8 review caught: rav1d refuses a 10-bit frame with `ENOPROTOOPT`, the pump
//! reads a generic error as survivable, and a Main 10 HDR stream — which is what hardware
//! AV1 sessions are — freezes forever, one keyframe request per identical AU.
//!
//! Threading: openh264's `num_threads` is documented upstream as "will probably just
//! segfault", so this stays single-threaded — the old libavcodec rung's slice threading has
//! no equivalent here. rav1d gets the machine's cores, and **at least two frame contexts**;
//! [`Av1Software::new`] carries the whole argument, because "at least two" is not a
//! performance choice but the difference between an error and `abort()`.
//!
//! # Why this rung is NOT process-isolated
//!
//! The frame-context floor closes the one abort we hit and can prove. It does not make the
//! rung panic-proof, and nothing at this call site can: rav1d exposes dav1d's C ABI, every
//! internal `rav1d_*` entry point is `pub(crate)`, so any reachable panic crosses
//! `extern "C"` as `panic_cannot_unwind` → `abort()`. No `catch_unwind`, no rung demotion
//! and no [`NoSoftwareRung`] refusal can contain it. Counted in rav1d 1.1.0's 60 source
//! files: 285 `unwrap()`, 214 `assert!`, 19 `unreachable!`, 11 `expect()`, 10 `panic!` —
//! 539 sites that are an `abort()` if a stream can reach them. #97 fixed ONE.
//!
//! Isolating the decoder in its own process is the only defence that actually works, and
//! it is deliberately NOT taken. The decision, so it is not re-litigated from scratch:
//!
//! * **The defect is a dependency's, and it is one line.** memorysafety/rav1d#1497 was
//!   filed 2026-08-07 with the fix (`is_some_and` for the `unwrap`) and a reproducer.
//!   Paying a permanent architectural tax to route around a bug that costs upstream one
//!   line is the wrong trade while that line is still plausibly coming.
//! * **The residual risk is real but unquantified.** 539 panic sites is a scary number
//!   and a meaningless one: not one of them is known to be reachable from a punktfunk
//!   stream. The honest next step is to MEASURE reachability — fuzz this rung with
//!   truncated, reordered and bit-flipped AUs and see whether any input aborts — not to
//!   buy insurance against a number nobody has bounded. That is cheap; this is not.
//! * **The cost lands on the video path, and on three platforms.** pf-client-core builds
//!   into the Linux, Windows and Android clients (the Apple clients decode through
//!   VideoToolbox and never reach here). Each needs its own shared-memory transport for
//!   `CpuPlanarFrame`s, its own child lifecycle, crash detection and restart, and its own
//!   backpressure — and it adds a scheduling boundary to the rung that is ALREADY the
//!   slowest one on the ladder. Zero-copy is a hard requirement here; an IPC hop that
//!   copies frames would be rejected on its own terms.
//! * **What an abort actually costs is bounded.** This rung is reached because the GPU
//!   rungs already failed, so the session is degraded before rav1d sees a byte. Losing
//!   the process loses a session the user was going to have a bad time in regardless.
//!   That is bad, and it is not the same as losing a working session.
//!
//! **Revisit when the calculus changes, which is a specific event, not a feeling:** a
//! SECOND distinct abort observed in the field, or a fuzzer finding a reachable panic.
//! Either turns this from one upstream bug into a class of them, and a class is what
//! justifies isolation. Until then the floor plus the upstream fix is the proportionate
//! answer, and the fuzzing is the work that would tell us we were wrong.

use crate::video::{CpuPlanarFrame, RungLoss};
use crate::video_color::ColorDesc;
use anyhow::{anyhow, bail, Context as _, Result};
use pf_bitstream::h264::{H264Planner, PlanError};
use pf_vkdecode::RecoveryWatch;

/// The codecs this rung can decode at all. Deliberately its own enum rather than an
/// `ffmpeg::codec::Id`: the whole point of M8 was that nothing in here speaks FFmpeg, and
/// the ladder above still does only because its other rungs are not swapped yet (M10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwCodec {
    H264,
    Av1,
}

impl SwCodec {
    /// The wire codec bit this rung can serve, or `None` — the one place the
    /// "which codecs does software cover" question is answered.
    pub(crate) fn for_wire(codec: u8) -> Option<SwCodec> {
        match codec {
            punktfunk_core::quic::CODEC_H264 => Some(SwCodec::H264),
            punktfunk_core::quic::CODEC_AV1 => Some(SwCodec::Av1),
            _ => None,
        }
    }
}

/// This build has no software decoder for the session's stream — the ladder has run out
/// of rungs.
///
/// A distinct type, not a formatted string, because the SESSION layer must be able to
/// tell this apart from every other decode failure: everything else is survivable (feed
/// the next AU, ask for an IDR), and this one is not survivable at all — it can only be
/// answered by reconnecting with something this client can actually decode. It rides out
/// through `anyhow` and is recovered with `downcast_ref`, so no signature in the ladder
/// changes shape for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSoftwareRung {
    /// The `quic::CODEC_*` bit of the session that has no CPU rung.
    pub codec: u8,
    /// `None` — the CODEC itself has no CPU decoder (HEVC). `Some(what)` — the codec
    /// does, but not for THIS stream's picture shape (10-bit, 4:4:4).
    ///
    /// The two are one type on purpose. They are different diagnoses but the SAME
    /// available action: the codec is fixed at Welcome, so a shape this rung cannot
    /// decode can only be escaped the way a codec it cannot decode is — a reconnect that
    /// takes the codec off the table, after which the host resolves a new shape too. A
    /// blunt instrument for the shape case, and the only one the wire offers.
    ///
    /// The shape case cannot be answered at construction alone: a Windows HDR desktop
    /// flips to Main 10 IN-BAND with a new parameter set, so the Welcome's
    /// [`crate::video::StreamFormat`] can say 8-bit for a session that becomes 10-bit
    /// mid-stream. That is why this is raised from the per-AU path, off the bitstream's
    /// own headers, rather than from a negotiated field.
    pub shape: Option<&'static str>,
}

impl NoSoftwareRung {
    /// Which diagnosis this is, for the reconnect rule
    /// ([`last_rung_verdict`](crate::video::last_rung_verdict)). The two answers differ:
    /// a missing CODEC means every hardware rung already failed, a missing SHAPE means
    /// none of them was even asked.
    pub fn loss(&self) -> RungLoss {
        match self.shape {
            None => RungLoss::Codec,
            Some(_) => RungLoss::Shape,
        }
    }
}

impl std::fmt::Display for NoSoftwareRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let codec = crate::video::wire_codec_name(self.codec);
        match self.shape {
            None => write!(
                f,
                "no software decoder for {codec} — this build decodes H.264 and AV1 on \
                 the CPU (there is no permissively licensed software HEVC decoder)"
            ),
            Some(shape) => write!(
                f,
                "the software {codec} decoder cannot decode this stream: {shape} \
                 (the CPU rung is 8-bit 4:2:0 only)"
            ),
        }
    }
}

impl std::error::Error for NoSoftwareRung {}

// --- software backend ---------------------------------------------------------------

pub(crate) struct SoftwareDecoder {
    inner: Inner,
    /// Last colour signalling the stream actually stated. Held across AUs the metadata
    /// parser could not read (see [`H264Software::colour_of`]) so a stream never silently
    /// reverts to the SDR default mid-session; seeded with that default, which is what
    /// "unspecified" resolves to anyway (`csc_rows`).
    color: ColorDesc,
}

enum Inner {
    H264(H264Software),
    Av1(Av1Software),
}

impl SoftwareDecoder {
    /// Build the CPU rung for a WIRE codec bit.
    ///
    /// `Err` carrying a [`NoSoftwareRung`] means "there is no such rung", not "the rung
    /// failed to start" — the two are different questions for the caller and must not
    /// collapse into one string.
    pub(crate) fn new(codec: u8) -> Result<SoftwareDecoder> {
        let Some(sw) = SwCodec::for_wire(codec) else {
            return Err(NoSoftwareRung { codec, shape: None }.into());
        };
        let inner = match sw {
            SwCodec::H264 => Inner::H264(H264Software::new()?),
            SwCodec::Av1 => Inner::Av1(Av1Software::new()?),
        };
        tracing::info!(
            codec = crate::video::wire_codec_name(codec),
            decoder = match sw {
                SwCodec::H264 => "openh264",
                SwCodec::Av1 => "rav1d",
            },
            "software decoder opened (CPU, planar output)"
        );
        Ok(SoftwareDecoder {
            inner,
            // "Unspecified" everywhere: `csc_rows` resolves that to BT.709 limited, the
            // host's SDR default, which is also what E.2.1 inference produces for a
            // stream whose VUI is silent.
            color: ColorDesc {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false,
            },
        })
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<CpuPlanarFrame>> {
        match &mut self.inner {
            Inner::H264(h) => h.decode(au, &mut self.color),
            Inner::Av1(a) => a.decode(au, &mut self.color),
        }
    }
}

// --- H.264 (openh264) ----------------------------------------------------------------

struct H264Software {
    decoder: openh264::decoder::Decoder,
    /// The metadata half: colour signalling, the IDR flag and the display crop, from the
    /// same planner every hardware rung submits from. It does NOT drive openh264 —
    /// openh264 owns its own parsing — so a plan the narrow envelope refuses costs
    /// metadata for that AU, never the picture.
    ///
    /// Boxed: the planner's DPB dwarfs everything else here, and `Backend` (which holds
    /// this by value) is an enum whose other variants are pointer-sized — the same reason
    /// the native rungs are boxed there.
    planner: Box<H264Planner>,
    /// The recovery point SEI, folded per picture by the SAME rule the native Vulkan rung
    /// uses (`pf-vkdecode`'s watch, unchanged and shared): an intra-refresh session never
    /// emits an IDR, so without this the pump's post-loss freeze on THIS rung waits out
    /// its 500 ms backstop and then forces the very IDR the wave exists to avoid.
    recovery: RecoveryWatch,
    /// One warn per session for a stream whose AUs will not plan: the picture is fine
    /// (openh264 decodes it), but colour is then whatever the last plannable AU said,
    /// and a support engineer must be able to see that from the log rather than infer it
    /// from a hue.
    plan_warned: bool,
}

/// Everything the planner tells the software rung about the AU it is ABOUT to submit.
struct AuFacts {
    is_idr: bool,
    /// `None` = the AU did not plan; the caller keeps the last colour it saw.
    color: Option<ColorDesc>,
    recovery: punktfunk_core::reanchor::LocalRecovery,
}

impl H264Software {
    fn new() -> Result<H264Software> {
        // Default config: error concealment OFF, logging quiet, one thread. Concealment
        // is deliberately not enabled — this rung's contract is that its errors SURFACE
        // (the pump turns an `Err` into a keyframe request through the same throttle as
        // every other rung), and a decoder quietly inventing macroblocks is precisely
        // the "looked clean, wasn't" shape M4's telemetry exists to make impossible.
        let decoder =
            openh264::decoder::Decoder::new().map_err(|e| anyhow!("openh264 decoder: {e}"))?;
        Ok(H264Software {
            decoder,
            planner: Box::new(H264Planner::new()),
            recovery: RecoveryWatch::new(),
            plan_warned: false,
        })
    }

    fn decode(&mut self, au: &[u8], color: &mut ColorDesc) -> Result<Option<CpuPlanarFrame>> {
        // Plan FIRST: the plan describes the AU we are about to decode, and reading it
        // after would attribute this picture's colour to the next one on a decoder that
        // buffers.
        let facts = self.plan_facts(au)?;
        if let Some(c) = facts.color {
            *color = c;
        }
        let picture = self
            .decoder
            .decode(au)
            .map_err(|e| anyhow!("openh264 decode: {e}"))?;
        let Some(yuv) = picture else {
            return Ok(None);
        };
        use openh264::formats::YUVSource as _;
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let frame = CpuPlanarFrame::from_i420(
            w as u32,
            h as u32,
            [yuv.y(), yuv.u(), yuv.v()],
            [sy, su, sv],
            *color,
            facts.is_idr,
            facts.recovery,
        )?;
        Ok(Some(frame))
    }

    /// The IDR flag, the colour and the recovery mark for this AU, from the shared
    /// planner.
    ///
    /// `colour` is `None` when the AU could not be planned — which is NORMAL for the
    /// first AUs after a mid-session demotion onto this rung: the parameter sets arrive
    /// in-band on the next IDR (which the demotion has already requested), so until it
    /// lands the planner has no active SPS and says so. Answering `is_idr = false` there
    /// is the conservative direction: it costs the re-anchor gate one frame of patience,
    /// where a false `true` would lift a post-loss freeze onto a picture that is still
    /// concealed — and the recovery mark is empty for the same reason.
    ///
    /// `Err` is reserved for the ONE thing that is not a metadata problem: a picture
    /// shape this rung cannot decode. It travels as [`NoSoftwareRung`] so the session
    /// reconnects instead of erroring per AU forever — see the type's `shape` field for
    /// why that answer has to come from here rather than from the Welcome.
    fn plan_facts(&mut self, au: &[u8]) -> Result<AuFacts> {
        match self.planner.plan_au(au) {
            Ok(plan) => {
                // The envelope, read from the SPS the planner activated for THIS picture
                // — so an in-band flip to Main 10 (a Windows HDR desktop) is caught on
                // the AU that carries it, not left to openh264 to fail on repeatedly.
                if let Some(shape) = unsupported_shape(
                    plan.picture.chroma_format_idc,
                    plan.picture.bit_depth_luma_minus8,
                ) {
                    return Err(NoSoftwareRung {
                        codec: punktfunk_core::quic::CODEC_H264,
                        shape: Some(shape),
                    }
                    .into());
                }
                let c = plan.picture.colour;
                // The wave's own verdict for this picture. Folded even when openh264
                // then produces nothing: the watch counts `frame_num` increments, so
                // skipping a picture would leave the count owing forever. Losing the
                // MARK of a picture that never came out only makes the lift late, which
                // is the safe direction.
                let mark = self.recovery.note_h264(
                    plan.picture.frame_num,
                    plan.picture.is_idr,
                    plan.picture.recovery_point,
                );
                Ok(AuFacts {
                    is_idr: plan.picture.is_idr,
                    color: Some(ColorDesc {
                        primaries: c.colour_primaries,
                        transfer: c.transfer_characteristics,
                        matrix: c.matrix_coefficients,
                        full_range: c.video_full_range,
                    }),
                    recovery: punktfunk_core::reanchor::LocalRecovery {
                        sei_here: mark.sei_here,
                        is_recovery_point: mark.is_recovery_point,
                    },
                })
            }
            Err(e) => {
                // `NoActiveParamSet` before the first in-band IDR is expected and says
                // nothing; anything else means the stream is outside the envelope the
                // hardware rungs plan from, which is worth exactly one line.
                if !matches!(e, PlanError::NoActiveParamSet { .. }) && !self.plan_warned {
                    self.plan_warned = true;
                    tracing::warn!(
                        error = %e,
                        "software rung: AU did not plan — colour signalling and the \
                         keyframe flag now follow the last AU that did"
                    );
                }
                Ok(AuFacts {
                    is_idr: false,
                    color: None,
                    recovery: punktfunk_core::reanchor::LocalRecovery::NONE,
                })
            }
        }
    }
}

/// The CPU rung's picture envelope: 8-bit 4:2:0 and nothing else. `Some(what)` names what
/// falls outside it, for [`NoSoftwareRung::shape`].
///
/// Stated once and shared by both legs. Neither decoder is BUILT for anything wider —
/// openh264 has no 4:2:2/4:4:4 or high-bit-depth support at all, and rav1d is compiled
/// here with `bitdepth_8` only — so this is a refusal that reflects the build, not a
/// policy that could drift from it.
fn unsupported_shape(chroma_format_idc: u8, bit_depth_minus8: u8) -> Option<&'static str> {
    if bit_depth_minus8 != 0 {
        return Some("10-bit or deeper");
    }
    if chroma_format_idc != punktfunk_core::quic::CHROMA_IDC_420 {
        return Some("chroma other than 4:2:0");
    }
    None
}

// --- AV1 (rav1d) ----------------------------------------------------------------------

// rav1d ships dav1d's C ABI as `#[no_mangle] extern "C"` Rust functions over `#[repr(C)]`
// types — there is no linker and no `.so` in sight, but the calling contract is still
// dav1d's, so the FFI discipline below is dav1d's too: every context/picture is owned by
// exactly one value here, and `Drop` closes it exactly once.
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{Dav1dSequenceHeader, DAV1D_PIXEL_LAYOUT_I420};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings,
    dav1d_get_frame_delay, dav1d_get_picture, dav1d_open, dav1d_parse_sequence_header,
    dav1d_picture_unref, dav1d_send_data,
};
use std::ptr::NonNull;

/// Frame contexts rav1d must end up with — see [`Av1Software::new`] for why ONE is fatal.
///
/// rav1d derives its count as `n_fc = min(max_frame_delay, n_threads)` (`get_num_threads`),
/// so this is a floor on BOTH settings, not just on the delay. Two, not more: every extra
/// context is another 4K working set, and the drain in [`Av1Software::decode`] takes the
/// frame back out in the same call, so there is nothing to buy above the minimum.
const AV1_MIN_FRAME_CONTEXTS: i32 = 2;

struct Av1Software {
    /// `None` only between `Drop` taking it and the close returning — every other
    /// observer sees a live context.
    ctx: Option<Dav1dContext>,
}

/// An owned `Dav1dData`, unref'd exactly once on drop.
///
/// The same shape (and the same lesson) as the libavcodec rungs' `AvBuffer`: the send loop
/// below has several fallible exits between allocating the buffer and dav1d taking its
/// reference, and hand-unref'ing on each one is how a leak per failed AU gets written.
/// dav1d zeroes the struct when it takes the reference, so an already-consumed `Dav1dData`
/// drops to a no-op and the double-unref this type prevents cannot happen either.
struct Av1Data(Dav1dData);

impl Av1Data {
    fn create(au: &[u8]) -> Result<Av1Data> {
        let mut data = Dav1dData::default();
        // SAFETY: `data` is a live local; `dav1d_data_create` either writes an allocated
        // buffer of `au.len()` bytes into it and returns its start, or returns null.
        let buf = unsafe { dav1d_data_create(NonNull::new(&mut data as *mut Dav1dData), au.len()) };
        if buf.is_null() {
            bail!("rav1d: could not allocate {} bytes for an AU", au.len());
        }
        // SAFETY: `buf` is the start of the `au.len()`-byte allocation just returned, and
        // `au` is a distinct live slice of exactly that length.
        unsafe { std::ptr::copy_nonoverlapping(au.as_ptr(), buf, au.len()) };
        Ok(Av1Data(data))
    }
}

impl Drop for Av1Data {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live `Dav1dData` this value solely owns (no `Clone`, and
        // `Drop` runs once). `dav1d_data_unref` releases whatever reference is left — none
        // when a successful `dav1d_send_data` already took it — and rewrites the struct.
        unsafe { dav1d_data_unref(NonNull::new(&mut self.0)) };
    }
}

// SAFETY: `Dav1dContext` is a refcounted handle dav1d documents as usable from one
// thread at a time; this type owns it exclusively (no `Clone`, no `Sync`) and it lives
// on the pump thread with the rest of the decoder — the same promise every other backend
// in this crate makes for its device handles.
unsafe impl Send for Av1Software {}

impl Av1Software {
    fn new() -> Result<Av1Software> {
        let mut settings = av1_settings();
        // Ask rav1d itself what those settings actually bought, and refuse to open a
        // decoder that would abort the process on its first bad AU.
        //
        // `dav1d_get_frame_delay` IS `get_num_threads`' `n_fc`, so this is not a restatement
        // of [`av1_settings`]' arithmetic — it is rav1d's own answer, and it stays right if
        // rav1d's derivation ever changes. It is here because the alternative failure mode
        // is uniquely bad: a settings edit that quietly reinstates `n_fc = 1` costs nothing
        // at build time, nothing in the tests, nothing on a clean link, and then kills the
        // whole client the first time a frame arrives damaged. A refusal is a `bail!` the
        // session reports and recovers from; the thing it replaces is `abort()`.
        let delay = frame_delay(&mut settings);
        if delay < AV1_MIN_FRAME_CONTEXTS {
            bail!(
                "rav1d would run with {delay} frame context(s) (n_threads={}, \
                 max_frame_delay={}); anything below {AV1_MIN_FRAME_CONTEXTS} takes the \
                 single-frame-context path, which ABORTS the process on any decode error",
                settings.n_threads,
                settings.max_frame_delay,
            );
        }
        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: both pointers are live locals for the duration of the call, which is
        // dav1d_open's whole contract: it reads `settings` and writes the context out.
        let r = unsafe {
            dav1d_open(
                NonNull::new(&mut ctx as *mut Option<Dav1dContext>),
                NonNull::new(&mut settings as *mut Dav1dSettings),
            )
        };
        if r.0 < 0 || ctx.is_none() {
            bail!("rav1d (dav1d) decoder open failed: {}", r.0);
        }
        Ok(Av1Software { ctx })
    }
}

/// The settings the AV1 CPU leg opens rav1d with. Its own function so the invariant the
/// rung's life depends on — `n_fc >= AV1_MIN_FRAME_CONTEXTS` — is testable without opening
/// a decoder.
fn av1_settings() -> Dav1dSettings {
    let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: `dav1d_default_settings` fully initializes the `Dav1dSettings` behind
    // the pointer it is given; the storage is a live local that outlives the call.
    let mut settings = unsafe {
        dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
        settings.assume_init()
    };
    // ⚠⚠⚠ TWO frame contexts, not one, and this is a CORRECTNESS setting.
    //
    // rav1d 1.1.0 (and upstream `main` as of 2026-08-07) aborts the process on ANY decode
    // error whenever it is configured with a single frame context. The path, read out of
    // `rav1d/src/decode.rs`:
    //
    //   * `rav1d_submit_frame`'s `c.fc.len() == 1` branch calls `rav1d_decode_frame`
    //     inline, which ALWAYS finishes in `rav1d_decode_frame_exit` — and that does an
    //     unconditional `mem::take(&mut f.frame_hdr)` (`decode.rs:4873`).
    //   * If the decode returned `Err`, the same branch then re-enters the local
    //     `on_error`, whose first act is `f.frame_hdr.as_ref().unwrap()`
    //     (`decode.rs:4997`) — on the `None` the teardown above just left.
    //   * That panic unwinds into `dav1d_send_data`, which is `extern "C"`, so it is
    //     `panic_cannot_unwind` → `abort()`. No `catch_unwind` at our call site, no rung
    //     demotion and no `NoSoftwareRung` refusal can catch an abort.
    //
    // The `c.fc.len() > 1` branch never calls `rav1d_decode_frame`, so it never reaches
    // that `on_error` at all: it hands the frame to `rav1d_task_frame_init` and errors come
    // back through `cached_error`/`task_thread.retval` as ordinary `EINVAL`s — which the
    // pump already answers with a keyframe request.
    //
    // Measured on .21 (2026-08-07) against a captured 4K60 AV1 stream that the client had
    // itself made undecodable by flushing its backlog and jumping to live, so the next AU
    // referenced frames nobody had decoded (libdav1d gives the same verdict on the same
    // capture: 13 frames, then "Invalid data"):
    //
    //   n_threads=8 max_frame_delay=1  → n_fc=1 → ABORT
    //   n_threads=1 max_frame_delay=1  → n_fc=1 → ABORT
    //   n_threads=1 max_frame_delay=2  → n_fc=1 → ABORT   ← the one that proves the rule
    //   n_threads=8 max_frame_delay=2  → n_fc=2 → 13 pictures, EINVAL, survives
    //   n_threads=8 max_frame_delay=0  → n_fc=3 → survives
    //
    // The third row is why `n_threads` has a floor of two and not one: `n_fc` is
    // `min(max_frame_delay, n_threads)`, so a single thread silently puts the whole thing
    // back on the aborting path. Pinning threads to 1 was also tested as the suspected
    // trigger and is NOT one — the tile workers are innocent, the single frame context is
    // the whole defect.
    //
    // The frame of latency this would normally cost is bought back in `decode`, which
    // drains the in-flight frame in the same call instead of pipelining it — see there.
    //
    // Reported upstream as memorysafety/rav1d#1497, with the one-line fix
    // (`is_some_and` for the `unwrap`) and a reproducer that needs no capture: any AV1
    // stream with one temporal unit removed from the middle. If a release ever carries
    // that fix, THIS floor is still the right default — it is what makes a decoder error
    // an error — but the `bail!` in `Av1Software::new` could then relax.
    settings.max_frame_delay = AV1_MIN_FRAME_CONTEXTS;
    // `n_threads` drives the INTRA-frame tile/row workers, which add no delay, and now also
    // floors `n_fc`. Capped at 8 — this is the rung reached because the GPU already failed,
    // and it should not also take the machine over.
    settings.n_threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(AV1_MIN_FRAME_CONTEXTS as usize, 8))
        .unwrap_or(AV1_MIN_FRAME_CONTEXTS as usize) as i32;
    // Film grain synthesis is a post-process the hosts never signal and nobody can
    // afford on the rung that exists because the GPU already failed.
    settings.apply_grain = 0;
    settings
}

impl Av1Software {
    fn decode(&mut self, au: &[u8], color: &mut ColorDesc) -> Result<Option<CpuPlanarFrame>> {
        let ctx = self.ctx.context("rav1d context closed")?;
        if au.is_empty() {
            return Ok(None);
        }
        // The envelope, off the BITSTREAM's own sequence header, before a byte reaches
        // the decoder — exactly what the H.264 leg does with `plan_au`, and for the same
        // reason. rav1d is compiled `bitdepth_8` only, so a 10-bit frame makes
        // `rav1d_submit_frame` refuse with `ENOPROTOOPT`; that refusal is a per-AU error
        // the pump would answer with a keyframe request forever, on a stream where every
        // following AU is identically 10-bit. This is the shipping case, not a corner:
        // AV1 is advertised only where hardware AV1 exists, hardware AV1 + HDR is Main 10,
        // and a mid-session hardware failure demotes here.
        if let Some(shape) = self.unsupported_sequence(au) {
            return Err(NoSoftwareRung {
                codec: punktfunk_core::quic::CODEC_AV1,
                shape: Some(shape),
            }
            .into());
        }
        // A `Dav1dData` that owns its own copy: `dav1d_data_create` allocates, we fill
        // it, and `dav1d_send_data` takes the reference on success. Deliberately not
        // `dav1d_data_wrap` over the caller's `au` — that would hand the decoder a
        // borrow of a buffer the pump reuses on the next AU.
        let mut data = Av1Data::create(au)?;
        // dav1d consumes `data` incrementally: a partial send leaves bytes in it and asks
        // to be re-sent. A punktfunk AU is one temporal unit and the decoder is drained
        // every call, so the loop is bounded by the AU — but it is a LOOP, because
        // `EAGAIN` here means "take pictures out first", not "the AU is bad".
        //
        // Only the NEWEST picture survives, which matches every other backend's
        // `decode -> Option<Frame>` contract (the old libav rung's `while receive_frame`
        // did the same). It matters more here than elsewhere because an AV1 temporal unit
        // really can carry several shown frames — but this is the rung reached after the
        // hardware already failed, and showing the newest is the same answer the pump's
        // newest-wins frame queue would give a moment later anyway.
        let mut out: Option<CpuPlanarFrame> = None;
        loop {
            // SAFETY: `ctx` is the live context from `dav1d_open` (not yet closed) and
            // `data.0` is a live local dav1d is allowed to read from and write to. Its
            // reference is taken by dav1d on success; the guard's `Drop` releases only
            // what is left.
            let r = unsafe { dav1d_send_data(Some(ctx), NonNull::new(&mut data.0)) };
            let sent = r.0 >= 0;
            if !sent && dav1d_errno(r) != Some(libc::EAGAIN) {
                // A shape the build cannot decode reaches here only if it slipped past
                // the sequence-header check above (an AU whose OBUs carry no sequence
                // header of their own). Still typed rather than generic: the pump's
                // survivable branch would ask for a keyframe and get the same refusal on
                // every AU for the rest of the session — a permanent freeze with no
                // fallback, which is the one outcome this rung exists to end.
                if dav1d_errno(r) == Some(libc::ENOPROTOOPT) {
                    return Err(NoSoftwareRung {
                        codec: punktfunk_core::quic::CODEC_AV1,
                        shape: Some("10-bit or deeper"),
                    }
                    .into());
                }
                bail!("rav1d send_data: {}", r.0);
            }
            // The AU is fully inside the decoder — stop feeding and go and drain it.
            if sent && data.0.sz == 0 {
                break;
            }
            match self.take_picture(ctx, color)? {
                Some(f) => out = Some(f),
                // Nothing to take and the decoder still would not accept the rest: it
                // has neither produced nor consumed, which is a wedge, not back-pressure.
                None if !sent => bail!("rav1d: decoder accepted no data and produced no picture"),
                None => {}
            }
        }
        // Now drain — and drain PAST the first `EAGAIN`, which is the whole trick that
        // makes two frame contexts cost no latency.
        //
        // `rav1d_get_picture` only reaches its blocking `drain_picture` on a call whose own
        // `drain` flag is ALREADY set, and that flag is set by the PREVIOUS `get_picture`
        // and cleared by every `send_data` that carried bytes. So the first `EAGAIN` after
        // a send does not mean "no picture for this AU" — it means "ask again", and the
        // frame this AU coded comes out of the SECOND call, which waits for the tile
        // workers instead of leaving the frame in flight. Stopping at the first `None` is
        // what a single-frame-context reading of dav1d's API teaches, and with `n_fc = 2`
        // it silently puts the pipeline two frames behind.
        //
        // Measured on .21 (2026-08-07), 4K60 AV1, 14 temporal units, `n_fc = 2`:
        //   stopping at the first `None`  → units 0 and 1 produce NOTHING, then one frame
        //                                   per unit: a standing two-frame delay
        //   draining past it (this loop)  → one frame per unit from unit 0, and 20-42 ms
        //                                   per unit against 21-53 ms at `n_fc = 1`
        // i.e. it is not a trade at all — same cadence as the aborting configuration, and
        // slightly faster, because the tile workers overlap the drain.
        //
        // Terminating: every `Some` consumes one picture and a temporal unit codes finitely
        // many, and rav1d opens each frame context `finished = true` (`lib.rs:232`), so the
        // drain walk over an idle context returns rather than waiting for a frame nobody
        // submitted.
        let mut idle = 0;
        while idle < 2 {
            match self.take_picture(ctx, color)? {
                Some(f) => {
                    out = Some(f);
                    idle = 0;
                }
                None => idle += 1,
            }
        }
        Ok(out)
    }

    /// This AU's sequence header against the build's envelope: `Some(what)` names what
    /// falls outside it, `None` means "8-bit 4:2:0, or this AU carries no sequence header
    /// of its own".
    ///
    /// An AU without one is the AV1 twin of the H.264 leg's `NoActiveParamSet`: it says
    /// nothing, so it decodes against whatever sequence the decoder already holds — which
    /// a previous AU was checked for. Punktfunk hosts re-send the sequence header on every
    /// key frame, and the demotion onto this rung asks for one immediately, so the first
    /// AU this rung ever decodes carries one.
    fn unsupported_sequence(&self, au: &[u8]) -> Option<&'static str> {
        let mut seq = std::mem::MaybeUninit::<Dav1dSequenceHeader>::uninit();
        // SAFETY: `out` is a live local this call either fully writes or leaves untouched
        // (it writes only on success), and `au` is a live slice of exactly `au.len()`
        // bytes. Nothing is allocated or referenced: dav1d fills the struct by value.
        let r = unsafe {
            dav1d_parse_sequence_header(
                NonNull::new(seq.as_mut_ptr()),
                NonNull::new(au.as_ptr().cast_mut()),
                au.len(),
            )
        };
        if r.0 < 0 {
            return None; // no sequence header here (ENOENT), or an AU we cannot read
        }
        // SAFETY: the call returned success, which is its contract for having written
        // the whole `Dav1dSequenceHeader`.
        let seq = unsafe { seq.assume_init() };
        if seq.hbd != 0 {
            return Some("10-bit or deeper");
        }
        if seq.layout != DAV1D_PIXEL_LAYOUT_I420 {
            return Some("chroma other than 4:2:0");
        }
        None
    }

    /// One picture out of the decoder, converted. `None` = nothing ready yet.
    fn take_picture(
        &self,
        ctx: Dav1dContext,
        color: &mut ColorDesc,
    ) -> Result<Option<CpuPlanarFrame>> {
        let mut pic = Dav1dPicture::default();
        // SAFETY: `ctx` is live and `pic` is a live local dav1d writes the picture into.
        let r =
            unsafe { dav1d_get_picture(Some(ctx), NonNull::new(&mut pic as *mut Dav1dPicture)) };
        if dav1d_errno(r) == Some(libc::EAGAIN) {
            return Ok(None);
        }
        if r.0 < 0 {
            bail!("rav1d get_picture: {}", r.0);
        }
        // From here the picture is OURS and must be unref'd on every exit — including the
        // refusals below, which is why the conversion is a closure and the unref is not
        // in a branch.
        let converted = Self::convert(&pic, color);
        // SAFETY: `pic` is the live picture `dav1d_get_picture` just wrote; this releases
        // exactly the one reference it handed over, once.
        unsafe { dav1d_picture_unref(NonNull::new(&mut pic as *mut Dav1dPicture)) };
        converted.map(Some)
    }

    fn convert(pic: &Dav1dPicture, color: &mut ColorDesc) -> Result<CpuPlanarFrame> {
        // 8-bit 4:2:0 only, stated as a refusal rather than assumed — and as the SAME
        // typed refusal the H.264 leg raises, so a shape this rung cannot decode
        // reconnects instead of erroring once per AU for the rest of the session.
        // Treating a 4:4:4 picture's planes as 4:2:0 would decode correctly and display
        // wrong, which is the class this program exists to refuse.
        //
        // A BELT, not the gate: `unsupported_sequence` refuses these shapes before the
        // AU is submitted, and a `bitdepth_8`-only build cannot produce a 10-bit picture
        // anyway (`rav1d_submit_frame` refuses the frame setup). Kept because it costs
        // two comparisons and because the day this build gains `bitdepth_16` the layout
        // half stops being redundant.
        let shape = if pic.p.bpc != 8 {
            Some("10-bit or deeper")
        } else if pic.p.layout != DAV1D_PIXEL_LAYOUT_I420 {
            Some("chroma other than 4:2:0")
        } else {
            None
        };
        if let Some(shape) = shape {
            return Err(NoSoftwareRung {
                codec: punktfunk_core::quic::CODEC_AV1,
                shape: Some(shape),
            }
            .into());
        }
        let (w, h) = (pic.p.w.max(0) as u32, pic.p.h.max(0) as u32);
        // Colour rides the SEQUENCE header, which AV1 re-sends whenever it changes — the
        // same per-picture contract the H.264 leg gets from the SPS, so an in-band
        // SDR↔HDR flip is followed rather than latched.
        if let Some(seq) = pic.seq_hdr {
            // SAFETY: `seq_hdr` belongs to the picture we hold a reference to, so it is
            // live for this call; these are plain scalar field reads.
            let seq = unsafe { seq.as_ref() };
            *color = ColorDesc {
                primaries: seq.pri as u8,
                transfer: seq.trc as u8,
                matrix: seq.mtrx as u8,
                full_range: seq.color_range != 0,
            };
        }
        let keyframe = pic.frame_hdr.is_some_and(|f| {
            // SAFETY: same as `seq_hdr` above — owned by the live picture, scalar read.
            unsafe { f.as_ref() }.frame_type == rav1d::include::dav1d::headers::DAV1D_FRAME_TYPE_KEY
        });
        let (_, ch) = CpuPlanarFrame::chroma_dims(w, h);
        // dav1d gives ONE chroma stride for both planes (`stride[1]`), which is why the
        // triple below repeats it rather than looking for a third.
        let strides = [
            pic.stride[0].max(0) as usize,
            pic.stride[1].max(0) as usize,
            pic.stride[1].max(0) as usize,
        ];
        let sizes = [
            h as usize * strides[0],
            ch as usize * strides[1],
            ch as usize * strides[2],
        ];
        let mut planes: [&[u8]; 3] = [&[], &[], &[]];
        for i in 0..3 {
            let p = pic.data[i].with_context(|| format!("rav1d: plane {i} is null"))?;
            // SAFETY: dav1d's picture contract is that plane `i` spans
            // `height_of_plane * |stride|` bytes from `data[i]`, and the picture holds a
            // reference to that allocation for as long as we do (unref'd by the caller,
            // after this conversion returns). Negative strides (bottom-up pictures) are
            // rejected above by the `max(0)` collapsing them to a zero size, which the
            // copy below then refuses.
            planes[i] = unsafe { std::slice::from_raw_parts(p.as_ptr().cast::<u8>(), sizes[i]) };
        }
        // No local-recovery answer from this leg: AV1 has no recovery point SEI, and its
        // intra-refresh equivalent (`frame_refs_short_signaling` / S-frames) is not
        // something a punktfunk host emits — so the pump's re-anchor behaviour on AV1 is
        // exactly the wire's, as it is on every lane but H.264/H.265.
        CpuPlanarFrame::from_i420(
            w,
            h,
            planes,
            strides,
            *color,
            keyframe,
            punktfunk_core::reanchor::LocalRecovery::NONE,
        )
    }
}

impl Drop for Av1Software {
    fn drop(&mut self) {
        if self.ctx.is_none() {
            return;
        }
        // SAFETY: `self.ctx` is the one context `dav1d_open` produced and this is its
        // sole owner, so this runs exactly once; `dav1d_close` takes it through the
        // `&mut` and leaves `None`.
        unsafe { dav1d_close(NonNull::new(&mut self.ctx as *mut Option<Dav1dContext>)) };
    }
}

/// The errno behind a `Dav1dResult`, or `None` for success.
///
/// rav1d returns the NEGATED errno as a plain `c_int`, and the codes are `libc`'s own
/// (`Rav1dError::ENOPROTOOPT = libc::ENOPROTOOPT as u8`) — so they are matched against
/// `libc`'s rather than written out. Not a nicety: `EAGAIN` is 11 everywhere but
/// `ENOPROTOOPT` is **92 on Linux and 123 on Windows**, and a literal would therefore be
/// right on exactly one platform. The typed enum this would rather match on
/// (`Rav1dError`) lives in a `pub(crate)` module — rav1d re-exports only `Dav1dResult` —
/// so the errno is the only handle the crate actually offers.
fn dav1d_errno(r: rav1d::Dav1dResult) -> Option<i32> {
    (r.0 < 0).then_some(-r.0)
}

/// How many frame contexts (`n_fc`) rav1d will actually run with, given these settings.
///
/// rav1d's own `get_num_threads` answer rather than a copy of its arithmetic, so the floor
/// [`Av1Software::new`] enforces cannot drift away from the thing it is protecting against.
/// A negative return (settings rav1d would refuse outright) collapses to 0, which the
/// caller's floor check then rejects — the right answer for that case too.
fn frame_delay(settings: &mut Dav1dSettings) -> i32 {
    // SAFETY: `settings` is a live local for the duration of the call, which is this
    // function's whole contract — it `ptr::read`s the struct and writes nothing back. The
    // read is a bitwise copy of a plain-data struct (`Rav1dSettings` has no `Drop`), so it
    // cannot release anything the caller still owns.
    let r = unsafe { dav1d_get_frame_delay(NonNull::new(settings as *mut Dav1dSettings)) };
    r.0.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::csc_rows;

    /// The nine bars every fixture encodes, in x order: eight fully-saturated
    /// primaries/secondaries plus black and white, and then the one that carries the
    /// RANGE axis.
    ///
    /// ⚠ `(192, 128, 64)` is not decoration. On saturated bars a limited↔full mismatch
    /// only pushes values outside [0, 1], where the shader clamps — so the 709-FULL
    /// fixture decoded with the WRONG range comes back with max error **0** over the
    /// eight, and this test could not fail on range at all. Measured on these fixtures:
    /// the mid-tone gives max error 11 under the wrong range. A 50% grey does not do the
    /// job either (3, inside the ±4 tolerance) — it has to be OFF-neutral, so the chroma
    /// scale is exercised and not just the luma one.
    const BARS: [(u8, u8, u8); 9] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
        (192, 128, 64),
    ];

    /// The presenter's planar CSC shader, on the CPU: sample the three planes and apply
    /// `csc_rows` exactly as `planar_csc.frag` does (`rgb[i] = dot(r[i].xyz, yuv) +
    /// r[i].w`, then clamp). 8-bit, no MSB packing — the software rung's only shape.
    ///
    /// This is a MODEL of the shader, and it is the honest one: `csc_rows` is the single
    /// coefficient implementation the shader's push constants are filled from (and the
    /// Windows client's constant buffer, and the Apple client's Swift port), so what this
    /// exercises end to end is exactly what changes colour on screen — the decoder's
    /// plane layout, and the `ColorDesc` it read out of the bitstream. Sampling is
    /// nearest at bar centres, which is where the shader's quarter-texel 4:2:0 siting
    /// correction and its linear filter both make no difference.
    fn shader_rgb(f: &CpuPlanarFrame, x: u32, y: u32) -> [u8; 3] {
        let rows = csc_rows(f.color, 8, false);
        let (cw, _) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        let luma = f.plane(0)[(y * f.width + x) as usize];
        let (cx, cy) = (x / 2, y / 2);
        let cb = f.plane(1)[(cy * cw + cx) as usize];
        let cr = f.plane(2)[(cy * cw + cx) as usize];
        let yuv = [luma as f32 / 255.0, cb as f32 / 255.0, cr as f32 / 255.0];
        core::array::from_fn(|i| {
            let v = rows[i][0] * yuv[0] + rows[i][1] * yuv[1] + rows[i][2] * yuv[2] + rows[i][3];
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        })
    }

    fn decode_one(codec: u8, au: &[u8]) -> CpuPlanarFrame {
        let mut dec = SoftwareDecoder::new(codec).expect("software decoder");
        dec.decode(au)
            .expect("decode")
            .expect("no frame out of the fixture")
    }

    /// **M8's exit criterion.** Three lossless-ish H.264 colour-bar fixtures whose VUIs
    /// differ ONLY in matrix and range (see `tests/gen-bars.sh` for the recipe): decode
    /// each through the real CPU rung, then convert with the real `csc_rows`, and require
    /// the original RGB back.
    ///
    /// What it would have caught, one failure per axis:
    ///
    /// * **The BT.601 default** — the bug the deleted `convert_rgba` carried explicit
    ///   correction code for. swscale converts with BT.601 coefficients unless told
    ///   otherwise, so a rung that dropped the signalling (or hardcoded one matrix)
    ///   renders the 601 fixture with 709 coefficients or vice versa. On the saturated
    ///   bars that is tens of code points — e.g. pure red's green channel goes from 0 to
    ///   ~+40 — far outside the ±4 tolerance (measured on these fixtures: max error 22
    ///   for 709 read as 601, 39 the other way).
    /// * **Range** — the 709-full fixture differs from 709-limited by the 16..235 vs
    ///   0..255 expansion only. ⚠ On the eight saturated bars this axis CANNOT fail:
    ///   every one of them is at an extreme, so a mismatch only pushes values outside
    ///   [0, 1] where the shader clamps, and the fixture decodes with max error **0**
    ///   under the wrong range. The ninth bar, `(192, 128, 64)`, is what makes the axis
    ///   testable (max error 11 wrong-range, ~1 right) — see [`BARS`].
    /// * **Plane order and stride** — a Cb/Cr swap turns red into blue, and a stride
    ///   mistake shears the bars sideways, so both show up as a wrong bar rather than a
    ///   wrong shade.
    ///
    /// It is deliberately NOT a "did it decode" test: every assertion is a pixel value
    /// that depends on the colour signalling surviving the whole path.
    #[test]
    fn software_h264_reproduces_the_golden_bars_in_both_ranges() {
        let fixtures: [(&str, &[u8], ColorDesc); 3] = [
            (
                "601-limited",
                include_bytes!("../tests/bars-601-limited.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 5, // BT.470BG — what a Linux host's RGB-input NVENC signals
                    full_range: false,
                },
            ),
            (
                "709-limited",
                include_bytes!("../tests/bars-709-limited.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: false,
                },
            ),
            (
                "709-full",
                include_bytes!("../tests/bars-709-full.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: true,
                },
            ),
        ];
        for (name, au, want_color) in fixtures {
            let f = decode_one(punktfunk_core::quic::CODEC_H264, au);
            assert_eq!(f.color, want_color, "{name}: signalling");
            assert_eq!((f.width, f.height), (288, 64), "{name}: dims");
            assert!(f.keyframe, "{name}: the fixture is a single IDR");
            for (i, (r, g, b)) in BARS.iter().enumerate() {
                let px = shader_rgb(&f, i as u32 * 32 + 16, 32);
                for (got, want) in px.iter().zip([r, g, b]) {
                    assert!(
                        got.abs_diff(*want) <= 4,
                        "{name} bar {i}: got {px:?}, want ({r},{g},{b})"
                    );
                }
            }
        }
    }

    /// The same three fixtures, but asserting the thing a "it decoded" test cannot: the
    /// 601 and 709 pictures are DIFFERENT pixels, so a rung that ignored the signalling
    /// and converted both with one matrix would still pass a self-consistency check.
    ///
    /// Guards the fixtures themselves as much as the code — if a regeneration ever
    /// produced two identical bitstreams the colour test above would go vacuously green.
    #[test]
    fn the_601_and_709_fixtures_really_do_carry_different_luma() {
        let f601 = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-601-limited.h264"),
        );
        let f709 = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-709-limited.h264"),
        );
        // Pure red: Y = 0.299·255 ≈ 76 under 601, 0.2126·255 ≈ 54 under 709 (both then
        // range-compressed to 16..235). Same displayed colour, different code points.
        let (x, y) = (5 * 32 + 16, 32);
        let a = f601.plane(0)[(y * f601.width + x) as usize];
        let b = f709.plane(0)[(y * f709.width + x) as usize];
        assert!(
            a.abs_diff(b) > 10,
            "601 luma {a} vs 709 luma {b} — the fixtures do not differ, so the colour \
             test above proves nothing"
        );
        // ...and after the CSC both land on the same red.
        for (f, name) in [(&f601, "601"), (&f709, "709")] {
            let px = shader_rgb(f, x, y);
            assert!(
                px[0].abs_diff(255) <= 4 && px[1] <= 4 && px[2] <= 4,
                "{name}: red bar came out {px:?}"
            );
        }
    }

    /// HEVC has no CPU rung and must say so with the TYPE the session layer keys its
    /// reconnect off — not with a string, and not by quietly producing nothing.
    #[test]
    fn hevc_is_refused_with_the_typed_no_rung_error() {
        let err = SoftwareDecoder::new(punktfunk_core::quic::CODEC_HEVC)
            .err()
            .expect("HEVC must not build a software decoder");
        let typed = err
            .downcast_ref::<NoSoftwareRung>()
            .expect("the refusal must survive as NoSoftwareRung through anyhow");
        assert_eq!(typed.codec, punktfunk_core::quic::CODEC_HEVC);
        assert_eq!(typed.shape, None, "the CODEC is missing, not a shape");
        assert!(err.to_string().contains("HEVC"), "{err}");
        // And the two codecs that DO have one still build.
        assert!(SoftwareDecoder::new(punktfunk_core::quic::CODEC_H264).is_ok());
        assert!(SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).is_ok());
    }

    /// A picture shape the CPU rung cannot decode must raise the SAME typed refusal as a
    /// missing codec, because it has the same available answer (reconnect) and because
    /// the alternative — an `Err` per AU forever, or 8-bit maths over 10-bit samples — is
    /// respectively a frozen screen and a wrong one.
    ///
    /// Exercised as the pure rule plus the two shapes a punktfunk host can actually
    /// resolve: Main 10 (an HDR desktop, flipped IN-BAND, which is why the check reads
    /// the ACTIVE SPS and not the Welcome) and 4:4:4 (the "Full chroma" opt-in).
    #[test]
    fn a_shape_the_cpu_rung_cannot_decode_is_the_same_typed_refusal() {
        use punktfunk_core::quic::{CHROMA_IDC_420, CHROMA_IDC_444};
        // 8-bit 4:2:0 is the whole envelope.
        assert_eq!(unsupported_shape(CHROMA_IDC_420, 0), None);
        assert_eq!(
            unsupported_shape(CHROMA_IDC_420, 2),
            Some("10-bit or deeper")
        );
        assert_eq!(
            unsupported_shape(CHROMA_IDC_444, 0),
            Some("chroma other than 4:2:0")
        );
        // Depth is reported FIRST when both are wrong: it is the one that silently
        // mis-scales rather than merely mis-siting, so it is the more useful diagnosis.
        assert_eq!(
            unsupported_shape(CHROMA_IDC_444, 2),
            Some("10-bit or deeper")
        );
        // And the refusal reaches a caller as the type the session keys its reconnect
        // off, with a message that says which stream, not just "decode failed".
        let e: anyhow::Error = NoSoftwareRung {
            codec: punktfunk_core::quic::CODEC_AV1,
            shape: Some("10-bit or deeper"),
        }
        .into();
        let typed = e.downcast_ref::<NoSoftwareRung>().expect("typed");
        assert_eq!(typed.shape, Some("10-bit or deeper"));
        assert!(e.to_string().contains("AV1"), "{e}");
        assert!(e.to_string().contains("8-bit 4:2:0 only"), "{e}");
    }

    /// The 709-full fixture must be ABLE to fail on the range axis. It could not before
    /// the M8 review — every bar was saturated, so a limited↔full mismatch only pushed
    /// values past the shader's clamp and the decode came back byte-perfect with the
    /// WRONG range honoured.
    ///
    /// Guards the fixture, not the code: if `BARS` ever loses its mid-tone (or a
    /// regeneration drops the ninth bar), the range half of the test above goes vacuous
    /// and this is what says so.
    #[test]
    fn the_full_range_fixture_is_decoded_wrong_by_the_wrong_range() {
        let f = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-709-full.h264"),
        );
        let wrong = ColorDesc {
            full_range: false,
            ..f.color
        };
        let rows = csc_rows(wrong, 8, false);
        let (cw, _) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        let mut worst = 0u8;
        for (i, (r, g, b)) in BARS.iter().enumerate() {
            let (x, y) = (i as u32 * 32 + 16, 32u32);
            let luma = f.plane(0)[(y * f.width + x) as usize];
            let cb = f.plane(1)[((y / 2) * cw + x / 2) as usize];
            let cr = f.plane(2)[((y / 2) * cw + x / 2) as usize];
            let yuv = [luma as f32 / 255.0, cb as f32 / 255.0, cr as f32 / 255.0];
            let px: [u8; 3] = core::array::from_fn(|c| {
                let v =
                    rows[c][0] * yuv[0] + rows[c][1] * yuv[1] + rows[c][2] * yuv[2] + rows[c][3];
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            });
            for (got, want) in px.iter().zip([r, g, b]) {
                worst = worst.max(got.abs_diff(*want));
            }
        }
        assert!(
            worst > 4,
            "decoding the FULL-range fixture as LIMITED was off by only {worst}, inside \
             the ±4 tolerance — the fixture no longer tests the range axis at all"
        );
    }

    /// **B1.** A 10-bit AV1 stream must be REFUSED with the typed error, not errored on
    /// per AU forever.
    ///
    /// This is the shipping case, not a corner: AV1 is advertised only where hardware AV1
    /// exists, hardware AV1 + HDR is Main 10, and a mid-session hardware failure demotes
    /// onto this rung. rav1d is built `bitdepth_8`, so its frame setup refuses with
    /// `ENOPROTOOPT`; before the review that surfaced as a generic `anyhow` the pump read
    /// as survivable — keyframe requested, next AU identically 10-bit, screen frozen for
    /// the rest of the session with no fallback.
    ///
    /// The fixture is a whole 10-bit AV1 temporal unit (SVT-AV1, 64x64 red, one key
    /// frame) inline rather than on disk: 38 bytes, and what is being tested is the
    /// SEQUENCE HEADER inside it.
    #[test]
    fn a_10bit_av1_stream_is_refused_with_the_typed_no_rung_error() {
        const TU_10BIT: [u8; 38] = [
            0x12, 0x00, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x02, 0xaf, 0xff, 0x8d, 0x5f, 0x38, 0x08,
            0x32, 0x16, 0x10, 0x00, 0xba, 0x02, 0x0b, 0x2c, 0x51, 0x41, 0x00, 0x00, 0x08, 0x00,
            0x95, 0xd1, 0xe2, 0x7e, 0xac, 0x4f, 0x04, 0xad, 0xa4, 0x70,
        ];
        let mut dec = SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).expect("av1 decoder");
        let err = dec
            .decode(&TU_10BIT)
            .err()
            .expect("a 10-bit AV1 AU must not decode on an 8-bit-only build");
        let typed = err.downcast_ref::<NoSoftwareRung>().expect(
            "the refusal must survive as NoSoftwareRung through anyhow — a generic \
                     error here is the permanent freeze this test exists to prevent",
        );
        assert_eq!(typed.codec, punktfunk_core::quic::CODEC_AV1);
        assert_eq!(typed.shape, Some("10-bit or deeper"));
        // ...and the session layer's rule reads it as a SHAPE loss, so the retry is not
        // narrowed to codecs with a CPU rung.
        assert_eq!(typed.loss(), crate::video::RungLoss::Shape);
        // Every following AU raises the same refusal rather than the decoder wedging:
        // the pump breaks out on the first one, but a stuck loop here is the failure
        // mode, so prove it stays a refusal.
        assert!(dec
            .decode(&TU_10BIT)
            .err()
            .and_then(|e| e.downcast_ref::<NoSoftwareRung>().copied())
            .is_some());
    }

    /// **The rung must never open rav1d with one frame context.**
    ///
    /// With `n_fc == 1`, rav1d 1.1.0 `abort()`s the whole client on ANY decode error:
    /// `rav1d_submit_frame`'s single-frame-context branch runs `rav1d_decode_frame`, whose
    /// `rav1d_decode_frame_exit` unconditionally takes `f.frame_hdr`, and then — only if
    /// the decode failed — calls an `on_error` that opens with
    /// `f.frame_hdr.as_ref().unwrap()`. The panic crosses `dav1d_send_data`'s `extern "C"`
    /// frame as `panic_cannot_unwind`, so nothing in this crate can catch it: not the
    /// pump's survivable-error arm, not a rung demotion, not a `NoSoftwareRung` refusal.
    /// Reproduced on .21 on 2026-08-07 with a 4K60 AV1 capture, twice.
    ///
    /// The assertion is rav1d's OWN `n_fc` (`dav1d_get_frame_delay` is literally
    /// `get_num_threads`' answer), not a re-derivation of it here, so it also holds if
    /// rav1d changes how the number is computed.
    #[test]
    fn the_av1_rung_never_opens_rav1d_with_a_single_frame_context() {
        let mut s = av1_settings();
        let n_fc = frame_delay(&mut s);
        assert!(
            n_fc >= AV1_MIN_FRAME_CONTEXTS,
            "rav1d would run with n_fc={n_fc} (n_threads={}, max_frame_delay={}) — one \
             frame context ABORTS the process on the first damaged AU",
            s.n_threads,
            s.max_frame_delay,
        );
        // ...and the constructor refuses rather than opening such a decoder, so a machine
        // whose settings somehow land there loses the rung instead of the process.
        assert!(Av1Software::new().is_ok());
    }

    /// ⚠ The trap that makes the setting above look like a one-liner when it is two.
    ///
    /// `n_fc = min(max_frame_delay, n_threads)`, so `max_frame_delay = 2` on its own is NOT
    /// enough: one decode thread drags `n_fc` back to 1 and reinstates the abort. Measured
    /// on .21 — `n_threads=1, max_frame_delay=2` aborted on the same capture that
    /// `n_threads=8, max_frame_delay=2` survives — which is also the datapoint that rules
    /// out "the tile workers are the trigger": fewer threads made it worse, not better.
    ///
    /// Asserted against rav1d's own arithmetic so it cannot rot into folklore.
    #[test]
    fn one_decode_thread_would_put_the_rung_back_on_the_aborting_path() {
        let mut one_thread = av1_settings();
        one_thread.n_threads = 1;
        assert_eq!(
            frame_delay(&mut one_thread),
            1,
            "n_fc is min(max_frame_delay, n_threads) — this is why n_threads has a floor"
        );
        // And the floor in `av1_settings` is what keeps the shipping config off it.
        assert!(av1_settings().n_threads >= AV1_MIN_FRAME_CONTEXTS);
    }

    /// The AV1 leg decodes a real stream and reports the sequence header's own colour.
    /// Fixture: the vendored cros-codecs AV1 vector (IVF), whose first temporal unit is a
    /// key frame — enough to prove the rav1d FFI (open → send → get → unref → close),
    /// the I420 plane copy and the colour read, none of which the H.264 leg exercises.
    #[test]
    fn software_av1_decodes_and_reports_its_sequence_colour() {
        const IVF: &[u8] = include_bytes!(
            "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
        );
        // IVF: 32-byte file header, then per-frame [u32 size][u64 pts][payload].
        let mut off = 32usize;
        let mut dec = SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).expect("av1 decoder");
        let mut first = None;
        let (mut units, mut frames) = (0u32, 0u32);
        // Drive the WHOLE vector, not just the first unit: the send loop runs once per
        // temporal unit and its `Dav1dData` guard drops on every path through it, so a
        // leak or a wedge shows up as the run failing rather than as a slow drift nobody
        // reproduces.
        while off + 12 <= IVF.len() {
            let sz = u32::from_le_bytes(IVF[off..off + 4].try_into().unwrap()) as usize;
            off += 12;
            if off + sz > IVF.len() {
                break;
            }
            units += 1;
            if let Some(f) = dec.decode(&IVF[off..off + sz]).expect("av1 decode") {
                frames += 1;
                if first.is_none() {
                    first = Some(f);
                }
            }
            off += sz;
        }
        assert!(
            units > 100,
            "expected the full 25 fps vector, got {units} units"
        );
        // ⚠ This equality is also the LATENCY guard for the two-frame-context config.
        // rav1d with `n_fc = 2` pipelines by default: `dav1d_get_picture` answers `EAGAIN`
        // once after each send and only reaches its blocking drain on the call after that.
        // A `decode` that stopped at the first `EAGAIN` would still decode every frame
        // eventually — it would just hand each one back two AUs late, and `frames` would
        // come up exactly `n_fc` short of `units` here. So this line is what says the drain
        // loop still returns THIS AU's picture in THIS call.
        assert_eq!(frames, units, "every temporal unit here shows a picture");
        let f = first.expect("no AV1 frame decoded");
        assert_eq!((f.width, f.height), (320, 240));
        assert!(f.keyframe, "the first temporal unit is a key frame");
        // The vector signals nothing, so E.2.1-equivalent "unspecified" (2) must come
        // through UNTOUCHED — `csc_rows` is what resolves it to the BT.709 SDR default,
        // and a decoder that resolved it early would make an in-band HDR flip invisible.
        assert_eq!(f.color.matrix, 2, "unspecified matrix must survive as 2");
        assert!(!f.color.full_range);
        // Planes are tightly packed at the picture's own size — the presenter uploads
        // them with no stride, so this invariant is load-bearing, not cosmetic.
        assert_eq!(f.plane(0).len(), (f.width * f.height) as usize);
        let (cw, ch) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        assert_eq!(f.plane(1).len(), (cw * ch) as usize);
        assert_eq!(f.plane(2).len(), (cw * ch) as usize);
    }
}
