//! The stable `extern "C"` surface. `cbindgen` turns this module into
//! `include/punktfunk_core.h` (see `build.rs`).
//!
//! ## Principles (plan §5)
//! - Opaque handles only: C sees `PunktfunkSession*`, never a Rust type's fields.
//! - All cross-boundary structs are `#[repr(C)]`; buffers are pointer + length.
//! - Explicit ownership: every handle from `*_new` / `*_pair` must be passed to
//!   [`punktfunk_session_free`]. A [`PunktfunkFrame`]'s `data` is borrowed until the next
//!   `poll`/`free` on that session — copy it out before then.
//! - Versioned: [`punktfunk_abi_version`] + `PunktfunkConfig::struct_size` for forward-compat.
//! - Panics never cross the boundary: every entry point is wrapped in `catch_unwind`.

use crate::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use crate::error::PunktfunkStatus;
use crate::input::InputEvent;
use crate::session::Session;
use crate::stats::Stats;
use crate::transport::{loopback_pair, Transport, UdpTransport};
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::ptr;

/// Opaque session handle. Pointer-only from C.
pub struct PunktfunkSession {
    inner: Session,
    /// Keeps the most recently polled frame alive so [`PunktfunkFrame::data`] stays valid
    /// until the next poll or free.
    last_frame: Option<crate::session::Frame>,
    input_cb: Option<(PunktfunkInputCb, *mut c_void)>,
}

/// Forward-compatible session configuration. The caller MUST set `struct_size` to
/// `sizeof(PunktfunkConfig)`; the core uses it to detect ABI skew.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkConfig {
    pub struct_size: u32,
    /// 0 = host, 1 = client.
    pub role: u32,
    /// 1 = P1 (GameStream-compatible), 2 = P2 (`punktfunk/1`).
    pub phase: u32,
    /// 0 = GF(2⁸), 1 = GF(2¹⁶).
    pub fec_scheme: u32,
    pub fec_percent: u32,
    pub max_data_per_block: u32,
    pub shard_payload: u32,
    /// Non-zero enables AES-128-GCM.
    pub encrypt: u32,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Test hook for the loopback transport; 0 in production.
    pub loopback_drop_period: u32,
    /// Largest encoded access unit the receiver will accept (bounds reassembler memory).
    pub max_frame_bytes: u64,
}

impl PunktfunkConfig {
    fn to_config(self) -> Result<Config, PunktfunkStatus> {
        let role = match self.role {
            0 => Role::Host,
            1 => Role::Client,
            _ => return Err(PunktfunkStatus::InvalidArg),
        };
        let phase = match self.phase {
            1 => ProtocolPhase::P1GameStream,
            2 => ProtocolPhase::P2Punktfunk,
            _ => return Err(PunktfunkStatus::InvalidArg),
        };
        // Range-check before narrowing: a `300` fec_percent or `65600` block size must be
        // rejected, not silently truncated to a valid-looking value.
        let scheme = u8::try_from(self.fec_scheme)
            .ok()
            .and_then(FecScheme::from_u8)
            .ok_or(PunktfunkStatus::InvalidArg)?;
        let fec_percent =
            u8::try_from(self.fec_percent).map_err(|_| PunktfunkStatus::InvalidArg)?;
        let max_data_per_block =
            u16::try_from(self.max_data_per_block).map_err(|_| PunktfunkStatus::InvalidArg)?;
        let cfg = Config {
            role,
            phase,
            fec: FecConfig {
                scheme,
                fec_percent,
                max_data_per_block,
            },
            shard_payload: self.shard_payload as usize,
            max_frame_bytes: self.max_frame_bytes as usize,
            encrypt: self.encrypt != 0,
            key: self.key,
            salt: self.salt,
            loopback_drop_period: self.loopback_drop_period,
        };
        cfg.validate().map_err(|e| e.status())?;
        Ok(cfg)
    }
}

/// Read a `PunktfunkConfig` from a caller pointer, enforcing the `struct_size` ABI-skew
/// guard *before* reading the whole struct: a caller compiled against a smaller (older)
/// layout is rejected rather than causing an out-of-bounds read.
///
/// # Safety
/// `cfg` must either be null or point to at least its own declared `struct_size` bytes.
unsafe fn config_from_ptr(cfg: *const PunktfunkConfig) -> Result<Config, PunktfunkStatus> {
    if cfg.is_null() {
        return Err(PunktfunkStatus::NullPointer);
    }
    // Read only the 4-byte size prefix first to bound the subsequent full read.
    let declared = unsafe { std::ptr::addr_of!((*cfg).struct_size).read_unaligned() } as usize;
    if declared < std::mem::size_of::<PunktfunkConfig>() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    unsafe { *cfg }.to_config()
}

/// A reassembled access unit. `data`/`len` borrow session-owned memory valid until the
/// next `punktfunk_client_poll_frame`/`punktfunk_session_free` on the same session.
#[repr(C)]
pub struct PunktfunkFrame {
    pub data: *const u8,
    pub len: usize,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
}

/// Snapshot of session counters.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkStats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    /// Packets dropped on the host send path because the kernel buffer was full (WouldBlock) — the
    /// dominant loss mode at very high bitrate; distinct from `packets_dropped` (recv-side).
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl From<Stats> for PunktfunkStats {
    fn from(s: Stats) -> Self {
        PunktfunkStats {
            frames_submitted: s.frames_submitted,
            frames_completed: s.frames_completed,
            frames_dropped: s.frames_dropped,
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            packets_dropped: s.packets_dropped,
            packets_send_dropped: s.packets_send_dropped,
            fec_recovered_shards: s.fec_recovered_shards,
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
        }
    }
}

