//! GameStream (P1) control plane — what a stock Moonlight/Artemis client talks to around
//! the media streams: mDNS discovery, the nvhttp serverinfo + pairing HTTP(S) API, RTSP,
//! and the ENet control stream. `tokio`/`axum` live here (control plane, I/O-bound — never
//! the per-frame hot path; that is `punktfunk_core`'s P1 wire codec). See `docs/m2-plan.md`.
//!
//! Status: P1.1 — mDNS `_nvstream._tcp` advertisement + `/serverinfo`. Pairing, RTSP, and
//! the media streams follow (see the M2 task list / plan).

pub mod apps;
// Platform-neutral wire/negotiation logic + the Linux capture/encode pipeline (non-Linux
// builds get a stub `start` inside the module).
mod audio;
pub(crate) mod cert;
mod control;
mod crypto;
pub mod gamepad;
mod input;
mod mdns;
mod nvhttp;
mod pairing;
mod rtsp;
mod serverinfo;
mod stream;
pub(crate) mod tls;
mod video;

use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;

/// nvhttp ports (Moonlight derives all stream ports by offset from the HTTP base 47989).
pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;
pub const RTSP_PORT: u16 = 48010;
pub const VIDEO_PORT: u16 = 47998;
pub const CONTROL_PORT: u16 = 47999;
pub const AUDIO_PORT: u16 = 48000;

/// Advertised host version. Major ≥ 7 tells Moonlight to use SHA-256 for pairing.
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";
/// `ServerCodecModeSupport` flags, from moonlight-common-c `src/Limelight.h` (verified
/// against master, 2026-06-10): SCM_H264 0x1, SCM_HEVC 0x100, SCM_HEVC_MAIN10 0x200,
/// SCM_AV1_MAIN8 0x10000, SCM_AV1_MAIN10 0x20000 (+ 4:4:4 Sunshine extensions we don't do).
pub const SCM_H264: u32 = 0x0000_0001;
pub const SCM_HEVC: u32 = 0x0000_0100;
pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
pub const SCM_AV1_MAIN8: u32 = 0x0001_0000;
pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
/// What we actually encode via NVENC: H.264, HEVC Main, AV1 Main 8-bit (= 65793). The
/// 10-bit flags are deliberately NOT advertised: Moonlight only selects Main10 profiles for
/// HDR streaming, and our capture path is 8-bit SDR BGRx with no HDR metadata plumbing —
/// advertising them would let clients enable an HDR mode we can't deliver. (The previous
/// placeholder 3843 = 0xF03 wrongly claimed HEVC Main10 + 4:4:4 and *no* AV1.)
pub const SERVER_CODEC_MODE_SUPPORT: u32 = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;

/// Stable host identity + advertised capabilities, shared across control-plane handlers.
pub struct Host {
    pub hostname: String,
    /// Stable per-host id (persisted), echoed in serverinfo + matched on pairing.
    pub uniqueid: String,
    pub local_ip: IpAddr,
    pub http_port: u16,
    pub https_port: u16,
    // Pairing state (server cert, paired client certs) lands in the next P1.1 slice.
}

impl Host {
    pub fn detect() -> Result<Host> {
        Ok(Host {
            hostname: hostname_string(),
            uniqueid: load_or_create_uniqueid()?,
            local_ip: primary_local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
        })
    }
}

/// The stream parameters a client passes at `/launch`, shared with the RTSP + media stages.
#[derive(Clone, Copy, Debug)]
pub struct LaunchSession {
    /// AES-128 key for the RTSP/control/video/audio planes (from `rikey`).
    pub gcm_key: [u8; 16],
    /// `rikeyid` — seeds the per-stream GCM IVs.
    pub rikeyid: i32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `/launch?appid=N` — selects the app-catalog entry (session recipe).
    pub appid: u32,
}

/// Shared control-plane state used as the axum app state.
pub struct AppState {
    pub host: Host,
    pub identity: cert::ServerIdentity,
    pub pairing: pairing::Pairing,
    /// Pinned (paired) client certificate DERs — the post-pair allow-list.
    pub paired: std::sync::Mutex<Vec<Vec<u8>>>,
    /// The active launch session (set by `/launch`, consumed by RTSP/media).
    pub launch: std::sync::Mutex<Option<LaunchSession>>,
    /// Negotiated video config from RTSP ANNOUNCE (consumed by the stream on PLAY).
    pub stream: std::sync::Mutex<Option<stream::StreamConfig>>,
    /// Negotiated audio parameters from RTSP ANNOUNCE (channels/quality/packet duration);
    /// defaults to stereo when a client never ANNOUNCEs them.
    pub audio_params: std::sync::Mutex<audio::AudioParams>,
    /// True while the video stream thread is running (also its keep-running flag).
    pub streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// True while the audio stream thread is running (also its keep-running flag).
    pub audio_streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the control stream when the client requests an IDR / invalidates reference
    /// frames (recovery after loss); the video thread forces a keyframe and clears it.
    pub force_idr: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Persistent screen capturer, reused across streams so reconnects don't spawn a second
    /// (conflicting) screencast session. The video thread borrows it for the stream's duration
    /// and returns it; `set_active` gates its cost while idle.
    pub video_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::capture::Capturer>>>>,
    /// Persistent audio capturer, reused across streams when the channel count still matches
    /// (avoids a PipeWire stream setup per reconnect); drained on reuse so no stale audio is
    /// sent, dropped + reopened when a session negotiates a different channel count.
    pub audio_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>,
}

