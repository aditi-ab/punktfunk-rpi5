//! Display-tagged management endpoints: policy, live state, physical monitors, layout, and
//! custom presets.
//!
//! A PUT stores the next-session policy; a running session keeps the display it opened on.
//! `keep_alive: forever` pins until `POST /display/release`. Off Linux, `capture_monitor` is
//! dropped on write — there is no mirror backend.
//!
//! See `design/display-management.md` and `design/per-monitor-portal-capture.md`.

use super::shared::*;

/// Picker row. `fields` is the expansion so the console does not hardcode it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PresetInfo {
    /// `default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation`.
    id: String,
    summary: String,
    /// Same fields a `Custom` policy carries.
    fields: crate::vdisplay::policy::EffectivePolicy,
}

/// Stored policy, preset expansions, effective policy, and which options this build enforces.
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplaySettingsState {
    /// Stored policy, or the built-in default when unconfigured.
    settings: crate::vdisplay::policy::DisplayPolicy,
    /// True once `display-settings.json` exists.
    configured: bool,
    effective: crate::vdisplay::policy::EffectivePolicy,
    presets: Vec<PresetInfo>,
    /// Saved custom presets (`display-presets.json`). Apply via a `Custom` policy of their fields.
    custom_presets: Vec<crate::vdisplay::policy::CustomPreset>,
    /// Names this build acts on (live vs coming-soon). Per-backend nuance is on `/display/state`.
    enforced: Vec<String>,
}

pub(crate) fn preset_summary(id: &str) -> &'static str {
    match id {
        "default" => "Good for most setups. Reconnects resume quickly, the stream is the whole desktop, and extra viewers each get their own screen.",
        "gaming-rig" => "For a machine with no monitor that you only stream from. The game keeps running when you disconnect, and whoever connects next takes it over.",
        "shared-desktop" => "For a PC you also use in person. Your real monitors are never blanked or left with a leftover display, and extra viewers each get their own screen.",
        "hotdesk" => "One person at a time — roam between your own devices with an instant reconnect. Anyone else is told the box is busy.",
        "workstation" => "Your multi-monitor daily driver. Displays come back exactly where you arranged them, each client keeps its own settings, and the desktop is yours alone.",
        _ => "",
    }
}

pub(crate) fn display_settings_state() -> DisplaySettingsState {
    use crate::vdisplay::policy::{self, Preset};
    let store = policy::prefs();
    let settings = store.get();
    let configured = store.configured().is_some();
    let presets = [
        ("default", Preset::Default),
        ("gaming-rig", Preset::GamingRig),
        ("shared-desktop", Preset::SharedDesktop),
        ("hotdesk", Preset::Hotdesk),
        ("workstation", Preset::Workstation),
    ]
    .into_iter()
    .filter_map(|(id, p)| {
        policy::preset_fields(p).map(|e| PresetInfo {
            id: id.to_string(),
            summary: preset_summary(id).to_string(),
            fields: e,
        })
    })
    .collect();
    let mut enforced: Vec<String> = vec![
        "keep_alive".into(),
        "topology".into(),
        "mode_conflict".into(),
        "identity".into(),
        "layout".into(),
        "game_session".into(),
        // Windows-only at the exclusive isolate (`vdisplay/windows/manager.rs`); inert elsewhere.
        "ddc_power_off".into(),
        "pnp_disable_monitors".into(),
        // Windows + ADL only; inert without `atiadlxx.dll`. Console hides the toggle unless an AMD GPU is present.
        "edid_lock".into(),
    ];
    // Linux-only: `capture_monitor` needs the MIRROR backend (`vdisplay::open`). Do not
    // advertise it off Linux — a stored pin would never take effect.
    if cfg!(target_os = "linux") {
        enforced.push("capture_monitor".into());
    }
    DisplaySettingsState {
        effective: settings.effective(),
        settings,
        configured,
        presets,
        custom_presets: policy::load_custom_presets(),
        enforced,
    }
}