/// Host-side callback invoked for each input event drained by `punktfunk_host_poll_input`.
pub type PunktfunkInputCb = extern "C" fn(event: *const InputEvent, user: *mut c_void);

#[inline]
fn guard<F: FnOnce() -> PunktfunkStatus>(f: F) -> PunktfunkStatus {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(PunktfunkStatus::Panic)
}

fn new_handle(session: Session) -> *mut PunktfunkSession {
    Box::into_raw(Box::new(PunktfunkSession {
        inner: session,
        last_frame: None,
        input_cb: None,
    }))
}

/// Current ABI version. Mismatch with [`crate::ABI_VERSION`] means incompatible core.
#[no_mangle]
pub extern "C" fn punktfunk_abi_version() -> u32 {
    crate::ABI_VERSION
}

/// Create a session over a real UDP transport (`local`/`peer` are `host:port` strings).
/// Returns NULL on error.
///
/// # Safety
/// `cfg`, `local`, `peer` must be valid pointers; the strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_session_new(
    cfg: *const PunktfunkConfig,
    local: *const c_char,
    peer: *const c_char,
) -> *mut PunktfunkSession {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if cfg.is_null() || local.is_null() || peer.is_null() {
            return ptr::null_mut();
        }
        let config = match unsafe { config_from_ptr(cfg) } {
            Ok(c) => c,
            Err(_) => return ptr::null_mut(),
        };
        let local = match unsafe { CStr::from_ptr(local) }.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        let peer = match unsafe { CStr::from_ptr(peer) }.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        let transport: Box<dyn Transport> = match UdpTransport::connect(local, peer) {
            Ok(t) => Box::new(t),
            Err(_) => return ptr::null_mut(),
        };
        match Session::new(config, transport) {
            Ok(s) => new_handle(s),
            Err(_) => ptr::null_mut(),
        }
    }));
    result.unwrap_or(ptr::null_mut())
}

/// Create a connected host+client session pair sharing an in-process loopback
/// transport. Test/dev only — exercises the full FEC + framing path without a network.
///
/// # Safety
/// All four pointers must be valid; the two out-params receive owned handles.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_test_loopback_pair(
    host_cfg: *const PunktfunkConfig,
    client_cfg: *const PunktfunkConfig,
    out_host: *mut *mut PunktfunkSession,
    out_client: *mut *mut PunktfunkSession,
) -> PunktfunkStatus {
    guard(|| {
        if host_cfg.is_null() || client_cfg.is_null() || out_host.is_null() || out_client.is_null()
        {
            return PunktfunkStatus::NullPointer;
        }
        let hconf = match unsafe { config_from_ptr(host_cfg) } {
            Ok(c) => c,
            Err(s) => return s,
        };
        let cconf = match unsafe { config_from_ptr(client_cfg) } {
            Ok(c) => c,
            Err(s) => return s,
        };
        let (ht, ct) = loopback_pair(hconf.loopback_drop_period, cconf.loopback_drop_period);
        let hs = match Session::new(hconf, Box::new(ht)) {
            Ok(s) => s,
            Err(e) => return e.status(),
        };
        let cs = match Session::new(cconf, Box::new(ct)) {
            Ok(s) => s,
            Err(e) => return e.status(),
        };
        unsafe {
            *out_host = new_handle(hs);
            *out_client = new_handle(cs);
        }
        PunktfunkStatus::Ok
    })
}

/// Free a session handle. Safe to call with NULL.
///
/// # Safety
/// `s` must be a handle from `punktfunk_session_new`/`punktfunk_test_loopback_pair`, freed once.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_session_free(s: *mut PunktfunkSession) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s) });
    }
}

/// Host: FEC-protect, packetize, seal and send one encoded access unit.
///
/// # Safety
/// `s` is a valid host handle; `data` points to `len` readable bytes (or `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn punktfunk_host_submit_frame(
    s: *mut PunktfunkSession,
    data: *const u8,
    len: usize,
    pts_ns: u64,
    flags: u32,
) -> PunktfunkStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if data.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        let slice = if len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(data, len) }
        };
        match s.inner.submit_frame(slice, pts_ns, flags) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Client: poll for the next reassembled access unit. Returns [`PunktfunkStatus::NoFrame`]
