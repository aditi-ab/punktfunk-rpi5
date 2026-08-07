//! Native VAAPI decode — M6 of the native-decode program, and the FFmpeg-free
//! replacement for `video_vaapi`, the libavcodec VAAPI rung M10 deleted.
//!
//! `pf-vaadec` turns one pf-bitstream `AuPlan` into the buffers a
//! `vaRenderPicture` call carries; this module is everything libva-shaped around
//! that: the display, the config and context, the surface pool, the submission, and
//! the DRM-PRIME export the presenter imports. Its output is
//! [`DecodedImage::NativeDmabuf`] — physically identical to what the libavcodec VAAPI
//! rung delivered, and deliberately a different variant so that while both existed no
//! log could confuse them (see that variant's docs).
//!
//! # libva is dlopen'd, never linked
//!
//! Everything here resolves `libva.so.2` and `libva-drm.so.2` at runtime. Three
//! things follow, and all three are the point:
//!
//! * `pf-client-core` gains no build-time libva dependency, so the **pf-lxcheck2
//!   container compiles and clippies this whole rung** even though it has no
//!   `libva-dev`. On a program where `cfg(windows)` code could only ever be checked
//!   on a box, that is the difference between a defect found on a laptop and one
//!   found on hardware.
//! * A machine without libva gets a clean refusal at construction — the ladder
//!   falls through exactly as it does for any other unavailable rung — instead of a
//!   packaging dependency or a link error.
//! * Nothing in the shipped packages needs to change to try it.
//!
//! # The surface pool, and why it is not the slot map
//!
//! VAAPI has no DPB slots: `VAPictureH264::picture_id` is a `VASurfaceID`, and
//! `vaBeginPicture` takes the target surface itself. The slot ledger
//! ([`pf_vaadec::SlotMap`], borrowed from the Vulkan rung) is our own indirection
//! from a stable `PicId` to a small integer, and a slot is emphatically NOT a
//! surface index.
//!
//! It cannot be, because [`pf_vaadec::SlotMap::assign`] hands out the lowest free
//! slot and a slot freed by this access unit's own removals is free by then —
//! measured at **225 of the vendored vector's 250 access units** (pf-vaadec's
//! `the_setup_picture_routinely_inherits_a_just_freed_slot`). A surface bound by
//! slot index would therefore decode, on nine frames in ten, straight into the
//! surface holding the picture that was just displayed. Under zero-copy the
//! presenter is still sampling that surface: it holds the frame until its fence has
//! been waited, which is exactly what "zero-copy" costs.
//!
//! So the pool follows pf-vkdecode's image model. Surfaces outnumber slots by
//! [`pf_vaadec::config::PRESENTER_HEADROOM`], the decode target is taken from a free
//! list at activation time and bound to its slot afterwards, and a surface the
//! presenter holds simply stays off the free list until its release token comes
//! back. A surface is free when no live picture is bound to it AND no consumer holds
//! it — two conditions, tracked separately, because they end at different times.
//!
//! # Why this rung is exempt from the decode-into-a-reference defect
//!
//! The D3D11VA and Vulkan rungs both had to grow a `release_after_decode` deferral:
//! their conversions released the pictures an access unit displaces INSIDE the
//! conversion, then assigned the decode target a slot, and [`pf_vaadec::SlotMap::assign`]
//! handed back the slot just vacated — so one surface was named as both the decode
//! target and one of that submission's own references. On H.264 that fired on **117 of
//! 120** access units of a punktfunk host's low-delay output.
//!
//! `pf-vaadec`'s conversions still release inline and this rung is still exempt, for a
//! reason that is a property of the interface rather than of any stream: **a slot is
//! not a surface here.** `plan_to_va` never invents a surface — every reference it can
//! name is read out of the `surfaces` table it is handed — and the decode target is a
//! separate parameter the caller takes from OUTSIDE that table. Two things carry that,
//! and both are load-bearing:
//!
//! * [`Session::acquire_target`] returns the target and the table **together, from one
//!   snapshot**, because they are only safe together. A free surface is by construction
//!   a surface no slot binds, and the table is exactly what the slots bind, so the
//!   target cannot be in it. Taking the two at different moments — the table before
//!   this access unit's removals, where references must resolve, and the free surface
//!   after them, where the displaced picture's surface has become free — is precisely
//!   the defect, and `taking_the_free_surface_after_the_removals_would_hand_out_a_
//!   referenced_surface` shows it happening.
//! * The conversion's half is pinned across every platform by `pf-vaadec`'s
//!   `no_submission_names_its_decode_target_as_one_of_its_own_references`, driven over
//!   the same low-delay stream, with
//!   `taking_the_decode_target_from_the_slot_table_aliases_on_the_low_delay_stream` as
//!   the counterfactual that shows the walk can see the defect when it is there.
//!
//! It holds for all three codecs and for the same one-line reason: `setup_surface`
//! reaches the submission at exactly ONE field in each conversion — H.264 and H.265'
//! `curr_pic.picture_id`, AV1's `current_frame`/`current_display_picture` — and every
//! reference field is resolved through the `surfaces` table. HEVC is doubly covered:
//! its per-slice `RefPicList` stores an INDEX into `reference_frames`, so it cannot
//! name a surface that array does not already hold.
//!
//! ⚠ One documented exception, and it is not this defect: `plan_to_va_av1` substitutes
//! a live surface for a reference slot the planner reports empty, and where the store
//! resolved NOTHING at all the fallback is the decode target itself (that conversion's
//! module docs say why, and prefer a resolved reference wherever one exists). It names
//! the target only when there is no other live surface to name, on a frame that is
//! already concealed and will not be shown.
//!
//! ⚠ And one assumption, stated because it is the only way the argument fails: the pool
//! holds DISTINCT `VASurfaceID`s. Two pool entries with one id would let a free index
//! resolve to a bound surface. `vaCreateSurfaces` cannot return duplicates — this rung
//! also destroys each exactly once, which the same duplication would double-free — so
//! it is an assumption about libva rather than about this file.

use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;
use std::os::raw::c_char;
use std::os::raw::c_int;
use std::os::raw::c_uint;
use std::os::raw::c_void;
use std::sync::mpsc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;

use crate::video::DecodeHealth;
use crate::video::DmabufFrame;
use crate::video::DmabufPlane;
use crate::video::DrmFrameGuard;
use crate::video::StreamFormat;
use crate::video_color::ColorDesc;

/// `PUNKTFUNK_DECODER=native-vaapi` — the pin that selects this rung.
///
/// ⚠ This rung has decoded nothing on any hardware (`video::native_evidence`). Since M10
/// deleted libavcodec's VAAPI hwaccel it is the only VAAPI there is, so `auto` reaches it
/// in the vendor order and the session log says at `warn` that nothing has run it.
///
/// The PIN is what makes the missing evidence generatable: it skips the vendor order, so a
/// lab run can reach this rung on a box where `auto` puts Vulkan first. A rule that gated
/// the pin too would be a rule no hardware run could ever satisfy.
pub(crate) const DECODER_PIN: &str = "native-vaapi";

// ---------------------------------------------------------------------------
// libva, resolved at runtime
// ---------------------------------------------------------------------------

/// `VADisplay` — an opaque driver handle.
type VaDisplay = *mut c_void;
type VaStatus = c_int;
type VaSurfaceId = c_uint;
type VaConfigId = c_uint;
type VaContextId = c_uint;
type VaBufferId = c_uint;

const VA_STATUS_SUCCESS: VaStatus = 0;
/// `VA_INVALID_ID` — also the "no surface" sentinel in a slot table.
const VA_INVALID_ID: c_uint = 0xffff_ffff;
/// `VA_PROGRESSIVE` — the only picture structure this rung's envelope contains.
const VA_PROGRESSIVE: c_uint = 0x0001;

/// `VAGenericValue` — 16 bytes, value at offset 8, align 8 (measured).
///
/// The C type's `value` is a union of `int`/`float`/`void*`/function pointer. Two
/// consequences are written out here rather than left to a Rust `union` declaration:
///
/// * the union holds a pointer, so it is **eight-byte aligned** — which is why the
///   enum ahead of it is followed by four bytes of padding, and why the whole thing
///   is 16 bytes and not 12. The compile-time assertion below caught exactly that
///   mistake in this file;
/// * a Rust union initialised through its `i32` arm leaves the other four bytes
///   **uninitialised**, and those are the bytes a driver reading the pointer arm
///   would see. Naming the remainder and writing zero means everything crossing the
///   FFI boundary was written by us.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct VaGenericValue {
    kind: c_int,
    _pad: u32,
    /// The union's integer arm, first in the union — where `VAGenericValue.i` lives.
    i: i32,
    /// The rest of the union. Always zero.
    _rest: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaSurfaceAttrib {
    kind: c_int,
    flags: c_uint,
    value: VaGenericValue,
}

/// `VASurfaceAttribPixelFormat` / `VAGenericValueTypeInteger` /
/// `VA_SURFACE_ATTRIB_SETTABLE` — measured by `pf-vaadec/layout-probe.c`.
const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
const VA_GENERIC_VALUE_TYPE_INTEGER: c_int = 1;
const VA_SURFACE_ATTRIB_SETTABLE: c_uint = 0x0002;

// The layouts these calls pass by value, measured (`pf-vaadec/layout-probe.c`).
const _: () = {
    assert!(size_of::<VaGenericValue>() == 16);
    assert!(std::mem::offset_of!(VaGenericValue, i) == 8);
    assert!(size_of::<VaSurfaceAttrib>() == 24);
    assert!(std::mem::offset_of!(VaSurfaceAttrib, flags) == 4);
    assert!(std::mem::offset_of!(VaSurfaceAttrib, value) == 8);
};

/// The libva entry points this rung calls, resolved from `libva.so.2` and
/// `libva-drm.so.2` at runtime (the same pattern the host's NVML and CUDA loaders
/// use — no link-time dependency, absent library = clean refusal).
struct Libva {
    _va: libloading::Library,
    _drm: libloading::Library,
    get_display_drm: unsafe extern "C" fn(c_int) -> VaDisplay,
    initialize: unsafe extern "C" fn(VaDisplay, *mut c_int, *mut c_int) -> VaStatus,
    terminate: unsafe extern "C" fn(VaDisplay) -> VaStatus,
    error_str: unsafe extern "C" fn(VaStatus) -> *const c_char,
    query_config_entrypoints:
        unsafe extern "C" fn(VaDisplay, c_int, *mut c_int, *mut c_int) -> VaStatus,
    max_entrypoints: unsafe extern "C" fn(VaDisplay) -> c_int,
    create_config: unsafe extern "C" fn(
        VaDisplay,
        c_int,
        c_int,
        *mut c_void,
        c_int,
        *mut VaConfigId,
    ) -> VaStatus,
    destroy_config: unsafe extern "C" fn(VaDisplay, VaConfigId) -> VaStatus,
    create_surfaces: unsafe extern "C" fn(
        VaDisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VaSurfaceId,
        c_uint,
        *mut VaSurfaceAttrib,
        c_uint,
    ) -> VaStatus,
    destroy_surfaces: unsafe extern "C" fn(VaDisplay, *mut VaSurfaceId, c_int) -> VaStatus,
    create_context: unsafe extern "C" fn(
        VaDisplay,
        VaConfigId,
        c_int,
        c_int,
        c_int,
        *mut VaSurfaceId,
        c_int,
        *mut VaContextId,
    ) -> VaStatus,
    destroy_context: unsafe extern "C" fn(VaDisplay, VaContextId) -> VaStatus,
    create_buffer: unsafe extern "C" fn(
        VaDisplay,
        VaContextId,
        c_uint,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VaBufferId,
    ) -> VaStatus,
    destroy_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId) -> VaStatus,
    begin_picture: unsafe extern "C" fn(VaDisplay, VaContextId, VaSurfaceId) -> VaStatus,
    render_picture:
        unsafe extern "C" fn(VaDisplay, VaContextId, *mut VaBufferId, c_int) -> VaStatus,
    end_picture: unsafe extern "C" fn(VaDisplay, VaContextId) -> VaStatus,
    sync_surface: unsafe extern "C" fn(VaDisplay, VaSurfaceId) -> VaStatus,
    /// `vaExportSurfaceHandle(dpy, surface_id, mem_type, flags, descriptor)` — five
    /// parameters, and the descriptor's type is decided by `mem_type`.
    export_surface_handle:
        unsafe extern "C" fn(VaDisplay, VaSurfaceId, c_uint, c_uint, *mut c_void) -> VaStatus,
}

impl Libva {
    fn load() -> Result<Libva> {
        // SAFETY: `Library::new` runs the trusted system libva's initialisers, and each
        // `lib.get` resolves a documented libva symbol to the matching `unsafe extern "C"`
        // signature transcribed from `va.h` / `va_drm.h` (by-value integers and pointers
        // throughout, no callbacks). Both `Library` handles are stored in the returned
        // struct, so every resolved pointer outlives its uses.
        unsafe {
            let va = libloading::Library::new("libva.so.2")
                .context("libva.so.2 (no VAAPI runtime on this system)")?;
            let drm = libloading::Library::new("libva-drm.so.2")
                .context("libva-drm.so.2 (no VAAPI DRM backend on this system)")?;
            // Each symbol is resolved AT the field's own type — `Library::get` is
            // generic, so the struct's declared signature is what `dlsym`'s pointer
            // is read as. No `transmute` anywhere: a mistyped entry point is then a
            // mismatch the reader can see next to the declaration rather than a cast
            // that accepts anything. Bound with `let` (not inline in the literal) so
            // each borrow of the `Library` ends before it is moved into the struct.
            macro_rules! get {
                ($lib:expr, $name:literal) => {
                    *$lib
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| anyhow!(concat!("dlsym ", $name, ": {}"), e))?
                };
            }
            let get_display_drm = get!(drm, "vaGetDisplayDRM");
            let initialize = get!(va, "vaInitialize");
            let terminate = get!(va, "vaTerminate");
            let error_str = get!(va, "vaErrorStr");
            let query_config_entrypoints = get!(va, "vaQueryConfigEntrypoints");
            let max_entrypoints = get!(va, "vaMaxNumEntrypoints");
            let create_config = get!(va, "vaCreateConfig");
            let destroy_config = get!(va, "vaDestroyConfig");
            let create_surfaces = get!(va, "vaCreateSurfaces");
            let destroy_surfaces = get!(va, "vaDestroySurfaces");
            let create_context = get!(va, "vaCreateContext");
            let destroy_context = get!(va, "vaDestroyContext");
            let create_buffer = get!(va, "vaCreateBuffer");
            let destroy_buffer = get!(va, "vaDestroyBuffer");
            let begin_picture = get!(va, "vaBeginPicture");
            let render_picture = get!(va, "vaRenderPicture");
            let end_picture = get!(va, "vaEndPicture");
            let sync_surface = get!(va, "vaSyncSurface");
            let export_surface_handle = get!(va, "vaExportSurfaceHandle");
            Ok(Libva {
                get_display_drm,
                initialize,
                terminate,
                error_str,
                query_config_entrypoints,
                max_entrypoints,
                create_config,
                destroy_config,
                create_surfaces,
                destroy_surfaces,
                create_context,
                destroy_context,
                create_buffer,
                destroy_buffer,
                begin_picture,
                render_picture,
                end_picture,
                sync_surface,
                export_surface_handle,
                _va: va,
                _drm: drm,
            })
        }
    }

    /// libva's own text for a status code, so a driver's reason reaches the log
    /// instead of a bare number.
    fn err(&self, what: &str, status: VaStatus) -> anyhow::Error {
        // SAFETY: `vaErrorStr` is documented total — it returns a pointer into libva's
        // static string table for any input, valid while the library is loaded, which
        // `&self` proves.
        let text = unsafe {
            let p = (self.error_str)(status);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        if text.is_empty() {
            anyhow!("{what} failed ({status})")
        } else {
            anyhow!("{what} failed: {text} ({status})")
        }
    }

    fn check(&self, what: &str, status: VaStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS {
            Ok(())
        } else {
            Err(self.err(what, status))
        }
    }
}

