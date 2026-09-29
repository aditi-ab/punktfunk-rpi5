//! PipeWire library init, shared by the video portal and audio capture threads.
//! `pw_init` is not concurrent-safe on first use; RTSP PLAY starts both paths
//! at once, so init goes through a `Once`.

#[cfg(target_os = "linux")]
pub fn ensure_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        pipewire::init();
        // Below 1.6 the producer may re-send a held buffer (`node.reliable` unknown); the
        // version says whether that re-hold arm is live on this host.
        // SAFETY: libpipewire returns its own static, NUL-terminated version string.
        let version = unsafe { std::ffi::CStr::from_ptr(pipewire::sys::pw_get_library_version()) };
        tracing::info!(version = %version.to_string_lossy(), "pipewire library");
    });
}