/// when nothing is ready yet. On `Ok`, `*out` borrows session memory until the next poll.
///
/// # Safety
/// `s` is a valid client handle; `out` points to a writable `PunktfunkFrame`.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_client_poll_frame(
    s: *mut PunktfunkSession,
    out: *mut PunktfunkFrame,
) -> PunktfunkStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match s.inner.poll_frame() {
            Ok(frame) => {
                s.last_frame = Some(frame);
                let f = s.last_frame.as_ref().unwrap();
                unsafe {
                    *out = PunktfunkFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Client: serialize and send one input event to the host.
///
/// # Safety
/// `s` is a valid client handle; `ev` points to a valid [`InputEvent`].
#[no_mangle]
pub unsafe extern "C" fn punktfunk_send_input(
    s: *mut PunktfunkSession,
    ev: *const InputEvent,
) -> PunktfunkStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        let ev = match unsafe { ev.as_ref() } {
            Some(e) => e,
            None => return PunktfunkStatus::NullPointer,
        };
        match s.inner.send_input(ev) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Register the host-side input callback (pass a NULL fn pointer to clear). The callback
/// fires from within [`punktfunk_host_poll_input`], on the calling thread.
///
/// # Safety
/// `s` is a valid host handle; `user` is passed back verbatim to `cb`.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_set_input_callback(
    s: *mut PunktfunkSession,
    // Written as an explicit `Option<fn>` (not the `PunktfunkInputCb` alias) so cbindgen
    // emits a nullable C function pointer rather than an opaque wrapper struct.
    cb: Option<extern "C" fn(event: *const InputEvent, user: *mut c_void)>,
    user: *mut c_void,
) -> PunktfunkStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        s.input_cb = cb.map(|c| (c, user));
        PunktfunkStatus::Ok
    })
}

/// Host: drain all pending input events, invoking the registered callback for each.
/// Returns the count dispatched (≥ 0), or a negative [`PunktfunkStatus`] on error.
///
/// # Safety
/// `s` is a valid host handle.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_host_poll_input(s: *mut PunktfunkSession) -> i32 {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer as i32,
        };
        let cb = s.input_cb;
        let mut count = 0i32;
        loop {
            match s.inner.poll_input() {
                Ok(Some(ev)) => {
                    if let Some((cb, user)) = cb {
                        cb(&ev as *const InputEvent, user);
                    }
                    count += 1;
                }
                Ok(None) => break,
                Err(e) => return e.status() as i32,
            }
        }
        count
    }));
    r.unwrap_or(PunktfunkStatus::Panic as i32)
}

/// Copy session counters into `*out`.
///
/// # Safety
/// `s` is a valid handle; `out` points to a writable `PunktfunkStats`.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_get_stats(
    s: *mut PunktfunkSession,
    out: *mut PunktfunkStats,
) -> PunktfunkStatus {
    guard(|| {
        let s = match unsafe { s.as_ref() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let stats = s.inner.stats();
        unsafe { *out = PunktfunkStats::from(stats) };
        PunktfunkStatus::Ok
    })
}

// ---------------------------------------------------------------------------------------------
// punktfunk/1 connection API (`quic` feature) — the embeddable client connector platform clients
// link (SwiftUI/VideoToolbox, Android, …). In the generated header these are guarded by
// `PUNKTFUNK_FEATURE_QUIC`; define it when linking a punktfunk-core built with `--features quic`.
// ---------------------------------------------------------------------------------------------

/// Opaque handle to a live `punktfunk/1` connection (QUIC control plane + UDP data plane, all
/// pumped on internal threads).
///
/// Thread contract: each plane (video `next_au`, audio `next_audio`, rumble `next_rumble`)
/// may be pulled from its own thread, at most one thread per plane. The accessors only
/// take shared references internally (per-plane mutexed borrow slots), so cross-plane
/// concurrency is sound — never two threads on the *same* plane.
#[cfg(feature = "quic")]
pub struct PunktfunkConnection {
    inner: crate::client::NativeClient,
    /// Backs the pointer returned by the last `punktfunk_connection_next_au` (borrow-until-next-call).
    last: std::sync::Mutex<Option<crate::session::Frame>>,
    /// Same, for `punktfunk_connection_next_audio` (independent of the video slot).
    last_audio: std::sync::Mutex<Option<crate::client::AudioPacket>>,
}

/// `PunktfunkHidOutput::kind` — lightbar RGB (`r`/`g`/`b` valid).
pub const PUNKTFUNK_HIDOUT_LED: u8 = 1;
/// `PunktfunkHidOutput::kind` — player-indicator LEDs (`player_bits` valid, low 5 bits).
pub const PUNKTFUNK_HIDOUT_PLAYER_LEDS: u8 = 2;
/// `PunktfunkHidOutput::kind` — one adaptive-trigger effect (`which` + `effect`/`effect_len` valid).
pub const PUNKTFUNK_HIDOUT_TRIGGER: u8 = 3;
/// Capacity of `PunktfunkHidOutput::effect` (the DualSense trigger parameter block).
pub const PUNKTFUNK_HID_EFFECT_MAX: u8 = 11;

/// One DualSense HID-output feedback event a game wrote to the host's virtual pad
/// ([`punktfunk_connection_next_hidout`]). `kind` selects which fields are meaningful — replay it
/// on a real DualSense (lightbar color, player LEDs, or an adaptive-trigger effect via the
/// platform's `GCDualSenseAdaptiveTrigger`-style API).
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHidOutput {
    /// One of `PUNKTFUNK_HIDOUT_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// LED: lightbar red.
    pub r: u8,
    /// LED: lightbar green.
    pub g: u8,
    /// LED: lightbar blue.
    pub b: u8,
    /// PlayerLeds: lit player indicators (low 5 bits).
    pub player_bits: u8,
    /// Trigger: 0 = L2, 1 = R2.
    pub which: u8,
    /// Trigger: number of valid bytes in `effect` (≤ `PUNKTFUNK_HID_EFFECT_MAX`).
    pub effect_len: u8,
    /// Trigger: the raw DualSense trigger parameter block (mode + params).
    pub effect: [u8; 11],
}

#[cfg(feature = "quic")]
impl PunktfunkHidOutput {
    fn from_hid(h: &crate::quic::HidOutput) -> PunktfunkHidOutput {
        use crate::quic::HidOutput;
        let mut out = PunktfunkHidOutput {
            kind: 0,
            pad: 0,
            r: 0,
            g: 0,
            b: 0,
            player_bits: 0,
            which: 0,
            effect_len: 0,
            effect: [0u8; 11],
        };
        match h {
            HidOutput::Led { pad, r, g, b } => {
                out.kind = PUNKTFUNK_HIDOUT_LED;
                out.pad = *pad;
                out.r = *r;
                out.g = *g;
                out.b = *b;
            }
            HidOutput::PlayerLeds { pad, bits } => {
                out.kind = PUNKTFUNK_HIDOUT_PLAYER_LEDS;
                out.pad = *pad;
                out.player_bits = *bits;
            }
            HidOutput::Trigger { pad, which, effect } => {
                out.kind = PUNKTFUNK_HIDOUT_TRIGGER;
                out.pad = *pad;
                out.which = *which;
                let n = effect.len().min(out.effect.len());
                out.effect[..n].copy_from_slice(&effect[..n]);
                out.effect_len = n as u8;
            }
        }
        out
    }
}

/// `PunktfunkRichInput::kind` — a touchpad contact (`finger`/`active`/`x`/`y` valid).
pub const PUNKTFUNK_RICH_TOUCHPAD: u8 = 1;
/// `PunktfunkRichInput::kind` — a motion sample (`gyro`/`accel` valid).
pub const PUNKTFUNK_RICH_MOTION: u8 = 2;

/// One rich client→host input for the host's virtual DualSense
/// ([`punktfunk_connection_send_rich_input`]): a touchpad contact or a motion sample. Set `kind`
/// and the matching fields; the others are ignored.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInput {
    /// One of `PUNKTFUNK_RICH_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad: contact id (0 or 1).
    pub finger: u8,
    /// Touchpad: 1 = finger down, 0 = lifted.
    pub active: u8,
    /// Touchpad: normalized x, 0..=65535 across the touchpad.
    pub x: u16,
    /// Touchpad: normalized y, 0..=65535 across the touchpad.
    pub y: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInput {
    fn to_rich(self) -> Option<crate::quic::RichInput> {
        use crate::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x,
                y: self.y,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            _ => None,
        }
    }
}

