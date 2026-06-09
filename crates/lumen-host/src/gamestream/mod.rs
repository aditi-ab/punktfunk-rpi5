//! GameStream (P1) control plane — what a stock Moonlight/Artemis client talks to around
//! the media streams: mDNS discovery, the nvhttp serverinfo + pairing HTTP(S) API, RTSP,
//! and the ENet control stream. `tokio`/`axum` live here (control plane, I/O-bound — never
//! the per-frame hot path; that is `lumen_core`'s P1 wire codec). See `docs/m2-plan.md`.
//!
//! Status: P1.1 — mDNS `_nvstream._tcp` advertisement + `/serverinfo`. Pairing, RTSP, and
//! the media streams follow (see the M2 task list / plan).

mod audio;
pub(crate) mod cert;
mod control;
mod crypto;
mod input;
mod mdns;
mod nvhttp;
mod pairing;
mod rtsp;
mod serverinfo;
mod stream;
mod tls;
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
/// Codec support bitmask: 3=H264, 259=+HEVC, 3843=+AV1 (we encode HEVC/H264/AV1 via NVENC).
pub const SERVER_CODEC_MODE_SUPPORT: u32 = 3843;

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
    /// Persistent audio capturer, reused across streams (avoids leaking a PipeWire capture
    /// thread per reconnect); drained on reuse so no stale audio is sent.
    pub audio_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>,
}

impl AppState {
    /// Fresh control-plane state: no active session, empty pairing store.
    pub fn new(host: Host, identity: cert::ServerIdentity) -> AppState {
        AppState {
            host,
            identity,
            pairing: pairing::Pairing::new(),
            paired: std::sync::Mutex::new(Vec::new()),
            launch: std::sync::Mutex::new(None),
            stream: std::sync::Mutex::new(None),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force_idr: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            video_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// Run the GameStream control plane (blocks): mDNS advertisement, the nvhttp servers, and
/// the management REST API.
pub fn serve(mgmt: crate::mgmt::Options) -> Result<()> {
    let host = Host::detect()?;
    let identity = cert::ServerIdentity::load_or_create().context("host certificate")?;
    let state = Arc::new(AppState::new(host, identity));
    tracing::info!(
        hostname = %state.host.hostname,
        uniqueid = %state.host.uniqueid,
        ip = %state.host.local_ip,
        "lumen GameStream host (P1.1: serverinfo + pairing + mDNS)"
    );
    let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    rt.block_on(async move {
        // rustls needs a process-wide crypto provider before any TLS config is built.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let _advert = mdns::advertise(&state.host).context("mDNS advertise")?;
        rtsp::spawn(state.clone()).context("start RTSP server")?;
        control::spawn(state.clone()).context("start ENet control server")?;
        tokio::try_join!(nvhttp::run(state.clone()), crate::mgmt::run(state, mgmt))?;
        Ok(())
    })
}

/// `~/.config/lumen`, created on demand — host identity + (later) pairing state live here.
fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lumen")
}

fn hostname_string() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "lumen-host".to_string())
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