// ---------------------------------------------------------------------------
// The display
// ---------------------------------------------------------------------------

/// An initialised `VADisplay` over a DRM render node.
struct Display {
    va: Libva,
    display: VaDisplay,
    /// The render node. libva does NOT dup the fd it is given, so the display is
    /// only valid while this is open — it is dropped after `vaTerminate`.
    node: Option<OwnedFd>,
    /// Which node, for the field report that asks "which GPU decoded?".
    path: String,
    version: (c_int, c_int),
}

// SAFETY: the display is created and used from ONE thread (the pump), and `Send` only
// permits MOVING that ownership. libva is not safe for concurrent calls on one
// display, which is why `Sync` is deliberately absent: every path into it goes through
// `&mut NativeVaapiDecoder`, and that is the serialisation.
unsafe impl Send for Display {}

impl Display {
    /// Open a render node and initialise libva on it.
    ///
    /// `PUNKTFUNK_VAAPI_DEVICE` pins a node explicitly. Otherwise nodes are tried in
    /// name order and the first that initialises wins — the rule libavcodec's VAAPI
    /// device creation uses when given no device string, so a box that got hardware
    /// decode from the libavcodec rung this replaced gets the same GPU here.
    ///
    /// ⚠ On a multi-GPU box that is not necessarily the PRESENTER's GPU, and a dmabuf
    /// exported from one GPU and imported into another either fails outright or
    /// copies. The libavcodec rung had the same property; the env pin is the
    /// escape hatch, and the chosen node is logged so a field report can name it.
    fn open(va: Libva) -> Result<Display> {
        if let Some(pin) = std::env::var_os("PUNKTFUNK_VAAPI_DEVICE") {
            let path = pin.to_string_lossy().into_owned();
            let (display, node, version) = Display::probe(&va, &path)
                .with_context(|| format!("PUNKTFUNK_VAAPI_DEVICE={path}"))?;
            return Ok(Display {
                va,
                display,
                node: Some(node),
                path,
                version,
            });
        }
        let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .context("/dev/dri (no DRM devices on this machine)")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        let mut tried: Vec<String> = Vec::new();
        for node in &nodes {
            let path = node.to_string_lossy().into_owned();
            match Display::probe(&va, &path) {
                Ok((display, node, version)) => {
                    return Ok(Display {
                        va,
                        display,
                        node: Some(node),
                        path,
                        version,
                    })
                }
                Err(e) => {
                    tracing::debug!(node = %path, reason = %format!("{e:#}"), "not a VAAPI device");
                    tried.push(path);
                }
            }
        }
        bail!(
            "no render node initialised a VAAPI display ({})",
            if tried.is_empty() {
                "/dev/dri has no renderD* nodes".to_string()
            } else {
                format!("tried {}", tried.join(", "))
            }
        )
    }

    /// Try ONE node, borrowing the loaded library — so a box with several GPUs
    /// dlopens libva once rather than once per node, and a failure carries only its
    /// reason.
    fn probe(va: &Libva, path: &str) -> Result<(VaDisplay, OwnedFd, (c_int, c_int))> {
        let node = OwnedFd::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| format!("open {path}"))?,
        );
        // SAFETY: `vaGetDisplayDRM` takes the render node's fd by value and returns an
        // opaque display or null; `vaInitialize` writes the two version ints through
        // the out-pointers, which are locals live across the call. The fd stays open in
        // `node` for as long as the display exists — libva does not dup it.
        unsafe {
            let display = (va.get_display_drm)(node.as_raw_fd());
            if display.is_null() {
                bail!("vaGetDisplayDRM({path}) returned no display");
            }
            let (mut major, mut minor) = (0, 0);
            let status = (va.initialize)(display, &mut major, &mut minor);
            if status != VA_STATUS_SUCCESS {
                let e = va.err("vaInitialize", status);
                // The display is unusable but still allocated; terminate it so the
                // driver's own state goes with the attempt.
                (va.terminate)(display);
                // No path in the context: every caller already names the node it
                // asked about, and on a box where nothing initialises that printed
                // the node twice on every line.
                return Err(e);
            }
            Ok((display, node, (major, minor)))
        }
    }
}

impl Display {
    /// Does this device decode that profile?
    ///
    /// Asked BEFORE `vaCreateConfig` so an unsupported profile is a clean refusal
    /// naming the profile, not a driver status code — the ladder falls through
    /// either way, but only one of them tells a field report why.
    fn require_entrypoint(&self, profile: c_int) -> Result<()> {
        // SAFETY: `vaMaxNumEntrypoints` returns the array size this display needs;
        // the vector is allocated to exactly that and `count` is a local written
        // through by the call.
        unsafe {
            let max = (self.va.max_entrypoints)(self.display);
            if max <= 0 {
                bail!("vaMaxNumEntrypoints returned {max}");
            }
            let mut entrypoints = vec![0 as c_int; max as usize];
            let mut count: c_int = 0;
            self.va.check(
                "vaQueryConfigEntrypoints",
                (self.va.query_config_entrypoints)(
                    self.display,
                    profile,
                    entrypoints.as_mut_ptr(),
                    &mut count,
                ),
            )?;
            let vld = pf_vaadec::VA_ENTRYPOINT_VLD as c_int;
            if !entrypoints[..count.clamp(0, max) as usize].contains(&vld) {
                bail!("this device has no VLD decode entrypoint for VAProfile {profile}");
            }
        }
        Ok(())
    }

    /// `vaCreateBuffer` with the data copied in — libva's documented behaviour for a
    /// non-null `data` pointer, and what makes the caller's structs free to die
    /// straight after.
    ///
    /// `size` is ONE element's size and `count` is how many follow, because that is
    /// how `vaCreateBuffer` is declared and the two are not interchangeable. Every
    /// H.264 and H.265 buffer here passes `count = 1`; **AV1's tile-parameter buffer
    /// is the one exception** — libavcodec's `vaapi_av1.c` sends a whole tile group's
    /// records in a single buffer beside that group's one data buffer, and a driver
    /// reads `num_elements` records out of it.
    fn create_buffer(
        &self,
        context: VaContextId,
        kind: u32,
        size: usize,
        count: usize,
        data: *const c_void,
    ) -> Result<VaBufferId> {
        let mut id: VaBufferId = VA_INVALID_ID;
        // SAFETY: a live display and context; `data` points at `size * count` readable
        // bytes for the duration of the call (the caller's live struct or slice), and
        // `id` is a local written through. libva copies the payload before returning.
        self.va.check("vaCreateBuffer", unsafe {
            (self.va.create_buffer)(
                self.display,
                context,
                kind as c_uint,
                size as c_uint,
                count as c_uint,
                data.cast_mut(),
                &mut id,
            )
        })?;
        Ok(id)
    }

    /// Destroy every buffer of a submission.
    ///
    /// ⚠ Not optional and not automatic. `va.h` is explicit — *"The user must call
    /// vaDestroyBuffer() to destroy a buffer"*, and *"a buffer can be re-used and
    /// sent to the server by another Begin/Render/End sequence if vaDestroyBuffer()
    /// is not called"*. The libva 0.x behaviour where `vaEndPicture` consumed them is
    /// long gone; leaking two-plus buffers per picture at 60 fps exhausts the
    /// driver's buffer store in minutes.
    fn destroy_buffers(&self, buffers: &[VaBufferId]) {
        for &b in buffers {
            if b == VA_INVALID_ID {
                continue;
            }
            // SAFETY: each id came from `create_buffer` on this display and is
            // destroyed exactly once — the submission's list is consumed here.
            unsafe { (self.va.destroy_buffer)(self.display, b) };
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: `self.display` was initialised in `open_node` and nothing else
        // terminates it; `Drop` runs once. The node fd is dropped AFTER this, which is
        // the order libva requires — it holds the fd, it does not own it.
        unsafe { (self.va.terminate)(self.display) };
        self.node = None;
    }
}

// ---------------------------------------------------------------------------
// The stream shape a session is built for
// ---------------------------------------------------------------------------

/// Everything about a stream that sizes or configures a session. Any change rebuilds
/// the whole thing: the config, the context, the surface pool and the slot map all
/// derive from it, and a half-rebuilt session hands out surfaces the pool does not
/// have. (M5's review found the depth/chroma half of this missing on the D3D11 rung —
/// the Windows host flips an HDR desktop to PQ in-band with a NEW SPS at unchanged
/// size, so a shape keyed on size alone decodes 10-bit samples into an 8-bit pool.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamShape {
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth: u8,
}

/// Which codec, and the planner that plans it.
enum Planner {
    H264(Box<pf_vaadec::H264Planner>),
    H265(Box<pf_vaadec::H265Planner>),
    Av1(Box<pf_vaadec::Av1Planner>),
}

impl Planner {
    fn name(&self) -> &'static str {
        match self {
            Planner::H264(_) => "native-vaapi h264",
            Planner::H265(_) => "native-vaapi h265",
            Planner::Av1(_) => "native-vaapi av1",
        }
    }
}

// ---------------------------------------------------------------------------
// Surface release: the consumer's half of the zero-copy contract
// ---------------------------------------------------------------------------

/// What a shipped frame hands back when the consumer is done with it.
///
/// `generation` is what makes a renegotiation safe: a token from a retired pool
/// names a surface index that no longer exists, and freeing that index in the NEW
/// pool would hand a live surface to the decoder as if it were spare.
#[derive(Debug, Clone, Copy)]
struct VaRelease {
    surface: usize,
    generation: u64,
}

/// Holds one shipped picture's surface out of the decoder's free list, and owns the
/// fds exported for it.
///
/// The presenter DUPS every fd it imports (`pf-presenter`'s dmabuf import says so in
/// as many words) and drops the frame — and so this guard — only after the fence for
/// its sampling submission has been waited. So "guard dropped" means "the GPU is done
/// reading", which is exactly when the surface may be decoded into again.
pub struct VaFrameGuard {
    /// The exported PRIME fds, closed by this field's own drop. One per OBJECT, not
    /// per plane: several planes routinely name one object, and closing a shared fd
    /// twice would close an unrelated file.
    _fds: Vec<OwnedFd>,
    tx: mpsc::Sender<VaRelease>,
    release: VaRelease,
}

impl Drop for VaFrameGuard {
    fn drop(&mut self) {
        // A dead channel means the decoder is gone; there is nothing to release to.
        let _ = self.tx.send(self.release);
    }
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

/// The live config, context and surface pool for one [`StreamShape`].
struct Session {
    shape: StreamShape,
    config: VaConfigId,
    context: VaContextId,
    /// The pool. Indices into this are what everything else here refers to.
    surfaces: Vec<VaSurfaceId>,
    /// A consumer holds this surface. Cleared when its release token returns.
    held: Vec<bool>,
    /// DPB slot → pool index, rebound at ACTIVATION (module docs). `None` for a slot
    /// holding no picture.
    slot_surface: Vec<Option<usize>>,
    /// Decoded pictures the planner has not output yet, `(PicId, pool index)`.
    /// Separate from the slot binding because the two end at different times: a
    /// non-reference picture leaves the DPB immediately but still owes an output.
    pending: Vec<(u64, usize)>,
    slots: pf_vaadec::SlotMap,
    /// The surface fourcc the pool was created with (NV12 or P010).
    fourcc: u32,
    /// Bumped on every rebuild; stamped into release tokens.
    generation: u64,
}

impl Session {
    /// A surface bound to no live picture, owed no output, and held by no consumer.
    ///
    /// All three conditions, because they end at different moments: a picture leaves
    /// the DPB when the planner removes it, stops being pending when it is output,
    /// and stops being held when the presenter's fence has been waited — and the
    /// display is usually the LAST of the three.
    fn free_surface(&self) -> Option<usize> {
        (0..self.surfaces.len()).find(|i| {
            !self.held[*i]
                && !self.slot_surface.contains(&Some(*i))
                && !self.pending.iter().any(|(_, p)| p == i)
        })
    }

    /// Re-derive the slot bindings from the ledger: a slot the planner released
    /// binds nothing.
    ///
    /// Done by reading the ledger rather than by tracking `removed` here, so there is
    /// exactly one source of truth for which slots are live. `plan_to_va` has already
    /// applied this AU's removals by the time it returns.
    fn sync_slot_bindings(&mut self) {
        let mut live = vec![false; self.slot_surface.len()];
        for (slot, _) in self.slots.held() {
            if let Some(l) = live.get_mut(usize::from(slot)) {
                *l = true;
            }
        }
        for (slot, bound) in self.slot_surface.iter_mut().enumerate() {
            if !live[slot] {
                *bound = None;
            }
        }
    }

    /// Slot → `VASurfaceID`, for the pictures the DPB holds RIGHT NOW.
    ///
    /// Built before the conversion, because references resolve against the
    /// pre-removal state. An unbound slot reads [`VA_INVALID_ID`], never 0 — a zero
    /// there is a plausible surface id, and the conversion only ever indexes slots
    /// the ledger says are live, so the sentinel exists to make a bug in that
    /// argument visible rather than silent.
    fn surface_table(&self) -> Vec<VaSurfaceId> {
        self.slot_surface
            .iter()
            .map(|b| b.map_or(VA_INVALID_ID, |i| self.surfaces[i]))
            .collect()
    }

    /// The decode target — pool index and `VASurfaceID` — together with the reference
    /// table the conversion resolves against. `None` when the pool is exhausted.
    ///
    /// **The three are returned together because they are only safe together**, and
    /// that is this rung's whole exemption from the aliasing defect the other two
    /// backends had to defer their way out of (module docs). A free surface is by
    /// definition a surface no slot binds; [`Self::surface_table`] is exactly what the
    /// slots bind; so a target drawn from the same snapshot cannot appear in the table,
    /// and no reference the conversion resolves through that table can be the surface
    /// it is about to write.
    ///
    /// ⚠ Taking the two at DIFFERENT moments is the defect. References must resolve
    /// against the store as it stood BEFORE this access unit's removals, so the table
    /// has to be the pre-removal one; and a free list consulted AFTER those removals
    /// offers the displaced picture's surface, which the pre-removal table still names.
    /// `taking_the_free_surface_after_the_removals_would_hand_out_a_referenced_surface`
    /// is that mismatch, made to happen. Returning a tuple is what stops a future edit
    /// from reintroducing it by moving one call and not the other.
    fn acquire_target(&self) -> Option<(usize, VaSurfaceId, Vec<VaSurfaceId>)> {
        let index = self.free_surface()?;
        Some((index, self.surfaces[index], self.surface_table()))
    }

