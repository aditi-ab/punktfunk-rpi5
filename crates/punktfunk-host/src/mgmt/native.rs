//! Native (punktfunk/1) pairing endpoints: arm/disarm a window, paired-device management, and
//! delegated approval of pending knocks. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use crate::native_pairing::{Access, PairedClient};
use punktfunk_core::quic::{
    GRANT_ALL, GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_FULL, GRANT_PRESET_VIEW_ONLY,
    GRANT_RESERVED,
};

/// Host wall clock, unix seconds — the clock every stored access deadline is expressed in
/// (design §4). The API takes expiry RELATIVE (`expires_in_secs`) and converts here, at handling
/// time, so the client never has to know the host's clock.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The absolute deadline a relative expiry means, saturating rather than wrapping on absurd
/// inputs (a u64 of seconds can overflow i64 arithmetic; "effectively forever" is the only
/// sane reading of such a request).
fn absolute_expiry(expires_in_secs: u64) -> i64 {
    unix_now().saturating_add(i64::try_from(expires_in_secs).unwrap_or(i64::MAX))
}

/// 400 for a mask with reserved bits set. Rejected, never silently cleared (design §3): a console
/// speaking a NEWER grant vocabulary must learn its bit didn't take, not have it vanish.
fn reject_reserved(grants: u32) -> Option<Response> {
    if grants & GRANT_RESERVED != 0 {
        return Some(api_error(
            StatusCode::BAD_REQUEST,
            &format!("grants has reserved bits set (the valid mask is 0x{GRANT_ALL:x})"),
        ));
    }
    None
}

/// The operator's access choice from a request's optional `grants` + `expires_in_secs` pair:
/// `None` when neither field is present (back-compat — the request behaves exactly as it did
/// before grants existed), else an [`Access`] with absent halves defaulted the way the dialogs
/// read (`grants` alone = permanent; `expires_in_secs` alone = full control until then) and the
/// relative expiry converted to the absolute deadline the store keeps. The caller has already
/// run [`reject_reserved`] — this only merges.
fn chosen_access(grants: Option<u32>, expires_in_secs: Option<u64>) -> Option<Access> {
    if grants.is_none() && expires_in_secs.is_none() {
        return None;
    }
    Some(Access {
        grants: grants.unwrap_or(GRANT_ALL),
        expires_unix: expires_in_secs.map(absolute_expiry),
    })
}

/// The display preset a grant mask amounts to: exactly a preset's mask reads as that preset,
/// anything else is `custom`. Derived from the mask (reserved bits ignored, absent = full — the
/// same reading enforcement uses), never stored: two representations of one fact would drift.
fn access_level(grants: Option<u32>) -> &'static str {
    match grants.unwrap_or(GRANT_ALL) & GRANT_ALL {
        m if m == GRANT_PRESET_FULL => "full",
        m if m == GRANT_PRESET_CONTROLLER_ONLY => "controller",
        m if m == GRANT_PRESET_VIEW_ONLY => "view",
        _ => "custom",
    }
}

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
    /// Optional access choice for whichever device completes this window's ceremony: a grant
    /// bitmask (`GRANT_*` bits 0–5). Reserved bits are a 400. Omit (with `expires_in_secs`) for
    /// today's behavior — a new device gets full control, a re-pairing device keeps what it has.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Optional access expiry for the pairing device, in seconds **from now** (relative — the
    /// host stores the absolute deadline). NOT the pairing window's length; that is `ttl_secs`.
    /// Omit for permanent access (when `grants` is set) or preserved access (when neither is).
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
}

