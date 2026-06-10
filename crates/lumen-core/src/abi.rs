//! The stable `extern "C"` surface. `cbindgen` turns this module into
//! `include/lumen_core.h` (see `build.rs`).
//!
//! ## Principles (plan §5)
//! - Opaque handles only: C sees `LumenSession*`, never a Rust type's fields.
//! - All cross-boundary structs are `#[repr(C)]`; buffers are pointer + length.
//! - Explicit ownership: every handle from `*_new` / `*_pair` must be passed to
//!   [`lumen_session_free`]. A [`LumenFrame`]'s `data` is borrowed until the next
//!   `poll`/`free` on that session — copy it out before then.
//! - Versioned: [`lumen_abi_version`] + `LumenConfig::struct_size` for forward-compat.
//! - Panics never cross the boundary: every entry point is wrapped in `catch_unwind`.

use crate::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use crate::error::LumenStatus;
use crate::input::InputEvent;
use crate::session::Session;
use crate::stats::Stats;
use crate::transport::{loopback_pair, Transport, UdpTransport};
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::ptr;

/// Opaque session handle. Pointer-only from C.
pub struct LumenSession {
    inner: Session,
    /// Keeps the most recently polled frame alive so [`LumenFrame::data`] stays valid
    /// until the next poll or free.
    last_frame: Option<crate::session::Frame>,
    input_cb: Option<(LumenInputCb, *mut c_void)>,
}

/// Forward-compatible session configuration. The caller MUST set `struct_size` to
/// `sizeof(LumenConfig)`; the core uses it to detect ABI skew.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LumenConfig {
    pub struct_size: u32,
    /// 0 = host, 1 = client.
    pub role: u32,
    /// 1 = P1 (GameStream-compatible), 2 = P2 (`lumen/1`).
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

impl LumenConfig {
    fn to_config(self) -> Result<Config, LumenStatus> {
        let role = match self.role {
            0 => Role::Host,
            1 => Role::Client,
            _ => return Err(LumenStatus::InvalidArg),
        };
        let phase = match self.phase {
            1 => ProtocolPhase::P1GameStream,
            2 => ProtocolPhase::P2Lumen,
            _ => return Err(LumenStatus::InvalidArg),
        };
        // Range-check before narrowing: a `300` fec_percent or `65600` block size must be
        // rejected, not silently truncated to a valid-looking value.
        let scheme = u8::try_from(self.fec_scheme)
            .ok()
            .and_then(FecScheme::from_u8)
            .ok_or(LumenStatus::InvalidArg)?;
        let fec_percent = u8::try_from(self.fec_percent).map_err(|_| LumenStatus::InvalidArg)?;
        let max_data_per_block =
            u16::try_from(self.max_data_per_block).map_err(|_| LumenStatus::InvalidArg)?;
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

/// Read a `LumenConfig` from a caller pointer, enforcing the `struct_size` ABI-skew
/// guard *before* reading the whole struct: a caller compiled against a smaller (older)
/// layout is rejected rather than causing an out-of-bounds read.
///
/// # Safety
/// `cfg` must either be null or point to at least its own declared `struct_size` bytes.
unsafe fn config_from_ptr(cfg: *const LumenConfig) -> Result<Config, LumenStatus> {
    if cfg.is_null() {
        return Err(LumenStatus::NullPointer);
    }
    // Read only the 4-byte size prefix first to bound the subsequent full read.
    let declared = unsafe { std::ptr::addr_of!((*cfg).struct_size).read_unaligned() } as usize;
    if declared < std::mem::size_of::<LumenConfig>() {
        return Err(LumenStatus::InvalidArg);
    }
    unsafe { *cfg }.to_config()
}

/// A reassembled access unit. `data`/`len` borrow session-owned memory valid until the
/// next `lumen_client_poll_frame`/`lumen_session_free` on the same session.
#[repr(C)]
pub struct LumenFrame {
    pub data: *const u8,
    pub len: usize,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
}

/// Snapshot of session counters.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LumenStats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    pub fec_recovered_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl From<Stats> for LumenStats {
    fn from(s: Stats) -> Self {
        LumenStats {
            frames_submitted: s.frames_submitted,
            frames_completed: s.frames_completed,
            frames_dropped: s.frames_dropped,
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            packets_dropped: s.packets_dropped,
            fec_recovered_shards: s.fec_recovered_shards,
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
        }
    }
}

/// Host-side callback invoked for each input event drained by `lumen_host_poll_input`.
pub type LumenInputCb = extern "C" fn(event: *const InputEvent, user: *mut c_void);

#[inline]
fn guard<F: FnOnce() -> LumenStatus>(f: F) -> LumenStatus {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(LumenStatus::Panic)
}

fn new_handle(session: Session) -> *mut LumenSession {
    Box::into_raw(Box::new(LumenSession {
        inner: session,
        last_frame: None,
        input_cb: None,
    }))
}

/// Current ABI version. Mismatch with [`crate::ABI_VERSION`] means incompatible core.
#[no_mangle]
pub extern "C" fn lumen_abi_version() -> u32 {
    crate::ABI_VERSION
}

/// Create a session over a real UDP transport (`local`/`peer` are `host:port` strings).
/// Returns NULL on error.
///
/// # Safety
/// `cfg`, `local`, `peer` must be valid pointers; the strings must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn lumen_session_new(
    cfg: *const LumenConfig,
    local: *const c_char,
    peer: *const c_char,
) -> *mut LumenSession {
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
pub unsafe extern "C" fn lumen_test_loopback_pair(
    host_cfg: *const LumenConfig,
    client_cfg: *const LumenConfig,
    out_host: *mut *mut LumenSession,
    out_client: *mut *mut LumenSession,
) -> LumenStatus {
    guard(|| {
        if host_cfg.is_null() || client_cfg.is_null() || out_host.is_null() || out_client.is_null()
        {
            return LumenStatus::NullPointer;
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
        LumenStatus::Ok
    })
}

/// Free a session handle. Safe to call with NULL.
///
/// # Safety
/// `s` must be a handle from `lumen_session_new`/`lumen_test_loopback_pair`, freed once.
#[no_mangle]
pub unsafe extern "C" fn lumen_session_free(s: *mut LumenSession) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s) });
    }
}

