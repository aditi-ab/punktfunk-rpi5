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

/// Send a Wake-on-LAN magic packet to wake sleeping host NIC(s).
///
/// `macs` points to `mac_count` contiguous 6-byte MAC addresses (`mac_count * 6` bytes total) —
/// a host may report several NICs; all are woken. `last_known_ip`, if non-NULL, is an IPv4
/// dotted-quad string additionally targeted by unicast (pass NULL to skip). The packet is
/// broadcast to every local interface's subnet-directed broadcast and to `255.255.255.255` on
/// ports 9 and 7. This does NOT require an open connection and is not part of the QUIC surface.
///
/// Returns `Ok` if at least one datagram was sent. Call off the UI thread.
///
/// # Safety
/// `macs` must point to at least `mac_count * 6` readable bytes. `last_known_ip`, if non-NULL,
/// must be a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn punktfunk_wake_on_lan(
    macs: *const u8,
    mac_count: usize,
    last_known_ip: *const c_char,
) -> PunktfunkStatus {
    guard(|| {
        if macs.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if mac_count == 0 {
            return PunktfunkStatus::InvalidArg;
        }
        let bytes = unsafe { std::slice::from_raw_parts(macs, mac_count * 6) };
        let mac_vec: Vec<crate::wol::Mac> = bytes
            .chunks_exact(6)
            .map(|c| {
                let mut m = [0u8; 6];
                m.copy_from_slice(c);
                m
            })
            .collect();
        let ip = if last_known_ip.is_null() {
            None
        } else {
            match unsafe { CStr::from_ptr(last_known_ip) }
                .to_str()
                .ok()
                .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
            {
                Some(ip) => Some(ip),
                None => return PunktfunkStatus::InvalidArg,
            }
        };
        match crate::wol::send_magic_packet(&mac_vec, ip) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(_) => PunktfunkStatus::Io,
        }
    })
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
    /// Decode-in-core state for `punktfunk_connection_next_audio_pcm` (Apple / any embedder
    /// without a multistream Opus decoder). The decoder is built lazily from the negotiated
    /// `inner.audio_channels`; `pcm` is a fixed-capacity reusable buffer the returned pointer
    /// borrows until the next PCM call (same contract as `last_audio`).
    audio_pcm: std::sync::Mutex<AudioPcmState>,
}

/// Lazily-initialized in-core Opus decode state. A coupled-1-stream multistream decoder is
/// equivalent to a plain stereo decoder, so one [`opus::MSDecoder`] handles 2/6/8 channels.
#[cfg(feature = "quic")]
#[derive(Default)]
struct AudioPcmState {
    decoder: Option<opus::MSDecoder>,
    /// Interleaved f32 PCM, wire channel order. Pre-sized to the largest legal Opus frame
    /// (120 ms @ 48 kHz = 5760 samples/ch) × 8 channels so decode never reallocates (which would
    /// dangle the pointer handed to the embedder).
    pcm: Vec<f32>,
}

/// `PunktfunkHidOutput::kind` — lightbar RGB (`r`/`g`/`b` valid).
pub const PUNKTFUNK_HIDOUT_LED: u8 = 1;
/// `PunktfunkHidOutput::kind` — player-indicator LEDs (`player_bits` valid, low 5 bits).
pub const PUNKTFUNK_HIDOUT_PLAYER_LEDS: u8 = 2;
/// `PunktfunkHidOutput::kind` — one adaptive-trigger effect (`which` + `effect`/`effect_len` valid).
pub const PUNKTFUNK_HIDOUT_TRIGGER: u8 = 3;
/// `PunktfunkHidOutput::kind` — a trackpad haptic pulse (Steam Controller voice-coils). `which` =
/// side (0 = right pad, 1 = left pad); `effect[0..6]` packs `amplitude` / `period` / `count` as
/// little-endian `u16`s with `effect_len = 6`. Clients without trackpad coils drop it.
pub const PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC: u8 = 4;
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
            HidOutput::TrackpadHaptic {
                pad,
                side,
                amplitude,
                period,
                count,
            } => {
                // No new struct (PunktfunkHidOutput has no size guard): pack into the existing
                // `which` (side) + `effect[0..6]` (amplitude/period/count LE), `effect_len = 6`.
                out.kind = PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC;
                out.pad = *pad;
                out.which = *side;
                out.effect[0..2].copy_from_slice(&amplitude.to_le_bytes());
                out.effect[2..4].copy_from_slice(&period.to_le_bytes());
                out.effect[4..6].copy_from_slice(&count.to_le_bytes());
                out.effect_len = 6;
            }
        }
        out
    }
}