    /// Release every libva object this session owns, in creation-reverse order.
    /// Called explicitly (a `Drop` here could not reach the display).
    fn destroy(mut self, d: &Display) {
        // SAFETY: every handle was created on this display by `build` and is
        // destroyed exactly once — `destroy` consumes `self`. Surfaces are freed
        // after the context that referenced them, which is the order libva documents.
        unsafe {
            (d.va.destroy_context)(d.display, self.context);
            (d.va.destroy_surfaces)(
                d.display,
                self.surfaces.as_mut_ptr(),
                self.surfaces.len() as c_int,
            );
            (d.va.destroy_config)(d.display, self.config);
        }
    }

    /// Build a config, a surface pool and a context for one stream shape.
    fn build(d: &Display, codec: pf_vaadec::Codec, shape: StreamShape) -> Result<Session> {
        let profile = pf_vaadec::profile_for(codec, shape.chroma_format_idc, shape.bit_depth)
            .map_err(|e| anyhow!("{e}"))?;
        let rt_format = pf_vaadec::rt_format(shape.chroma_format_idc, shape.bit_depth)
            .map_err(|e| anyhow!("{e}"))?;
        let fourcc = match shape.bit_depth {
            8 => pf_vaadec::VA_FOURCC_NV12,
            10 => pf_vaadec::VA_FOURCC_P010,
            other => bail!("no VAAPI surface format for {other}-bit output"),
        };
        d.require_entrypoint(profile.value)?;

        // `VAConfigAttribRTFormat` (= 0, measured) is set explicitly rather than left
        // to the driver's default: on a Main 10 stream the default is the 8-bit
        // format, and a decoder writing 10-bit samples into an 8-bit surface is the
        // silent-narrowing failure this program refuses everywhere else.
        let mut attrib = VaConfigAttrib {
            kind: VA_CONFIG_ATTRIB_RT_FORMAT,
            value: rt_format,
        };
        let mut config: VaConfigId = VA_INVALID_ID;
        // SAFETY: a live display; `attrib` and `config` are locals that outlive the
        // call, and the count matches the slice length.
        d.va.check("vaCreateConfig", unsafe {
            (d.va.create_config)(
                d.display,
                profile.value,
                pf_vaadec::VA_ENTRYPOINT_VLD as c_int,
                (&mut attrib as *mut VaConfigAttrib).cast::<c_void>(),
                1,
                &mut config,
            )
        })?;

        // From here every early return must destroy what has been created, so the
        // fallible tail is written as a closure and unwound once.
        let built = (|| -> Result<Session> {
            let count = pf_vaadec::surface_count(shape.max_dpb_frames);
            let mut surfaces: Vec<VaSurfaceId> = vec![VA_INVALID_ID; count];
            let mut pixel = VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue {
                    kind: VA_GENERIC_VALUE_TYPE_INTEGER,
                    _pad: 0,
                    // The fourcc is an i32 in libva's integer arm; the top bit is
                    // clear for every fourcc here, so the cast is value-preserving.
                    i: fourcc as i32,
                    _rest: 0,
                },
            };
            // Surfaces are allocated at the CODED size. The conformance window is a
            // display-time crop, and a pool sized to the display region would be
            // short by the codec's granule padding — the scar that smears rows.
            // SAFETY: live display; the surface array and the attribute are locals
            // that outlive the call and the counts match their lengths.
            d.va.check("vaCreateSurfaces", unsafe {
                (d.va.create_surfaces)(
                    d.display,
                    rt_format,
                    shape.coded_width,
                    shape.coded_height,
                    surfaces.as_mut_ptr(),
                    count as c_uint,
                    &mut pixel,
                    1,
                )
            })?;

            let mut context: VaContextId = VA_INVALID_ID;
            // SAFETY: live display and the config/surfaces just created; `context` is
            // a local that outlives the call. libva copies the surface array.
            let status = unsafe {
                (d.va.create_context)(
                    d.display,
                    config,
                    shape.coded_width as c_int,
                    shape.coded_height as c_int,
                    VA_PROGRESSIVE as c_int,
                    surfaces.as_mut_ptr(),
                    count as c_int,
                    &mut context,
                )
            };
            if let Err(e) = d.va.check("vaCreateContext", status) {
                // SAFETY: destroying the surfaces this closure just created, on the
                // unwind path, before they are moved into a Session.
                unsafe {
                    (d.va.destroy_surfaces)(
                        d.display,
                        surfaces.as_mut_ptr(),
                        surfaces.len() as c_int,
                    )
                };
                return Err(e);
            }

            let slots = pf_vaadec::SlotMap::new(shape.max_dpb_frames);
            let slot_count = slots.capacity();
            tracing::info!(
                node = %d.path,
                va = format_args!("{}.{}", d.version.0, d.version.1),
                profile = profile.name,
                coded = format_args!("{}x{}", shape.coded_width, shape.coded_height),
                display = format_args!("{}x{}", shape.display_width, shape.display_height),
                bit_depth = shape.bit_depth,
                surfaces = count,
                dpb_slots = slot_count,
                "native VAAPI decode session built"
            );
            Ok(Session {
                shape,
                config,
                context,
                surfaces,
                held: vec![false; count],
                slot_surface: vec![None; slot_count],
                pending: Vec::new(),
                slots,
                fourcc,
                generation: 0,
            })
        })();
        if built.is_err() {
            // SAFETY: destroying the config created above, on the unwind path; no
            // Session took ownership of it.
            unsafe { (d.va.destroy_config)(d.display, config) };
        }
        built
    }
}

/// `VAConfigAttrib` — 8 bytes, `{type, value}` at 0 and 4 (measured).
#[repr(C)]
#[derive(Clone, Copy)]
struct VaConfigAttrib {
    kind: c_int,
    value: c_uint,
}

/// `VAConfigAttribRTFormat` — measured, and 0 is a real enumerator here rather than
/// a "left unset", which is why it is named.
const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;

/// `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2` — the export memory type that yields a
/// [`pf_vaadec::VaDrmPrimeSurfaceDescriptor`]. Measured; the older
/// `..._DRM_PRIME` (0x2000_0000) hands back a different, smaller structure.
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: c_uint = 0x4000_0000;

const _: () = {
    assert!(size_of::<VaConfigAttrib>() == 8);
    assert!(std::mem::offset_of!(VaConfigAttrib, value) == 4);
};

// ---------------------------------------------------------------------------
// The decoder
// ---------------------------------------------------------------------------

/// The native VAAPI rung.
pub(crate) struct NativeVaapiDecoder {
    display: Display,
    planner: Planner,
    session: Option<Session>,
    health: DecodeHealth,
    /// A concealed AU asks the pump for a re-anchor, through the same one throttle
    /// every other ask uses. Drained by [`Self::take_recovery_request`].
    recovery_request: bool,
    generation: u64,
    release_tx: mpsc::Sender<VaRelease>,
    release_rx: mpsc::Receiver<VaRelease>,
    /// Pool-index releases that arrived for a RETIRED generation, so the count of
    /// what is still outstanding is honest in the log.
    stale_releases: u64,
}

impl NativeVaapiDecoder {
    /// Build the rung, refusing anything this device or this crate cannot decode
    /// BEFORE the ladder commits to it.
    ///
    /// The refusal is at construction on purpose, and it is M3 WP-2's lesson: a
    /// backend that accepts a session and then refuses its first access unit has
    /// already cost the ladder its fall-through — the refusal arrives as a decode
    /// error, burns the demotion streak, and lands the session on a rung far below
    /// the one it would have had. So the negotiated [`StreamFormat`] is probed here,
    /// where "no" simply means the next rung is tried.
    pub(crate) fn new(codec: pf_vaadec::Codec, stream: StreamFormat) -> Result<NativeVaapiDecoder> {
        let depth = stream.bit_depth;
        pf_vaadec::profile_for(codec, stream.chroma_format_idc, depth)
            .map_err(|e| anyhow!("{e}"))
            .context("the negotiated stream shape has no VAAPI decode profile")?;
        let va = Libva::load().context("libva")?;
        let display = Display::open(va)?;
        let planner = match codec {
            pf_vaadec::Codec::H264 => Planner::H264(Box::new(pf_vaadec::H264Planner::new())),
            pf_vaadec::Codec::H265 => Planner::H265(Box::new(pf_vaadec::H265Planner::new())),
            pf_vaadec::Codec::Av1 => Planner::Av1(Box::new(pf_vaadec::Av1Planner::new())),
        };
        let (release_tx, release_rx) = mpsc::channel();
        Ok(NativeVaapiDecoder {
            display,
            planner,
            session: None,
            health: DecodeHealth {
                // VAAPI has no per-picture decode-status query — there is no
                // counterpart to Vulkan's `RESULT_STATUS_ONLY`, exactly as on
                // D3D11VA. Saying so is what keeps "clean" and "unmeasured"
                // distinguishable on the stats line: `failed` is structurally 0
                // here, and `DecodeHealth::note` enforces that rather than trusting
                // this rung to never pass a verdict it cannot have.
                status_queries: false,
                ..DecodeHealth::default()
            },
            recovery_request: false,
            generation: 0,
            release_tx,
            release_rx,
            stale_releases: 0,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        self.planner.name()
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    /// Drain the re-anchor request a concealed AU raised.
    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_request)
    }

    /// Return surfaces the consumer has finished with to the free list.
    fn drain_releases(&mut self) {
        drain_releases_into(
            &self.release_rx,
            self.session.as_mut(),
            &mut self.stale_releases,
        );
    }

    /// Decode one access unit.
    ///
    /// `Ok(None)` means "no picture from this AU", and covers three different
    /// things, deliberately none of them errors:
    ///
    /// * the planner output nothing yet (reordering, or the very first AUs);
    /// * the picture was CONCEALED — an integrity warning says a reference was
    ///   substituted, so the output is released unshown, [`DecodeHealth::damaged`]
    ///   records it and a re-anchor is requested through the pump's one throttle.
    ///   Not an error, because three errors in a second demote the rung on exactly
    ///   the lossy links it exists to diagnose — libavcodec concealed the same event
    ///   silently and kept its job;
    /// * an HEVC RASL picture skipped after an open-GOP join. `PlanError::RaslSkipped`
    ///   is the spec's own answer (8.1.3 NOTE) and must NEVER reach the reanchor
    ///   path — mapping it to an error would make every open-GOP join beg the host
    ///   for a keyframe it has no reason to send.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        self.drain_releases();
        let result = match self.planner {
            Planner::H264(_) => self.decode_h264(au),
            Planner::H265(_) => self.decode_h265(au),
            // ⚠ An AV1 "access unit" is a TEMPORAL UNIT and may carry several
            // frames; this arm is the only one whose planner returns a `Vec`.
            Planner::Av1(_) => self.decode_av1(au),
        };
        // ONE verdict per access unit, folded here and nowhere else. Damage is
        // reported by the codec arm rather than counted inside it, so a failure
        // AFTER a clean plan (a submission, an export) is a refusal and only a
        // refusal — not a clean AU that also refused, which would reset the run
        // counter a support engineer reads first.
        match &result {
            Ok((_, damaged)) => self.health.note(*damaged, false, 0),
            Err(_) => self.health.note(false, true, 0),
        }
        result.map(|(frame, _)| frame)
    }

    fn decode_h264(&mut self, au: &[u8]) -> Result<(Option<DmabufFrame>, bool)> {
        let plan = match &mut self.planner {
            Planner::H264(p) => p.plan_au(au).map_err(|e| anyhow!("{e:?}"))?,
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let shape = shape_of(
            plan.picture.coded_width,
            plan.picture.coded_height,
            plan.picture.display_crop,
            plan.picture.max_dpb_frames,
            plan.picture.chroma_format_idc,
            8 + plan.picture.bit_depth_luma_minus8,
        )?;
        let damaged = plan.warnings.iter().any(pf_vaadec::is_integrity_warning);
        if !plan.warnings.is_empty() {
            tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI plan warnings");
        }

        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaadec::Codec::H264,
            shape,
            &mut self.generation,
        )?;
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = pf_vaadec::plan_to_va(&plan, au, &mut s.slots, &table, target)
            .map_err(|e| anyhow!("{e}"))?;

        bind_setup(s, plan.dpb.stored, Some(free));

        let iq = Some(as_ptr(&converted.iq_matrix));
        let slices = one_record_each(&converted.slices, &converted.slice_data)?;
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            iq,
            &slices,
            au,
        )?;