/// Host: FEC-protect, packetize, seal and send one encoded access unit.
///
/// # Safety
/// `s` is a valid host handle; `data` points to `len` readable bytes (or `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn lumen_host_submit_frame(
    s: *mut LumenSession,
    data: *const u8,
    len: usize,
    pts_ns: u64,
    flags: u32,
) -> LumenStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer,
        };
        if data.is_null() && len != 0 {
            return LumenStatus::NullPointer;
        }
        let slice = if len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(data, len) }
        };
        match s.inner.submit_frame(slice, pts_ns, flags) {
            Ok(()) => LumenStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Client: poll for the next reassembled access unit. Returns [`LumenStatus::NoFrame`]
/// when nothing is ready yet. On `Ok`, `*out` borrows session memory until the next poll.
///
/// # Safety
/// `s` is a valid client handle; `out` points to a writable `LumenFrame`.
#[no_mangle]
pub unsafe extern "C" fn lumen_client_poll_frame(
    s: *mut LumenSession,
    out: *mut LumenFrame,
) -> LumenStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer,
        };
        if out.is_null() {
            return LumenStatus::NullPointer;
        }
        match s.inner.poll_frame() {
            Ok(frame) => {
                s.last_frame = Some(frame);
                let f = s.last_frame.as_ref().unwrap();
                unsafe {
                    *out = LumenFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                    };
                }
                LumenStatus::Ok
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
pub unsafe extern "C" fn lumen_send_input(
    s: *mut LumenSession,
    ev: *const InputEvent,
) -> LumenStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer,
        };
        let ev = match unsafe { ev.as_ref() } {
            Some(e) => e,
            None => return LumenStatus::NullPointer,
        };
        match s.inner.send_input(ev) {
            Ok(()) => LumenStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Register the host-side input callback (pass a NULL fn pointer to clear). The callback
/// fires from within [`lumen_host_poll_input`], on the calling thread.
///
/// # Safety
/// `s` is a valid host handle; `user` is passed back verbatim to `cb`.
#[no_mangle]
pub unsafe extern "C" fn lumen_set_input_callback(
    s: *mut LumenSession,
    // Written as an explicit `Option<fn>` (not the `LumenInputCb` alias) so cbindgen
    // emits a nullable C function pointer rather than an opaque wrapper struct.
    cb: Option<extern "C" fn(event: *const InputEvent, user: *mut c_void)>,
    user: *mut c_void,
) -> LumenStatus {
    guard(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer,
        };
        s.input_cb = cb.map(|c| (c, user));
        LumenStatus::Ok
    })
}

/// Host: drain all pending input events, invoking the registered callback for each.
/// Returns the count dispatched (≥ 0), or a negative [`LumenStatus`] on error.
///
/// # Safety
/// `s` is a valid host handle.
#[no_mangle]
pub unsafe extern "C" fn lumen_host_poll_input(s: *mut LumenSession) -> i32 {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer as i32,
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
    r.unwrap_or(LumenStatus::Panic as i32)
}