/// Static HDR metadata for an HDR session ([`punktfunk_connection_next_hdr_meta`]): SMPTE ST.2086
/// mastering display colour volume + CEA-861.3 content light level. All fields are in the standard
/// HDR10 SEI fixed-point units (primaries/white in 1/50000, luminance in 0.0001 cd/m²), ready for
/// DXGI `DXGI_HDR_METADATA_HDR10` / Apple `CAEDRMetadata` / Android `KEY_HDR_STATIC_INFO`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHdrMeta {
    /// Display-primaries x-chromaticities in 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_x: [u16; 3],
    /// Display-primaries y-chromaticities in 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_y: [u16; 3],
    /// White-point x-chromaticity, 1/50000 units.
    pub white_point_x: u16,
    /// White-point y-chromaticity, 1/50000 units.
    pub white_point_y: u16,
    /// Max display mastering luminance, 0.0001 cd/m² units.
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m² units.
    pub min_display_mastering_luminance: u32,
    /// Maximum content light level (MaxCLL), nits. 0 = unknown.
    pub max_cll: u16,
    /// Maximum frame-average light level (MaxFALL), nits. 0 = unknown.
    pub max_fall: u16,
}

#[cfg(feature = "quic")]
impl PunktfunkHdrMeta {
    fn from_meta(m: &crate::quic::HdrMeta) -> PunktfunkHdrMeta {
        PunktfunkHdrMeta {
            display_primaries_x: [
                m.display_primaries[0][0],
                m.display_primaries[1][0],
                m.display_primaries[2][0],
            ],
            display_primaries_y: [
                m.display_primaries[0][1],
                m.display_primaries[1][1],
                m.display_primaries[2][1],
            ],
            white_point_x: m.white_point[0],
            white_point_y: m.white_point[1],
            max_display_mastering_luminance: m.max_display_mastering_luminance,
            min_display_mastering_luminance: m.min_display_mastering_luminance,
            max_cll: m.max_cll,
            max_fall: m.max_fall,
        }
    }
}

/// One access unit's host-side processing time ([`punktfunk_connection_next_host_timing`]):
/// capture → fully sent, i.e. the whole host pipeline (capture read/convert, encode, FEC+seal,
/// paced send). Correlate to the AU whose `PunktfunkFrame::pts_ns` equals `pts_ns`, then
/// `network = (received_instant + clock_offset − pts_ns) − host_us` — the unified stats HUD's
/// `host` / `network` split (design/stats-unification.md Phase 2). Best-effort: a lost datagram
/// means that frame simply contributes no sample.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHostTiming {
    /// The AU's capture stamp (host capture clock — matches `PunktfunkFrame::pts_ns` exactly).
    pub pts_ns: u64,
    /// Host capture→sent duration, µs.
    pub host_us: u32,
}

/// `PunktfunkRichInput::kind` — a touchpad contact (`finger`/`active`/`x`/`y` valid).
pub const PUNKTFUNK_RICH_TOUCHPAD: u8 = 1;
/// `PunktfunkRichInput::kind` — a motion sample (`gyro`/`accel` valid).
pub const PUNKTFUNK_RICH_MOTION: u8 = 2;
/// `RichInput::TouchpadEx` kind on the wire — an extended trackpad contact that identifies the
/// surface (0 single / 1 Steam-left / 2 Steam-right) and carries click + pressure. The host decodes
/// it today; *sending* it from a C client needs the size-prefixed `PunktfunkRichInputEx` +
/// `punktfunk_connection_send_rich_input2` (added with client capture).
pub const PUNKTFUNK_RICH_TOUCHPAD_EX: u8 = 3;

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