        let display_size = (s.shape.display_width, s.shape.display_height);
        let frame = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            plan.picture.is_idr,
            colour_of(&plan.picture.colour),
            display_size,
            &mut self.recovery_request,
            &self.release_tx,
        )?;
        Ok((frame, damaged))
    }

    fn decode_h265(&mut self, au: &[u8]) -> Result<(Option<DmabufFrame>, bool)> {
        let plan = match &mut self.planner {
            Planner::H265(p) => match p.plan_au(au) {
                Ok(plan) => plan,
                // The contract pf-bitstream's h265 module docs record for this
                // wiring: a skipped RASL picture is an Ok-skip, never an error and
                // never a re-anchor. See [`Self::decode`].
                Err(pf_vaadec::PlanErrorH265::RaslSkipped { .. }) => return Ok((None, false)),
                Err(e) => return Err(anyhow!("{e:?}")),
            },
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let shape = shape_of(
            plan.picture.coded_width,
            plan.picture.coded_height,
            plan.picture.display_crop,
            plan.picture.max_dpb_frames,
            plan.picture.chroma_format_idc,
            8 + plan.picture.bit_depth_luma_minus8,
        )?;
        let damaged = plan
            .warnings
            .iter()
            .any(pf_vaadec::is_integrity_warning_h265);
        if !plan.warnings.is_empty() {
            tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI plan warnings");
        }

        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaadec::Codec::H265,
            shape,
            &mut self.generation,
        )?;
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = pf_vaadec::plan_to_va_h265(&plan, au, &mut s.slots, &table, target)
            .map_err(|e| anyhow!("{e}"))?;

        bind_setup(s, plan.dpb.stored, Some(free));

        // The IQ matrix is submitted ONLY where the sequence codes scaling lists.
        // Handing the driver an all-zero matrix on a "use the defaults" stream is
        // not a harmless extra buffer: the driver must apply what it is given, every
        // residual dequantises to zero, and the picture drifts to flat prediction.
        // (M5's review caught exactly this on the DXVA rung, where the buffer was
        // unconditional. The conversion answers `None` here so the rung cannot.)
        let iq = converted.iq_matrix.as_ref().map(as_ptr);
        let slices = one_record_each(&converted.slices, &converted.slice_data)?;
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            iq,
            &slices,
            au,
        )?;

        let display_size = (s.shape.display_width, s.shape.display_height);
        let frame = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            plan.picture.is_idr,
            colour_of(&plan.picture.colour),
            display_size,
            &mut self.recovery_request,
            &self.release_tx,
        )?;
        Ok((frame, damaged))
    }

    /// One AV1 **temporal unit**: decode every frame in it, present at most one.
    ///
    /// This is the whole of what AV1 adds to this rung's contract, and it is the
    /// SPEC's shape rather than an assumption about punktfunk hosts. A temporal unit
    /// may carry several frame headers; the vendored 250-packet conformance vector
    /// decodes **274 frames** and shows 250, so 24 of its units carry a hidden
    /// picture (an alt-ref later frames predict from) ahead of the one that
    /// displays. Those hidden frames must be DECODED — they are references — and
    /// must never reach the presenter, which would show each of them for a frame and
    /// stutter every time.
    ///
    /// AV1 admits at most one shown frame per temporal unit, so "the last shown
    /// frame wins" cannot silently drop a picture.
    ///
    /// # Concealment is per UNIT here, per picture on the other two codecs
    ///
    /// A damaged frame is still CONVERTED and still SUBMITTED, exactly as the H.264
    /// and H.265 arms above do it. Converting is what assigns its ledger slot, and
    /// skipping that would desynchronise this rung's slot map from the planner's store
    /// and turn every later reference to it into a hard `Err` — a demotion streak
    /// earned by one lost packet. Submitting is what puts a decoded picture in the
    /// surface, which matters because a hidden frame's surface is a REFERENCE for
    /// later frames and can still be exported by a later `show_existing_frame`; a
    /// surface the driver never wrote is uninitialised video memory, not a stale
    /// picture.
    ///
    /// What concealment does instead is withhold the DISPLAY: nothing from the unit
    /// is presented, because a shown frame that predicts from a concealed reference in
    /// the same unit is not fit to display either, and the unit is the smallest thing
    /// this rung can honestly drop.
    ///
    /// ⚠ Submitting a frame whose references were lost needs one thing from the
    /// conversion, and `va_dec_av1.h:352` is where it comes from: *"Driver is not
    /// responsible to validate reference frames' id … If missing frame is identified,
    /// application may choose to perform error recovery by pointing problematic index
    /// to an alternative frame buffer."* So `plan_to_va_av1` points every empty
    /// `ref_frame_map` entry at a live surface and reports which
    /// (`DecodePlanVaAv1::substituted_refs`); no `VA_INVALID_ID` reaches a driver that
    /// says it will not check.
    ///
    /// The one frame that is NOT submitted is the one with nothing to submit: an
    /// access unit whose tile groups were lost. That refusal is handled in
    /// [`Self::frame_av1`] and binds no surface at all, so its picture can be neither
    /// exported nor predicted from.
    fn decode_av1(&mut self, au: &[u8]) -> Result<(Option<DmabufFrame>, bool)> {
        let plans = match &mut self.planner {
            Planner::Av1(p) => p.plan_au(au).map_err(|e| anyhow!("{e}"))?,
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let mut shown = None;
        let mut damaged_unit = false;
        for plan in &plans {
            let damaged = plan
                .warnings
                .iter()
                .any(pf_vaadec::is_integrity_warning_av1);
            damaged_unit |= damaged;
            if !plan.warnings.is_empty() {
                tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI AV1 plan warnings");
            }
            if let Some(frame) = self.frame_av1(au, plan, damaged)? {
                shown = Some(frame);
            }
        }
        if damaged_unit {
            // A frame may already have been exported before a LATER frame of the
            // same unit turned out to be damaged. Dropping it here is safe rather
            // than merely tolerable: `DmabufFrame`'s guard closes its fds and returns
            // the surface to the free list, which is exactly what an unshown picture
            // should do.
            drop(shown);
            return Ok((None, true));
        }
        Ok((shown, false))
    }

    /// One frame of a temporal unit: converted, submitted, and exported only if it is
    /// the frame the unit displays and the unit is clean.
    ///
    /// `damaged` changes two things and neither of them is the submission. It decides
    /// whether the picture may be SHOWN (through [`finish`]), and it decides how a
    /// conversion refusal is answered: a lost tile group on an already-damaged plan is
    /// concealed, the same refusal on a plan that arrived whole is a defect and stays
    /// an error.
    fn frame_av1(
        &mut self,
        au: &[u8],
        plan: &pf_vaadec::AuPlanAv1,
        damaged: bool,
    ) -> Result<Option<DmabufFrame>> {
        // `show_existing_frame` decodes nothing at all: it re-displays a picture some
        // earlier hidden frame put in a reference slot.
        if plan.dpb.stored.is_none() {
            return self.show_existing_av1(plan, damaged);
        }
        let shape = shape_of_av1(plan);
        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaadec::Codec::Av1,
            shape,
            &mut self.generation,
        )?;
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = match pf_vaadec::plan_to_va_av1(plan, au, &mut s.slots, &table, target) {
            Ok(converted) => converted,
            Err(e) => {
                // ⚠ The ledger has already been mutated — the conversion assigns the
                // setup slot before its tile walk, so that a refusal here does not
                // desynchronise it from the planner's store — and the caller's half of
                // that contract is to bind NOTHING (see [`bind_setup`]). Unconditional,
                // because it is also correct for the refusals that fire before any
                // mutation: there is no slot to clear and no surface to bind either
                // way.
                bind_setup(s, plan.dpb.stored, None);
                // A lost tile group on a plan the planner ALREADY called damaged is
                // concealment, not a defect: the access unit simply did not carry the
                // tiles its frame header announced, which is what one dropped packet
                // looks like. Answering with an error instead would burn the demotion
                // streak on exactly the lossy links this rung exists to diagnose. The
                // frame is not submitted (there is nothing to submit), its surface is
                // bound to nothing, and the unit is dropped by the caller.
                if damaged && e.lost_tiles() {
                    tracing::debug!(
                        error = %e,
                        id = plan.dpb.stored,
                        "native VAAPI AV1: concealed a truncated access unit"
                    );
                    finish(
                        display,
                        s,
                        &plan.dpb.outputs,
                        &plan.dpb.removed,
                        true,
                        plan.picture.is_key,
                        colour_of(&plan.picture.colour),
                        // Unread — `finish` returns before it looks at the display
                        // region when `damaged` — but written the same way as the
                        // submitting path below, so the two cannot drift apart.
                        (
                            plan.picture.render_width.min(plan.picture.upscaled_width),
                            plan.picture.render_height.min(plan.picture.frame_height),
                        ),
                        &mut self.recovery_request,
                        &self.release_tx,
                    )?;
                    return Ok(None);
                }
                return Err(anyhow!("{e}"));
            }
        };

        // `bind_setup` asks the LEDGER where the picture landed rather than being
        // told, which is what makes it right for the AV1 frame that refreshes no
        // slot: the conversion has already handed that slot back, so nothing binds
        // the surface and only the pending-output claim keeps it out of the free
        // list. (`DecodePlanVaAv1::setup_slot` is `None` there; it is not consulted
        // here for exactly that reason.)
        bind_setup(s, plan.dpb.stored, Some(free));

        if converted.substituted_refs != 0 {
            tracing::debug!(
                slots = format_args!("{:#010b}", converted.substituted_refs),
                "native VAAPI AV1: concealed reference slot(s) with a live surface"
            );
        }
        let mut slices: Vec<SlicePair> = Vec::with_capacity(converted.tile_groups.len());
        for group in &converted.tile_groups {
            slices.push(SlicePair {
                params: group.tiles.as_ptr().cast::<c_void>(),
                record_size: size_of::<pf_vaadec::va_av1::VaSliceParameterBufferAV1>(),
                // ⚠ Several records in ONE buffer — the only place this rung does
                // that, and what libavcodec's `vaapi_av1.c` does per tile group.
                records: group.tiles.len(),
                data: group.data.clone(),
            });
        }
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            // AV1 transmits no quantisation matrix: its matrices are SELECTED by
            // index out of tables the decoder already holds.
            None,
            &slices,
            au,
        )?;

        // AV1's display region is the RENDER size, not the coded size — and it is a
        // per-FRAME value, so it cannot live in the session shape the way a
        // conformance window does.
        //
        // ⚠ CLAMPED to the decoded picture. AV1 5.9.6 puts no upper bound on the
        // render size — a stream may legally ask to be shown at more than it coded —
        // and an unclamped crop would hand the presenter a region larger than the
        // surface. The same clamp is in the Vulkan and D3D11 rungs.
        //
        // ⚠ Treated as a CROP, which is what both other native rungs do. libavcodec
        // instead keeps the frame at `upscaled_width` x `frame_height` and expresses
        // the render size as a sample aspect RATIO, so on a stream where the two
        // differ this rung shows less picture than libavcodec would. No
        // punktfunk host emits such a stream; the choice is here so the three native
        // rungs answer alike, not because it is settled.
        let display_size = (
            plan.picture.render_width.min(plan.picture.upscaled_width),
            plan.picture.render_height.min(plan.picture.frame_height),
        );
        let frame = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            plan.picture.is_key,
            colour_of(&plan.picture.colour),
            display_size,
            &mut self.recovery_request,
            &self.release_tx,
        )?;

        // A frame that enters no reference slot AND displays nothing is dead the
        // moment it is decoded, and it is the one picture nothing else ever retires:
        // the planner cannot report it removed (it was never stored) and cannot
        // output it, so its pending entry — and therefore its surface — is held for
        // the session's whole life. Seventeen of them exhaust the pool.
        //
        // Legal AV1 syntax that no encoder emits, which is exactly why it is worth a
        // line: a damaged or truncated header can parse to it, and the symptom would
        // be a session that dies of "pool exhausted" some minutes later with nothing
        // pointing back here.
        if converted.setup_slot.is_none() && !plan.dpb.outputs.contains(&converted.setup_id) {
            s.pending.retain(|(id, _)| *id != converted.setup_id);
        }
        Ok(frame)
    }

    /// A `show_existing_frame` access unit: export a surface the pool already holds.
    ///
    /// No conversion and no submission — the picture was decoded by an earlier frame
    /// of an earlier temporal unit and is still in [`Session::pending`], because a
    /// hidden frame is never output when it decodes. [`finish`] resolves the output
    /// id to its surface exactly as it does for any other picture, so this path needs
    /// no per-surface facts table: the plan's own `picture` carries the SHOWN frame's
    /// geometry and type, which the vendored parser restores from the reference
    /// (`load_reference_frame` copies `ref_upscaled_width` / `ref_frame_height` /
    /// `ref_render_*` / `ref_frame_type` into the display-only header).
    ///
    /// ⚠ **Untested.** The vendored conformance vector uses `show_existing_frame`
    /// zero times — pf-bitstream's planner test asserts that count stays 0 — so
    /// nothing in any gate reaches this function.
    fn show_existing_av1(
        &mut self,
        plan: &pf_vaadec::AuPlanAv1,
        damaged: bool,
    ) -> Result<Option<DmabufFrame>> {
        let Self {
            display, session, ..
        } = self;
        // Nothing has decoded yet: the unit is already concealed (the planner
        // reported `MissingShowExisting`) and there is no session to look in.
        let Some(s) = session.as_mut() else {
            return Ok(None);
        };
        // Showing a KEY frame this way resets the whole reference store (AV1 7.20),
        // so the plan's removals are real and this rung's ledger has to follow them —
        // or the map fills up and the next assignment fails. No conversion runs on
        // this path, so this is the only place they can be applied.
        for &id in &plan.dpb.removed {
            s.slots.release(id);
        }
        s.sync_slot_bindings();
        let display_size = (
            plan.picture.render_width.min(plan.picture.upscaled_width),
            plan.picture.render_height.min(plan.picture.frame_height),
        );
        finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            plan.picture.is_key,
            colour_of(&plan.picture.colour),
            display_size,
            &mut self.recovery_request,
            &self.release_tx,
        )
    }
}

