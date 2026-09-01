//! Session lifecycle and media-plane JNI wiring.
//!
//! Kotlin receives an opaque integer key, never a Rust pointer. A process-local table stores
//! `Arc<SessionHandle>` values so close-vs-call races retain the session until each active JNI call
//! finishes; duplicate or stale closes become no-ops.
//!
//! [`connect`] owns identity, trust, connect, and close. [`planes`] owns video/audio/mic lifecycles,
//! [`input`] forwards control events, and [`probe`] runs bandwidth measurements. Decode and audio
//! workers share the `Sync` connector while the table controls the outer session lifetime.

mod access;
mod clipboard;
mod connect;
mod input;
mod planes;
mod probe;

use punktfunk_core::client::NativeClient;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Run a JNI body, catching any panic at the FFI boundary and returning `default` instead.
///
/// A panic unwinding out of an `extern "system"` function aborts the whole process on Rust ≥ 1.81 —
/// a hard crash of the embedding Android app with no logcat trace. This mirrors the discipline the C
/// ABI already enforces (`punktfunk_core::abi` wraps every entry point in `catch_unwind`); the
/// `panic = "unwind"` profile in the workspace `Cargo.toml` exists precisely so these guards work.
/// We apply it to the teardown + background-thread shims (the "leaving a stream" path), where an
/// unexpected panic (e.g. a poisoned `Mutex` during concurrent teardown) must degrade to a logged
/// no-op rather than kill the app.
pub(crate) fn jni_guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log::error!("punktfunk JNI: caught a panic at the FFI boundary (returning default)");
        default
    })
}

/// Poison-recovering lock for the JNI entry points that are NOT behind [`jni_guard`]: a
/// `.lock().unwrap()` there turns a poisoned mutex into a panic across the `extern "system"`
/// boundary — an abort of the whole app on Rust ≥ 1.81 (the panic-in-extern grep gate's class).
/// The slots behind these mutexes are plane-thread handles and last-value caches; whatever a
/// poisoned writer left is still valid to inspect or replace.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One table-owned live session: its connector, media workers, and session-scoped state.
pub(crate) struct SessionHandle {
    // Read only by the android decode path (`nativeStartVideo` → `crate::decode`); on the host
    // build (CI's workspace clippy/build) those readers are cfg'd out, so it's intentionally unused.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub client: Arc<NativeClient>,
    /// Live decode stats, written by the decode thread and drained ~1 Hz by `nativeVideoStats`.
    /// Session-lifetime (not per `VideoThread`) so the HUD's enable gate set via
    /// `nativeSetVideoStatsEnabled` survives surface teardown/recreate and can land before
    /// `nativeStartVideo` — enabling resets the window, so no stale data leaks across restarts.
    pub stats: Arc<crate::stats::VideoStats>,
    video: Mutex<Option<VideoThread>>,
    #[cfg(target_os = "android")]
    audio: Mutex<Option<crate::audio::AudioPlayback>>,
    #[cfg(target_os = "android")]
    mic: Mutex<Option<crate::mic::MicCapture>>,
    /// Tier-A DualSense pad audio (the 0xD1 plane), started by `nativeStartPadAudio` once Kotlin
    /// has claimed the pad's audio interface and handed its descriptor over. Session-lifetime and
    /// `Option` because a session may have no wired DualSense at all, which is the common case.
    #[cfg(target_os = "android")]
    pub(crate) pad_audio: Mutex<Option<crate::pad_audio::PadAudio>>,
    /// In-stream mic mute, set via `nativeSetMicMuted` and read per 10 ms frame by the mic's
    /// encode loop ([`crate::mic`]). Session-lifetime rather than per-[`crate::mic::MicCapture`]
    /// for the same reason the stats gate is: the mic stops and restarts across a surface
    /// recreate, and a mute the user set must come back with it — with no window in which the
    /// fresh capture could send an unmuted frame. Per session and never persisted: a new session
    /// starts unmuted.
    pub mic_muted: Arc<AtomicBool>,
    /// Count of `AccessUpdate`s drained from the connector's event plane, bumped by the
    /// `nativeAccessState` poll ([`access`]) — how the Kotlin poller tells a fresh update
    /// (the host's expiry warnings) arrived without holding a blocking event thread.
    pub(crate) access_seq: AtomicU32,
    /// The video `SurfaceView`'s LIVE on-screen pixel size ([`pack_surface_size`]), written by
    /// `nativeStartVideo` and by every `nativeVideoSurfaceSize` the `surfaceChanged` callback
    /// sends, read by the ASurfaceControl presenter before each present.
    ///
    /// Shared and live rather than a start-time parameter because the view RESIZES under a surface
    /// that is never recreated: hiding the system bars and switching the window to
    /// `LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS` both happen a frame or two AFTER `surfaceCreated`,
    /// and each one grows the video view. A destination rect captured once at creation then keeps
    /// compositing the picture at its old, smaller size anchored at the layer's origin — the
    /// "stream in the top-left corner" field report. `0` = nothing reported yet, and the layer
    /// falls back to the window's buffer geometry.
    pub surface_size: Arc<AtomicU64>,
}