impl AppState {
    /// Fresh control-plane state: no active session; the pairing allow-list is loaded from
    /// disk (pairings persist across restarts).
    pub fn new(host: Host, identity: cert::ServerIdentity) -> AppState {
        AppState {
            host,
            identity,
            pairing: pairing::Pairing::new(),
            paired: std::sync::Mutex::new(load_paired()),
            launch: std::sync::Mutex::new(None),
            stream: std::sync::Mutex::new(None),
            audio_params: std::sync::Mutex::new(audio::AudioParams::default()),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force_idr: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            video_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// Run the host (blocks): mDNS, the nvhttp servers, and the management REST API.
/// `native = Some(cfg)` makes this the **unified** host — it also runs the native punktfunk/1
/// QUIC server on `cfg.port` in the same process, sharing one [`crate::native_pairing`] handle with
/// the management API so the web console can arm pairing and show the PIN. `None` = GameStream only
/// (the mgmt API's native endpoints report `enabled: false`).
pub fn serve(mgmt: crate::mgmt::Options, native: Option<crate::m3::NativeServe>) -> Result<()> {
    let host = Host::detect()?;
    let identity = cert::ServerIdentity::load_or_create().context("host certificate")?;
    let state = Arc::new(AppState::new(host, identity));
    // The shared native-pairing handle exists only when we run the native host; it links the QUIC
    // ceremony and the management API.
    let np = match &native {
        Some(_) => Some(Arc::new(
            crate::native_pairing::NativePairing::load_with(None, None, false)
                .context("native pairing store")?,
        )),
        None => None,
    };
    tracing::info!(
        hostname = %state.host.hostname,
        uniqueid = %state.host.uniqueid,
        ip = %state.host.local_ip,
        native = native.is_some(),
        require_pairing = native.as_ref().map(|n| n.require_pairing),
        "punktfunk host (GameStream P1.1: serverinfo + pairing + mDNS)"
    );
    let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    rt.block_on(async move {
        // rustls needs a process-wide crypto provider before any TLS config is built.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let _advert = mdns::advertise(&state.host).context("mDNS advertise")?;
        rtsp::spawn(state.clone()).context("start RTSP server")?;
        control::spawn(state.clone()).context("start ENet control server")?;
        match (native, np) {
            (Some(cfg), Some(np)) => {
                tracing::info!(
                    port = cfg.port,
                    require_pairing = cfg.require_pairing,
                    "unified host: also serving native punktfunk/1 (QUIC)"
                );
                tokio::try_join!(
                    nvhttp::run(state.clone()),
                    crate::mgmt::run(state.clone(), mgmt, Some(np.clone())),
                    crate::m3::serve(crate::m3::native_serve_opts(&cfg), np),
                )?;
            }
            _ => {
                tokio::try_join!(
                    nvhttp::run(state.clone()),
                    crate::mgmt::run(state, mgmt, None)
                )?;
            }
        }
        Ok(())
    })
}

/// The host config dir (host identity, pairing state, mgmt token, library) — created on demand.
/// Linux: `$XDG_CONFIG_HOME/punktfunk` or `~/.config/punktfunk`. Windows: `%ProgramData%\punktfunk`
/// (machine-wide — the SYSTEM service and the interactive user share ONE dir that survives logout).
/// `PUNKTFUNK_CONFIG_DIR` overrides on both platforms (used by the Windows service config / tests).
pub(crate) fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PUNKTFUNK_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    // Windows: %ProgramData% (e.g. C:\ProgramData\punktfunk) — machine-wide, SYSTEM-readable,
    // persists across user logout, correct for a SYSTEM service. Falls back to %APPDATA% then CWD.
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("ProgramData")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("punktfunk")
}

fn hostname_string() -> String {
    #[cfg(target_os = "windows")]
    if let Some(n) = std::env::var_os("COMPUTERNAME") {
        let s = n.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "punktfunk-host".to_string())
}

/// Load the persisted host uniqueid, or mint one (from the kernel UUID source) and store it.
fn load_or_create_uniqueid() -> Result<String> {
    let path = config_dir().join("uniqueid");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    let id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|u| u.trim().replace('-', ""))
        .unwrap_or_else(|_| format!("{:016x}{:016x}", std::process::id(), HTTP_PORT));
    std::fs::create_dir_all(config_dir()).ok();
    std::fs::write(&path, &id).with_context(|| format!("write {}", path.display()))?;
    Ok(id)
}

/// Best-effort primary LAN IP: open a UDP socket "toward" a public address and read the
/// local address the OS would route through. No packets are actually sent.
fn primary_local_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// Where the paired-client allow-list persists (survives host restarts, like Sunshine).
fn paired_path() -> Option<std::path::PathBuf> {
    // Same dir as the host identity (HOME/.config/punktfunk on Linux, %APPDATA%\punktfunk on Windows).
    Some(config_dir().join("paired.json"))
}

/// Load the persisted paired-client certificate DERs (empty on first run / parse failure).
fn load_paired() -> Vec<Vec<u8>> {
    let Some(path) = paired_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<Vec<u8>>>(&raw) {
        Ok(v) => {
            tracing::info!(clients = v.len(), "loaded persisted pairings");
            v
        }
        Err(e) => {
            tracing::warn!(error = %e, "paired.json unreadable — starting unpaired");
            Vec::new()
        }
    }
}

/// Persist the paired-client allow-list (called after each successful pairing).
pub(crate) fn save_paired(paired: &[Vec<u8>]) {
    let Some(path) = paired_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_vec(paired) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                tracing::warn!(error = %e, "persisting pairings failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "serializing pairings failed"),
    }
}
