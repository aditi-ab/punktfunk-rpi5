//! PipeWire library init, shared by the video portal and audio capture threads.
//! `pw_init` is not concurrent-safe on first use; RTSP PLAY starts both paths
//! at once, so init goes through a `Once`.

#[cfg(target_os = "linux")]
pub fn ensure_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(pipewire::init);
}