static NEXT_SESSION_HANDLE: AtomicU64 = AtomicU64::new(0x1000_0000_0000_0001);

fn session_handles() -> &'static Mutex<HashMap<i64, Arc<SessionHandle>>> {
    static HANDLES: OnceLock<Mutex<HashMap<i64, Arc<SessionHandle>>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn insert_session(session: SessionHandle) -> i64 {
    let session = Arc::new(session);
    let mut handles = lock_recover(session_handles());
    loop {
        let handle = NEXT_SESSION_HANDLE.fetch_add(1, Ordering::Relaxed) as i64;
        if handle != 0 && !handles.contains_key(&handle) {
            handles.insert(handle, session);
            return handle;
        }
    }
}

pub(crate) fn get_session(handle: i64) -> Option<Arc<SessionHandle>> {
    if handle == 0 {
        return None;
    }
    lock_recover(session_handles()).get(&handle).cloned()
}

pub(crate) fn remove_session(handle: i64) -> Option<Arc<SessionHandle>> {
    if handle == 0 {
        return None;
    }
    lock_recover(session_handles()).remove(&handle)
}

/// Pack a surface's pixel size into one `u64` — so the presenter reads width and height as a
/// single atomic load and can never see a torn pair (a new width against an old height).
/// Non-positive values pack as `0`, the "not reported yet" sentinel.
pub(crate) fn pack_surface_size(w: i32, h: i32) -> u64 {
    if w <= 0 || h <= 0 {
        return 0;
    }
    ((w as u64) << 32) | (h as u64 & 0xffff_ffff)
}

/// The inverse of [`pack_surface_size`]: `None` for the `0` sentinel.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn unpack_surface_size(packed: u64) -> Option<(i32, i32)> {
    if packed == 0 {
        return None;
    }
    Some((((packed >> 32) as u32) as i32, (packed as u32) as i32))
}

struct VideoThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Stop and join the decode thread once, recovering a poisoned slot during teardown.
    fn stop_video(&self) {
        if let Some(mut vt) = lock_recover(&self.video).take() {
            vt.shutdown.store(true, Ordering::SeqCst);
            if let Some(j) = vt.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Drop audio playback once; its destructor joins decode and closes AAudio.
    #[cfg(target_os = "android")]
    fn stop_audio(&self) {
        let _ = lock_recover(&self.audio).take();
    }

    /// Drop mic capture once; its destructor joins encode and closes AAudio.
    #[cfg(target_os = "android")]
    fn stop_mic(&self) {
        let _ = lock_recover(&self.mic).take();
    }

    /// Drop pad audio once and join its renderer before Kotlin closes the USB connection.
    #[cfg(target_os = "android")]
    pub(crate) fn stop_pad_audio(&self) {
        let _ = lock_recover(&self.pad_audio).take();
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop_video();
        #[cfg(target_os = "android")]
        self.stop_audio();
        #[cfg(target_os = "android")]
        self.stop_mic();
        #[cfg(target_os = "android")]
        self.stop_pad_audio();
    }
}

/// SHA-256 fingerprint → 64 lowercase hex chars (matches the host log + client-rs).
fn hex32(fp: &[u8; 32]) -> String {
    use std::fmt::Write;
    fp.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 64-hex → [u8; 32]; `None` on bad length/char.
fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{pack_surface_size, unpack_surface_size};

    /// The pair the presenter reads as one atomic load must survive the round trip — including a
    /// size wider than a signed 16-bit value, which every panel this runs on now is.
    #[test]
    fn surface_size_round_trips() {
        assert_eq!(
            unpack_surface_size(pack_surface_size(2800, 1260)),
            Some((2800, 1260))
        );
        assert_eq!(unpack_surface_size(pack_surface_size(1, 1)), Some((1, 1)));
    }

    /// "Not reported yet" — and anything nonsensical — is the one sentinel, so the layer falls back
    /// to the window's buffer geometry rather than composing into an empty rectangle.
    #[test]
    fn non_positive_sizes_are_the_sentinel() {
        assert_eq!(pack_surface_size(0, 0), 0);
        assert_eq!(pack_surface_size(1920, 0), 0);
        assert_eq!(pack_surface_size(-1, 1080), 0);
        assert_eq!(unpack_surface_size(0), None);
    }
}
