//! Display-tagged management endpoints: virtual-display policy, state, layout, and custom
//! presets. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;

/// One preset's human-facing description + the fields it expands to, so the console can render a
/// preset picker with an accurate "what this does" preview without hardcoding the expansion.
#[derive(Serialize, ToSchema)]
pub(crate) struct PresetInfo {
    /// The preset id (`default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation`).
    id: String,
    /// One-line story shown next to the option.
    summary: String,
    /// The effective policy this preset expands to (the same fields a `custom` policy carries).
    fields: crate::vdisplay::policy::EffectivePolicy,
}

/// Full display-management state for the console: the stored policy, every preset's expansion, the
/// resolved effective policy, and which options this build actually enforces yet (Stage 0 wires
/// keep-alive linger + topology; the rest are stored but not yet acted on).
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplaySettingsState {
    /// The stored policy (preset + custom fields), or the built-in default when unconfigured.
    settings: crate::vdisplay::policy::DisplayPolicy,
    /// True once a `display-settings.json` exists (the console has configured this host).
    configured: bool,
    /// The effective (preset-expanded) policy currently in force.
    effective: crate::vdisplay::policy::EffectivePolicy,
    /// Every named preset and what it expands to (for the picker's preview).
    presets: Vec<PresetInfo>,
    /// The operator's saved custom presets (`display-presets.json`) — named field-bundles rendered
    /// alongside the built-ins. Managed via `POST/PUT/DELETE /display/presets`; applied by writing a
    /// `Custom` policy carrying the preset's fields.
    custom_presets: Vec<crate::vdisplay::policy::CustomPreset>,
    /// Option names this build enforces right now. All five axes are now acted on (keep_alive +
    /// topology since Stage 0-2, identity Stage 3, mode_conflict Stage 4, layout Stage 5) — the console
    /// reads this to know which controls are live vs. "coming soon" (per-backend nuance, e.g. layout
    /// position apply being KWin-only, is reported per display in `/display/state`).
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
    DisplaySettingsState {
        effective: settings.effective(),
        settings,
        configured,
        presets,
        custom_presets: policy::load_custom_presets(),
        enforced: vec![
            "keep_alive".into(),
            "topology".into(),
            "mode_conflict".into(),
            "identity".into(),
            "layout".into(),
            "game_session".into(),
            // EXPERIMENTAL, Windows-only in effect: acted on at the `exclusive` isolate
            // (`vdisplay/windows/manager.rs`); stored-but-inert elsewhere.
            "ddc_power_off".into(),
            "pnp_disable_monitors".into(),
        ],
    }
}

/// Display-management policy
///
/// The stored virtual-display policy (lifecycle, topology, conflict handling, identity, layout),
/// every preset's expansion, and which options this build enforces yet. See
/// `design/display-management.md`.
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
/// Persists a new policy (validated + clamped) and applies it from the next connect/teardown — a
/// running session keeps the display it opened on. `keep_alive: forever` (the gaming-rig preset) is
/// honored (the display is Pinned; free it via `POST /display/release`).
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
    // `keep_alive: forever` (the gaming-rig preset) is now honored: the display is Pinned (Linux
    // registry + Windows `MgrState::Pinned`) and freed via `POST /display/release` (the escape hatch).
    if let Err(e) = crate::vdisplay::policy::prefs().set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist display policy: {e:#}"),
        );
    }
    tracing::info!("management API: display policy updated");
    Json(display_settings_state()).into_response()
}

/// One live or kept virtual display.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiDisplayInfo {
    /// Stable-enough id for the `/display/release` `slot` argument.
    slot: u64,
    /// Backend name (`pf-vdisplay`, `kwin`, …).
    backend: String,
    /// `WIDTHxHEIGHT@HZ`.
    mode: String,
    /// `active` | `lingering` | `pinned`.
    state: String,
    /// Milliseconds until a lingering display is torn down (absent when active/pinned).
    expires_in_ms: Option<u64>,
    /// Live sessions holding the display.
    sessions: u32,
    /// Short client label, when the owner tracks it.
    client: Option<String>,
    /// Display group (shared desktop) id — several displays with the same group form one desktop (§6A).
    group: u32,
    /// This display's ordinal within its group, in acquire order (0-based).
    display_index: u32,
    /// Desktop-space top-left `x` (auto-row or the console's manual arrangement, §6.2).
    x: i32,
    /// Desktop-space top-left `y`.
    y: i32,
    /// Stable per-client identity slot keying persistent config + manual layout (absent = shared/anonymous).
    identity_slot: Option<u32>,
    /// Effective topology for this display's group (`extend` | `primary` | `exclusive`).
    topology: String,
}

/// The host's managed virtual displays right now.
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayStateResponse {
    displays: Vec<ApiDisplayInfo>,
}

/// Request body for `releaseDisplay`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ReleaseDisplayRequest {
    /// Slot to release (see `state`); omit to release **all** kept displays.
    #[serde(default)]
    slot: Option<u64>,
}

/// Result of a `/display/release`.
#[derive(Serialize, ToSchema)]
pub(crate) struct ReleaseDisplayResult {
    /// Number of kept displays torn down.
    released: usize,
}

/// Live virtual displays
///
/// The host's managed virtual displays right now — active (streaming), lingering (kept after
/// disconnect, counting down to teardown), or pinned (kept indefinitely). See
/// `design/display-management.md`.
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
/// Tear down lingering/pinned displays now — so a physical-screen user gets their screen back
/// without waiting out the linger. `slot` releases one; omit it to release all kept displays.
/// Active (streaming) displays are never torn down here (that is session control).
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

/// Request body for `setDisplayLayout`: per-identity-slot desktop offsets, keyed by the identity-slot
/// id as a string (the same id `/display/state` reports as `identity_slot`).
#[derive(Deserialize, ToSchema)]
pub(crate) struct DisplayLayoutRequest {
    /// `{"<identity_slot>": {"x": …, "y": …}}` — where each arranged display's top-left sits.
    #[serde(default)]
    positions: std::collections::BTreeMap<String, crate::vdisplay::policy::Position>,
}

/// Arrange virtual displays
///
/// Set the **manual** desktop arrangement — per-identity-slot `(x, y)` offsets so a multi-monitor
/// group (§6A/§6B) comes back where the operator placed it. Persisted into the policy's layout block
/// and switched to manual mode; applied from the next connect (a live group re-applies on its next
/// acquire). Locks in the current effective behavior as explicit fields, so arranging displays never
/// silently changes keep-alive/topology/conflict/identity. See `design/display-management.md` §6.2.
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
    // Lock the current effective behavior into explicit fields + set the manual arrangement (pure
    // transform, unit-tested in `policy.rs`) — so arranging displays is orthogonal to the other policy
    // axes. (`effective` keep_alive is never `Forever` via the API — the settings PUT rejects it.)
    let policy = store.get().effective().with_manual_layout(
        req.positions,
        store.game_session(),
        store.ddc_power_off(),
        store.pnp_disable_monitors(),
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
/// The operator's named field-bundles (`display-presets.json`). These also ride the
/// `GET /display/settings` response (`custom_presets`), so the console rarely needs this directly.
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
/// Stores a named bundle of the display-behavior axes (+ the game-session axis) the operator can
/// apply later. The host assigns a stable id, returned in the body. Applying a preset is a
/// `PUT /display/settings` with a `Custom` policy carrying its `fields` — no separate apply route.
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
/// Removes it from the catalog. The active policy is untouched — if this preset was the one applied,
/// the running behavior stays exactly as it was (the catalog and `display-settings.json` are decoupled).
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