/// Read an optional NUL-terminated UTF-8 string parameter; `Err` = invalid pointer/UTF-8.
#[cfg(feature = "quic")]
unsafe fn opt_cstr<'a>(p: *const std::os::raw::c_char) -> std::result::Result<Option<&'a str>, ()> {
    if p.is_null() {
        return Ok(None);
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .map(Some)
        .map_err(|_| ())
}

/// Compositor preference for [`punktfunk_connect_ex`] (`compositor` arg). `AUTO` lets the host
/// pick (auto-detect from its running desktop); a concrete value is honored only if that backend
/// is available on the host right now, else the host falls back to auto-detect. The resolved
/// choice is reported back over the protocol (see `punktfunk/1` `Welcome`).
pub const PUNKTFUNK_COMPOSITOR_AUTO: u32 = 0;
/// KWin / KDE Plasma.
pub const PUNKTFUNK_COMPOSITOR_KWIN: u32 = 1;
/// wlroots (Sway / Hyprland).
pub const PUNKTFUNK_COMPOSITOR_WLROOTS: u32 = 2;
/// Mutter / GNOME.
pub const PUNKTFUNK_COMPOSITOR_MUTTER: u32 = 3;
/// gamescope (spawned nested).
pub const PUNKTFUNK_COMPOSITOR_GAMESCOPE: u32 = 4;

/// Gamepad-backend preference for [`punktfunk_connect_ex2`] (`gamepad` arg): which virtual pad
/// the host creates for this session's controllers. Precedence host-side: an explicit client
/// choice > the host's `PUNKTFUNK_GAMEPAD` env var > X-Box 360. `AUTO` (or any unrecognized
/// value) = host decides. The resolved choice is echoed over the protocol (`Welcome`) and
/// readable via [`punktfunk_connection_gamepad`].
pub const PUNKTFUNK_GAMEPAD_AUTO: u32 = 0;
/// uinput X-Box 360 pad (the universal default — every game speaks XInput).
pub const PUNKTFUNK_GAMEPAD_XBOX360: u32 = 1;
/// UHID DualSense (kernel `hid-playstation`): adaptive triggers, lightbar, touchpad, motion —
/// feedback arrives on the HID-output plane ([`punktfunk_connection_next_hidout`]). Honored
/// only where available (Linux hosts); otherwise the host falls back to X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSENSE: u32 = 2;

/// Connect to a `punktfunk/1` host and start a session at `width`x`height`@`refresh_hz`.
/// Blocks up to `timeout_ms` for the handshake. Returns NULL on failure. Equivalent to
/// [`punktfunk_connect_ex`] with `compositor = PUNKTFUNK_COMPOSITOR_AUTO`.
///
/// Trust: `pin_sha256` (NULL or 32 bytes) is the expected SHA-256 fingerprint of the host's
/// certificate — a mismatching host is rejected. NULL = trust on first use; persist the
/// fingerprint written to `observed_sha256_out` (NULL or 32 bytes, filled on success) and
/// pass it as the pin on every later connect.
///
/// Identity: `client_cert_pem`/`client_key_pem` (both NULL, or both NUL-terminated PEM
/// strings — see [`punktfunk_generate_identity`]) are presented via TLS client auth so a
/// host can recognize this client once paired ([`punktfunk_pair`]). NULL = anonymous;
/// hosts running `--require-pairing` reject anonymous sessions.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string (IP or hostname resolvable by the platform);
/// `pin_sha256`/`observed_sha256_out` are each NULL or valid for 32 bytes;
/// `client_cert_pem`/`client_key_pem` are each NULL or NUL-terminated UTF-8.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connect(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    unsafe {
        punktfunk_connect_ex(
            host,
            port,
            width,
            height,
            refresh_hz,
            PUNKTFUNK_COMPOSITOR_AUTO,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect`], but requests a specific `compositor` backend on the host (one of
/// the `PUNKTFUNK_COMPOSITOR_*` values). `PUNKTFUNK_COMPOSITOR_AUTO` (or any unrecognized value)
/// lets the host decide; a concrete value is honored only if available, else the host falls back
/// to auto-detect. The resolved choice is logged host-side and returned over the protocol.
/// Equivalent to [`punktfunk_connect_ex2`] with `gamepad = PUNKTFUNK_GAMEPAD_AUTO`.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connect_ex(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    unsafe {
        punktfunk_connect_ex2(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            PUNKTFUNK_GAMEPAD_AUTO,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex`], but additionally requests which virtual `gamepad` backend the
/// host creates for this session's pads (one of the `PUNKTFUNK_GAMEPAD_*` values).
/// `PUNKTFUNK_GAMEPAD_AUTO` (or any unrecognized value) lets the host decide (its
/// `PUNKTFUNK_GAMEPAD` env var, else X-Box 360); a concrete value is honored only if that
/// backend is available on the host. The resolved choice is readable via
/// [`punktfunk_connection_gamepad`] — only a DualSense session emits HID-output feedback.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connect_ex2(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    unsafe {
        punktfunk_connect_ex3(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            0, // bitrate_kbps = 0: let the host pick its default
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex2`], but additionally requests the video encoder `bitrate_kbps`
/// (kilobits per second). `0` lets the host pick its default; any other value is clamped to the
/// host's supported range. After a speed test ([`punktfunk_connection_speed_test`]) a client can
/// reconnect (or pick at connect time) with the measured rate. The value the host actually
/// configured is readable via [`punktfunk_connection_bitrate`].
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connect_ex3(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // Delegate to the launch-aware variant with no game requested (the host's default session).
    unsafe {
        punktfunk_connect_ex4(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            std::ptr::null(),
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex3`], but additionally asks the host to launch a library title in
/// this session. `launch_id` is a store-qualified [`crate::library`-style] id as returned by the
/// host's `GET /api/v1/library` (`steam:<appid>` / `custom:<id>`); the host resolves it against
/// its OWN library and runs the matching recipe — the client never sends a raw command. `NULL`
/// (or an empty / unknown id) ⇒ the host's default session, no game launched.
///
/// # Safety
/// Same as [`punktfunk_connect`]; `launch_id`, when non-NULL, must be a NUL-terminated C string.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connect_ex4(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if host.is_null() {
            return std::ptr::null_mut();
        }
        let host = match unsafe { std::ffi::CStr::from_ptr(host) }.to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        // A bad-UTF-8 launch id is non-fatal — treat it as "no game" rather than failing connect.
        let launch = match unsafe { opt_cstr(launch_id) } {
            Ok(Some(s)) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        };
        let mode = crate::config::Mode {
            width,
            height,
            refresh_hz,
        };
        // "Any unrecognized value = Auto" must hold for the FULL u32 domain — `as u8`
        // would wrap 0x101 into a concrete choice before from_u8's fallback could apply.
        let pref = u8::try_from(compositor)
            .map(crate::config::CompositorPref::from_u8)
            .unwrap_or_default();
        let gamepad = u8::try_from(gamepad)
            .map(crate::config::GamepadPref::from_u8)
            .unwrap_or_default();
        let pin = if pin_sha256.is_null() {
            None
        } else {
            let mut p = [0u8; 32];
            p.copy_from_slice(unsafe { std::slice::from_raw_parts(pin_sha256, 32) });
            Some(p)
        };
        let identity = match (unsafe { opt_cstr(client_cert_pem) }, unsafe {
            opt_cstr(client_key_pem)
        }) {
            (Ok(Some(c)), Ok(Some(k))) => Some((c.to_string(), k.to_string())),
            (Ok(None), Ok(None)) => None,
            _ => return std::ptr::null_mut(), // half an identity / bad UTF-8: fail closed
        };
        match crate::client::NativeClient::connect(
            host,
            port,
            mode,
            pref,
            gamepad,
            bitrate_kbps,
            // 8-bit only over the C ABI for now — the ABI doesn't yet carry the embedder's video
            // caps (Apple/Android decode 8-bit). The native Windows client advertises 10-bit/HDR.
            0,
            launch,
            pin,
            identity,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Ok(c) => {
                if !observed_sha256_out.is_null() {
                    unsafe {
                        std::slice::from_raw_parts_mut(observed_sha256_out, 32)
                            .copy_from_slice(&c.host_fingerprint);
                    }
                }
                Box::into_raw(Box::new(PunktfunkConnection {
                    inner: c,
                    last: std::sync::Mutex::new(None),
                    last_audio: std::sync::Mutex::new(None),
                }))
            }
            Err(_) => std::ptr::null_mut(),
        }
    }));
    r.unwrap_or(std::ptr::null_mut())
}

