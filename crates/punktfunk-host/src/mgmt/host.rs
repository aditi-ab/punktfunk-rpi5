//! Host-tagged management endpoints: identity + capabilities, liveness, compositor list, live
//! runtime status, and the loopback tray summary. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use crate::encode::Codec;
use crate::gamestream::APP_VERSION;
use crate::gamestream::AUDIO_PORT;
use crate::gamestream::CONTROL_PORT;
use crate::gamestream::GFE_VERSION;
use crate::gamestream::RTSP_PORT;
use crate::gamestream::VIDEO_PORT;
use std::sync::atomic::Ordering;

/// Liveness + version probe.
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

/// Host identity and advertised capabilities (static for the life of the process).
#[derive(Serialize, ToSchema)]
pub(crate) struct HostInfo {
    hostname: String,
    /// Stable per-host id (persisted across restarts), matched on pairing.
    uniqueid: String,
    /// Best-effort primary LAN IP.
    local_ip: String,
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
    abi_version: u32,
    /// GameStream host version advertised to Moonlight clients.
    app_version: String,
    /// GFE version advertised to Moonlight clients.
    gfe_version: String,
    /// OS identity chain, generic → most specific, slash-separated (`windows` | `macos` |
    /// `linux[/<family>][/<id>]`). A client walks it most-specific-first and shows the first
    /// token it has an icon for, so an unknown distro still degrades to its family's mark.
    #[schema(example = "linux/fedora/bazzite")]
    os: String,
    /// Human-readable OS name (os-release `PRETTY_NAME`; `"Windows"`/`"macOS"` elsewhere).
    #[schema(example = "Bazzite 42 (Kinoite)")]
    os_name: String,
    /// Codecs the host can encode (NVENC).
    codecs: Vec<ApiCodec>,
    /// Whether the GameStream/Moonlight-compat planes are running (`--gamestream`). `false` on the
    /// secure default (native punktfunk/1 only) — a console can hide Moonlight-only UI (e.g. the
    /// Moonlight PIN pairing card, which could never receive a PIN when this is `false`).
    gamestream: bool,
    ports: PortMap,
}

/// Every port a client integration may need (Moonlight derives the stream ports from the
/// HTTP base; a control pane should not have to).
#[derive(Serialize, ToSchema)]
pub(crate) struct PortMap {
    /// This management API.
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

/// Video codec identifier. The wire token matches the codec's canonical name used across the
/// stack (SDP/GameStream advertisement, the stats-capture `CaptureMeta.codec`, and the encoder's
/// [`Codec::label`]) — notably `H.265` serializes as `"hevc"`, not `"h265"`, so the same codec
/// reads identically on every console page.
#[derive(Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApiCodec {
    H264,
    #[serde(rename = "hevc")]
    H265,
    Av1,
    /// PyroWave — the opt-in wired-LAN intra-only wavelet codec.
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

/// Live host status (changes as clients launch/end sessions).
#[derive(Serialize, ToSchema)]
pub(crate) struct RuntimeStatus {
    /// True while the video stream thread is running.
    video_streaming: bool,
    /// True while the audio stream thread is running.
    audio_streaming: bool,
    /// True while a pairing handshake is parked waiting for the user's PIN
    /// (submit it via `POST /api/v1/pair/pin`).
    pin_pending: bool,
    /// Number of pinned (paired) GameStream client certificates. Native (punktfunk/1) devices pair
    /// against a separate store and are counted in `native_paired_clients` — sum the two for
    /// "how many clients are paired with this host".
    paired_clients: u32,
    /// Number of paired native (punktfunk/1) devices — the default plane, so on a host that has
    /// never been touched by Moonlight this is the only non-zero one of the pair.
    native_paired_clients: u32,
    /// Number of live streaming sessions across BOTH planes (GameStream + native punktfunk/1). The
    /// native server admits concurrent sessions, so this can exceed 1; `session`/`stream` below
    /// describe a single representative session for the detail card.
    active_sessions: u32,
    /// A representative active session. GameStream's launch (Moonlight `/launch`) when present, else
    /// the first live native session. `null` when nothing is streaming.
    session: Option<SessionInfo>,
    /// The active stream's parameters — RTSP-negotiated for GameStream, or the live native session's
    /// mode/codec/bitrate. `null` when nothing is streaming.
    stream: Option<StreamInfo>,
    /// Every launched game the host is tracking: one row per live session that launched a title, plus
    /// any game whose session has ended and which is waiting out its reconnect window before being
    /// ended (`state: "grace"`). Empty when nothing was launched — a plain desktop stream has no game.
    games: Vec<ActiveGame>,
}

/// One launched game, for the console's running-game card.
#[derive(Serialize, ToSchema)]
pub(crate) struct ActiveGame {
    /// The session streaming it; `null` for a game waiting out its reconnect window.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    /// Client-supplied device name of the session that launched it; may be empty.
    client: String,
    /// Store-qualified library id (`steam:570`) — the key the console matches against `GET /library`
    /// to show box art. Absent for an operator-typed GameStream command.
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    /// Display title.
    title: String,
    /// Which store surfaced it (`steam`, `heroic`, `custom`, …), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<String>,
    /// `native` or `gamestream`.
    plane: crate::events::Plane,
    /// `launching` (launched, not seen running yet), `running`, `exited`, or `grace` (its session is
    /// gone and it will be ended when the reconnect window closes).
    #[schema(example = "running")]
    state: String,
    /// Seconds until this game is ended — only present on a `grace` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_remaining_s: Option<u64>,
}

/// Client-requested launch parameters (key material is never exposed here).
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionInfo {
    width: u32,
    height: u32,
    fps: u32,
}

/// RTSP-negotiated stream parameters.
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
    /// Session bring-up total, hello → first video packet, in ms (native sessions; `null` on the
    /// GameStream plane or while the session is still bringing up).
    time_to_first_frame_ms: Option<u32>,
    /// Most recent mid-stream resize total, reconfigure → pipeline rebuilt, in ms (native sessions;
    /// `null` when no resize happened / GameStream).
    last_resize_ms: Option<u32>,
}

