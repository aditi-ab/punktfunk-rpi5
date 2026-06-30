//! GameStream (P1) control plane — what a stock Moonlight/Artemis client talks to around
//! the media streams: mDNS discovery, the nvhttp serverinfo + pairing HTTP(S) API, RTSP,
//! and the ENet control stream. `tokio`/`axum` live here (control plane, I/O-bound — never
//! the per-frame hot path; that is `punktfunk_core`'s P1 wire codec). See `design/gamestream-host-plan.md`.
//!
//! Status: P1.1 — mDNS `_nvstream._tcp` advertisement + `/serverinfo`. Pairing, RTSP, and
//! the media streams follow (see the GameStream host task list / plan).

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
/// The **SDR baseline** codec mask: H.264, HEVC Main, AV1 Main 8-bit (= 65793). HEVC Main10 (HDR) is
/// layered on top of this at runtime by `serverinfo::codec_mode_support` when — and only when — the
/// host can actually deliver it ([`host_hdr_capable`]); it is never a static claim, because a non-HDR
/// host (Linux, or a Windows host without the `PUNKTFUNK_10BIT` opt-in) must not invite a client into
/// an HDR mode it can't produce. (The previous placeholder 3843 = 0xF03 wrongly claimed HEVC Main10 +
/// 4:4:4 and *no* AV1.) 4:4:4 stays off entirely: stock Moonlight is 4:2:0 and the Windows IDD-push
/// capturer can't yet deliver full-chroma frames (`crate::capture::capturer_supports_444`).
pub const SERVER_CODEC_MODE_SUPPORT: u32 = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;

/// Whether this host can deliver an **HDR** (HEVC Main10 / BT.2020 PQ) GameStream — the single gate
/// for advertising [`SCM_HEVC_MAIN10`] in serverinfo and `IsHdrSupported` per app, and for honoring a
/// client's `dynamicRangeMode` request. HDR capture+encode is **Windows-only** (the Linux host is
/// 8-bit, blocked upstream) and behind the operator's `PUNKTFUNK_10BIT` opt-in — the same policy gate
/// the native punktfunk/1 plane honors. When this is true the IDD-push capturer streams HEVC Main10 PQ
/// whenever the desktop is HDR, and a client HDR request makes the GameStream video path proactively
/// enable advanced color on the per-session virtual display so PQ flows even from an SDR desktop.
pub fn host_hdr_capable() -> bool {
    cfg!(target_os = "windows") && crate::config::config().ten_bit
}

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
    /// Source IP of the paired HTTPS client that issued `/launch`. The unauthenticated RTSP/UDP
    /// media plane binds to this so only the launching peer can start/own the stream — an
    /// unpaired RTSP peer cannot ride a paired client's launch (security-review 2026-06-28 #4).
    /// `None` if the address could not be captured (then RTSP falls back to launch-present only).
    pub peer_ip: Option<std::net::IpAddr>,
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
    /// A client reference-frame-invalidation request carrying the lost frame range (0x0301). The
    /// video thread drains it and calls `Encoder::invalidate_ref_frames`, falling back to a full
    /// IDR when the encoder can't invalidate (range too old / no NVENC RFI). `None` = nothing pending.
    pub rfi_range: std::sync::Arc<std::sync::Mutex<Option<(i64, i64)>>>,
    /// Persistent screen capturer, reused across streams so reconnects don't spawn a second
    /// (conflicting) screencast session. The video thread borrows it for the stream's duration
    /// and returns it; `set_active` gates its cost while idle.
    pub video_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::capture::Capturer>>>>,
    /// Persistent audio capturer, reused across streams when the channel count still matches
    /// (avoids a PipeWire stream setup per reconnect); drained on reuse so no stale audio is
    /// sent, dropped + reopened when a session negotiates a different channel count.
    pub audio_cap: std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>,
    /// Shared streaming-stats recorder (web-console capture/graph). The GameStream encode loop
    /// reads `is_armed()` per frame and emits samples; the same `Arc` is shared with the mgmt API
    /// and the native punktfunk/1 loops so one capture spans whichever path is streaming.
    pub stats: Arc<crate::stats_recorder::StatsRecorder>,
}

impl AppState {
    /// Fresh control-plane state: no active session; the pairing allow-list is loaded from
    /// disk (pairings persist across restarts). `stats` is the shared recorder handed to both the
    /// mgmt API and the streaming loops.
    pub fn new(
        host: Host,
        identity: cert::ServerIdentity,
        stats: Arc<crate::stats_recorder::StatsRecorder>,
    ) -> AppState {
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
            rfi_range: std::sync::Arc::new(std::sync::Mutex::new(None)),
            video_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            audio_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            stats,
        }
    }
}