/// Generate a persistent client identity: a self-signed certificate + private key, both
/// PEM, NUL-terminated, written into the caller's buffers. Generate ONCE, store both
/// strings (Keychain etc.), pass them to [`punktfunk_pair`] and every
/// [`punktfunk_connect`] — the certificate's fingerprint is how hosts recognize this
/// client. 4096-byte buffers are ample.
///
/// # Safety
/// `cert_pem_out` is writable for `cert_cap` bytes; `key_pem_out` for `key_cap`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_generate_identity(
    cert_pem_out: *mut std::os::raw::c_char,
    cert_cap: usize,
    key_pem_out: *mut std::os::raw::c_char,
    key_cap: usize,
) -> PunktfunkStatus {
    guard(|| {
        if cert_pem_out.is_null() || key_pem_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let (cert, key) = match crate::quic::endpoint::generate_identity() {
            Ok(t) => t,
            Err(_) => return PunktfunkStatus::Io,
        };
        if cert.len() + 1 > cert_cap || key.len() + 1 > key_cap {
            return PunktfunkStatus::InvalidArg;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(cert.as_ptr(), cert_pem_out as *mut u8, cert.len());
            *cert_pem_out.add(cert.len()) = 0;
            std::ptr::copy_nonoverlapping(key.as_ptr(), key_pem_out as *mut u8, key.len());
            *key_pem_out.add(key.len()) = 0;
        }
        PunktfunkStatus::Ok
    })
}