/// Forward-compatible superset of [`PunktfunkRichInput`] that can also express the rich Steam
/// surfaces: a *second* trackpad (`surface`), a distinct `click` vs touch, signed coordinates, and
/// pressure. Sent via [`punktfunk_connection_send_rich_input2`] — the only way a C client can emit a
/// `TouchpadEx`. The caller MUST set `struct_size = sizeof(PunktfunkRichInputEx)` (the ABI-skew
/// guard, like [`PunktfunkConfig`]); the legacy [`PunktfunkRichInput`] +
/// [`punktfunk_connection_send_rich_input`] stay byte-for-byte for existing callers.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInputEx {
    /// MUST equal `sizeof(PunktfunkRichInputEx)`.
    pub struct_size: u32,
    /// One of `PUNKTFUNK_RICH_*` (`TOUCHPAD` / `MOTION` / `TOUCHPAD_EX`).
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad/TouchpadEx: contact id.
    pub finger: u8,
    /// Touchpad/TouchpadEx: 1 = finger down / touching, 0 = lifted.
    pub active: u8,
    /// TouchpadEx: which surface — 0 = single/DualSense, 1 = Steam left pad, 2 = Steam right pad.
    pub surface: u8,
    /// TouchpadEx: 1 = the pad is physically clicked (depressed), distinct from a touch contact.
    pub click: u8,
    /// Reserved for alignment; set to 0.
    pub _reserved: [u8; 2],
    /// TouchpadEx: x coordinate — **signed**, centred at 0 (the real Steam report convention). For a
    /// legacy `TOUCHPAD` kind sent through this struct, store the unsigned `0..=65535` value's bits.
    pub x: i16,
    /// TouchpadEx: y coordinate — signed, centred at 0.
    pub y: i16,
    /// TouchpadEx: contact pressure (`0` if the surface has no force sensor).
    pub pressure: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInputEx {
    fn to_rich(self) -> Option<crate::quic::RichInput> {
        use crate::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD_EX => Some(RichInput::TouchpadEx {
                pad: self.pad,
                surface: self.surface,
                finger: self.finger,
                touch: self.active != 0,
                click: self.click != 0,
                x: self.x,
                y: self.y,
                pressure: self.pressure,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x as u16,
                y: self.y as u16,
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
/// uinput X-Box One / Series pad — the X-Box 360 backend with the One/Series USB identity, so
/// games show One/Series glyphs. XInput-identical to `XBOX360` otherwise (no game-visible gain;
/// impulse-trigger rumble is unreachable through a virtual pad). Useful for glyph-matching a
/// physical X-Box One/Series controller on the client.
pub const PUNKTFUNK_GAMEPAD_XBOXONE: u32 = 3;
/// UHID DualShock 4 (kernel `hid-playstation` ≥ 6.2): lightbar, touchpad, motion, rumble — the
/// touchpad/motion arrive over the rich-input plane and lightbar over the HID-output plane, like
/// DualSense (minus adaptive triggers / player LEDs / mute). Honored only where available (Linux
/// hosts); otherwise the host falls back to X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSHOCK4: u32 = 4;
/// UHID classic Steam Controller (Valve `28DE:1102`, kernel `hid-steam`): dual trackpads, gyro,
/// two grip paddles. Reserved — currently folds to `XBOX360` until its backend lands.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER: u32 = 5;
/// UHID Steam Deck controller (Valve `28DE:1205`, kernel `hid-steam`): full Deck gamepad incl. the
/// four back grips, a right trackpad, and the IMU; re-grabbed by Steam Input with native glyphs when
/// Steam runs on the host. Honored only where available (Linux hosts); else folds to X-Box 360.
pub const PUNKTFUNK_GAMEPAD_STEAMDECK: u32 = 6;

/// Extended `InputEvent` gamepad button bits for embedders building raw events: the four back grips
/// (Steam L4/L5/R4/R5 ≙ Xbox-Elite P1–P4) + the misc/capture button, in Moonlight's
/// `buttonFlags2 << 16` namespace. Mirror `input::gamepad::BTN_PADDLE1..4` / `BTN_MISC1`.
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE1: u32 = 0x0001_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE2: u32 = 0x0002_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE3: u32 = 0x0004_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE4: u32 = 0x0008_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_MISC1: u32 = 0x0020_0000;

/// Connect to a `punktfunk/1` host and start a session at `width`x`height`@`refresh_hz`.
/// Blocks up to `timeout_ms` for the handshake. Returns NULL on failure. Equivalent to
/// [`punktfunk_connect_ex`] with `compositor = PUNKTFUNK_COMPOSITOR_AUTO`.
///
/// Video-capability bit for [`punktfunk_connect_ex5`] (`video_caps`): the client can decode a
/// 10-bit (Main10) HEVC stream. (Mirrors `quic::VIDEO_CAP_10BIT`.)
pub const PUNKTFUNK_VIDEO_CAP_10BIT: u8 = 0x01;
/// Video-capability bit for [`punktfunk_connect_ex5`] (`video_caps`): the client can present
/// BT.2020 PQ HDR10 (implies 10-bit). (Mirrors `quic::VIDEO_CAP_HDR`.)
pub const PUNKTFUNK_VIDEO_CAP_HDR: u8 = 0x02;
/// Video-capability bit for [`punktfunk_connect_ex5`] (`video_caps`): the client can decode a
/// full-chroma 4:4:4 HEVC stream (Range Extensions). The host emits 4:4:4 only when this is set,
/// the host opted in, the codec is HEVC, and the GPU supports it — else the stream stays 4:2:0 and
/// [`punktfunk_connection_chroma_format`] reports the real value. (Mirrors `quic::VIDEO_CAP_444`.)
pub const PUNKTFUNK_VIDEO_CAP_444: u8 = 0x04;

/// Codec bit for [`punktfunk_connect_ex7`] (`video_codecs` / `preferred_codec`) and the value
/// [`punktfunk_connection_codec`] returns: H.264 / AVC. (Mirrors `quic::CODEC_H264`.)
pub const PUNKTFUNK_CODEC_H264: u8 = 0x01;
/// Codec bit: H.265 / HEVC — the default codec. (Mirrors `quic::CODEC_HEVC`.)
pub const PUNKTFUNK_CODEC_HEVC: u8 = 0x02;
/// Codec bit: AV1. (Mirrors `quic::CODEC_AV1`.)
pub const PUNKTFUNK_CODEC_AV1: u8 = 0x04;

// Keep the ABI cap bits in lockstep with the wire constants (compile-time guard against drift).
#[cfg(feature = "quic")]
const _: () = {
    assert!(PUNKTFUNK_VIDEO_CAP_10BIT == crate::quic::VIDEO_CAP_10BIT);
    assert!(PUNKTFUNK_VIDEO_CAP_HDR == crate::quic::VIDEO_CAP_HDR);
    assert!(PUNKTFUNK_VIDEO_CAP_444 == crate::quic::VIDEO_CAP_444);
    assert!(PUNKTFUNK_CODEC_H264 == crate::quic::CODEC_H264);
    assert!(PUNKTFUNK_CODEC_HEVC == crate::quic::CODEC_HEVC);
    assert!(PUNKTFUNK_CODEC_AV1 == crate::quic::CODEC_AV1);
};

// Keep the ABI gamepad constants in lockstep with the wire enum (compile-time guard against drift).
const _: () = {
    use crate::config::GamepadPref;
    use crate::input::gamepad as g;
    assert!(PUNKTFUNK_GAMEPAD_AUTO == GamepadPref::Auto.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOX360 == GamepadPref::Xbox360.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSENSE == GamepadPref::DualSense.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOXONE == GamepadPref::XboxOne.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSHOCK4 == GamepadPref::DualShock4.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER == GamepadPref::SteamController.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMDECK == GamepadPref::SteamDeck.to_u8() as u32);
    // Extended button bits mirror the wire `input::gamepad` constants.
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE1 == g::BTN_PADDLE1);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE2 == g::BTN_PADDLE2);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE3 == g::BTN_PADDLE3);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE4 == g::BTN_PADDLE4);
    assert!(PUNKTFUNK_GAMEPAD_BTN_MISC1 == g::BTN_MISC1);
};

