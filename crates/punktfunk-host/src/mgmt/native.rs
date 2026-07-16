//! Native (punktfunk/1) pairing endpoints: arm/disarm a window, paired-device management, and
//! delegated approval of pending knocks. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;

/// Native (punktfunk/1) pairing status. Unlike GameStream, the **host** mints the PIN (the SPAKE2
/// ceremony needs it client-side first), so the console **displays** `pin` for the user to enter on
/// their device — armed on demand for a short window.
#[derive(Serialize, ToSchema)]
pub(crate) struct NativePairStatus {
    /// Whether the native host is running (the unified host started with `--native`).
    enabled: bool,
    /// True while a pairing window is open.
    armed: bool,
    /// The PIN to display while armed (null when disarmed).
    #[schema(example = "1234")]
    pin: Option<String>,
    /// Seconds left in the window (null = disarmed, or armed with no expiry via the CLI flag).
    expires_in_secs: Option<u64>,
    /// Number of paired native clients.
    paired_clients: u32,
}

/// Arm-native-pairing request body.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ArmNativePairing {
    /// Window length in seconds (default 120; clamped to 15–600).
    #[schema(example = 120)]
    ttl_secs: Option<u32>,
    /// Optional: bind the window to ONE device fingerprint (hex SHA-256, e.g. from a pending knock).
    /// When set, only a pairing attempt from that fingerprint consumes the window — so an unpaired
    /// LAN peer can neither pair nor burn a window armed for a specific device (security-review #9).
    /// Omit for an unbound window (any device may use the PIN — trusted-LAN only).
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: Option<String>,
}

/// A paired native (punktfunk/1) client.
#[derive(Serialize, ToSchema)]
pub(crate) struct NativeClient {
    /// The name the client supplied when pairing.
    #[schema(example = "Living Room iPad")]
    name: String,
    /// Hex SHA-256 of the client certificate — its stable id here.
    fingerprint: String,
}

/// An unpaired device that tried to connect while the host requires pairing — awaiting
/// **delegated approval** (approve it here instead of fetching the host PIN out of band).
#[derive(Serialize, ToSchema)]
pub(crate) struct PendingDevice {
    /// Id to address approve/deny (per-process; entries expire after ~10 minutes).
    id: u32,
    /// Best-effort device label (the client's own name, else fingerprint-derived).
    #[schema(example = "Enrico's MacBook")]
    name: String,
    /// Hex SHA-256 of the device's certificate — what approval pins.
    fingerprint: String,
    /// Seconds since the device last knocked.
    age_secs: u64,
}

/// Approve-pending-device request body. Send `{}` to keep the device's own name.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ApprovePending {
    /// Operator-chosen label for the device (defaults to the name it knocked with).
    #[schema(example = "Living Room TV")]
    name: Option<String>,
}

pub(crate) fn native_status(st: &MgmtState) -> NativePairStatus {
    match &st.native {
        Some(np) => {
            let s = np.status();
            NativePairStatus {
                enabled: true,
                armed: s.armed,
                pin: s.pin,
                expires_in_secs: s.expires_in_secs,
                paired_clients: s.paired_clients,
            }
        }
        None => NativePairStatus {
            enabled: false,
            armed: false,
            pin: None,
            expires_in_secs: None,
            paired_clients: 0,
        },
    }
}