/// Copy session counters into `*out`.
///
/// # Safety
/// `s` is a valid handle; `out` points to a writable `LumenStats`.
#[no_mangle]
pub unsafe extern "C" fn lumen_get_stats(
    s: *mut LumenSession,
    out: *mut LumenStats,
) -> LumenStatus {
    guard(|| {
        let s = match unsafe { s.as_ref() } {
            Some(s) => s,
            None => return LumenStatus::NullPointer,
        };
        if out.is_null() {
            return LumenStatus::NullPointer;
        }
        let stats = s.inner.stats();
        unsafe { *out = LumenStats::from(stats) };
        LumenStatus::Ok
    })
}

// ---------------------------------------------------------------------------------------------
// lumen/1 connection API (`quic` feature) — the embeddable client connector platform clients
// link (SwiftUI/VideoToolbox, Android, …). In the generated header these are guarded by
// `LUMEN_FEATURE_QUIC`; define it when linking a lumen-core built with `--features quic`.
// ---------------------------------------------------------------------------------------------

/// Opaque handle to a live `lumen/1` connection (QUIC control plane + UDP data plane, all
/// pumped on internal threads).
#[cfg(feature = "quic")]
pub struct LumenConnection {
    inner: crate::client::NativeClient,
    /// Backs the pointer returned by the last `lumen_connection_next_au` (borrow-until-next-call).
    last: Option<crate::session::Frame>,
}

/// Connect to a `lumen/1` host and start a session at `width`x`height`@`refresh_hz`.
/// Blocks up to `timeout_ms` for the handshake. Returns NULL on failure.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string (IP or hostname resolvable by the platform).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn lumen_connect(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    timeout_ms: u32,
) -> *mut LumenConnection {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if host.is_null() {
            return std::ptr::null_mut();
        }
        let host = match unsafe { std::ffi::CStr::from_ptr(host) }.to_str() {
            Ok(s) => s,
            Err(_) => return std::ptr::null_mut(),
        };
        let mode = crate::config::Mode {
            width,
            height,
            refresh_hz,
        };
        match crate::client::NativeClient::connect(
            host,
            port,
            mode,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Ok(c) => Box::into_raw(Box::new(LumenConnection {
                inner: c,
                last: None,
            })),
            Err(_) => std::ptr::null_mut(),
        }
    }));
    r.unwrap_or(std::ptr::null_mut())
}

/// Pull the next reassembled access unit, waiting up to `timeout_ms`. Returns
/// [`LumenStatus::NoFrame`] on timeout and [`LumenStatus::Closed`] once the session ended.
/// On `Ok`, `*out` borrows connection memory **until the next call** on this handle.
///
/// # Safety
/// `c` is a valid connection handle used from a single thread; `out` is writable.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn lumen_connection_next_au(
    c: *mut LumenConnection,
    out: *mut LumenFrame,
    timeout_ms: u32,
) -> LumenStatus {
    guard(|| {
        let c = match unsafe { c.as_mut() } {
            Some(c) => c,
            None => return LumenStatus::NullPointer,
        };
        if out.is_null() {
            return LumenStatus::NullPointer;
        }
        match c
            .inner
            .next_frame(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(frame) => {
                c.last = Some(frame);
                let f = c.last.as_ref().unwrap();
                unsafe {
                    *out = LumenFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                    };
                }
                LumenStatus::Ok
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
pub unsafe extern "C" fn lumen_connection_send_input(
    c: *mut LumenConnection,
    ev: *const InputEvent,
) -> LumenStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return LumenStatus::NullPointer,
        };
        let ev = match unsafe { ev.as_ref() } {
            Some(e) => e,
            None => return LumenStatus::NullPointer,
        };
        match c.inner.send_input(ev) {
            Ok(()) => LumenStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// The host-confirmed session mode (from the Welcome). Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs are skipped).
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn lumen_connection_mode(
    c: *const LumenConnection,
    width: *mut u32,
    height: *mut u32,
    refresh_hz: *mut u32,
) -> LumenStatus {
    guard(|| {
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return LumenStatus::NullPointer,
        };
        unsafe {
            if !width.is_null() {
                *width = c.inner.mode.width;
            }
            if !height.is_null() {
                *height = c.inner.mode.height;
            }
            if !refresh_hz.is_null() {
                *refresh_hz = c.inner.mode.refresh_hz;
            }
        }
        LumenStatus::Ok
    })
}

/// Close the connection and free the handle (joins the internal threads). NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`lumen_connect`] and is not used after this call.
#[cfg(feature = "quic")]
#[no_mangle]
pub unsafe extern "C" fn lumen_connection_close(c: *mut LumenConnection) {
    if !c.is_null() {
        drop(unsafe { Box::from_raw(c) });
    }
}
