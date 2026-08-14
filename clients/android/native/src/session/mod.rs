//! Session lifecycle + plane wiring over JNI.
//!
//! A connected session is a [`SessionHandle`] — an `Arc<NativeClient>` plus the decode thread it
//! feeds — boxed and handed to Kotlin as an opaque `jlong`. The connector is `Sync`, so the decode
//! thread pulls the video plane (`next_frame`) directly while Kotlin still holds the handle.
//!
//! Wired: connect/close, the video plane (HEVC `next_frame` → NDK AMediaCodec → the SurfaceView's
//! `ANativeWindow`, see [`crate::decode`]), host→client audio ([`crate::audio`]), input
//! (`send_input` — mouse/keyboard/gamepad), rumble/DualSense HID feedback ([`crate::feedback`]),
//! and the trust surface: `nativeGenerateIdentity` (persistent identity, Keystore-wrapped on the
//! Kotlin side), `nativeConnect` with identity + pin (TOFU / pinned), and `nativePair` (SPAKE2 PIN).
//!
//! Split by concern: [`connect`] (identity + connect/close + the trust surface), [`planes`]
//! (video/audio/mic start/stop + the stats drain), [`input`] (the input-plane shims), [`probe`]
//! (the bandwidth speed test). This module keeps the shared infrastructure they all deref through.
//!
//! TODO(M4 Android stage 1): client→host DualSense rich input (`send_rich_input`), mode
//! renegotiation. Port the remaining orchestration from `clients/linux`.

mod access;
mod clipboard;
mod connect;
mod input;
mod planes;
mod probe;

use punktfunk_core::client::NativeClient;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
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

/// A live session behind the `jlong` handle: the connector + the decode thread it feeds.
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
}

struct VideoThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Signal the decode thread to stop and join it. Idempotent.
    fn stop_video(&self) {
        if let Some(mut vt) = self.video.lock().unwrap().take() {
            vt.shutdown.store(true, Ordering::SeqCst);
            if let Some(j) = vt.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Stop + close audio playback. Dropping the [`crate::audio::AudioPlayback`] joins its decode
    /// thread and closes the AAudio stream. Idempotent.
    #[cfg(target_os = "android")]
    fn stop_audio(&self) {
        let _ = self.audio.lock().unwrap().take();
    }

    /// Stop mic uplink. Dropping the [`crate::mic::MicCapture`] joins its encode thread and closes
    /// the AAudio input stream. Idempotent.
    #[cfg(target_os = "android")]
    fn stop_mic(&self) {
        let _ = self.mic.lock().unwrap().take();
    }

    /// Stop pad audio. Dropping the [`crate::pad_audio::PadAudio`] joins its render thread, which
    /// is what guarantees nothing is still writing to the descriptor when Kotlin closes the
    /// `UsbDeviceConnection`. Idempotent.
    #[cfg(target_os = "android")]
    pub(crate) fn stop_pad_audio(&self) {
        let _ = self.pad_audio.lock().unwrap().take();
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