/// Display-management policy
///
/// Stored policy, preset expansions, and which options this build enforces.
/// See `design/display-management.md`.
#[utoipa::path(
    get,
    path = "/display/settings",
    tag = "display",
    operation_id = "getDisplaySettings",
    responses(
        (status = OK, description = "Stored policy + preset expansions + enforced options", body = DisplaySettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_settings() -> Json<DisplaySettingsState> {
    Json(display_settings_state())
}

/// Set the display-management policy
///
/// Persists (validated + clamped). Applies on the next connect/teardown; a running session keeps
/// the display it opened on. `keep_alive: forever` pins until `POST /display/release`.
#[utoipa::path(
    put,
    path = "/display/settings",
    tag = "display",
    operation_id = "setDisplaySettings",
    request_body = crate::vdisplay::policy::DisplayPolicy,
    responses(
        (status = OK, description = "Policy stored; the new state", body = DisplaySettingsState),
        (status = BAD_REQUEST, description = "Malformed policy body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Policy could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_settings(
    ApiJson(policy): ApiJson<crate::vdisplay::policy::DisplayPolicy>,
) -> Response {
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let mut policy = policy;
    // Off Linux there is no mirror backend. Drop `capture_monitor` rather than 400: this PUT is
    // whole-object, so a stored pin would reject every later save over a field the operator cannot
    // see. Self-heals on write; the response shows what was stored.
    #[cfg(not(target_os = "linux"))]
    if let Some(dropped) = policy.capture_monitor.take() {
        tracing::warn!(
            "management API: ignoring capture_monitor={dropped:?} — streaming a chosen physical \
             monitor is Linux-only (no Windows mirror backend); the pin was NOT stored"
        );
    }
    if let Err(e) = crate::vdisplay::policy::prefs().set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist display policy: {e:#}"),
        );
    }
    tracing::info!("management API: display policy updated");
    // Re-aim absolute input now (and clear it when the pin is cleared); do not wait for a restart.
    #[cfg(target_os = "linux")]
    crate::refresh_capture_monitor_anchor("display policy updated");
    Json(display_settings_state()).into_response()
}

/// One live or kept virtual display.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiDisplayInfo {
    /// Stable-enough id for the `/display/release` `slot` argument.
    slot: u64,
    /// `pf-vdisplay`, `kwin`, …
    backend: String,
    /// `WIDTHxHEIGHT@HZ`.
    mode: String,
    /// `active` | `lingering` | `pinned`.
    state: String,
    /// Milliseconds until a lingering display is torn down (absent when active/pinned).
    expires_in_ms: Option<u64>,
    sessions: u32,
    client: Option<String>,
    /// Shared-desktop group id; same group = one desktop.
    group: u32,
    /// Ordinal within the group, acquire order, 0-based.
    display_index: u32,
    /// Desktop-space top-left (auto-row or manual layout).
    x: i32,
    y: i32,
    /// Per-client identity slot (absent = shared/anonymous). Keys persistent config and manual layout.
    identity_slot: Option<u32>,
    /// Group topology: `extend` | `primary` | `exclusive`.
    topology: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayStateResponse {
    displays: Vec<ApiDisplayInfo>,
}

/// Physical monitor as the compositor reports it.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiMonitorInfo {
    /// Connector (`DP-1`, `HDMI-A-2`) — the value `PUNKTFUNK_CAPTURE_MONITOR` takes.
    connector: String,
    /// Picker label (`make model`, else the connector).
    description: String,
    /// `WIDTHxHEIGHT@HZ` of the current mode (size only when the refresh is unknown).
    mode: String,
    /// Desktop-space top-left. Distinguishes two heads of the same size.
    x: i32,
    y: i32,
    scale: f64,
    primary: bool,
    /// Driven right now. Disabled heads stay listed so they are not missing from the picker.
    enabled: bool,
    /// Best-effort: one of our virtual displays, not a real head. Reliable on KWin only.
    managed: bool,
    /// True when `PUNKTFUNK_CAPTURE_MONITOR` currently names this monitor.
    selected: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MonitorsResponse {
    /// Enumeration source (`kwin`, `mutter`, `windows`), when resolved.
    compositor: Option<String>,
    /// Heads, ordered left-to-right by desktop position.
    monitors: Vec<ApiMonitorInfo>,
    /// Configured pin, even when it matches no head (console can show a dangling pin).
    pinned: Option<String>,
    /// True when this build can stream a chosen physical head.
    ///
    /// Enumeration and capture are separate. Off Linux, heads are listed but there is no
    /// mirror backend (`vdisplay::open` has no Windows arm; `pf-capture` only has
    /// `open_idd_push`). The console treats `false` as a read-only picker.
    pin_supported: bool,
    /// Enumeration failure. `None` with an empty list means the host has no heads.
    error: Option<String>,
}

/// Physical monitors
///
/// Heads this host has, for a capture-pin picker. Read-only; does not create, move, or disable.
/// Managed virtual displays are `/display/state`. See `design/per-monitor-portal-capture.md`.
#[utoipa::path(
    get,
    path = "/display/monitors",
    tag = "display",
    operation_id = "getDisplayMonitors",
    responses(
        (status = OK, description = "The host's physical monitors", body = MonitorsResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_monitors() -> Json<MonitorsResponse> {
    // Effective pin (env override, else stored policy): highlight what sessions will mirror.
    #[cfg(target_os = "linux")]
    let pinned = crate::vdisplay::capture_monitor();
    // No mirror backend. Report `None` even if a pin is stored — highlighting a head nothing will
    // capture is the false signal this field exists to avoid. `pin_supported: false` is the flag.
    #[cfg(not(target_os = "linux"))]
    let pinned: Option<String> = None;
    let pin_supported = cfg!(target_os = "linux");
    // Shells out / D-Bus / Wayland, and on Windows walks CCD (can serialize on the display-config
    // lock). Off the async worker.
    let (compositor, listed) = tokio::task::spawn_blocking(|| {
        // No compositor to detect. Label the CCD walk as `windows` instead of Linux XDG advice.
        #[cfg(windows)]
        {
            (
                Some("windows".to_string()),
                crate::vdisplay::monitors::list_windows(),
            )
        }
        #[cfg(not(windows))]
        match crate::vdisplay::detect() {
            Ok(c) => (Some(c.id().to_string()), crate::vdisplay::monitors::list(c)),
            Err(e) => (None, Err(e)),
        }
    })
    .await
    .unwrap_or_else(|e| (None, Err(anyhow::anyhow!("enumeration task failed: {e}"))));
    let (monitors, error) = match listed {
        Ok(ms) => (
            ms.into_iter()
                .map(|m| ApiMonitorInfo {
                    mode: m.mode_label(),
                    selected: pinned
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&m.connector)),
                    connector: m.connector,
                    description: m.description,
                    x: m.x,
                    y: m.y,
                    scale: m.scale,
                    primary: m.primary,
                    enabled: m.enabled,
                    managed: m.managed,
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    Json(MonitorsResponse {
        compositor,
        monitors,
        pinned,
        pin_supported,
        error,
    })
}

/// Request body for `releaseDisplay`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ReleaseDisplayRequest {
    /// Slot to release (see `state`); omit to release **all** kept displays.
    #[serde(default)]
    slot: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReleaseDisplayResult {
    released: usize,
}

/// Live virtual displays
///
/// Active (streaming), lingering (countdown to teardown), or pinned.
/// See `design/display-management.md`.
#[utoipa::path(
    get,
    path = "/display/state",
    tag = "display",
    operation_id = "getDisplayState",
    responses(
        (status = OK, description = "The live/kept virtual displays", body = DisplayStateResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_state() -> Json<DisplayStateResponse> {
    let snap = crate::vdisplay::registry::snapshot();
    Json(DisplayStateResponse {
        displays: snap
            .displays
            .into_iter()
            .map(|d| ApiDisplayInfo {
                slot: d.slot,
                backend: d.backend,
                mode: format!("{}x{}@{}", d.mode.0, d.mode.1, d.mode.2),
                state: d.state,
                expires_in_ms: d.expires_in_ms,
                sessions: d.sessions,
                client: d.client,
                group: d.group,
                display_index: d.display_index,
                x: d.position.0,
                y: d.position.1,
                identity_slot: d.identity_slot,
                topology: d.topology,
            })
            .collect(),
    })
}

/// Release kept virtual displays
///
/// Tear down lingering/pinned displays now. `slot` releases one; omit to release all.
/// Active (streaming) displays are never torn down here — that is session control.
#[utoipa::path(
    post,
    path = "/display/release",
    tag = "display",
    operation_id = "releaseDisplay",
    request_body = ReleaseDisplayRequest,
    responses(
        (status = OK, description = "The number of kept displays released", body = ReleaseDisplayResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn release_display(
    ApiJson(req): ApiJson<ReleaseDisplayRequest>,
) -> Json<ReleaseDisplayResult> {
    let released = crate::vdisplay::registry::release(req.slot);
    tracing::info!(slot = ?req.slot, released, "management API: display release");
    Json(ReleaseDisplayResult { released })
}

/// Manual layout: identity-slot id as string (same id `/display/state` reports) → desktop offset.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DisplayLayoutRequest {
    /// `{"<identity_slot>": {"x": …, "y": …}}` desktop top-left per slot.
    #[serde(default)]
    positions: std::collections::BTreeMap<String, crate::vdisplay::policy::Position>,
}

/// Arrange virtual displays
///
/// Persist per-identity-slot `(x, y)` offsets and switch the layout to manual. Applies on the next
/// connect (a live group re-applies on its next acquire). Locks current effective behavior into
/// explicit fields so arranging never silently changes keep-alive/topology/conflict/identity.
/// See `design/display-management.md`.
#[utoipa::path(
    put,
    path = "/display/layout",
    tag = "display",
    operation_id = "setDisplayLayout",
    request_body = DisplayLayoutRequest,
    responses(
        (status = OK, description = "Layout stored; the new settings state", body = DisplaySettingsState),
        (status = INTERNAL_SERVER_ERROR, description = "Layout could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_layout(ApiJson(req): ApiJson<DisplayLayoutRequest>) -> Response {
    let store = crate::vdisplay::policy::prefs();
    let policy = store.get().effective().with_manual_layout(
        req.positions,
        store.game_session(),
        store.ddc_power_off(),
        store.pnp_disable_monitors(),
        store.edid_lock(),
        store.get().capture_monitor,
    );
    if let Err(e) = store.set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist display layout: {e:#}"),
        );
    }
    tracing::info!(
        positions = display_settings_state().settings.layout.positions.len(),
        "management API: display layout updated"
    );
    Json(display_settings_state()).into_response()
}

/// List the saved custom presets
///
/// Named field-bundles in `display-presets.json`. Also on `GET /display/settings` as `custom_presets`.
#[utoipa::path(
    get,
    path = "/display/presets",
    tag = "display",
    operation_id = "listCustomPresets",
    responses(
        (status = OK, description = "The saved custom presets", body = Vec<crate::vdisplay::policy::CustomPreset>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_custom_presets() -> Json<Vec<crate::vdisplay::policy::CustomPreset>> {
    Json(crate::vdisplay::policy::load_custom_presets())
}

/// Save a custom preset
///
/// Named bundle of the display-behavior axes. Host assigns a stable id in the body. Apply with
/// `PUT /display/settings` carrying a `Custom` policy of its `fields` — no separate apply route.
#[utoipa::path(
    post,
    path = "/display/presets",
    tag = "display",
    operation_id = "createCustomPreset",
    request_body = crate::vdisplay::policy::CustomPresetInput,
    responses(
        (status = CREATED, description = "Preset created", body = crate::vdisplay::policy::CustomPreset),
        (status = BAD_REQUEST, description = "Empty name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn create_custom_preset(
    ApiJson(input): ApiJson<crate::vdisplay::policy::CustomPresetInput>,
) -> Response {
    if input.name.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "preset name must not be empty");
    }
    match crate::vdisplay::policy::add_custom_preset(input) {
        Ok(preset) => (StatusCode::CREATED, Json(preset)).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Update a custom preset
#[utoipa::path(
    put,
    path = "/display/presets/{id}",
    tag = "display",
    operation_id = "updateCustomPreset",
    params(("id" = String, Path, description = "The custom preset id")),
    request_body = crate::vdisplay::policy::CustomPresetInput,
    responses(
        (status = OK, description = "Preset updated", body = crate::vdisplay::policy::CustomPreset),
        (status = BAD_REQUEST, description = "Empty name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom preset with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn update_custom_preset(
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::vdisplay::policy::CustomPresetInput>,
) -> Response {
    if input.name.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "preset name must not be empty");
    }
    match crate::vdisplay::policy::update_custom_preset(&id, input) {
        Ok(Some(preset)) => Json(preset).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no custom preset with that id"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Delete a custom preset
///
/// Removes it from the catalog. The active policy is untouched — catalog and
/// `display-settings.json` are decoupled.
#[utoipa::path(
    delete,
    path = "/display/presets/{id}",
    tag = "display",
    operation_id = "deleteCustomPreset",
    params(("id" = String, Path, description = "The custom preset id")),
    responses(
        (status = NO_CONTENT, description = "Preset deleted"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom preset with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_custom_preset(Path(id): Path<String>) -> Response {
    match crate::vdisplay::policy::delete_custom_preset(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no custom preset with that id"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