/// A paired native (punktfunk/1) client.
#[derive(Serialize, ToSchema)]
pub(crate) struct NativeClient {
    /// The name the client supplied when pairing.
    #[schema(example = "Living Room iPad")]
    name: String,
    /// Hex SHA-256 of the client certificate — its stable id here.
    fingerprint: String,
    /// Grant bitmask (`GRANT_*` bits 0–5). `null` = a record from before grants existed, which
    /// means full control.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Absolute access expiry, unix seconds on the host's wall clock. `null` = permanent. Whether
    /// it has already passed is the reader's arithmetic — an expired device stays listed (shown
    /// as "Expired"), it just isn't authorized.
    expires_unix: Option<i64>,
    /// When access was last granted, unix seconds — display/audit only, never enforced.
    granted_unix: Option<i64>,
    /// The preset this device's mask amounts to, for display: `full` | `controller` | `view` |
    /// `custom`. Derived from `grants` on the host; absent only on hosts older than the field.
    #[schema(example = "controller")]
    access_level: Option<String>,
}

impl NativeClient {
    /// The response payload for a stored record — one place derives `access_level`, so the list,
    /// the approve response, and the access PATCH can never disagree on what a mask is called.
    fn from_record(c: PairedClient) -> NativeClient {
        NativeClient {
            access_level: Some(access_level(c.grants).to_string()),
            name: c.name,
            fingerprint: c.fingerprint,
            grants: c.grants,
            expires_unix: c.expires_unix,
            granted_unix: c.granted_unix,
        }
    }
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
    /// The grant mask this fingerprint is ALREADY stored with, if it was paired before (the
    /// expired-guest re-knock: the approve dialog can offer "re-grant what they had"). `null`
    /// when the device is unknown, or known with a pre-grants record (= full).
    grants: Option<u32>,
    /// The stored record's absolute expiry (unix seconds; likely in the past — that's why it's
    /// knocking). `null` when unknown or permanent.
    expires_unix: Option<i64>,
    /// When the stored record's access was granted (unix seconds). `null` when unknown.
    granted_unix: Option<i64>,
    /// The stored mask's preset name (`full` | `controller` | `view` | `custom`) — `null` for a
    /// device with no stored record, unlike [`NativeClient`] where it is always derivable.
    #[schema(example = "controller")]
    access_level: Option<String>,
}

/// Approve-pending-device request body. Send `{}` to keep the device's own name and — for a
/// re-approved device — its existing access (the full/permanent default for a first pairing).
#[derive(Deserialize, ToSchema)]
pub(crate) struct ApprovePending {
    /// Operator-chosen label for the device (defaults to the name it knocked with).
    #[schema(example = "Living Room TV")]
    name: Option<String>,
    /// Access choice: grant bitmask (`GRANT_*` bits 0–5). Reserved bits are a 400. Omitting BOTH
    /// access fields keeps a re-approved device's stored access; `grants` without
    /// `expires_in_secs` grants permanently.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Access expiry in seconds **from now** (relative — the host stores the absolute deadline
    /// and stamps the grant time). Alone, it means full control until then.
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
}

