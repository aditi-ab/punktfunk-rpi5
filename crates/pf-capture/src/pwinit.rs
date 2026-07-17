//! One-time PipeWire library initialization, shared by the video (portal) and audio capture
//! threads. `pw_init` must not be called concurrently from multiple threads on first use; both
//! capture paths connect to PipeWire at nearly the same moment (RTSP PLAY starts video + audio
//! together), so we serialize the init through a `Once`.

#[cfg(target_os = "linux")]
pub fn ensure_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(pipewire::init);
}