/// Run the host (blocks): mDNS, the nvhttp servers, and the management REST API.
/// `native = Some(cfg)` makes this the **unified** host — it also runs the native punktfunk/1
/// QUIC server on `cfg.port` in the same process, sharing one [`crate::native_pairing`] handle with
/// the management API so the web console can arm pairing and show the PIN. `None` = GameStream only
/// (the mgmt API's native endpoints report `enabled: false`).
/// Run the host. The **native punktfunk/1 plane + management API always run** (the secure default —
/// SPAKE2 pairing, per-direction AEAD nonces); `gamestream` additionally brings up the
/// GameStream/Moonlight-compat planes (nvhttp pairing, RTSP, ENet control, `_nvstream` mDNS), which
/// carry inherent on-path weaknesses (plain-HTTP pairing + legacy GCM nonce reuse, security-review
/// #5/#9) — so it is **opt-in** (`serve --gamestream`) and gated on a trusted LAN.
pub fn serve(
    mgmt: crate::mgmt::Options,
    native: crate::punktfunk1::NativeServe,
    gamestream: bool,
) -> Result<()> {
    let host = Host::detect()?;
    let identity = cert::ServerIdentity::load_or_create().context("host certificate")?;
    // The shared streaming-stats recorder: one handle for the mgmt API, the GameStream encode loop
    // (via `AppState`), and the native punktfunk/1 loops (passed to `punktfunk1::serve`).
    let stats = crate::stats_recorder::StatsRecorder::new(crate::stats_recorder::default_dir());
    let state = Arc::new(AppState::new(host, identity, stats.clone()));
    // The native plane always runs, so the shared native-pairing handle (linking the QUIC ceremony
    // and the management API) always exists.
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(None, None, false)
            .context("native pairing store")?,
    );
    tracing::info!(
        hostname = %state.host.hostname,
        uniqueid = %state.host.uniqueid,
        ip = %state.host.local_ip,
        native_port = native.port,
        require_pairing = native.require_pairing,
        gamestream,
        "punktfunk host"
    );
    if gamestream {
        tracing::warn!(
            "GameStream/Moonlight compat ENABLED (--gamestream): its pairing runs over plain HTTP and \
             its legacy control encryption can reuse GCM nonces (security-review #5/#9) — an on-path \
             LAN attacker could MITM pairing or recover input. Enable only on a TRUSTED network; prefer \
             the native punktfunk/1 plane + clients for untrusted/WAN use."
        );
    }
    let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
    rt.block_on(async move {
        // rustls needs a process-wide crypto provider before any TLS config is built.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let native_opts = crate::punktfunk1::native_serve_opts(&native);
        if gamestream {
            // Unified host: GameStream compat planes + native + mgmt.
            let _advert = mdns::advertise(&state.host).context("mDNS advertise")?;
            rtsp::spawn(state.clone()).context("start RTSP server")?;
            control::spawn(state.clone()).context("start ENet control server")?;
            tracing::info!(
                port = native.port,
                "unified host: GameStream/Moonlight compat + native punktfunk/1 (QUIC)"
            );
            tokio::try_join!(
                nvhttp::run(state.clone()),
                crate::mgmt::run(state.clone(), mgmt, Some(np.clone()), stats.clone()),
                crate::punktfunk1::serve(native_opts, native.mgmt_port, np, stats.clone()),
            )?;
        } else {
            // Secure default: native punktfunk/1 + management API only (no GameStream surface).
            tracing::info!(
                port = native.port,
                "secure host: native punktfunk/1 (QUIC) + management API \
                 (GameStream OFF — pass --gamestream for stock-Moonlight compat)"
            );
            tokio::try_join!(
                crate::mgmt::run(state.clone(), mgmt, Some(np.clone()), stats.clone()),
                crate::punktfunk1::serve(native_opts, native.mgmt_port, np, stats.clone()),
            )?;
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

/// Create `dir` (and parents) owner-private — **0700** on Unix (so the host's secrets aren't readable
/// by other local users via a traversable config path). On Windows, applies a restrictive DACL
/// ([`restrict_dir_to_system_admins`]) so a local unprivileged user can't pre-create / plant files in
/// the config tree (the default `%ProgramData%` ACL grants Users *create*; security-review
/// 2026-06-28 #3/#11). Tightens (and re-owns) an already-existing dir too.
pub(crate) fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let r = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        // `recursive` doesn't re-chmod an existing dir — tighten it so an old 0755 dir gets locked.
        if dir.exists() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        r
    }
    #[cfg(not(unix))]
    {
        let r = std::fs::create_dir_all(dir);
        #[cfg(windows)]
        restrict_dir_to_system_admins(dir);
        r
    }
}

/// Best-effort Windows DACL lockdown of the config *directory* (the companion to
/// [`restrict_to_system_admins`] for files). The default `%ProgramData%` ACL lets `BUILTIN\Users`
/// create subfolders/files (and become `CREATOR OWNER`), so a non-admin could pre-create the
/// `punktfunk` dir or plant a `host.env`/`apps.json` that the privileged SYSTEM service then trusts
/// (LPE; security-review 2026-06-28 #3). This re-owns the dir to Administrators (defeating a
/// pre-creation), strips inheritance, and sets an explicit DACL: SYSTEM/Administrators/OWNER full
/// (object+container inherit so child files/dirs inherit it), and Users **read-only** (so existing
/// reads of non-secret config keep working but a local user can no longer write/plant). Secret files
/// are additionally locked to SYSTEM/Admins by [`write_secret_file`]. Hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path; never fatal.
#[cfg(windows)]
fn restrict_dir_to_system_admins(dir: &std::path::Path) {
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    // Reset ownership of the directory object to Administrators first, so a dir a non-admin may have
    // pre-created can't keep OWNER control (an owner can always rewrite the DACL). No `/T` — re-owning
    // the dir itself is what defeats the pre-creation; recursing a large captures tree each call is
    // needless churn (secret files are individually owner-locked by `write_secret_file`).
    let _ = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]) // BUILTIN\Administrators
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)(F)", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)(F)", // BUILTIN\Administrators
            "/grant:r",
            "*S-1-3-4:(OI)(CI)(F)", // OWNER RIGHTS
            "/grant:r",
            "*S-1-5-32-545:(OI)(CI)(RX)", // BUILTIN\Users — read-only (no create/write → no plant)
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => tracing::warn!(
            dir = %dir.display(),
            "config-dir DACL hardening did not fully succeed — a local user may be able to plant config files"
        ),
    }
}