// The additive M3 kinds (TouchpadEx / TrackpadHaptic) must never grow the legacy ABI structs —
// they have no `struct_size` guard, so a layout change would corrupt old-built callers' buffers.
#[cfg(feature = "quic")]
const _: () = {
    assert!(core::mem::size_of::<PunktfunkRichInput>() == 20);
    assert!(core::mem::size_of::<PunktfunkHidOutput>() == 19);
};

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
    // Back-compat: ex4 advertises no video caps (8-bit BT.709 SDR). HDR-capable embedders call
    // `punktfunk_connect_ex5` with the cap bits.
    unsafe {
        punktfunk_connect_ex5(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            0,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex4`], but additionally advertises the embedder's video decode/present
/// capabilities as `video_caps` — a bitfield of `PUNKTFUNK_VIDEO_CAP_10BIT` (can decode 10-bit
/// Main10) and `PUNKTFUNK_VIDEO_CAP_HDR` (can present BT.2020 PQ HDR10). The host upgrades to a
/// 10-bit / HDR encode ONLY when the matching bit is set (and the host opted in); `0` keeps the
/// 8-bit BT.709 SDR stream. After connecting, read the resolved colour via
/// [`punktfunk_connection_color_info`] and drain the mastering metadata via
/// [`punktfunk_connection_next_hdr_meta`].
///
/// # Safety
/// Same as [`punktfunk_connect`]; `launch_id`, when non-NULL, must be a NUL-terminated C string.
#[cfg(feature = "quic")]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex5(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // Delegate to the surround-aware variant requesting stereo (the pre-surround behaviour).
    unsafe {
        punktfunk_connect_ex6(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            2, // audio_channels = stereo
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex5`], but additionally requests the audio channel count:
/// `2` (stereo, the default behaviour of every earlier variant), `6` (5.1) or `8` (7.1). The host
/// clamps the request to what it can actually capture and echoes the resolved count via
/// [`punktfunk_connection_audio_channels`]. Advertises HEVC-only with no codec preference (call
/// [`punktfunk_connect_ex7`] to negotiate the codec).
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex6(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    unsafe {
        punktfunk_connect_ex7(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            PUNKTFUNK_CODEC_HEVC, // pre-negotiation default: HEVC-only, no preference
            0,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// Like [`punktfunk_connect_ex6`], but additionally advertises the codecs the client can decode
/// (`video_codecs` — a bitfield of [`PUNKTFUNK_CODEC_H264`] / [`PUNKTFUNK_CODEC_HEVC`] /
/// [`PUNKTFUNK_CODEC_AV1`]) and a soft `preferred_codec` (a single codec bit, `0` = no preference).
/// The host resolves the codec it emits from these (preference honored when it can also produce it,
/// else best shared codec) and reports it via [`punktfunk_connection_codec`]. A client that omits
/// this (calls `ex6`) advertises HEVC-only, no preference — the pre-negotiation behaviour.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex7(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
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
            video_caps,
            crate::audio::normalize_channels(audio_channels),
            video_codecs,
            preferred_codec,
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
                    audio_pcm: std::sync::Mutex::new(AudioPcmState::default()),
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

/// Read the audio channel count the host resolved for this session (from its Welcome): `2`
/// (stereo), `6` (5.1) or `8` (7.1). `*out` is filled when non-NULL. The `0xC9` Opus frames are
/// (multistream-)encoded for this layout; an embedder decoding raw frames itself must build its
/// decoder from THIS value (see [`crate::audio::layout_for`]) — or use
/// [`punktfunk_connection_next_audio_pcm`], which decodes in-core. Available immediately after a
/// successful connect (it doesn't change without a reconfigure).
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_audio_channels(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.audio_channels };
        }
        PunktfunkStatus::Ok
    })
}