/// Native pairing status
///
/// The native (punktfunk/1) pairing window. Poll while armed to show the PIN + countdown.
/// `enabled: false` means this host runs GameStream only (no `--native`).
#[utoipa::path(
    get,
    path = "/native/pair",
    tag = "native",
    operation_id = "getNativePairing",
    responses(
        (status = OK, description = "Native pairing status", body = NativePairStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_native_pairing(State(st): State<Arc<MgmtState>>) -> Json<NativePairStatus> {
    Json(native_status(&st))
}

/// Arm native pairing
///
/// Opens a pairing window and mints a fresh PIN to display. The user enters it on their device
/// within `ttl_secs`; the device then appears in the native client list.
#[utoipa::path(
    post,
    path = "/native/pair/arm",
    tag = "native",
    operation_id = "armNativePairing",
    request_body = ArmNativePairing,
    responses(
        (status = OK, description = "Pairing armed; the response carries the PIN to display", body = NativePairStatus),
        (status = SERVICE_UNAVAILABLE, description = "Native host not available in this process", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn arm_native_pairing(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<ArmNativePairing>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "native host not available in this process",
        );
    };
    let ttl = req.ttl_secs.unwrap_or(120).clamp(15, 600);
    // A bound window (operator selected a specific device) is DoS-proof: only that fingerprint can
    // consume it (#9). An unbound window (no fingerprint) keeps the legacy any-device behavior.
    let bound = req
        .fingerprint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let bound_to_device = bound.is_some();
    let _pin = np.arm_for(std::time::Duration::from_secs(ttl as u64), bound);
    tracing::info!(
        ttl_secs = ttl,
        bound_to_device,
        "management API: native pairing armed"
    );
    Json(native_status(&st)).into_response()
}

/// Disarm native pairing
///
/// Closes the pairing window immediately (no new ceremonies accepted).
#[utoipa::path(
    delete,
    path = "/native/pair",
    tag = "native",
    operation_id = "disarmNativePairing",
    responses(
        (status = NO_CONTENT, description = "Pairing disarmed"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn disarm_native_pairing(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    np.disarm();
    StatusCode::NO_CONTENT.into_response()
}

/// List native paired clients
#[utoipa::path(
    get,
    path = "/native/clients",
    tag = "native",
    operation_id = "listNativeClients",
    responses(
        (status = OK, description = "Paired native clients", body = [NativeClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_native_clients(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<NativeClient>> {
    let clients = match &st.native {
        Some(np) => np
            .list()
            .into_iter()
            .map(|c| NativeClient {
                name: c.name,
                fingerprint: c.fingerprint,
            })
            .collect(),
        None => Vec::new(),
    };
    Json(clients)
}

/// Unpair a native client
///
/// Removes a punktfunk/1 client from the native trust store by fingerprint.
#[utoipa::path(
    delete,
    path = "/native/clients/{fingerprint}",
    tag = "native",
    operation_id = "unpairNativeClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 of the client certificate (case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = NOT_FOUND, description = "No paired native client with that fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn unpair_native_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.remove(&fingerprint) {
        Ok(true) => {
            tracing::info!(fingerprint, "management API: native client unpaired");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not persist trust store: {e}"),
        ),
    }
}

/// List devices awaiting pairing approval
///
/// Unpaired devices that tried to connect while the host requires pairing. Approve one to pair
/// it without a PIN (delegated approval); entries expire after ~10 minutes.
#[utoipa::path(
    get,
    path = "/native/pending",
    tag = "native",
    operation_id = "listPendingDevices",
    responses(
        (status = OK, description = "Devices awaiting approval (empty when none, or when the \
                                     native host is not enabled)", body = Vec<PendingDevice>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_pending_devices(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<PendingDevice>> {
    let pending = st
        .native
        .as_ref()
        .map(|np| np.pending())
        .unwrap_or_default();
    Json(
        pending
            .into_iter()
            .map(|p| PendingDevice {
                id: p.id,
                name: p.name,
                fingerprint: p.fingerprint,
                age_secs: p.age_secs,
            })
            .collect(),
    )
}

/// Approve a pending device
///
/// Pairs the device's certificate fingerprint — it can connect immediately (no PIN). Optionally
/// relabel it via the body; send `{}` to keep the name it knocked with.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/approve",
    tag = "native",
    operation_id = "approvePendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    request_body = ApprovePending,
    responses(
        (status = OK, description = "Device paired", body = NativeClient),
        (status = NOT_FOUND, description = "No pending request with that id (expired?)", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the trust store", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn approve_pending_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u32>,
    ApiJson(req): ApiJson<ApprovePending>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.approve_pending(id, req.name.as_deref()) {
        Ok(Some(client)) => {
            tracing::info!(name = %client.name, fingerprint = %client.fingerprint,
                "management API: pending device approved (delegated pairing)");
            Json(NativeClient {
                name: client.name,
                fingerprint: client.fingerprint,
            })
            .into_response()
        }
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "no pending request with that id (it may have expired — have the device retry)",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not persist trust store: {e}"),
        ),
    }
}

/// Deny a pending device
///
/// Drops the request. Not a blocklist — the device's next attempt knocks again.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/deny",
    tag = "native",
    operation_id = "denyPendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    responses(
        (status = NO_CONTENT, description = "Request dropped"),
        (status = NOT_FOUND, description = "No pending request with that id", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn deny_pending_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u32>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    if np.deny_pending(id) {
        tracing::info!(id, "management API: pending device denied");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "no pending request with that id")
    }
}