impl Drop for NativeVaapiDecoder {
    fn drop(&mut self) {
        if self.stale_releases > 0 {
            // Not an error — a renegotiated session's frames come home to a pool
            // that no longer exists — but a count worth seeing, because the only
            // other thing that produces it is release bookkeeping gone wrong.
            tracing::debug!(
                count = self.stale_releases,
                "native VAAPI: releases for retired surface pools"
            );
        }
        if let Some(s) = self.session.take() {
            s.destroy(&self.display);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared decode plumbing (free functions: the two codec arms differ only in the
// types their conversions produce, and splitting the borrow of `self` here is
// what lets one implementation serve both)
// ---------------------------------------------------------------------------

/// Apply the consumer's finished-with tokens to the pool's holds.
///
/// A free function rather than a method so the rule can be tested without a
/// `Display`: it is pure bookkeeping, and the one thing it must get right — refusing
/// a token from a pool that no longer exists — is precisely what a test can check and
/// hardware cannot.
fn drain_releases_into(
    rx: &mpsc::Receiver<VaRelease>,
    mut session: Option<&mut Session>,
    stale: &mut u64,
) {
    while let Ok(token) = rx.try_recv() {
        let Some(s) = session.as_deref_mut() else {
            *stale += 1;
            continue;
        };
        if token.generation != s.generation {
            // A retired pool's surface. Its whole session is gone, so there is
            // nothing to free — and freeing that index in the CURRENT pool would hand
            // a live surface to the decoder as a spare.
            *stale += 1;
            continue;
        }
        match s.held.get_mut(token.surface) {
            Some(h) => *h = false,
            None => *stale += 1,
        }
    }
}

/// A live struct as a `(pointer, size)` for `vaCreateBuffer`, which COPIES.
fn as_ptr<T>(value: &T) -> (*const c_void, usize) {
    ((value as *const T).cast::<c_void>(), size_of::<T>())
}

/// H.273 code points straight off the picture's ACTIVE SPS/VUI — per frame, never
/// latched, because the Windows host switches an HDR desktop to PQ/BT.2020 IN-BAND
/// with a new SPS while the Welcome still says SDR.
fn colour_of(c: &pf_vaadec::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: c.colour_primaries,
        transfer: c.transfer_characteristics,
        matrix: c.matrix_coefficients,
        full_range: c.video_full_range,
    }
}

/// Derive the session shape, refusing what the hand-off cannot express.
fn shape_of(
    coded_width: u32,
    coded_height: u32,
    crop: pf_vaadec::DisplayCrop,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth: u8,
) -> Result<StreamShape> {
    // A non-zero conformance-window ORIGIN would have to shift every plane's offset,
    // and nothing downstream carries one: the dmabuf planes are handed over with the
    // driver's own offsets and the consumer samples from (0,0). Refused rather than
    // cropped from the wrong corner — the same latent gap M5 flagged on the D3D11
    // rung, closed here instead of inherited. Our hosts emit origin (0,0); a stream
    // that does not simply falls through to the next rung.
    if crop.x != 0 || crop.y != 0 {
        bail!(
            "conformance window at ({}, {}) — this rung hands the surface over \
             uncropped and cannot express a non-zero origin",
            crop.x,
            crop.y
        );
    }
    Ok(StreamShape {
        coded_width,
        coded_height,
        display_width: crop.width,
        display_height: crop.height,
        max_dpb_frames,
        chroma_format_idc,
        bit_depth,
    })
}

/// The session shape one AV1 plan implies.
///
/// ⚠ The pool is sized from the SEQUENCE header's **maximum** frame size, not this
/// frame's. AV1 lets every frame pick its own size up to that maximum without a key
/// frame, and sizing the session from the frame would rebuild the config, the
/// surface pool and the ledger — dropping every reference — the first time a stream
/// resized downward. libavcodec does the same (`set_context_with_sequence` calls
/// `ff_set_dimensions(avctx, seq->max_frame_width_minus_1 + 1, …)`).
///
/// The display fields carry the same maximum rather than the render region, for the
/// same reason: the render size is a per-FRAME value and putting it here would make
/// every render-size change a renegotiation. What actually reaches the presenter is
/// [`finish`]'s `display` parameter.
///
/// The DPB depth is a constant of the codec — `NUM_REF_FRAMES` — never anything a
/// sequence header says. There is no conformance window to refuse, so unlike
/// [`shape_of`] this cannot fail.
fn shape_of_av1(plan: &pf_vaadec::AuPlanAv1) -> StreamShape {
    let coded_width = u32::from(plan.sequence.max_frame_width_minus_1) + 1;
    let coded_height = u32::from(plan.sequence.max_frame_height_minus_1) + 1;
    StreamShape {
        coded_width,
        coded_height,
        display_width: coded_width,
        display_height: coded_height,
        max_dpb_frames: pf_vaadec::AV1_MAX_DPB_FRAMES,
        chroma_format_idc: plan.picture.chroma_format_idc,
        // AV1 codes ONE bit depth for all three planes, so there is no luma/chroma
        // pair to reconcile the way H.264 and H.265 need.
        bit_depth: plan.picture.bit_depth,
    }
}

/// The session for this shape, rebuilt whole if the stream renegotiated.
fn ensure_session<'a>(
    d: &Display,
    slot: &'a mut Option<Session>,
    codec: pf_vaadec::Codec,
    shape: StreamShape,
    generation: &mut u64,
) -> Result<&'a mut Session> {
    if slot.as_ref().is_some_and(|s| s.shape == shape) {
        return Ok(slot.as_mut().expect("just matched"));
    }
    if let Some(old) = slot.take() {
        tracing::info!(
            from = ?old.shape,
            to = ?shape,
            "native VAAPI stream renegotiated — rebuilding the session"
        );
        // Dropped BEFORE the replacement is built so the old pool's video memory is
        // released first; a 4K pool is on the order of a hundred megabytes.
        //
        // Surfaces the CONSUMER still holds are destroyed here too, and that is
        // sound: an exported PRIME fd holds its own reference on the underlying
        // buffer object, and the presenter dup'd every fd it imported. The pixels
        // outlive the VASurface — which is the whole mechanism zero-copy rests on.
        old.destroy(d);
    }
    // A NEW generation, always — this is what makes those outstanding frames safe to
    // let go of. Their release tokens name surface indices in a pool that no longer
    // exists, and applying one to the new pool would mark a live surface free and
    // hand it to the decoder as a spare. The bump is here, at the one place a pool is
    // ever replaced, rather than at the call sites.
    *generation += 1;
    let mut built = Session::build(d, codec, shape)?;
    built.generation = *generation;
    Ok(slot.insert(built))
}

/// Record which surface holds the picture just planned — or that NOTHING does.
///
/// The slot bindings are re-derived from the ledger FIRST — the conversion has
/// already applied this AU's removals, so a slot the planner released binds nothing
/// — and only then is the setup picture bound, by asking the ledger where it landed.
/// Asking rather than assuming is what handles the one awkward case: a non-reference
/// picture with no free frame buffer is stored and evicted inside a single plan, so
/// it holds NO slot when the conversion returns. Its surface is kept out of the free
/// list by `pending` instead, until it has been output.
///
/// ⚠ `surface` is `None` on the AV1 refusal path, and that call is not optional. The
/// AV1 conversion assigns the ledger slot BEFORE its tile walk — deliberately, so a
/// lost tile group does not desynchronise the ledger from the planner's store forever
/// (`pf_vaadec::plan_to_va_av1`'s docs) — which means a refusal can leave a slot
/// re-assigned to a picture that never decoded while `slot_surface` still holds the
/// surface of whatever occupied that slot BEFORE. That is not a missing reference, it
/// is a WRONG one, and nothing downstream could tell. Binding `None` makes the slot
/// read back as `VA_INVALID_ID`, which the conversion then substitutes with a live
/// surface. Nothing is pushed to `pending` either: an undecoded surface must never be
/// exportable.
fn bind_setup(s: &mut Session, stored: Option<u64>, surface: Option<usize>) {
    s.sync_slot_bindings();
    let Some(id) = stored else { return };
    if let Some(slot) = s.slots.slot_of(id) {
        s.slot_surface[usize::from(slot)] = surface;
    }
    if let Some(surface) = surface {
        s.pending.push((id, surface));
    }
}

/// One slice-parameter (AV1: tile-parameter) buffer and the bitstream region its
/// records address.
///
/// The two travel together because `vaRenderPicture` is what establishes which data
/// buffer a record's `slice_data_offset` is relative to — it is handed the pair.
struct SlicePair {
    /// The record array. Borrowed from the caller's converted plan, which outlives
    /// the `submit` call that reads it.
    params: *const c_void,
    /// ONE record's size. `vaCreateBuffer` takes the element size and the element
    /// count separately and they are not interchangeable.
    record_size: usize,
    /// How many records this buffer carries: **1** for H.264 and H.265 — one slice,
    /// one buffer, exactly as libavcodec sends them — and a whole tile group's worth
    /// for AV1, which is the one codec libavcodec packs several records into a single
    /// buffer for.
    records: usize,
    /// The bitstream those records address, in ACCESS-UNIT coordinates.
    data: std::ops::Range<usize>,
}

/// The H.264/H.265 shape of the above: one record per buffer, parallel to its data
/// range.
///
/// ⚠ The length check is not ceremony, even though both conversions build the two
/// vectors in one loop today and cannot produce a mismatch. `zip` would answer a
/// future divergence by SILENTLY TRUNCATING — a picture submitted with some of its
/// slices, which decodes to a partial frame rather than to an error, and which no
/// gate here has hardware to catch. A refusal is the honest answer and costs one
/// comparison per access unit.
fn one_record_each<T>(records: &[T], data: &[std::ops::Range<usize>]) -> Result<Vec<SlicePair>> {
    if records.len() != data.len() {
        bail!(
            "{} slice record(s) for {} data range(s) — the conversion's two halves \
             disagree",
            records.len(),
            data.len()
        );
    }
    Ok(records
        .iter()
        .zip(data)
        .map(|(record, range)| SlicePair {
            params: (record as *const T).cast::<c_void>(),
            record_size: size_of::<T>(),
            records: 1,
            data: range.clone(),
        })
        .collect())
}

/// One picture's buffers, in the order libavcodec's VAAPI path submits them: the
/// parameter buffers in one `vaRenderPicture`, then the interleaved
/// slice-parameter/slice-data pairs in another. Matching the path drivers are
/// validated against is worth more than any tidier arrangement.
fn submit(
    d: &Display,
    s: &Session,
    target: VaSurfaceId,
    pic: (*const c_void, usize),
    iq: Option<(*const c_void, usize)>,
    slices: &[SlicePair],
    au: &[u8],
) -> Result<()> {
    let mut params: Vec<VaBufferId> = Vec::with_capacity(2);
    let mut slice_buffers: Vec<VaBufferId> = Vec::with_capacity(slices.len() * 2);
    // A picture that was BEGUN must be ended even if a step in between failed, or
    // the context stays mid-picture and every later `vaBeginPicture` fails on a
    // stream that was otherwise recoverable. libavcodec's VAAPI path has the same
    // `fail_with_picture` label for the same reason.
    let mut begun = false;
    // Every buffer created below must be destroyed whatever happens next — libva
    // does not reclaim them at `vaEndPicture` (see `Display::destroy_buffers`).
    let result = (|| -> Result<()> {
        params.push(
            d.create_buffer(
                s.context,
                pf_vaadec::va::VA_PICTURE_PARAMETER_BUFFER_TYPE,
                pic.1,
                1,
                pic.0,
            )
            .context("picture parameter buffer")?,
        );
        if let Some((ptr, size)) = iq {
            params.push(
                d.create_buffer(
                    s.context,
                    pf_vaadec::va::VA_IQ_MATRIX_BUFFER_TYPE,
                    size,
                    1,
                    ptr,
                )
                .context("IQ matrix buffer")?,
            );
        }
        for (n, pair) in slices.iter().enumerate() {
            let range = pair.data.clone();
            let data = au.get(range.clone()).ok_or_else(|| {
                anyhow!(
                    "slice {n}: range {range:?} is outside a {}-byte access unit",
                    au.len()
                )
            })?;
            if pair.records == 0 {
                bail!("slice {n}: a parameter buffer with no records");
            }
            slice_buffers.push(
                d.create_buffer(
                    s.context,
                    pf_vaadec::va::VA_SLICE_PARAMETER_BUFFER_TYPE,
                    pair.record_size,
                    pair.records,
                    pair.params,
                )
                .with_context(|| format!("slice {n} parameter buffer"))?,
            );
            slice_buffers.push(
                d.create_buffer(
                    s.context,
                    pf_vaadec::va::VA_SLICE_DATA_BUFFER_TYPE,
                    data.len(),
                    1,
                    data.as_ptr().cast::<c_void>(),
                )
                .with_context(|| format!("slice {n} data buffer"))?,
            );
        }

        // SAFETY: a live display, context and target surface; both buffer arrays are
        // locals that outlive their calls and their counts match their lengths.
        unsafe {
            d.va.check(
                "vaBeginPicture",
                (d.va.begin_picture)(d.display, s.context, target),
            )?;
            begun = true;
            d.va.check(
                "vaRenderPicture(parameters)",
                (d.va.render_picture)(
                    d.display,
                    s.context,
                    params.as_mut_ptr(),
                    params.len() as c_int,
                ),
            )?;
            d.va.check(
                "vaRenderPicture(slices)",
                (d.va.render_picture)(
                    d.display,
                    s.context,
                    slice_buffers.as_mut_ptr(),
                    slice_buffers.len() as c_int,
                ),
            )?;
            begun = false;
            d.va.check("vaEndPicture", (d.va.end_picture)(d.display, s.context))?;
        }
        Ok(())
    })();
    if begun {
        // SAFETY: a live display and context with a picture open; the status is
        // deliberately discarded — the real failure is `result`, and reporting this
        // one would replace the cause with its consequence.
        unsafe { (d.va.end_picture)(d.display, s.context) };
    }
    d.destroy_buffers(&params);
    d.destroy_buffers(&slice_buffers);
    result
}

/// Turn this AU's OUTPUT list into at most one shipped frame.
///
/// Display order, not decode order. `plan.dpb.outputs` is what the planner says is
/// ready to be shown and in what order, and the surface for each is looked up by
/// `PicId` — so a reordering stream presents correctly rather than in the order the
/// pictures happened to decode. (The native D3D11VA rung does present in decode
/// order; that is a known finding on a rung that blits its output away, and there
/// was no reason to inherit it here where the display-order queue costs a lookup.)
///
/// Newest wins, which is the same rule the FFmpeg VAAPI rung applies inside its
/// receive loop: on a live stream a picture already superseded is not worth a frame
/// interval. Superseded outputs are released rather than exported.
///
/// The retirement rule is `pf_vkdecode`'s `settle_dpb`, reimplemented here over this
/// rung's flat pending list rather than reasoned out again, because both halves of it
/// are easy to get wrong:
///
/// * **`removed` retires a pending picture too.** A picture can leave the DPB without
///   ever being output (`no_output_of_prior_pics` at an IDR is the everyday case), and
///   a pending list that only shrinks on OUTPUT keeps its surface off the free list
///   for the rest of the session — a slow, silent walk into pool exhaustion.
/// * **An output naming no pending picture is a TRACE, not an error.** Ids planned
///   before this decoder existed, or dropped across a session rebuild, are
///   display-order gaps.
#[allow(clippy::too_many_arguments)]
fn finish(
    d: &Display,
    s: &mut Session,
    outputs: &[u64],
    removed: &[u64],
    damaged: bool,
    keyframe: bool,
    color: ColorDesc,
    // The DISPLAY region for this picture. A parameter rather than a read of
    // `s.shape` because AV1's is per-FRAME: its render size may change without a key
    // frame, so it cannot live in the shape that rebuilds the session.
    display: (u32, u32),
    recovery_request: &mut bool,
    tx: &mpsc::Sender<VaRelease>,
) -> Result<Option<DmabufFrame>> {
    // A concealed picture is not shown: it was decoded from a substitute reference,
    // so shipping it paints the substitution on screen. Nothing this AU output is
    // shown, the pump is asked to re-anchor, and the caller records the damage.
    let shown = if damaged {
        None
    } else {
        outputs.last().copied()
    };
    // OUTPUTS FIRST, and the shown one is taken out before anything else runs.
    // A picture is normally output and removed by the SAME access unit — that is
    // what bumping is — so retiring `removed` before claiming the frame would
    // discard the very picture about to be displayed, on essentially every AU.
    let claimed = shown.and_then(|id| {
        let found = s.pending.iter().position(|(pid, _)| *pid == id);
        if found.is_none() {
            tracing::trace!(id, "output id without a pending picture");
        }
        found.map(|index| s.pending.remove(index).1)
    });
    for id in outputs {
        if Some(*id) != shown {
            s.pending.retain(|(pid, _)| pid != id);
        }
    }
    // Whatever left the DPB is retired from the pending list whether or not it was
    // ever output. Its SURFACE only becomes free if nothing else holds it — a
    // reference still bound to a slot, or a frame the consumer has, stays put.
    for id in removed {
        s.pending.retain(|(pid, _)| pid != id);
    }
    if damaged {
        *recovery_request = true;
        return Ok(None);
    }
    let Some(surface_index) = claimed else {
        return Ok(None);
    };
    let surface = s.surfaces[surface_index];

    // OWNED from here. `export` wraps the descriptor's fds the moment the call
    // succeeds, so every refusal below closes them by dropping rather than by
    // remembering to — an earlier draft leaked one fd per refused frame.
    let (exported, fds) = export(d, surface)?;
    if exported.fourcc != s.fourcc {
        // The pool was created with an explicit pixel format; a surface exporting a
        // different one means the driver silently substituted, and the consumer
        // would import the wrong layout.
        bail!(
            "surface exported fourcc {:#010x}, the pool was created as {:#010x}",
            exported.fourcc,
            s.fourcc
        );
    }
    if exported.planes.len() < 2 {
        bail!(
            "a two-plane surface exported {} plane(s) — the chroma is missing",
            exported.planes.len()
        );
    }

    s.held[surface_index] = true;
    let planes = exported
        .planes
        .iter()
        .map(|p| DmabufPlane {
            fd: p.fd,
            offset: p.offset,
            stride: p.stride,
        })
        .collect();
    Ok(Some(DmabufFrame {
        // The DISPLAY region. The surface is allocated at the coded size and is
        // taller/wider than the picture; handing over the coded size would show the
        // codec's granule padding.
        width: display.0,
        height: display.1,
        fourcc: exported.fourcc,
        modifier: exported.modifier,
        planes,
        color,
        keyframe,
        guard: DrmFrameGuard(VaFrameGuard {
            _fds: fds,
            tx: tx.clone(),
            release: VaRelease {
                surface: surface_index,
                generation: s.generation,
            },
        }),
    }))
}

/// Wait for the decode and export the surface as DRM-PRIME dmabufs.
///
/// The `vaSyncSurface` is what makes the hand-off safe: VAAPI exposes no fence to
/// the importer, so the accepted contract on this path — and what libavcodec's own
/// VAAPI→DRM_PRIME mapping does — is to sync before the fds leave. It is a blocking
/// wait on the pump thread, which is worth naming: at 60 fps against decodes of a
/// millisecond or two it is slack, and the alternative is handing the presenter a
/// surface the GPU has not finished writing.
///
/// Returns the flattened surface AND the fds it owns, together — so that from the
/// instant the export succeeds those fds are RAII-owned and every later refusal
/// closes them by dropping. `ExportedSurface::planes` still carries the raw fds,
/// borrowed from these: several planes routinely name one object, and each object's
/// fd must be closed exactly once.
fn export(d: &Display, surface: VaSurfaceId) -> Result<(pf_vaadec::ExportedSurface, Vec<OwnedFd>)> {
    // SAFETY: a live display and a surface from its own pool.
    d.va.check("vaSyncSurface", unsafe {
        (d.va.sync_surface)(d.display, surface)
    })?;

    let mut desc = pf_vaadec::VaDrmPrimeSurfaceDescriptor::zeroed();
    // SAFETY: a live display and surface; `desc` is a local of exactly the layout
    // `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2` writes (measured by
    // `pf-vaadec/layout-probe.c` and compile-asserted), and it outlives the call.
    d.va.check("vaExportSurfaceHandle", unsafe {
        (d.va.export_surface_handle)(
            d.display,
            surface,
            VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            pf_vaadec::VA_EXPORT_SURFACE_SEPARATE_LAYERS | pf_vaadec::VA_EXPORT_SURFACE_READ_ONLY,
            (&mut desc as *mut pf_vaadec::VaDrmPrimeSurfaceDescriptor).cast::<c_void>(),
        )
    })?;

    match pf_vaadec::flatten(&desc) {
        Ok(exported) => {
            let fds = exported
                .object_fds
                .iter()
                // SAFETY: each fd came out of a successful `vaExportSurfaceHandle`
                // and is owned by this process exactly once. `flatten` lists one per
                // OBJECT, so no fd is wrapped twice even where planes share it.
                .map(|fd| unsafe { OwnedFd::from_raw_fd(*fd) })
                .collect();
            Ok((exported, fds))
        }
        Err(e) => {
            // The export SUCCEEDED, so its fds belong to this process even though the
            // descriptor cannot be read as a surface. Every writable slot is swept
            // rather than the first `num_objects` — a refusal for a bogus
            // `num_objects` is exactly the case where that count cannot be trusted to
            // bound anything.
            for object in &desc.objects {
                if object.fd >= 0 {
                    // SAFETY: an fd this process owns from the successful export;
                    // wrapping it in an `OwnedFd` that immediately drops closes it once.
                    drop(unsafe { OwnedFd::from_raw_fd(object.fd) });
                }
            }
            Err(anyhow!("{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session whose libva handles are never used — every field exercised below is
    /// plain bookkeeping, which is exactly why this rule can be tested at all without
    /// a GPU. The pool is deliberately small so an exhausted free list is reachable.
    fn session(surfaces: usize, slots: usize) -> Session {
        Session {
            shape: StreamShape {
                coded_width: 64,
                coded_height: 64,
                display_width: 64,
                display_height: 64,
                max_dpb_frames: slots - 1,
                chroma_format_idc: 1,
                bit_depth: 8,
            },
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            surfaces: (0..surfaces as u32).map(|i| 0x100 + i).collect(),
            held: vec![false; surfaces],
            slot_surface: vec![None; slots],
            pending: Vec::new(),
            slots: pf_vaadec::SlotMap::new(slots - 1),
            fourcc: pf_vaadec::VA_FOURCC_NV12,
            generation: 1,
        }
    }

    /// The whole rule, in one test: a surface is free only when NOTHING claims it,
    /// and the three claims end at different moments.
    #[test]
    fn a_surface_is_free_only_when_no_slot_no_output_and_no_consumer_claims_it() {
        let mut s = session(4, 3);
        assert_eq!(
            s.free_surface(),
            Some(0),
            "a fresh pool starts at the front"
        );

        // 0: a live DPB reference. 1: decoded, still owing an output. 2: on screen.
        s.slot_surface[0] = Some(0);
        s.pending.push((7, 1));
        s.held[2] = true;
        assert_eq!(
            s.free_surface(),
            Some(3),
            "the first three are each claimed a different way"
        );

        // Losing ONE claim is not enough when another still stands.
        s.held[3] = true;
        s.slot_surface[1] = Some(1);
        assert_eq!(
            s.free_surface(),
            None,
            "an exhausted pool must say so rather than hand out a claimed surface"
        );

        // Surface 1 is both slot-bound and pending: releasing only the output keeps
        // it out, and only when the slot goes too does it come back.
        s.pending.clear();
        assert_eq!(s.free_surface(), None);
        s.slot_surface[1] = None;
        assert_eq!(s.free_surface(), Some(1));
    }

    /// The consumer's release is what ends the third claim — and it must be matched
    /// to the generation that issued it, or a renegotiation hands a live surface out.
    #[test]
    fn a_release_from_a_retired_pool_never_frees_a_surface_in_the_new_one() {
        let mut s = session(4, 3);
        s.held[2] = true;
        let (tx, rx) = mpsc::channel();
        let mut stale = 0u64;

        // A token from the pool that was retired before this one. Surface index 2
        // exists in BOTH pools, which is what makes this dangerous: the index is
        // valid, and only the generation says it means a different surface.
        tx.send(VaRelease {
            surface: 2,
            generation: 0,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(
            s.held[2],
            "a stale generation must not clear a hold in the CURRENT pool"
        );
        assert_eq!(stale, 1, "and it must be counted, not silent");

        // The matching generation does free it.
        tx.send(VaRelease {
            surface: 2,
            generation: 1,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(!s.held[2]);
        assert_eq!(stale, 1);

        // An index the pool does not have is counted, never a panic.
        tx.send(VaRelease {
            surface: 99,
            generation: 1,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert_eq!(stale, 2);
    }

    /// The bindings follow the ledger: a slot the planner released binds nothing,
    /// and the surface it held is only free if nothing else claims it.
    #[test]
    fn syncing_bindings_drops_the_slots_the_ledger_no_longer_holds() {
        let mut s = session(4, 3);
        s.slot_surface[0] = Some(0);
        s.slot_surface[1] = Some(1);
        s.slots.assign(11).expect("a free slot");
        s.sync_slot_bindings();
        assert_eq!(
            s.slot_surface,
            vec![Some(0), None, None],
            "slot 0 is held by picture 11; slot 1's picture is gone"
        );
    }

    /// The decode target is never a surface the reference table names — swept over
    /// every binding state a small pool can be in.
    ///
    /// This rung's exemption from the aliasing defect the D3D11VA and Vulkan rungs had
    /// to defer their way out of (module docs), stated as the one thing it actually
    /// rests on. `pf-vaadec` proves the conversion can only name surfaces out of the
    /// table it is handed; this proves the table and the target cannot overlap.
    ///
    /// Swept rather than exemplified because the claim is structural — a free surface
    /// is by definition one no slot binds, and the table is exactly what the slots bind
    /// — so it should hold in states an ordinary run never reaches, and a sweep is what
    /// says so. The `held` and `pending` masks are varied too even though they can only
    /// ever REMOVE candidates from the free list: a future claim that could add one
    /// back is exactly what this would catch.
    #[test]
    fn the_decode_target_can_never_be_a_surface_the_reference_table_names() {
        const SURFACES: usize = 4;
        const SLOTS: usize = 3;
        let choices: Vec<Option<usize>> = std::iter::once(None)
            .chain((0..SURFACES).map(Some))
            .collect();

        let (mut states, mut with_a_target, mut exhausted) = (0usize, 0usize, 0usize);
        for a in &choices {
            for b in &choices {
                for c in &choices {
                    let bound = [*a, *b, *c];
                    // Two slots binding ONE surface is not a state the pool can reach —
                    // `bind_setup` only ever binds a surface nothing else claims — and
                    // asserting about it would be asserting about a defect elsewhere.
                    let mut distinct: Vec<usize> = bound.iter().flatten().copied().collect();
                    let claimed = distinct.len();
                    distinct.sort_unstable();
                    distinct.dedup();
                    if distinct.len() != claimed {
                        continue;
                    }
                    for held_mask in 0..(1u32 << SURFACES) {
                        for pending_mask in 0..(1u32 << SURFACES) {
                            let mut s = session(SURFACES, SLOTS);
                            s.slot_surface = bound.to_vec();
                            s.held = (0..SURFACES).map(|i| held_mask >> i & 1 == 1).collect();
                            s.pending = (0..SURFACES)
                                .filter(|i| pending_mask >> i & 1 == 1)
                                .map(|i| (100 + i as u64, i))
                                .collect();
                            states += 1;

                            let Some((index, target, table)) = s.acquire_target() else {
                                exhausted += 1;
                                continue;
                            };
                            with_a_target += 1;
                            assert_eq!(
                                target, s.surfaces[index],
                                "the target must be the pool's surface at the index it \
                                 returned, or the caller binds one and submits another"
                            );
                            assert_eq!(table.len(), SLOTS, "one table entry per slot");
                            assert!(
                                !table.contains(&target),
                                "bindings {bound:?}, held {held_mask:#06b}, pending \
                                 {pending_mask:#06b}: the decode target {target:#x} is \
                                 in the reference table {table:x?} — every submission \
                                 built from that pair decodes into a surface it may be \
                                 predicting from"
                            );
                        }
                    }
                }
            }
        }
        // The sweep has to reach both answers, or it is asserting about one branch.
        assert!(states > 1000, "only {states} states swept");
        assert!(with_a_target > 0 && exhausted > 0);
    }

    /// The order the rung must NOT be written in, and the reason
    /// [`Session::acquire_target`] hands the target and the table back together.
    ///
    /// The exemption above is not a property of the pool alone: it needs the target and
    /// the table to come from ONE snapshot. Split them and this rung acquires the
    /// D3D11VA/Vulkan defect exactly — because the table must be the PRE-removal one
    /// (a reference an access unit names can be a picture the same access unit evicts,
    /// which on a punktfunk host's own low-delay H.264 is 117 access units in 120),
    /// while a free list consulted after those removals offers precisely the displaced
    /// picture's surface.
    #[test]
    fn taking_the_free_surface_after_the_removals_would_hand_out_a_referenced_surface() {
        let mut s = session(4, 3);

        // Two decoded reference pictures, each in its own surface, both already
        // displayed and returned by the presenter — so only the SLOT binding keeps
        // their surfaces off the free list. That is the steady state of a low-delay
        // stream, where a picture is output by its own access unit and evicted by the
        // sliding window several units later.
        s.slots.assign(11).expect("a free slot");
        bind_setup(&mut s, Some(11), Some(0));
        s.slots.assign(12).expect("a free slot");
        bind_setup(&mut s, Some(12), Some(1));
        s.pending.clear();

        // What the conversion resolves its references through, taken BEFORE this access
        // unit's removals — which is not a choice, it is where the references are.
        let table = s.surface_table();
        assert!(
            table.contains(&s.surfaces[0]),
            "picture 11's surface must still be a resolvable reference"
        );

        // The order the rung is written in: one snapshot, and the target cannot be in
        // the table it came with.
        let (_, target, same_table) = s.acquire_target().expect("the pool has spares");
        assert_eq!(
            same_table, table,
            "acquire_target must not re-derive the table"
        );
        assert!(!table.contains(&target));

        // The defect: the conversion applies the removal, the bindings follow it, and
        // only THEN is the free list consulted.
        s.slots.release(11);
        s.sync_slot_bindings();
        let late = s.free_surface().expect("the pool has spares");
        assert_eq!(
            s.surfaces[late], table[0],
            "the late free list offers the surface of the picture this access unit just \
             displaced, and the pre-removal table still names it as a reference — \
             decode into that and the driver predicts from the picture it is writing"
        );
    }

    /// A picture the conversion REFUSED binds no surface — so nothing can show it and
    /// nothing can predict from it.
    ///
    /// Both halves matter and they fail differently. The AV1 conversion assigns its
    /// ledger slot before the tile walk, so a refusal leaves a slot re-assigned to a
    /// picture that never decoded; leaving the slot's PREVIOUS binding in place would
    /// hand the next frame a real, decoded, WRONG picture, which no later check could
    /// notice. And a `pending` entry for it would let a later `show_existing_frame`
    /// claim the surface and ship it — a surface the driver never wrote, which is
    /// uninitialised video memory rather than a stale frame.
    #[test]
    fn a_refused_picture_binds_nothing_and_can_never_be_exported() {
        let mut s = session(4, 3);

        // Picture 11 decoded into surface 0 and took slot 0.
        s.slots.assign(11).expect("a free slot");
        bind_setup(&mut s, Some(11), Some(0));
        assert_eq!(s.slot_surface[0], Some(0));
        assert_eq!(s.surface_table()[0], s.surfaces[0]);
        assert_eq!(s.pending, vec![(11, 0)]);

        // Picture 12's access unit lost its tile groups. The conversion released 11,
        // handed 12 the slot it just gave back — the routine case, not a contrived one
        // — and then refused.
        s.slots.release(11);
        assert_eq!(s.slots.assign(12).expect("the slot 11 gave back"), 0);
        bind_setup(&mut s, Some(12), None);

        assert_eq!(
            s.slot_surface[0], None,
            "the slot must not keep picture 11's surface: picture 12 never decoded, \
             and a reference to 12 that reads 11 is a wrong picture, not a missing one"
        );
        assert_eq!(
            s.surface_table()[0],
            VA_INVALID_ID,
            "and the table the conversion reads must say so, so it can substitute"
        );
        assert!(
            !s.pending.iter().any(|(id, _)| *id == 12),
            "an undecoded picture owes no output — a pending entry is what would let \
             a later show_existing_frame export a surface the driver never wrote"
        );

        // The slot is still LIVE in the ledger, which is the whole point of the
        // conversion mutating before it refuses: the next frame resolves picture 12
        // rather than hard-erroring on it.
        assert_eq!(s.slots.slot_of(12), Some(0));
    }

    /// A conformance window with a non-zero ORIGIN is refused, not cropped from the
    /// wrong corner: nothing downstream carries an origin.
    #[test]
    fn a_non_zero_crop_origin_is_refused() {
        let ok = shape_of(
            1920,
            1088,
            pf_vaadec::DisplayCrop {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            4,
            1,
            8,
        )
        .expect("the ordinary 1088-coded 1080 picture");
        assert_eq!((ok.display_width, ok.display_height), (1920, 1080));
        assert_eq!((ok.coded_width, ok.coded_height), (1920, 1088));

        assert!(shape_of(
            1920,
            1088,
            pf_vaadec::DisplayCrop {
                x: 8,
                y: 0,
                width: 1912,
                height: 1080,
            },
            4,
            1,
            8,
        )
        .is_err());
    }

    /// One synthetic AV1 plan: a sequence that permits `max` and a frame that codes
    /// `frame`, so the two can be told apart.
    fn av1_plan(max: (u16, u16), frame: (u32, u32), render: (u32, u32)) -> pf_vaadec::AuPlanAv1 {
        let sequence = pf_vaadec::ParsedSequenceHeaderAv1 {
            max_frame_width_minus_1: max.0 - 1,
            max_frame_height_minus_1: max.1 - 1,
            ..Default::default()
        };
        pf_vaadec::AuPlanAv1 {
            picture: pf_vaadec::PicturePlanAv1 {
                frame_type: pf_vaadec::FrameTypeAv1::KeyFrame,
                is_key: true,
                show_frame: true,
                showable_frame: false,
                order_hint: 0,
                upscaled_width: frame.0,
                frame_width: frame.0,
                frame_height: frame.1,
                render_width: render.0,
                render_height: render.1,
                bit_depth: 8,
                chroma_format_idc: 1,
                colour: pf_vaadec::ColourDescription {
                    colour_primaries: 1,
                    transfer_characteristics: 1,
                    matrix_coefficients: 1,
                    video_full_range: false,
                },
            },
            tiles: Vec::new(),
            refs: [None; 7],
            dpb: pf_vaadec::DpbUpdateAv1::default(),
            dpb_refs: Vec::new(),
            warnings: Vec::new(),
            sequence: std::rc::Rc::new(sequence),
            header: std::rc::Rc::new(pf_vaadec::ParsedFrameHeaderAv1::default()),
        }
    }

    /// An AV1 session is sized from the SEQUENCE, never from the frame — and its DPB
    /// depth is the codec's constant.
    ///
    /// Both halves are the difference between a stream that survives a mid-GOP resize
    /// and one that rebuilds its pool, drops every reference and conceals its way back
    /// to a keyframe. AV1 permits a frame to code any size up to the sequence maximum
    /// with no key frame in sight, so a shape derived from the frame changes when
    /// nothing renegotiated.
    #[test]
    fn an_av1_session_is_sized_from_the_sequence_maximum_not_the_frame() {
        let big = shape_of_av1(&av1_plan((1920, 1080), (1920, 1080), (1920, 1080)));
        assert_eq!((big.coded_width, big.coded_height), (1920, 1080));
        assert_eq!(
            big.max_dpb_frames, 8,
            "NUM_REF_FRAMES, not a stream property"
        );

        // The same sequence, a frame coded smaller and shown smaller still. Neither
        // may move the shape, or this is a renegotiation.
        let small = shape_of_av1(&av1_plan((1920, 1080), (1280, 720), (960, 540)));
        assert_eq!(
            small, big,
            "a frame that resized itself must not rebuild the session"
        );

        // A genuinely different sequence does move it.
        let other = shape_of_av1(&av1_plan((1280, 720), (1280, 720), (1280, 720)));
        assert_ne!(other, big);
    }

    /// The render size reaches the presenter CLAMPED to the decoded picture.
    ///
    /// AV1 5.9.6 puts no upper bound on the render size, so a stream may legally ask
    /// to be shown at more than it coded; handing that to the presenter as a crop
    /// would address rows the surface does not have. This restates the clamp in
    /// `frame_av1`, which cannot itself be reached without a device.
    #[test]
    fn an_oversized_render_region_is_clamped_to_the_decoded_picture() {
        let plan = av1_plan((1920, 1080), (1280, 720), (4096, 4096));
        let display = (
            plan.picture.render_width.min(plan.picture.upscaled_width),
            plan.picture.render_height.min(plan.picture.frame_height),
        );
        assert_eq!(display, (1280, 720));

        let ordinary = av1_plan((1920, 1080), (1920, 1088), (1920, 1080));
        let display = (
            ordinary
                .picture
                .render_width
                .min(ordinary.picture.upscaled_width),
            ordinary
                .picture
                .render_height
                .min(ordinary.picture.frame_height),
        );
        assert_eq!(display, (1920, 1080), "the ordinary crop still crops");
    }

    /// H.264 and H.265 send ONE record per parameter buffer; AV1 is the exception.
    ///
    /// `vaCreateBuffer` takes an element size and an element count, and getting the
    /// pair backwards is a driver reading `size` records of `count` bytes. This pins
    /// the side every codec but AV1 is on.
    #[test]
    fn a_slice_pair_carries_one_record_unless_av1_says_otherwise() {
        let records = [7u32, 8, 9];
        let ranges = vec![0..4, 4..8, 8..12];
        let pairs = one_record_each(&records, &ranges).expect("parallel lengths");
        assert_eq!(pairs.len(), 3);
        for pair in &pairs {
            assert_eq!(pair.records, 1);
            assert_eq!(pair.record_size, size_of::<u32>());
        }
        assert_eq!(pairs[1].data, 4..8);

        // A record without its data range REFUSES. `zip` would drop it silently and
        // submit a picture missing a slice, which decodes rather than fails.
        assert!(one_record_each(&records, &ranges[..2]).is_err());
        assert!(one_record_each(&records[..1], &ranges).is_err());
    }

    /// A shape this rung cannot decode is refused BEFORE libva is even loaded.
    ///
    /// The ordering is the point, not the refusal. M3 WP-2's review caught the
    /// opposite arrangement on the Vulkan rung: a backend that accepts a session and
    /// then refuses its first access unit has already cost the ladder its
    /// fall-through — the refusal arrives as a decode error, burns the demotion
    /// streak, and lands the session several rungs lower than it would have been.
    /// Asserting on the MESSAGE is what pins the order: this test passes on a machine
    /// with libva and on one without, and only stays passing while the profile probe
    /// comes first.
    #[test]
    fn a_shape_with_no_profile_is_refused_before_libva_is_loaded() {
        let e = NativeVaapiDecoder::new(
            pf_vaadec::Codec::H264,
            StreamFormat {
                chroma_format_idc: 3,
                bit_depth: 8,
            },
        )
        .err()
        .expect("4:4:4 H.264 has no VAAPI profile in this rung's envelope");
        let text = format!("{e:#}");
        assert!(
            text.contains("profile"),
            "the refusal must name the stream shape, not whatever libva said: {text}"
        );
    }

    /// On-glass probe: resolve every entry point against the REAL libva on this
    /// machine, then say what each render node does.
    ///
    /// This is the one thing no gate can check. `dlsym` takes a STRING: a mistyped
    /// entry point compiles, clippies and unit-tests perfectly and fails only on a
    /// machine with libva — so the 19 names are worth exercising once against a real
    /// runtime, and this test is how. It is also the honest report of a box's VAAPI
    /// situation: a node that will not initialise is a legitimate outcome (NVIDIA has
    /// no usable VAAPI driver), printed rather than failed, because the rung's claim
    /// is that such a box REFUSES CLEANLY and lets the ladder fall through.
    ///
    /// `cargo test -p pf-client-core --lib probe_this_machines_libva -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a machine with a libva runtime"]
    fn probe_this_machines_libva() {
        let va = match Libva::load() {
            Ok(va) => {
                eprintln!("libva: every entry point resolved");
                va
            }
            Err(e) => {
                eprintln!("libva: NOT LOADED — {e:#}");
                eprintln!("(this is the clean-refusal path; the ladder falls through here)");
                return;
            }
        };

        let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .expect("/dev/dri")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        eprintln!("render nodes: {nodes:?}");

        let mut opened = 0;
        for node in &nodes {
            let path = node.to_string_lossy().into_owned();
            match Display::probe(&va, &path) {
                Ok((display, fd, version)) => {
                    opened += 1;
                    eprintln!("  {path}: VA-API {}.{}", version.0, version.1);
                    let d = Display {
                        va: Libva::load().expect("libva loaded once already"),
                        display,
                        node: Some(fd),
                        path: path.clone(),
                        version,
                    };
                    for (name, profile) in [
                        ("H.264 High", pf_vaadec::config::VA_PROFILE_H264_HIGH),
                        ("HEVC Main", pf_vaadec::config::VA_PROFILE_HEVC_MAIN),
                        ("HEVC Main 10", pf_vaadec::config::VA_PROFILE_HEVC_MAIN10),
                        // AV1 was MISSING from this loop until 2026-08-07, which is part
                        // of why the rung's evidence row could say "never decoded a frame"
                        // for so long without anyone noticing what had not been asked.
                        // `profile_for` maps both 8- and 10-bit AV1 4:2:0 onto Profile 0.
                        ("AV1 Profile 0", pf_vaadec::config::VA_PROFILE_AV1_PROFILE0),
                        ("AV1 Profile 1", pf_vaadec::config::VA_PROFILE_AV1_PROFILE1),
                    ] {
                        match d.require_entrypoint(profile) {
                            Ok(()) => eprintln!("    {name}: VLD decode"),
                            Err(e) => eprintln!("    {name}: no ({e})"),
                        }
                    }
                }
                Err(e) => eprintln!("  {path}: {e:#}"),
            }
        }
        eprintln!(
            "{opened} of {} node(s) initialised a VAAPI display",
            nodes.len()
        );
    }

    /// The vendored AV1 vector, as IVF: 320x240 Main 4:2:0 8-bit, 250 temporal units
    /// carrying 274 coded frames (24 units carry two, and those extras are HIDDEN —
    /// decoded, referenced, never shown), so **250 frames are displayed**. The same
    /// file `pf-vkdecode`'s Vulkan parity leg and `video_d3d11_native`'s D3D11VA leg
    /// walk, so a count that disagrees with 250 is this rung's problem, not the
    /// vector's.
    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// One temporal unit per IVF packet: 32 bytes of `DKIF` header, then
    /// `[u32 size][u64 pts][size bytes]`. Hand-rolled because `pf-client-core` does
    /// not depend on the vendored parser crate — the same reason and the same walk as
    /// `video_d3d11_native`'s `split_ivf`, and kept honest by the unit count asserted
    /// at the top of the test below.
    fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
        assert_eq!(&stream[0..4], b"DKIF", "the AV1 vector must be an IVF file");
        let header = usize::from(u16::from_le_bytes([stream[6], stream[7]]));
        let mut out = Vec::new();
        let mut at = header;
        while at + 12 <= stream.len() {
            let size =
                u32::from_le_bytes(stream[at..at + 4].try_into().expect("four bytes")) as usize;
            at += 12;
            assert!(
                at + size <= stream.len(),
                "an IVF frame header claims {size} bytes past the end of the file"
            );
            out.push(&stream[at..at + size]);
            at += size;
        }
        out
    }

    /// Does this machine's VAAPI actually DECODE AV1 — the question the evidence table
    /// has answered "no hardware has ever tried" since M6.
    ///
    /// This is deliberately weaker than the Vulkan and D3D11VA AV1 legs, and the
    /// difference is worth stating rather than hiding: those two hash every decoded
    /// frame against libavcodec's goldens, because both can read their decoded surface
    /// back. This rung hands out a **DRM-PRIME dmabuf** whose memory is tiled by the
    /// driver, so there is no CPU-readable image to hash without adding a
    /// `vaDeriveImage`/`vaGetImage` path that production does not use and does not
    /// want. So this asserts what CAN be asserted honestly — that every temporal unit
    /// is accepted, that the expected number of frames comes back, and that each one
    /// is a real exported surface of the right shape — and it is NOT frame-hash parity.
    /// It is what turns "never decoded a frame anywhere" into a measurement; promoting
    /// the rung to `verified` still wants parity, and that wants a readback path first.
    ///
    /// Fails loudly rather than skipping when the device has no AV1 entry point: it is
    /// `#[ignore]`d, so it only runs when someone deliberately points it at a box that
    /// is supposed to have one, and a silent pass there is the invisible-failure mode
    /// this whole program exists to end.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an AV1 VLD entry point"]
    fn av1_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let units = split_ivf(AV1_25FPS);
        assert_eq!(
            units.len(),
            250,
            "the vendored AV1 vector is 250 temporal units"
        );

        let mut decoder = NativeVaapiDecoder::new(pf_vaadec::Codec::Av1, StreamFormat::SDR_420_8)
            .expect("this box is supposed to have a VAAPI AV1 decode entry point");
        eprintln!("VAAPI AV1 rung constructed: {}", decoder.name());

        let mut delivered = 0usize;
        let mut first: Option<(u32, u32, u32, u64)> = None;
        for (index, unit) in units.iter().enumerate() {
            match decoder.decode(unit) {
                Ok(Some(frame)) => {
                    assert!(
                        !frame.planes.is_empty(),
                        "unit {index}: a delivered frame exported no dmabuf planes"
                    );
                    if first.is_none() {
                        assert!(
                            frame.keyframe,
                            "the vector opens on a keyframe, so the first delivered \
                             frame must be flagged as one"
                        );
                        first = Some((frame.width, frame.height, frame.fourcc, frame.modifier));
                    }
                    delivered += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("unit {index}: VAAPI AV1 decode failed: {e:#}"),
            }
        }

        let (w, h, fourcc, modifier) = first.expect("not one frame came back");
        eprintln!(
            "VAAPI AV1: {delivered} frames delivered, first {w}x{h} \
             fourcc={:?} modifier={modifier:#x}",
            std::str::from_utf8(&fourcc.to_le_bytes()).unwrap_or("?")
        );
        assert_eq!((w, h), (320, 240), "the vector is 320x240");
        assert_eq!(
            delivered, 250,
            "the vector displays 250 frames (274 coded, 24 hidden)"
        );
    }

    // ---------------------------------------------------------------------
    // H.264 / H.265 — the two legs that had never decoded a frame anywhere
    // ---------------------------------------------------------------------

    /// The vendored H.264 vector: **250 access units** of 320x240 High 4:2:0 8-bit,
    /// TWO slice NALUs per picture (500 slice NALs over 250 AUs, 4 IDRs). The same
    /// file, at the same relative path, that `pf-vkdecode`'s Vulkan legs and
    /// `video_d3d11_native`'s D3D11VA leg decode — so a count that disagrees with
    /// theirs is this rung's problem rather than the vector's.
    const H264_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// The vendored H.265 twin: 250 access units, 320x240 Main 8-bit 4:2:0, ONE slice
    /// per picture, one `IDR_N_LP` then 249 TRAIL pictures.
    const H265_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );

    /// HEVC **Main 10**: 50 access units of 320x240 4:2:0 ten-bit, from libx265.
    ///
    /// Worth a third leg rather than a variation on the second because ten bits is a
    /// different VAAPI PROFILE (`VAProfileHEVCMain10`), a different render-target
    /// format (`VA_RT_FORMAT_YUV420_10`) and a different surface fourcc (**P010**, not
    /// NV12) — three branches of `Session::build` that no 8-bit leg reaches, on the
    /// path every HDR session takes. `finish` refuses a surface whose exported fourcc
    /// is not the one the pool was created with, so this leg is also the only thing
    /// that would catch a driver quietly handing back NV12 for a ten-bit stream.
    const MAIN10_H265: &[u8] = include_bytes!("../../pf-vkdecode/tests/data/test-main10.h265");

    /// Both 8-bit vectors are 250 access units.
    const H26X_AU_COUNT: usize = 250;
    /// The Main 10 vector is 50.
    const MAIN10_AU_COUNT: usize = 50;

    /// How many frames each vector can yield THROUGH THIS RUNG — which is not how many
    /// frames it contains, and the gap is a property of the rung worth stating once
    /// here rather than three times below.
    ///
    /// [`finish`] shows `outputs.last()` and never more: one frame per access unit, at
    /// most. So an access unit whose plan bumps SEVERAL pictures out of the DPB — which
    /// is what an IDR with `no_output_of_prior_pics_flag` clear does, and what ordinary
    /// B-pyramid reordering does at every other picture — displays the last of them and
    /// drops the rest, and an access unit whose plan outputs nothing yet displays
    /// nothing. There is no end-of-stream flush either, so whatever is still in the DPB
    /// when the vector ends never comes out.
    ///
    /// Measured on `.25` and reproduced exactly by
    /// [`the_planner_already_says_how_many_frames_these_legs_can_deliver`], which is
    /// what keeps these three numbers explanations rather than recordings:
    ///
    /// | vector | pictures the planner outputs | this rung delivers | dropped |
    /// |---|---|---|---|
    /// | H.264 | 243 (7 stranded in the DPB) | **225** | 18, at the 3 IDRs that drain the DPB |
    /// | H.265 | 249 (1 stranded) | **204** | 45, one on each of the 45 AUs that bump two |
    /// | Main 10 | 48 (2 stranded) | **45** | 3, likewise |
    ///
    /// ⚠ This does NOT bite punktfunk's own streams and is not what these legs exist to
    /// find: hosts emit zero-reorder low-delay output with no B pictures, so `outputs`
    /// never holds more than one picture and the rung is exact. It is the same
    /// divergence `video_d3d11_native`'s parity module records for the D3D11VA rung,
    /// and it is written down here for the same reason — a conformance vector reorders,
    /// punktfunk does not, and a reader comparing 225 against "250 frames" needs to
    /// know which of the two they are looking at.
    const H264_DELIVERED: usize = 225;
    const H265_DELIVERED: usize = 204;
    const MAIN10_DELIVERED: usize = 45;

    /// Byte offsets of every Annex-B NAL header in `stream`, in order.
    ///
    /// Emulation prevention guarantees `00 00 01` cannot appear inside a NAL payload,
    /// so scanning for it finds start codes and nothing else; the header begins on the
    /// byte after. Hand-rolled for the same reason [`split_ivf`] is — `pf-client-core`
    /// does not depend on the vendored parser crate — and a VERBATIM port of
    /// `video_d3d11_native`'s, so the two platform rungs are driven over the same
    /// access units rather than over two splitters free to disagree. Kept honest by
    /// the AU counts [`the_annex_b_splitters_still_cut_the_vendored_vectors`] asserts
    /// on every ordinary Linux test run, which no plausible splitter bug survives.
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
    /// after it. Load-bearing rather than decorative on this vector: it codes two
    /// slices per picture, so without the flag every picture would be split in two.
    fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = matches!(s[h] & 0x1f, 1 | 5);
            let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// H.265: TWO-byte NAL header, `nal_unit_type` in bits 1..7 of the first byte and
    /// "is a slice" the numeric range `< 32`, so `first_slice_segment_in_pic_flag` is
    /// the top bit of the byte at `+2` where H.264 reads `+1`. Getting either wrong
    /// silently merges or splits AUs, which surfaces as a frame-count mismatch a long
    /// way from its cause.
    fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = (s[h] >> 1) & 0x3f < 32;
            let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// The splitters cut both vendored vectors into the access units every other rung's
    /// legs count, and the Main 10 vector really is ten-bit.
    ///
    /// NOT `#[ignore]`d, unlike everything below it: the splitters are pure CPU, they
    /// are a hand-rolled copy of code that lives in two other crates, and a drift in
    /// them would reach the hardware legs as a frame-count mismatch on a box someone
    /// had to walk to. Ordinary `cargo test -p pf-client-core --lib` catches it here
    /// instead.
    ///
    /// The ten-bit check is the same guard `video_d3d11_native` carries and for the
    /// same reason: a regenerated vector that came out 8-bit would turn
    /// [`hevc_main10_decodes_the_ten_bit_vector_on_this_machines_vaapi`] into a second
    /// run of the 8-bit path wearing a ten-bit name, and it would pass.
    #[test]
    fn the_annex_b_splitters_still_cut_the_vendored_vectors() {
        assert_eq!(
            split_h264_aus(H264_25FPS).len(),
            H26X_AU_COUNT,
            "H.264 vector access units"
        );
        assert_eq!(
            split_h265_aus(H265_25FPS).len(),
            H26X_AU_COUNT,
            "H.265 vector access units"
        );

        let main10 = split_h265_aus(MAIN10_H265);
        assert_eq!(main10.len(), MAIN10_AU_COUNT, "Main 10 vector access units");

        let mut planner = pf_vaadec::H265Planner::new();
        let plan = planner
            .plan_au(main10[0])
            .expect("the Main 10 vector's first access unit must plan");
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8
            ),
            (1, 2),
            "the Main 10 vector must be 4:2:0 at ten bits"
        );
    }

    /// Access units whose plan outputs at least one picture — one delivered frame each,
    /// and the ONLY thing that separates [`H264_DELIVERED`] and friends from the
    /// vectors' frame counts.
    ///
    /// Two small walks rather than one generic one because the two planners share no
    /// trait: `AuPlan` and `AuPlanH265` are different types with the same `dpb.outputs`
    /// field, which is exactly the shape a macro would obscure for six saved lines.
    fn output_bearing_aus_h264(aus: &[&[u8]]) -> usize {
        let mut planner = pf_vaadec::H264Planner::new();
        aus.iter()
            .filter(|au| {
                !planner
                    .plan_au(au)
                    .expect("the vendored H.264 vector plans")
                    .dpb
                    .outputs
                    .is_empty()
            })
            .count()
    }

    /// [`output_bearing_aus_h264`] for HEVC. A skipped RASL picture counts as no
    /// output, which is what the rung does with it too ([`NativeVaapiDecoder::decode`]).
    fn output_bearing_aus_h265(aus: &[&[u8]]) -> usize {
        let mut planner = pf_vaadec::H265Planner::new();
        aus.iter()
            .filter(|au| match planner.plan_au(au) {
                Ok(plan) => !plan.dpb.outputs.is_empty(),
                Err(pf_vaadec::PlanErrorH265::RaslSkipped { .. }) => false,
                Err(e) => panic!("the vendored HEVC vector must plan: {e:?}"),
            })
            .count()
    }

    /// The three delivered-frame counts the hardware legs assert are what the PLANNER
    /// implies, not what a hardware run happened to print.
    ///
    /// This is the difference between a number that explains itself and a number
    /// somebody wrote down: it runs on any Linux box, with no GPU and no libva, and it
    /// fails the moment a vector is regenerated or the planner's bumping changes —
    /// which would otherwise show up as three mysterious hardware failures on a machine
    /// somebody had to walk to. See [`H264_DELIVERED`] for why the counts are below the
    /// vectors' frame counts at all.
    #[test]
    fn the_planner_already_says_how_many_frames_these_legs_can_deliver() {
        assert_eq!(
            output_bearing_aus_h264(&split_h264_aus(H264_25FPS)),
            H264_DELIVERED,
            "H.264: access units whose plan outputs a picture"
        );
        assert_eq!(
            output_bearing_aus_h265(&split_h265_aus(H265_25FPS)),
            H265_DELIVERED,
            "H.265: access units whose plan outputs a picture"
        );
        assert_eq!(
            output_bearing_aus_h265(&split_h265_aus(MAIN10_H265)),
            MAIN10_DELIVERED,
            "Main 10: access units whose plan outputs a picture"
        );
    }

    /// The first frame a leg got back — enough to say the rung exported a real surface
    /// of the right shape and pixel format, which is the most it can honestly claim.
    #[derive(Clone, Copy)]
    struct FirstFrame {
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        keyframe: bool,
    }

    /// Drive one Annex-B vector's access units through a freshly built rung and report
    /// how many frames came back and what the first one was.
    ///
    /// Shared by all three H.26x legs so that "the H.265 leg proves the same thing the
    /// H.264 leg does" is a fact about one function rather than a claim about three
    /// hand-copied ones — the same reasoning `pf-vkdecode`'s `common` module records
    /// for binding its three decoders to one driver.
    ///
    /// # What these legs prove, and what they do not
    ///
    /// Deliberately weaker than the Vulkan and D3D11VA H.26x legs, and the difference
    /// is worth stating rather than hiding behind a test name: those hash every decoded
    /// frame against libavcodec's goldens, because both can read their decoded surface
    /// back. This rung hands out a **DRM-PRIME dmabuf** whose memory the driver tiles,
    /// so there is no CPU-readable image to hash without adding a
    /// `vaDeriveImage`/`vaGetImage` path that production does not use and does not
    /// want. So this asserts what CAN be asserted honestly — that every access unit is
    /// accepted, that the expected number of frames comes back, and that each one is a
    /// real exported surface of the right shape and fourcc — and it is **NOT frame-hash
    /// parity**. It is what turns "never decoded a frame anywhere" into a measurement;
    /// promoting these legs to `verified` still wants parity, and parity wants a
    /// readback path that does not exist yet.
    ///
    /// Fails loudly rather than skipping when the device has no entry point for the
    /// profile: these legs are `#[ignore]`d, so they only run when someone deliberately
    /// points them at a box that is supposed to have one, and a silent pass there is
    /// the invisible-failure mode this whole program exists to end.
    fn run_annex_b(
        codec: pf_vaadec::Codec,
        stream: StreamFormat,
        aus: &[&[u8]],
        label: &str,
    ) -> (usize, FirstFrame) {
        let mut decoder = NativeVaapiDecoder::new(codec, stream).unwrap_or_else(|e| {
            panic!(
                "{label}: this box is supposed to have a VAAPI {label} decode entry point: {e:#}"
            )
        });
        eprintln!("VAAPI {label} rung constructed: {}", decoder.name());

        let mut delivered = 0usize;
        let mut first: Option<FirstFrame> = None;
        for (index, au) in aus.iter().enumerate() {
            match decoder.decode(au) {
                Ok(Some(frame)) => {
                    assert!(
                        !frame.planes.is_empty(),
                        "{label} AU {index}: a delivered frame exported no dmabuf planes"
                    );
                    if first.is_none() {
                        first = Some(FirstFrame {
                            width: frame.width,
                            height: frame.height,
                            fourcc: frame.fourcc,
                            modifier: frame.modifier,
                            keyframe: frame.keyframe,
                        });
                    }
                    delivered += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("{label} AU {index}: VAAPI decode failed: {e:#}"),
            }
        }

        let first = first.unwrap_or_else(|| panic!("{label}: not one frame came back"));
        let FirstFrame {
            width,
            height,
            fourcc,
            modifier,
            keyframe,
        } = first;
        eprintln!(
            "VAAPI {label}: {delivered} of {} access units delivered a frame, first \
             {width}x{height} fourcc={:?} modifier={modifier:#x} keyframe={keyframe}",
            aus.len(),
            std::str::from_utf8(&fourcc.to_le_bytes()).unwrap_or("?"),
        );
        (delivered, first)
    }

    /// Does this machine's VAAPI actually DECODE H.264 — the question the evidence
    /// table has answered "no hardware has ever tried" since M6.
    ///
    /// See [`run_annex_b`] for what this proves and, more importantly, what it does
    /// not: it is a decode measurement, not frame-hash parity.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn h264_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let aus = split_h264_aus(H264_25FPS);
        assert_eq!(aus.len(), H26X_AU_COUNT, "the H.264 vector is 250 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaadec::Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            "H.264",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaadec::VA_FOURCC_NV12,
            "an 8-bit pool exports NV12"
        );
        assert_eq!(
            delivered,
            output_bearing_aus_h264(&aus),
            "every access unit whose plan outputs a picture must deliver one"
        );
        assert_eq!(
            delivered, H264_DELIVERED,
            "see H264_DELIVERED for why this is 225 and not 250"
        );

        // ⚠ A DEFECT this leg found, asserted so that fixing it is noticed rather than
        // so that it is preserved. `finish` is handed the CURRENT access unit's
        // `is_idr`, not the flag of the picture it is about to display — and on a
        // reordering stream those are different pictures. The first frame delivered
        // here IS the IDR, bumped out several access units after it decoded, and it
        // arrives flagged `keyframe: false`; conversely the AU that drains the DPB at a
        // later IDR flags whichever OLD picture it displays as a keyframe. The flag is
        // `DecodedImage::is_keyframe`, the pump's post-loss re-anchor signal, so a rung
        // that mislabels it would keep asking for a keyframe it has already been sent.
        // It cannot bite punktfunk today for the same reason the frame count cannot:
        // hosts emit zero-reorder output, so the decoded picture and the displayed one
        // are always the same picture. Fix it and this line is the one to delete.
        assert!(
            !first.keyframe,
            "the rung labels the ACCESS UNIT, not the picture it delivers — if this \
             now passes the label was fixed, which is good; delete this assertion"
        );
    }

    /// The same question for H.265, whose leg has never decoded a frame either.
    ///
    /// Not a redundant copy of the H.264 leg: HEVC reaches an entirely different
    /// conversion in `pf-vaadec` (its own picture parameters, its own reference-picture
    /// set, its own slice header) and a different arm of [`NativeVaapiDecoder::decode`]
    /// — including the `RaslSkipped` Ok-skip no other codec has.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main VLD entry point"]
    fn h265_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let aus = split_h265_aus(H265_25FPS);
        assert_eq!(aus.len(), H26X_AU_COUNT, "the H.265 vector is 250 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaadec::Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            "H.265",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaadec::VA_FOURCC_NV12,
            "an 8-bit pool exports NV12"
        );
        assert_eq!(
            delivered,
            output_bearing_aus_h265(&aus),
            "every access unit whose plan outputs a picture must deliver one"
        );
        assert_eq!(
            delivered, H265_DELIVERED,
            "see H264_DELIVERED for why this is 204 and not 250"
        );
        assert!(!first.keyframe, "the same mislabel the H.264 leg documents");
    }

    /// And the ten-bit leg, which is the one every HDR session lands on.
    ///
    /// The fourcc assertion is the point of running it at all: `Session::build` picks
    /// P010 from the SPS's bit depth, and a pool that came out NV12 would be a ten-bit
    /// stream decoded into an 8-bit surface.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main 10 VLD entry point"]
    fn hevc_main10_decodes_the_ten_bit_vector_on_this_machines_vaapi() {
        let aus = split_h265_aus(MAIN10_H265);
        assert_eq!(aus.len(), MAIN10_AU_COUNT, "the Main 10 vector is 50 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaadec::Codec::H265,
            StreamFormat {
                bit_depth: 10,
                ..StreamFormat::SDR_420_8
            },
            &aus,
            "HEVC Main 10",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaadec::VA_FOURCC_P010,
            "a ten-bit stream must build a P010 pool, not an 8-bit one"
        );
        assert_eq!(
            delivered,
            output_bearing_aus_h265(&aus),
            "every access unit whose plan outputs a picture must deliver one"
        );
        assert_eq!(
            delivered, MAIN10_DELIVERED,
            "see H264_DELIVERED for why this is 45 and not 50"
        );
        assert!(!first.keyframe, "the same mislabel the H.264 leg documents");
    }
}
