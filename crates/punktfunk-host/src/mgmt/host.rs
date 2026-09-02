//! Host-tagged `/api/v1` routes: identity, liveness, compositor list, live status,
//! and the loopback tray summary. Split out of the `mgmt` facade.

use super::shared::*;
use crate::encode::Codec;
use crate::gamestream::APP_VERSION;
use crate::gamestream::AUDIO_PORT;
use crate::gamestream::CONTROL_PORT;
use crate::gamestream::GFE_VERSION;
use crate::gamestream::RTSP_PORT;
use crate::gamestream::VIDEO_PORT;
use std::sync::atomic::Ordering;

#[derive(Serialize, ToSchema)]
pub(crate) struct Health {
    /// Always `"ok"` when the host responds.
    #[schema(example = "ok")]
    status: String,
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
    abi_version: u32,
}

/// Host identity and capabilities. Static for the process except `local_ip`.
#[derive(Serialize, ToSchema)]
pub(crate) struct HostInfo {
    hostname: String,
    /// Persisted host id; pairing matches on this.
    uniqueid: String,
    /// Fresh LAN IP each request — do not cache. Cold-boot and network-move report
    /// `127.0.0.1` until a real address exists.
    local_ip: String,
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
    abi_version: u32,
    /// GameStream host version advertised to Moonlight clients.
    app_version: String,
    /// GFE version advertised to Moonlight clients.
    gfe_version: String,
    /// OS chain, generic → specific, slash-separated (`windows` | `macos` |
    /// `linux[/<family>][/<id>]`). Walk most-specific-first; an unknown distro still matches its family.
    #[schema(example = "linux/fedora/bazzite")]
    os: String,
    /// Human-readable OS name (os-release `PRETTY_NAME`; `"Windows"`/`"macOS"` elsewhere).
    #[schema(example = "Bazzite 42 (Kinoite)")]
    os_name: String,
    /// Codecs this host can encode (`Codec::host_wire_caps`, not the compile-time list).
    codecs: Vec<ApiCodec>,
    /// GameStream/Moonlight-compat planes are running (`--gamestream`). `false` is the default (native only).
    gamestream: bool,
    ports: PortMap,
}

/// Ports a client needs. Moonlight derives stream ports from HTTP; a control pane should not.
#[derive(Serialize, ToSchema)]
pub(crate) struct PortMap {
    mgmt: u16,
    /// nvhttp plain HTTP (serverinfo, pairing).
    http: u16,
    /// nvhttp mutual-TLS HTTPS (post-pairing).
    https: u16,
    rtsp: u16,
    video: u16,
    control: u16,
    audio: u16,
}

/// Wire token is the stack's canonical codec name (`Codec::label`). `H265` serializes as `"hevc"`, not `"h265"`.
#[derive(Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApiCodec {
    H264,
    #[serde(rename = "hevc")]
    H265,
    Av1,
    /// Opt-in wired-LAN intra-only wavelet codec.
    PyroWave,
}

impl From<Codec> for ApiCodec {
    fn from(c: Codec) -> Self {
        match c {
            Codec::H264 => ApiCodec::H264,
            Codec::H265 => ApiCodec::H265,
            Codec::Av1 => ApiCodec::Av1,
            Codec::PyroWave => ApiCodec::PyroWave,
        }
    }
}

/// Live status; changes as sessions start and end.
#[derive(Serialize, ToSchema)]
pub(crate) struct RuntimeStatus {
    video_streaming: bool,
    audio_streaming: bool,
    /// Pairing handshake is waiting for a PIN (`POST /api/v1/pair/pin`).
    pin_pending: bool,
    /// GameStream paired-cert count. Native devices are `native_paired_clients`; sum both for the total.
    paired_clients: u32,
    /// Native-plane pairings (separate store).
    native_paired_clients: u32,
    /// Live sessions on both planes. Native admits concurrent sessions so this can exceed 1;
    /// `session`/`stream` are one representative.
    active_sessions: u32,
    /// GameStream launch if present, else the first live native session. `null` when idle.
    session: Option<SessionInfo>,
    /// Active stream parameters. `null` when idle.
    stream: Option<StreamInfo>,
    /// Launched titles: live sessions plus `state: "grace"` reconnect-window rows. Empty for a desktop-only stream.
    games: Vec<ActiveGame>,
    /// Windows audio-wiring verdict; absent off-Windows and before the first pass. Present while idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<AudioWiring>,
}