/// Write `contents` to `path` as an **owner-only secret**: created and re-chmod'd **0600** on Unix
/// (never even briefly group/world-readable), and DACL-restricted to SYSTEM/Administrators/owner on
/// Windows (the default `%ProgramData%` ACL is Users-readable). Mirrors the mgmt-token hardening; used
/// for the host private key and the persisted trust stores so a local unprivileged user can neither
/// read the key (impersonation) nor tamper with the paired allow-list (unauthorized pairing).
pub(crate) fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents)?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    restrict_to_system_admins(path);
    Ok(())
}

/// Best-effort Windows DACL lockdown of a secret file: strip inherited ACEs and grant Full only to
/// SYSTEM, Administrators, and OWNER RIGHTS (the creating account — the SYSTEM service or a manually
/// running user keeps access). Without this the host key under the default Users-readable
/// `%ProgramData%` ACL is readable by ANY local user. Uses `icacls` with hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path (a privileged service must not trust
/// `PATH`). Never fatal — on failure the file is simply left at the inherited ACL (today's behaviour).
#[cfg(windows)]
fn restrict_to_system_admins(path: &std::path::Path) {
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    let status = std::process::Command::new(icacls)
        .arg(path.as_os_str())
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(F)", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(F)", // BUILTIN\Administrators
            "/grant:r",
            "*S-1-3-4:(F)", // OWNER RIGHTS
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => tracing::warn!(
            path = %path.display(),
            "icacls hardening did not succeed — this secret may be readable by other local users"
        ),
    }
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

/// Persist the paired-client allow-list (called after each successful pairing). Written
/// atomically (temp file + rename) so a crash mid-write can't truncate `paired.json` — a partial
/// write would otherwise lock out every paired client until they re-pair.
pub(crate) fn save_paired(paired: &[Vec<u8>]) {
    let Some(path) = paired_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = create_private_dir(dir);
    }
    let bytes = match serde_json::to_vec(paired) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "serializing pairings failed");
            return;
        }
    };
    // Write to a sibling temp file (owner-only, so a local user can't tamper the allow-list), then
    // rename over the target (atomic replace on Unix and Windows). Never write `path` in place.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = write_secret_file(&tmp, &bytes) {
        tracing::warn!(error = %e, "persisting pairings failed (temp write)");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(error = %e, "persisting pairings failed (rename)");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{create_private_dir, write_secret_file};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secrets_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-secret-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_private_dir(&dir).expect("create private dir");
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "config dir must be owner-only (0700)");

        let key = dir.join("key.pem");
        write_secret_file(&key, b"-----BEGIN PRIVATE KEY-----\n...").expect("write secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "private key must be owner-only (0600)");

        // Overwriting an existing secret keeps it 0600 (the truncate+reopen path).
        write_secret_file(&key, b"new contents").expect("rewrite secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