/// One decoded audio frame from [`punktfunk_connection_next_audio_pcm`]: interleaved 32-bit
/// float PCM at 48 kHz, in the canonical wire channel order `FL FR FC LFE RL RR SL SR` (the
/// first `channels` of it). `samples` points at `frame_count * channels` floats and borrows
/// connection memory **until the next PCM call** on this handle.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPcm {
    /// Interleaved f32 samples (wire channel order), `frame_count * channels` long.
    pub samples: *const f32,
    /// Samples per channel in this frame.
    pub frame_count: u32,
    /// Channel count (2/6/8) — the negotiated [`punktfunk_connection_audio_channels`].
    pub channels: u8,
    /// Source packet sequence number.
    pub seq: u32,
    /// Capture presentation timestamp (ns).
    pub pts_ns: u64,
}

/// Pull the next audio frame and **decode it in-core** to interleaved f32 PCM — for embedders
/// without a multistream-capable Opus decoder (e.g. Apple, whose AudioToolbox Opus path is
/// stereo-only). The decoder is built once from the negotiated channel count and handles 2/6/8
/// channels (a 1-coupled-stream multistream decoder is exactly a stereo decoder). Same
/// timeout/closed semantics as [`punktfunk_connection_next_audio`]; `out->samples` borrows
/// connection memory until the next PCM call on this handle. Use EITHER this or
/// [`punktfunk_connection_next_audio`] on a given connection, from one dedicated audio thread —
/// not both (they share the underlying queue).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls audio.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_audio_pcm(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPcm,
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
        let channels = crate::audio::normalize_channels(c.inner.audio_channels);
        let pkt = match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => pkt,
            Err(e) => return e.status(),
        };
        let mut state = c.audio_pcm.lock().unwrap();
        if state.decoder.is_none() {
            let layout = crate::audio::layout_for(channels, false);
            match opus::MSDecoder::new(48_000, layout.streams, layout.coupled, layout.mapping) {
                Ok(d) => {
                    // Largest legal Opus frame is 120 ms = 5760 samples/ch.
                    state.pcm = vec![0f32; 5760 * channels as usize];
                    state.decoder = Some(d);
                }
                Err(_) => return PunktfunkStatus::Unsupported,
            }
        }
        let AudioPcmState { decoder, pcm } = &mut *state;
        let dec = decoder.as_mut().unwrap();
        // `decode_float` divides the output buffer length by the channel count to get the
        // per-channel capacity; an empty payload requests packet-loss concealment.
        match dec.decode_float(&pkt.data, pcm, false) {
            Ok(frame_count) => {
                unsafe {
                    *out = PunktfunkAudioPcm {
                        samples: pcm.as_ptr(),
                        frame_count: frame_count as u32,
                        channels,
                        seq: pkt.seq,
                        pts_ns: pkt.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(_) => PunktfunkStatus::BadPacket,
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

/// Pull the next static HDR metadata update (ST.2086 mastering display + content light level) for
/// an HDR session, into `*out`. [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`]
/// once the session ended. The host sends one near session start and re-sends it on mastering
/// changes / keyframes; apply the latest to the display (`SetHDRMetaData` / `CAEDRMetadata` /
/// `KEY_HDR_STATIC_INFO`). Only an HDR session (`punktfunk_connection_color_info` reports a PQ
/// transfer) ever emits these. Same threading rules as [`punktfunk_connection_next_rumble`] (one
/// puller, may run alongside the other planes).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHdrMeta`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_hdr_meta(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHdrMeta,
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
            .next_hdr_meta(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(m) => {
                unsafe { *out = PunktfunkHdrMeta::from_meta(&m) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next per-AU host timing (0xCF) into `*out`: the host's capture→sent duration for one
/// access unit, correlated to the AU by `pts_ns` (see [`PunktfunkHostTiming`]).
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once the session ended.
/// A stats consumer drains this non-blockingly (`timeout_ms = 0`) alongside its frame samples;
/// an older host never emits any — keep showing the combined `host+network` stage then. Same
/// threading rules as [`punktfunk_connection_next_rumble`] (one puller, may run alongside the
/// other planes).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHostTiming`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_next_host_timing(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHostTiming,
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
            .next_host_timing(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(t) => {
                unsafe {
                    *out = PunktfunkHostTiming {
                        pts_ns: t.pts_ns,
                        host_us: t.host_us,
                    }
                };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Read the session's resolved colour signalling + encode bit depth (from the host's Welcome).
/// Each out pointer is filled when non-NULL: `primaries`/`transfer`/`matrix` are CICP code points
/// (BT.709 = 1; BT.2020 = 9; PQ transfer = 16, HLG = 18; BT.2020-NCL matrix = 9), `full_range` is
/// 0 (limited) or 1 (full), `bit_depth` is 8 or 10. A `transfer` of 16/18 means HDR — configure an
/// HDR present path and drain [`punktfunk_connection_next_hdr_meta`]. Available immediately after a
/// successful connect (these don't change without a reconfigure).
///
/// # Safety
/// `c` is a valid connection handle; each out pointer is NULL or writable for its scalar.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_color_info(
    c: *mut PunktfunkConnection,
    primaries: *mut u8,
    transfer: *mut u8,
    matrix: *mut u8,
    full_range: *mut u8,
    bit_depth: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let color = c.inner.color;
        unsafe {
            if !primaries.is_null() {
                *primaries = color.primaries;
            }
            if !transfer.is_null() {
                *transfer = color.transfer;
            }
            if !matrix.is_null() {
                *matrix = color.matrix;
            }
            if !full_range.is_null() {
                *full_range = color.full_range;
            }
            if !bit_depth.is_null() {
                *bit_depth = c.inner.bit_depth;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Read the session's resolved chroma subsampling (from the host's Welcome) as the HEVC
/// `chroma_format_idc`: `1` = 4:2:0 (the default every pre-4:4:4 host produced), `3` = full-chroma
/// 4:4:4. `*out` is filled when non-NULL. The in-band SPS is authoritative; this lets the embedder
/// pre-size its decoder / pick a 4:4:4 pixel format up front. Available immediately after a
/// successful connect (it doesn't change without a reconfigure).
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_chroma_format(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.chroma_format };
        }
        PunktfunkStatus::Ok
    })
}

/// Read the video codec the host resolved for this session (from its Welcome): one of
/// [`PUNKTFUNK_CODEC_H264`] / [`PUNKTFUNK_CODEC_HEVC`] / [`PUNKTFUNK_CODEC_AV1`]. The embedder builds
/// its decoder from THIS (never assuming HEVC). `*out` is filled when non-NULL. Available
/// immediately after a successful connect (it doesn't change without a reconfigure). An older host
/// that didn't negotiate a codec reports [`PUNKTFUNK_CODEC_HEVC`].
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_codec(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.codec };
        }
        PunktfunkStatus::Ok
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

/// Send a rich client→host input via the forward-compatible [`PunktfunkRichInputEx`] — the only way
/// a C client can emit a `TouchpadEx` (a second trackpad / signed coords / pressure). Set
/// `rich->struct_size = sizeof(PunktfunkRichInputEx)`; a smaller (older-layout) value is rejected.
///
/// # Safety
/// `c` is a valid connection handle; `rich` is null or points to at least its declared
/// `struct_size` bytes.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_send_rich_input2(
    c: *mut PunktfunkConnection,
    rich: *const PunktfunkRichInputEx,
) -> PunktfunkStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if rich.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // Read only the 4-byte size prefix first to bound the subsequent full read (the
        // `PunktfunkConfig` ABI-skew precedent).
        let declared = unsafe { std::ptr::addr_of!((*rich).struct_size).read_unaligned() } as usize;
        if declared < std::mem::size_of::<PunktfunkRichInputEx>() {
            return PunktfunkStatus::InvalidArg;
        }
        match unsafe { *rich }.to_rich() {
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
/// delivered wire throughput to drive a bitrate choice from; `loss_pct` is the link loss and
/// `host_drop_pct` the host-side send-buffer drop (raise `net.core.wmem_max`) — they're measured
/// separately so a host that can't keep up reads differently from a lossy link.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PunktfunkProbeResult {
    /// 1 once the host's end-of-burst report arrived (measurement final); else 0 (partial).
    pub done: u8,
    /// Delivered wire bytes (header + shard) / packets the client received during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// The host's measured burst duration, milliseconds (the throughput denominator).
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kilobits/second).
    pub throughput_kbps: u32,
    /// Link loss `(wire_packets_sent − recv_packets) / wire_packets_sent` as a percentage.
    pub loss_pct: f32,
    /// Host-side send-buffer drop `send_dropped / (wire_packets_sent + send_dropped)`, percent.
    pub host_drop_pct: f32,
    /// Wire packets the host put on the link, and the ones its send buffer dropped (raw counts).
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
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
                host_drop_pct: o.host_drop_pct,
                wire_packets_sent: o.wire_packets_sent,
                send_dropped: o.send_dropped,
            };
        }
        PunktfunkStatus::Ok
    })
}

/// Signal a **deliberate quit** (a user "stop", not a network drop) before closing: the connection
/// closes with [`QUIT_CLOSE_CODE`] instead of code 0, so the host tears the session down immediately
/// (skips the keep-alive linger) rather than holding it for a reconnect. Call this right before
/// [`punktfunk_connection_close`] on a user-initiated disconnect; a plain close (network drop,
/// backgrounding) leaves the linger intact. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and remains valid (closed via `punktfunk_connection_close`).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn punktfunk_connection_disconnect_quit(c: *mut PunktfunkConnection) {
    if let Some(c) = unsafe { c.as_ref() } {
        c.inner.disconnect_quit();
    }
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