/// Non-sensitive host status for the local tray icon: counts and booleans only — no PIN values,
/// no fingerprints, no device names. Served unauthenticated to LOOPBACK peers only (see
/// `require_auth`): the bearer-token file is SYSTEM/Administrators-DACL'd on Windows, so the
/// per-user tray process cannot authenticate — this narrow read-only route is its status source.
#[derive(Serialize, ToSchema)]
pub(crate) struct LocalSummary {
    /// Host version (mirrors `/health`).
    version: String,
    /// True while video is streaming on EITHER plane: the GameStream media pipeline, or a live
    /// native (punktfunk/1) session — the default plane, invisible in the GameStream flag alone.
    video_streaming: bool,
    /// True while audio is streaming on either plane (same rule as `video_streaming`).
    audio_streaming: bool,
    /// The active session: GameStream's launch (Moonlight `/launch`) when present, else the first
    /// live native session. `null` when nothing is streaming.
    session: Option<SessionInfo>,
    /// Number of pinned (paired) GameStream client certificates.
    paired_clients: u32,
    /// Number of paired native (punktfunk/1) devices.
    native_paired_clients: u32,
    /// True while a GameStream pairing handshake is parked waiting for the user's PIN.
    pin_pending: bool,
    /// Native pairing knocks awaiting the operator's approval (count only).
    pending_approvals: u32,
    /// Virtual displays being KEPT with no live session — lingering (keep-alive window) or pinned
    /// (`keep_alive: forever`). Non-zero means a display (and, exclusive, your physical monitors) is
    /// held; the tray surfaces it + a one-click release. Active (in-use) displays are not counted.
    kept_displays: u32,
    /// Other Moonlight-compatible hosts (Sunshine/Apollo/…) detected on this machine at startup —
    /// running one alongside Punktfunk is unsupported. Compact labels (e.g. `Sunshine (running)`);
    /// the tray/console surface them so the clash is visible before pairing silently fails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
    /// Launched games the host is tracking, as compact labels (`Hades`, `Hades (closing in 4:12)`).
    ///
    /// The countdown form is the one that matters: it means the game's client is gone and the host
    /// will end the game when the window closes — something a user at the machine should be able to
    /// see (and stop) without opening the console. Empty when nothing was launched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    games: Vec<String>,
}

/// Liveness probe
///
/// Always available without authentication.
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
        local_ip: h.local_ip.to_string(),
        version: env!("PUNKTFUNK_VERSION").into(),
        abi_version: punktfunk_core::ABI_VERSION,
        app_version: APP_VERSION.into(),
        gfe_version: GFE_VERSION.into(),
        os: h.os_chain.clone(),
        os_name: h.os_name.clone(),
        // What this host can ACTUALLY encode on its resolved backend — GPU-aware, straight from the
        // same capability mask that drives GameStream/QUIC negotiation ([`Codec::host_wire_caps`]).
        // So an iGPU without AV1 encode won't advertise AV1, a software-only host reports H.264 only,
        // and PyroWave appears only when its opt-in feature is built and the backend can open.
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