/// PATCH body for a paired device's access (the console edit sheet: change the preset, extend,
/// "expire now", make permanent). **Partial**: an omitted `grants` keeps the current grants, and
/// omitted expiry fields keep the current expiry — send only what changes.
#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateNativeAccess {
    /// New grant bitmask (`GRANT_*` bits 0–5); reserved bits are a 400. Omit to keep the
    /// device's current grants.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// New expiry in seconds **from now** (relative; the host stores the absolute deadline).
    /// `0` expires the device now. Omit to keep the current expiry. Mutually exclusive with
    /// `clear_expiry` (400).
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
    /// `true` removes the expiry — access becomes permanent. Mutually exclusive with
    /// `expires_in_secs` (400).
    clear_expiry: Option<bool>,
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
/// within `ttl_secs`; the device then appears in the native client list. An access choice
/// (`grants` / `expires_in_secs`) applies to whichever device completes this window's ceremony.
#[utoipa::path(
    post,
    path = "/native/pair/arm",
    tag = "native",
    operation_id = "armNativePairing",
    request_body = ArmNativePairing,
    responses(
        (status = OK, description = "Pairing armed; the response carries the PIN to display", body = NativePairStatus),
        (status = BAD_REQUEST, description = "Reserved grant bits set", body = ApiError),
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
    // Validate the access choice BEFORE arming: a 400 must not leave a window open.
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    let access = chosen_access(req.grants, req.expires_in_secs);
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
    // The window carries the operator's access choice to whichever device completes the ceremony
    // (design §5.7 — the arm dialog is one of the three authorized grant paths); `None` keeps
    // today's behavior (full/permanent for a new device, preserved for a re-pairing one).
    let _pin = np.arm_for(std::time::Duration::from_secs(ttl as u64), bound, access);
    tracing::info!(
        ttl_secs = ttl,
        bound_to_device,
        with_access = access.is_some(),
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
            .map(NativeClient::from_record)
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
            // Revocation reaches a LIVE session too: without this, a mid-stream client kept
            // streaming after its pairing was removed, until it chose to disconnect.
            let stopped =
                crate::session_status::stop_by_fingerprint(&fingerprint.to_ascii_lowercase());
            if stopped > 0 {
                tracing::info!(
                    fingerprint,
                    stopped,
                    "unpair: live native session(s) stopped"
                );
            }
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

/// Update a native client's access
///
/// Partial edit of a paired device's grants/expiry (the console edit sheet: preset change,
/// extend, "expire now", make permanent). Omitted fields keep their current value; the edit
/// reaches the device's live sessions immediately. Not a way to pair a device (404 when the
/// fingerprint isn't in the trust store).
#[utoipa::path(
    patch,
    path = "/native/clients/{fingerprint}",
    tag = "native",
    operation_id = "updateNativeClientAccess",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 of the client certificate (case-insensitive)")
    ),
    request_body = UpdateNativeAccess,
    responses(
        (status = OK, description = "Access updated; the stored record as now in force", body = NativeClient),
        (status = BAD_REQUEST, description = "Reserved grant bits set, or expires_in_secs together with clear_expiry", body = ApiError),
        (status = NOT_FOUND, description = "No paired native client with that fingerprint", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the trust store", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn update_native_client_access(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
    ApiJson(req): ApiJson<UpdateNativeAccess>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    if req.clear_expiry == Some(true) && req.expires_in_secs.is_some() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "expires_in_secs and clear_expiry conflict — send one or the other",
        );
    }
    // PATCH semantics: read the current record to fill whichever halves the request omitted —
    // `set_access` below overwrites the WHOLE access, so the merge happens here.
    let Some(current) = np
        .list()
        .into_iter()
        .find(|c| c.fingerprint.eq_ignore_ascii_case(&fingerprint))
    else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        );
    };
    let access = Access {
        grants: match req.grants {
            Some(g) => g,
            // The current mask, read the way enforcement reads it (absent = full, reserved
            // bits off) — so an expiry-only PATCH re-stores exactly what is in force.
            None => current.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
        },
        expires_unix: if req.clear_expiry == Some(true) {
            None
        } else {
            match req.expires_in_secs {
                Some(s) => Some(absolute_expiry(s)),
                None => current.expires_unix,
            }
        },
    };
    match np.set_access(&fingerprint, access) {
        Ok(true) => {
            tracing::info!(
                fingerprint,
                grants = access.grants,
                expires_unix = access.expires_unix,
                "management API: native client access updated"
            );
            // Read the stored record back for the response: the store stamped `granted_unix`,
            // and the payload must report what is actually in force. (If the device was
            // unpaired in the instant since, fall back to what this edit wrote — the write DID
            // land before the removal.)
            let stored = np
                .list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(&fingerprint))
                .unwrap_or(PairedClient {
                    name: current.name,
                    fingerprint: current.fingerprint,
                    grants: Some(access.grants),
                    expires_unix: access.expires_unix,
                    granted_unix: Some(unix_now()),
                });
            Json(NativeClient::from_record(stored)).into_response()
        }
        // The store's own unknown-fingerprint answer — the record vanished between the read
        // above and the write (an unpair racing this edit).
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