/// Windows audio wiring: which endpoint carries each role. Names are the Sound-settings friendly names.
#[derive(Serialize, ToSchema)]
pub(crate) struct AudioWiring {
    /// `full` | `audio_only` | `mic_only` | `none`.
    #[schema(example = "full")]
    readiness: String,
    /// Desktop-audio loopback friendly name; absent = unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<String>,
    /// Virtual-mic write-target friendly name; absent = unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    mic: Option<String>,
    /// Mic withheld so game audio keeps the only working sink.
    mic_withheld: bool,
    /// Loopback is the degraded last resort; desktop audio may be silent until endpoints change.
    last_resort: bool,
    /// Why the chosen loopback endpoint NARROWS the desktop mix (rate/channels), when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    narrowing: Option<String>,
}

/// `None` off-Windows or before the first wiring pass.
fn audio_wiring() -> Option<AudioWiring> {
    use crate::audio::wiring_plan as wp;
    crate::audio::wiring_snapshot().map(|w| AudioWiring {
        readiness: match wp::readiness(&w) {
            wp::AudioReadiness::Full => "full",
            wp::AudioReadiness::AudioOnly => "audio_only",
            wp::AudioReadiness::MicOnly => "mic_only",
            wp::AudioReadiness::Nothing => "none",
        }
        .into(),
        loopback: w.loopback_render.map(|(n, _)| n),
        mic: w.mic_render.map(|(n, _)| n),
        mic_withheld: w.mic_withheld,
        last_resort: w.loopback_last_resort,
        narrowing: w.loopback_narrowing,
    })
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ActiveGame {
    /// Streaming session; `null` while waiting out the reconnect window.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    /// Client-supplied device name of the session that launched it; may be empty.
    client: String,
    /// Store-qualified library id (`steam:570`); matches `GET /library`. Absent for a typed GameStream command.
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    title: String,
    /// Which store surfaced it (`steam`, `heroic`, `custom`, …), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<String>,
    /// `native` or `gamestream`.
    plane: crate::events::Plane,
    /// `launching` | `running` | `exited` | `untracked` (exit will never be seen) | `grace` (reconnect window).
    #[schema(example = "running")]
    state: String,
    /// Seconds until this game is ended — only present on a `grace` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_remaining_s: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionInfo {
    width: u32,
    height: u32,
    fps: u32,
}

/// Negotiated stream parameters (RTSP on GameStream; live mode on native).
#[derive(Serialize, ToSchema)]
pub(crate) struct StreamInfo {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// Video payload size per packet (bytes).
    packet_size: u32,
    /// Client's parity floor per FEC block (`minRequiredFecPackets`).
    min_fec: u8,
    codec: ApiCodec,
    /// Hello → first video packet, ms. Native only; `null` on GameStream or while still bringing up.
    time_to_first_frame_ms: Option<u32>,
    /// Last mid-stream resize, reconfigure → rebuilt, ms. Native only; `null` if none / GameStream.
    last_resize_ms: Option<u32>,
}

/// Tray snapshot for loopback: counts, booleans, and `client_name`.
/// Unauthenticated; `require_auth` admits loopback only (the tray cannot read the bearer file).
#[derive(Serialize, ToSchema)]
pub(crate) struct LocalSummary {
    /// Host version (mirrors `/health`).
    version: String,
    /// Video streaming on either plane. The GameStream flag alone misses native sessions.
    video_streaming: bool,
    /// True while audio is streaming on either plane (same rule as `video_streaming`).
    audio_streaming: bool,
    /// GameStream launch if present, else the first live native session. `null` when idle.
    session: Option<SessionInfo>,
    /// First native session's display name (trust-store, else connect-time). `null` when idle, nameless, or GameStream.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    /// GameStream paired-cert count.
    paired_clients: u32,
    /// Native-plane pairing count.
    native_paired_clients: u32,
    /// GameStream pairing is waiting for a PIN.
    pin_pending: bool,
    /// Native pairing knocks awaiting the operator's approval (count only).
    pending_approvals: u32,
    /// Lingering or pinned virtual displays with no live session. Active (in-use) displays are not counted.
    kept_displays: u32,
    /// Other GameStream hosts on this machine, detected at startup. Running one alongside is unsupported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
    /// Compact labels (`Hades`, `Hades (closing in 4:12)`). Countdown means the client is gone and the host will end the game when the window closes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    games: Vec<String>,
}

/// Liveness probe. Unauthenticated (`require_auth` exempts it).
#[utoipa::path(
    get,
    path = "/health",
    tag = "host",
    operation_id = "getHealth",
    // Override the document-global bearerAuth: this route is exempt in `require_auth`.
    security(()),
    responses((status = OK, description = "Host is up", body = Health))
)]
pub(crate) async fn get_health() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        version: env!("PUNKTFUNK_VERSION").into(),
        abi_version: punktfunk_core::ABI_VERSION,
    })
}