/// A compositor backend the host can drive a virtual output on, and whether it's usable now.
#[derive(Serialize, ToSchema)]
pub(crate) struct AvailableCompositor {
    /// Stable identifier (`"kwin"` | `"wlroots"` | `"mutter"` | `"gamescope"`) — pass this to a
    /// client's `--compositor` flag.
    id: String,
    /// Human-readable label for UIs.
    label: String,
    /// Usable on this host right now: the live session's own compositor, or gamescope wherever
    /// its binary is installed.
    available: bool,
    /// True for the backend an `Auto` (unspecified) request resolves to right now.
    default: bool,
}

/// Available compositor backends
///
/// Lists every backend the host knows how to drive, flags which are usable right now, and marks
/// the one an unspecified (`Auto`) client request resolves to. Clients pass an `id` to their
/// `--compositor` flag (or `PUNKTFUNK_COMPOSITOR_*` over the C ABI) to request it.
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
    // Compositor backends are a Linux concept. On Windows the pf-vdisplay IddCx driver is the sole
    // virtual-display backend and `vdisplay::open` ignores the compositor argument entirely, so the
    // list could only ever be five Linux backends flagged unavailable with no default — which reads
    // as broken detection rather than "not applicable here". Report none and let the console say so.
    #[cfg(not(target_os = "linux"))]
    let list = Vec::new();
    #[cfg(target_os = "linux")]
    // One `/proc` scan backs BOTH columns (see `vdisplay::available`), so a backend can no longer
    // be the auto-detect default and "unavailable" on the same row — the contradiction that made
    // this list look broken on a box plainly running the compositor it called unavailable.
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
    // GameStream plane (set by RTSP/nvhttp on the compat path).
    let gs_launch = *st.app.launch.lock().unwrap_or_else(|e| e.into_inner());
    let gs_stream = *st.app.stream.lock().unwrap_or_else(|e| e.into_inner());
    let gs_video = st.app.streaming.load(Ordering::SeqCst);
    let gs_audio = st.app.audio_streaming.load(Ordering::SeqCst);
    // Native punktfunk/1 plane (published by the native video loop; the default plane). See
    // [`crate::session_status`] for why this lives outside `AppState`.
    let native = crate::session_status::snapshot();

    // Detail card is singular: prefer a live GameStream session, else the first native one.
    // `active_sessions` conveys the true count when several native clients stream at once.
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
    let stream = gs_stream
        .map(|c| StreamInfo {
            width: c.width,
            height: c.height,
            fps: c.fps,
            bitrate_kbps: c.bitrate_kbps,
            packet_size: c.packet_size as u32,
            min_fec: c.min_fec,
            codec: c.codec.into(),
            // Transition latencies are traced on the native plane only (latency plan P0.1).
            time_to_first_frame_ms: None,
            last_resize_ms: None,
        })
        .or_else(|| {
            native.first().map(|s| StreamInfo {
                width: s.width,
                height: s.height,
                fps: s.fps,
                bitrate_kbps: s.bitrate_kbps,
                // FEC/packetization are RTSP-negotiated (GameStream only); the native QUIC plane
                // shards differently, so these are 0 (not applicable) for a native session.
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
        pin_pending: st.app.pairing.pin.awaiting_pin(),
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
    })
}

/// Local status summary for the tray icon
///
/// Non-sensitive status (counts and booleans only — no PIN values, no fingerprints, no device
/// names). Unauthenticated, but served to loopback peers only.
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
    // Native punktfunk/1 plane (the DEFAULT plane; GameStream is opt-in) — read ONCE and used for
    // both the session card and the streaming flags below.
    let native = crate::session_status::snapshot();
    // GameStream launch, else the first live native session — so the tray reflects a native session
    // too (same GameStream-only blind spot the Dashboard `/status` had; see `session_status`).
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
        // Either plane counts, like `/status`: reading only the GameStream flags made the tray say
        // "idle" (and wear the idle icon) through an entire native session.
        video_streaming: st.app.streaming.load(Ordering::SeqCst) || !native.is_empty(),
        audio_streaming: st.app.audio_streaming.load(Ordering::SeqCst) || !native.is_empty(),
        session,
        paired_clients: st
            .app
            .paired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32,
        native_paired_clients,
        pin_pending: st.app.pairing.pin.awaiting_pin(),
        pending_approvals,
        kept_displays: crate::vdisplay::registry::snapshot()
            .displays
            .iter()
            .filter(|d| d.state == "lingering" || d.state == "pinned")
            .count() as u32,
        // Cached at `serve` startup (empty when nothing was detected / never scanned) — no per-poll
        // process enumeration.
        conflicts: crate::detect::summary_labels(crate::detect::snapshot()),
        games: crate::session_status::games()
            .into_iter()
            .map(|g| match g.grace_remaining_s {
                Some(left) => format!("{} (closing in {}:{:02})", g.title, left / 60, left % 60),
                None => g.title,
            })
            .collect(),
    })
}