/// Unpair every native client
///
/// The collection form of [`unpair_native_client`]: empties the punktfunk/1 trust store in ONE
/// persisted write (not a loop of them — a failure partway would leave a half-emptied store), and
/// ends every live native session the removed clients own.
///
/// Idempotent, hence a 200 rather than the single unpair's 204/404: an already-empty store
/// satisfies the request, and the count still tells the operator what it meant.
#[utoipa::path(
    delete,
    path = "/native/clients",
    tag = "native",
    operation_id = "unpairAllNativeClients",
    responses(
        (status = OK, description = "Every native client unpaired (possibly none)", body = UnpairAllResult),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the trust store", body = ApiError),
    )
)]
pub(crate) async fn unpair_all_native_clients(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.remove_all() {
        Ok(removed) => {
            // Revocation reaches LIVE sessions too — the same guarantee the single unpair gives,
            // applied across the set.
            let stopped: usize = removed
                .iter()
                .map(|fp| crate::session_status::stop_by_fingerprint(&fp.to_ascii_lowercase()))
                .sum();
            if stopped > 0 {
                tracing::info!(stopped, "unpair-all: live native session(s) stopped");
            }
            let unpaired = removed.len() as u32;
            tracing::info!(unpaired, "management API: all native clients unpaired");
            Json(UnpairAllResult { unpaired }).into_response()
        }
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
    // A knock can come from a fingerprint the store already lists — the expired guest asking
    // again. Surfacing that record's access here lets the approve dialog offer "re-grant what
    // they had" instead of making the operator reconstruct last night's choice.
    let (pending, paired) = match &st.native {
        Some(np) => (np.pending(), np.list()),
        None => (Vec::new(), Vec::new()),
    };
    Json(
        pending
            .into_iter()
            .map(|p| {
                let stored = paired
                    .iter()
                    .find(|c| c.fingerprint.eq_ignore_ascii_case(&p.fingerprint));
                PendingDevice {
                    id: p.id,
                    name: p.name,
                    fingerprint: p.fingerprint,
                    age_secs: p.age_secs,
                    grants: stored.and_then(|c| c.grants),
                    expires_unix: stored.and_then(|c| c.expires_unix),
                    granted_unix: stored.and_then(|c| c.granted_unix),
                    access_level: stored.map(|c| access_level(c.grants).to_string()),
                }
            })
            .collect(),
    )
}

/// Approve a pending device
///
/// Pairs the device's certificate fingerprint — it can connect immediately (no PIN). Optionally
/// relabel it and/or choose its access via the body; send `{}` to keep the name it knocked with
/// and its existing access (full/permanent for a first pairing). The response is the stored
/// record — what is actually in force, not necessarily this request's inputs.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/approve",
    tag = "native",
    operation_id = "approvePendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    request_body = ApprovePending,
    responses(
        (status = OK, description = "Device paired; the stored record as now in force", body = NativeClient),
        (status = BAD_REQUEST, description = "Reserved grant bits set", body = ApiError),
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
    // The approve dialog's access choice (design §5.7 — one of the three authorized grant
    // paths). `None` keeps a re-approved device's existing access, or the full/permanent
    // default for a first pairing — exactly the pre-grants behavior. A reserved-bit choice is
    // refused before anything is consumed — the knock stays pending for a corrected approve.
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    let access = chosen_access(req.grants, req.expires_in_secs);
    match np.approve_pending(id, req.name.as_deref(), access) {
        Ok(Some(client)) => {
            tracing::info!(name = %client.name, fingerprint = %client.fingerprint,
                with_access = access.is_some(),
                "management API: pending device approved (delegated pairing)");
            Json(NativeClient::from_record(client)).into_response()
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