/// Run the PIN pairing ceremony against a host (see the protocol docs in punktfunk-core):
/// the host displays a short PIN; the user types it into the client app, which passes it
/// here. On success the host has stored this client's identity, the now-verified host
/// fingerprint is written to `host_sha256_out` (32 bytes) — persist it and pass it as
/// `pin_sha256` to [`punktfunk_connect`] from then on. Returns
/// [`PunktfunkStatus::Crypto`] for a wrong PIN.
///
/// # Safety
/// `host`/`client_cert_pem`/`client_key_pem`/`pin`/`name` are NUL-terminated UTF-8;
/// `host_sha256_out` is writable for 32 bytes.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_pair(
    host: *const std::os::raw::c_char,
    port: u16,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    pin: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
    host_sha256_out: *mut u8,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let (Ok(Some(host)), Ok(Some(cert)), Ok(Some(key)), Ok(Some(pin)), Ok(Some(name))) = (
            unsafe { opt_cstr(host) },
            unsafe { opt_cstr(client_cert_pem) },
            unsafe { opt_cstr(client_key_pem) },
            unsafe { opt_cstr(pin) },
            unsafe { opt_cstr(name) },
        ) else {
            return PunktfunkStatus::NullPointer;
        };
        if host_sha256_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match crate::client::NativeClient::pair(
            host,
            port,
            (cert, key),
            pin,
            name,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Ok(fp) => {
                unsafe {
                    std::slice::from_raw_parts_mut(host_sha256_out, 32).copy_from_slice(&fp);
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next reassembled access unit, waiting up to `timeout_ms`. Returns
/// [`PunktfunkStatus::NoFrame`] on timeout and [`PunktfunkStatus::Closed`] once the session ended.
/// On `Ok`, `*out` borrows connection memory **until the next `next_au` call** on this
/// handle (the audio/rumble planes do not invalidate it).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls video —
/// it may run concurrently with one audio-pulling and one rumble-pulling thread.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_au(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkFrame,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // Shared reference only: video and audio threads must never alias a `&mut`.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_frame(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(frame) => {
                let mut slot = c.last.lock().unwrap();
                *slot = Some(frame);
                let f = slot.as_ref().unwrap();
                unsafe {
                    *out = PunktfunkFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// One Opus audio packet pulled off a `punktfunk/1` connection (48 kHz stereo, 5 ms frames).
/// `data` borrows connection memory until the next `punktfunk_connection_next_audio` call.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPacket {
    pub data: *const u8,
    pub len: usize,
    pub seq: u32,
    pub pts_ns: u64,
}

/// Pull the next Opus audio packet, waiting up to `timeout_ms`. Returns
/// [`PunktfunkStatus::NoFrame`] on timeout and [`PunktfunkStatus::Closed`] once the session ended.
/// On `Ok`, `out->data` borrows connection memory **until the next audio call** on this
/// handle (independent of the video slot). Drain from a dedicated audio thread — packets
/// arrive every 5 ms and the internal queue holds 320 ms.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls audio —
/// it may run concurrently with the video/rumble pullers.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_audio(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPacket,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => {
                let mut slot = c.last_audio.lock().unwrap();
                *slot = Some(pkt);
                let p = slot.as_ref().unwrap();
                unsafe {
                    *out = PunktfunkAudioPacket {
                        data: p.data.as_ptr(),
                        len: p.data.len(),
                        seq: p.seq,
                        pts_ns: p.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next rumble (force-feedback) update, waiting up to `timeout_ms`. Amplitudes
/// are 0..0xFFFF (`low` = low-frequency motor, `high` = high-frequency), `(0, 0)` = stop.
/// Same timeout/closed semantics as [`punktfunk_connection_next_audio`].
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs are skipped). At
/// most one thread pulls rumble — it may run concurrently with the video/audio pullers.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_rumble(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c
            .inner
            .next_rumble(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok((p, l, h)) => {
                unsafe {
                    if !pad.is_null() {
                        *pad = p;
                    }
                    if !low.is_null() {
                        *low = l;
                    }
                    if !high.is_null() {
                        *high = h;
                    }
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next DualSense HID-output feedback event (lightbar / player LEDs / adaptive trigger)
/// the host's virtual pad received from a game, into `*out`. [`PunktfunkStatus::NoFrame`] on
/// timeout, [`PunktfunkStatus::Closed`] once the session ended. Only the DualSense host backend
/// emits these. Same threading rules as [`punktfunk_connection_next_rumble`] (one puller, may run
/// alongside the other planes).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHidOutput`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_hidout(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHidOutput,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_hidout(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(h) => {
                unsafe { *out = PunktfunkHidOutput::from_hid(&h) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Send one input event to the host as a QUIC datagram (non-blocking enqueue).
///
/// # Safety
/// `c` is a valid connection handle; `ev` points to a valid [`InputEvent`].
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_send_input(
    c: *mut PunktfunkConnection,
    ev: *const InputEvent,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let ev = match unsafe { ev.as_ref() } {
            Some(e) => e,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.send_input(ev) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Send one Opus mic frame to the host as a QUIC datagram (48 kHz; the host decodes it into a
/// virtual microphone source its apps can record). Non-blocking enqueue; the host uses `seq`/
/// `pts_ns` (the caller's own counters) only for diagnostics. `opus_data`/`len` may be empty
/// (a DTX silence frame). The data is copied; the caller may reuse the buffer after this returns.
///
/// # Safety
/// `c` is a valid connection handle; `opus_data` is valid for `len` bytes (or `len == 0`).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_send_mic(
    c: *mut PunktfunkConnection,
    opus_data: *const u8,
    len: usize,
    seq: u32,
    pts_ns: u64,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if opus_data.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        let opus = if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(opus_data, len) }.to_vec()
        };
        match c.inner.send_mic(seq, pts_ns, opus) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Send one rich input event (DualSense touchpad contact or motion sample) to the host as a QUIC
/// datagram (non-blocking enqueue). The host applies it to its virtual DualSense pad — a no-op
/// unless the host runs the DualSense gamepad backend. [`PunktfunkStatus::InvalidArg`] on an
/// unknown `kind`.
///
/// # Safety
/// `c` is a valid connection handle; `rich` points to a valid [`PunktfunkRichInput`].
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_send_rich_input(
    c: *mut PunktfunkConnection,
    rich: *const PunktfunkRichInput,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let rich = match unsafe { rich.as_ref() } {
            Some(r) => r,
            None => return PunktfunkStatus::NullPointer,
        };
        match rich.to_rich() {
            Some(r) => match c.inner.send_rich_input(r) {
                Ok(()) => PunktfunkStatus::Ok,
                Err(e) => e.status(),
            },
            None => PunktfunkStatus::InvalidArg,
        }
    })
}

/// The currently active session mode — the Welcome's, until an accepted
/// [`punktfunk_connection_request_mode`] switches it. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs are skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_mode(
    c: *const PunktfunkConnection,
    width: *mut u32,
    height: *mut u32,
    refresh_hz: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let mode = c.inner.mode();
        unsafe {
            if !width.is_null() {
                *width = mode.width;
            }
            if !height.is_null() {
                *height = mode.height;
            }
            if !refresh_hz.is_null() {
                *refresh_hz = mode.refresh_hz;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// The virtual gamepad backend the host actually resolved for this session (one of the
/// `PUNKTFUNK_GAMEPAD_*` values; the `Welcome`'s echo of the [`punktfunk_connect_ex2`]
/// preference). `PUNKTFUNK_GAMEPAD_AUTO` = an older host that didn't say — assume X-Box 360,
/// no HID-output feedback. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `gamepad` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_gamepad(
    c: *const PunktfunkConnection,
    gamepad: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        unsafe {
            if !gamepad.is_null() {
                *gamepad = c.inner.resolved_gamepad.to_u8() as u32;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// The compositor backend the host actually resolved for this session (one of the
/// `PUNKTFUNK_COMPOSITOR_*` values; the `Welcome`'s echo of the [`punktfunk_connect_ex`]
/// preference). `PUNKTFUNK_COMPOSITOR_AUTO` = an older host that didn't say. Clients use it for
/// compositor-specific behavior — e.g. a client-side cursor by default on
/// `PUNKTFUNK_COMPOSITOR_GAMESCOPE`, whose PipeWire capture carries no cursor. Safe any time after
/// connect.
///
/// # Safety
/// `c` is a valid connection handle; `compositor` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_compositor(
    c: *const PunktfunkConnection,
    compositor: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        unsafe {
            if !compositor.is_null() {
                *compositor = c.inner.resolved_compositor.to_u8() as u32;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// The video encoder bitrate (kilobits per second) the host actually configured for this session
/// — the [`punktfunk_connect_ex3`] request clamped to the host's range, or its default when `0`
/// was requested. `0` = an older host that didn't report it. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `bitrate_kbps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_bitrate(
    c: *const PunktfunkConnection,
    bitrate_kbps: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        unsafe {
            if !bitrate_kbps.is_null() {
                *bitrate_kbps = c.inner.resolved_bitrate_kbps;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// The host↔client wall-clock offset (nanoseconds, **host minus client**) measured by the
/// connect-time skew handshake. Add it to a local receive/present timestamp (same realtime clock,
/// `CLOCK_REALTIME` / `gettimeofday`-epoch nanoseconds) to express that instant in the host's
/// capture clock — the clock the per-access-unit `pts_ns` is stamped in — so glass-to-glass latency
/// (e.g. present-time minus `pts_ns`) is valid across machines. `0` = no correction: either an older
/// host that didn't answer the handshake, or genuinely synchronized clocks. Safe any time after
/// connect.
///
/// # Safety
/// `c` is a valid connection handle; `offset_ns` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_clock_offset_ns(
    c: *const PunktfunkConnection,
    offset_ns: *mut i64,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        unsafe {
            if !offset_ns.is_null() {
                *offset_ns = c.inner.clock_offset_ns;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Ask the host to switch the live session to `width`x`height`@`refresh_hz` without
/// reconnecting (window resized, refresh changed). Non-blocking enqueue: on acceptance the
/// stream continues at the new mode — the first new-mode access unit is an IDR with
/// in-band parameter sets (rebuild the decoder from it) — and
/// [`punktfunk_connection_mode`] reflects the switch. A rejected request leaves the
/// session unchanged.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_request_mode(
    c: *const PunktfunkConnection,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_mode(crate::config::Mode {
            width,
            height,
            refresh_hz,
        }) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Ask the host's encoder to emit a fresh IDR keyframe now — client recovery when the
/// decoder has stalled (the infinite-GOP stream sends one opening IDR then P-frames only, so
/// a wedged decoder would otherwise freeze until the next loss-triggered recovery keyframe).
/// Non-blocking, fire-and-forget; the recovered keyframe is the only ack. The caller should
/// THROTTLE — the decode stays wedged for several frames until the IDR lands, so requesting
/// every frame would flood the control stream.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_request_keyframe(
    c: *const PunktfunkConnection,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_keyframe() {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Cumulative access units the host→client reassembler dropped as unrecoverable (FEC couldn't
/// rebuild them). A video loop polls this and calls [`punktfunk_connection_request_keyframe`]
/// when it climbs — the correct loss trigger under the host's infinite GOP, where unrecoverable
/// loss yields reference-missing delta frames the decoder *silently conceals* (frozen / garbage
/// picture, no decode error), so a decode-error trigger rarely fires. Monotonic for the session;
/// compare against the last observed value. Writes 0 to `out` on a NULL connection.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_frames_dropped(
    c: *const PunktfunkConnection,
    out: *mut u64,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        unsafe {
            if !out.is_null() {
                *out = c.inner.frames_dropped();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// A speed-test measurement, filled by [`punktfunk_connection_probe_result`]. `done` is 0 until
/// the host's end-of-burst report lands, then 1 (the numbers are final). `throughput_kbps` is the
/// measured goodput to drive a bitrate choice from; `loss_pct` is the delivery loss at that rate.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PunktfunkProbeResult {
    /// 1 once the host's end-of-burst report arrived (measurement final); else 0 (partial).
    pub done: u8,
    /// Probe payload bytes / packets the client received.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Probe payload bytes / packets the host reported sending.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// Client-measured receive window (first→last probe AU), milliseconds.
    pub elapsed_ms: u32,
    /// Measured goodput = `recv_bytes * 8 / elapsed_ms` (kilobits/second).
    pub throughput_kbps: u32,
    /// Delivery loss `(host_bytes - recv_bytes) / host_bytes` as a percentage (0 if unknown).
    pub loss_pct: f32,
}

/// Start a bandwidth speed test: ask the host to burst filler over the data plane at
/// `target_kbps` of goodput for `duration_ms` (each clamped host-side to ≤ 3 Gbps / ≤ 5 s),
/// *briefly pausing video*. Non-blocking — poll [`punktfunk_connection_probe_result`] until its
/// `done` field is 1. Starting a probe resets any prior measurement.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_speed_test(
    c: *const PunktfunkConnection,
    target_kbps: u32,
    duration_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_probe(target_kbps, duration_ms) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Read the current speed-test measurement into `*out` (partial until `out->done == 1`). Safe to
/// poll repeatedly after [`punktfunk_connection_speed_test`]; before any probe it reports zeros.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkProbeResult` (NULL is an
/// error).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_probe_result(
    c: *const PunktfunkConnection,
    out: *mut PunktfunkProbeResult,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let o = c.inner.probe_result();
        unsafe {
            *out = PunktfunkProbeResult {
                done: o.done as u8,
                recv_bytes: o.recv_bytes,
                recv_packets: o.recv_packets,
                host_bytes: o.host_bytes,
                host_packets: o.host_packets,
                elapsed_ms: o.elapsed_ms,
                throughput_kbps: o.throughput_kbps,
                loss_pct: o.loss_pct,
            };
        }
        PunktfunkStatus::Ok
    })
}

/// Close the connection and free the handle (joins the internal threads). NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and is not used after this call.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_close(c: *mut PunktfunkConnection) {
    if !c.is_null() {
        drop(unsafe { Box::from_raw(c) });
    }
}