/// Host identity and capabilities
#[utoipa::path(
    get,
    path = "/host",
    tag = "host",
    operation_id = "getHostInfo",
    responses(
        (status = OK, description = "Host identity, versions, codecs, and port map", body = HostInfo),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_host_info(State(st): State<Arc<MgmtState>>) -> Json<HostInfo> {
    let h = &st.app.host;
    Json(HostInfo {
        hostname: h.hostname.clone(),
        uniqueid: h.uniqueid.clone(),
        local_ip: h.local_ip().to_string(),
        version: env!("PUNKTFUNK_VERSION").into(),
        abi_version: punktfunk_core::ABI_VERSION,
        app_version: APP_VERSION.into(),
        gfe_version: GFE_VERSION.into(),
        os: h.os_chain.clone(),
        os_name: h.os_name.clone(),
        // Same mask as GameStream/QUIC negotiation (`Codec::host_wire_caps`), not the compile-time list.
        codecs: {
            let caps = Codec::host_wire_caps();
            use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
            [
                (CODEC_H264, ApiCodec::H264),
                (CODEC_HEVC, ApiCodec::H265),
                (CODEC_AV1, ApiCodec::Av1),
                (CODEC_PYROWAVE, ApiCodec::PyroWave),
            ]
            .into_iter()
            .filter(|(bit, _)| caps & bit != 0)
            .map(|(_, codec)| codec)
            .collect()
        },
        gamestream: st.gamestream_enabled,
        ports: PortMap {
            mgmt: st.port,
            http: h.http_port,
            https: h.https_port,
            rtsp: RTSP_PORT,
            video: VIDEO_PORT,
            control: CONTROL_PORT,
            audio: AUDIO_PORT,
        },
    })
}

/// A compositor backend and whether it is usable now.
#[derive(Serialize, ToSchema)]
pub(crate) struct AvailableCompositor {
    /// Stable id (`kwin` | `wlroots` | `mutter` | `gamescope`); pass to `--compositor`.
    id: String,
    label: String,
    /// Usable now: the live session's compositor, or gamescope if its binary is installed.
    available: bool,
    /// True for the backend an `Auto` (unspecified) request resolves to right now.
    default: bool,
}

/// Compositor backends the host can drive, with availability and the `Auto` default.
/// Clients pass `id` to `--compositor` or `PUNKTFUNK_COMPOSITOR_*`.
#[utoipa::path(
    get,
    path = "/compositors",
    tag = "host",
    operation_id = "listCompositors",
    responses(
        (status = OK, description = "Compositor backends with availability + the auto-detected default", body = [AvailableCompositor]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_compositors() -> Json<Vec<AvailableCompositor>> {
    // Empty off Linux: `vdisplay::open` ignores compositor there.
    // Listing unavailable Linux backends looks like a detection bug.
    #[cfg(not(target_os = "linux"))]
    let list = Vec::new();
    #[cfg(target_os = "linux")]
    // One `/proc` scan for both columns (`vdisplay::available`); default cannot be unavailable.
    let list = {
        let available = crate::vdisplay::available();
        let default = crate::vdisplay::detect().ok();
        crate::vdisplay::Compositor::all()
            .into_iter()
            .map(|c| AvailableCompositor {
                id: c.id().into(),
                label: c.label().into(),
                available: available.contains(&c),
                default: default == Some(c),
            })
            .collect()
    };
    Json(list)
}

/// Live host status
#[utoipa::path(
    get,
    path = "/status",
    tag = "host",
    operation_id = "getStatus",
    responses(
        (status = OK, description = "Streaming/pairing state and the active session, if any", body = RuntimeStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_status(State(st): State<Arc<MgmtState>>) -> Json<RuntimeStatus> {
    let gs_launch = *st.app.launch.lock().unwrap_or_else(|e| e.into_inner());
    // Stream slot is GameStream-featured only; a native-only build has no compat-plane stream.
    #[cfg(feature = "gamestream")]
    let gs_stream = *st.app.stream.lock().unwrap_or_else(|e| e.into_inner());
    let gs_video = st.app.streaming.load(Ordering::SeqCst);
    let gs_audio = st.app.audio_streaming.load(Ordering::SeqCst);
    // Native plane, published by the video loop; lives outside `AppState` (see `session_status`).
    let native = crate::session_status::snapshot();

    // Detail card is singular: GameStream if live, else the first native session. `active_sessions` is the true count.
    let session = gs_launch
        .map(|l| SessionInfo {
            width: l.width,
            height: l.height,
            fps: l.fps,
        })
        .or_else(|| {
            native.first().map(|s| SessionInfo {
                width: s.width,
                height: s.height,
                fps: s.fps,
            })
        });
    #[cfg(feature = "gamestream")]
    let gs_stream_info = gs_stream.map(|c| StreamInfo {
        width: c.width,
        height: c.height,
        fps: c.fps,
        bitrate_kbps: c.bitrate_kbps,
        packet_size: c.packet_size as u32,
        min_fec: c.min_fec,
        codec: c.codec.into(),
        // Transition latencies are native-plane only.
        time_to_first_frame_ms: None,
        last_resize_ms: None,
    });
    #[cfg(not(feature = "gamestream"))]
    let gs_stream_info: Option<StreamInfo> = None;
    let stream = gs_stream_info.or_else(|| {
        native.first().map(|s| StreamInfo {
            width: s.width,
            height: s.height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            // FEC/packetization are RTSP (GameStream); native QUIC shards differently, so 0 = not applicable.
            packet_size: 0,
            min_fec: 0,
            codec: s.codec.into(),
            time_to_first_frame_ms: (s.time_to_first_frame_ms > 0)
                .then_some(s.time_to_first_frame_ms),
            last_resize_ms: (s.last_resize_ms > 0).then_some(s.last_resize_ms),
        })
    });
    Json(RuntimeStatus {
        video_streaming: gs_video || !native.is_empty(),
        audio_streaming: gs_audio || !native.is_empty(),
        pin_pending: gs_pin_pending(&st),
        paired_clients: st
            .app
            .paired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32,
        native_paired_clients: st.native.as_ref().map_or(0, |n| n.status().paired_clients),
        active_sessions: native.len() as u32 + u32::from(gs_video),
        session,
        stream,
        games: crate::session_status::games()
            .into_iter()
            .map(|g| ActiveGame {
                session_id: g.session_id,
                client: g.client,
                app_id: g.app_id,
                title: g.title,
                store: g.store,
                plane: g.plane,
                state: g.state.to_string(),
                grace_remaining_s: g.grace_remaining_s,
            })
            .collect(),
        audio: audio_wiring(),
    })
}

/// Loopback tray summary. Unauthenticated; `require_auth` admits loopback only.
#[utoipa::path(
    get,
    path = "/local/summary",
    tag = "host",
    operation_id = "getLocalSummary",
    // Override the document-global bearerAuth: loopback peers are exempt in `require_auth`.
    security(()),
    responses(
        (status = OK, description = "Non-sensitive local host status (loopback peers only)", body = LocalSummary),
        (status = UNAUTHORIZED, description = "Non-loopback peer", body = ApiError),
    )
)]
pub(crate) async fn get_local_summary(State(st): State<Arc<MgmtState>>) -> Json<LocalSummary> {
    // Snapshot once; reused for the session card and the streaming flags below.
    let native = crate::session_status::snapshot();
    // GameStream launch, else the first live native session (same order as `/status`).
    let session = st
        .app
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|l| SessionInfo {
            width: l.width,
            height: l.height,
            fps: l.fps,
        })
        .or_else(|| {
            native.first().map(|s| SessionInfo {
                width: s.width,
                height: s.height,
                fps: s.fps,
            })
        });
    let (native_paired_clients, pending_approvals) = st
        .native
        .as_ref()
        .map(|n| (n.status().paired_clients, n.pending().len() as u32))
        .unwrap_or((0, 0));
    Json(LocalSummary {
        version: env!("PUNKTFUNK_VERSION").into(),
        // Either plane, like `/status`; GameStream flags alone miss a native session.
        video_streaming: st.app.streaming.load(Ordering::SeqCst) || !native.is_empty(),
        audio_streaming: st.app.audio_streaming.load(Ordering::SeqCst) || !native.is_empty(),
        session,
        // First native session's name. GameStream launches have no device name, so this stays absent there.
        client_name: native.first().and_then(|s| s.client_name.clone()),
        paired_clients: st
            .app
            .paired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32,
        native_paired_clients,
        pin_pending: gs_pin_pending(&st),
        pending_approvals,
        kept_displays: crate::vdisplay::registry::snapshot()
            .displays
            .iter()
            .filter(|d| d.state == "lingering" || d.state == "pinned")
            .count() as u32,
        // Startup cache (empty if nothing detected / never scanned); not a per-poll process scan.
        conflicts: crate::detect::summary_labels(crate::detect::snapshot()),
        games: crate::session_status::games()
            .into_iter()
            .map(|g| match (g.grace_remaining_s, g.state) {
                (Some(left), _) => {
                    format!("{} (closing in {}:{:02})", g.title, left / 60, left % 60)
                }
                // Untracked: the host cannot follow this process, so say so rather than a bare title.
                (None, "untracked") => format!("{} (not tracked)", g.title),
                (None, _) => g.title,
            })
            .collect(),
    })
}

/// GameStream PIN wait. `false` in a native-only build (pairing does not exist); the field stays so the schema matches across flavors.
#[cfg(feature = "gamestream")]
fn gs_pin_pending(st: &Arc<MgmtState>) -> bool {
    st.app.pairing.pin.awaiting_pin()
}
#[cfg(not(feature = "gamestream"))]
fn gs_pin_pending(_st: &Arc<MgmtState>) -> bool {
    false
}
