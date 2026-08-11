//! Client/pairing-tagged management endpoints: paired Moonlight clients and the GameStream
//! pairing PIN flow. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use sha2::{Digest, Sha256};

/// A paired (certificate-pinned) Moonlight client.
#[derive(Serialize, ToSchema)]
pub(crate) struct PairedClient {
    /// Lowercase hex SHA-256 of the client certificate DER — the client's stable id here.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: String,
    /// Certificate subject (e.g. `CN=NVIDIA GameStream Client`), if the DER parses.
    subject: Option<String>,
    /// Certificate validity start (unix seconds).
    not_before_unix: Option<i64>,
    /// Certificate validity end (unix seconds).
    not_after_unix: Option<i64>,
}

/// Pairing-flow status.
#[cfg(feature = "gamestream")]
#[derive(Serialize, ToSchema)]
pub(crate) struct PairingStatus {
    /// True while a pairing handshake is parked waiting for the user's PIN.
    pin_pending: bool,
}

/// The PIN Moonlight displays during pairing.
#[cfg(feature = "gamestream")]
#[derive(Deserialize, ToSchema)]
pub(crate) struct SubmitPin {
    /// 1–16 ASCII digits (Moonlight shows 4).
    #[schema(example = "1234")]
    pin: String,
}

/// List paired clients
#[utoipa::path(
    get,
    path = "/clients",
    tag = "clients",
    operation_id = "listPairedClients",
    responses(
        (status = OK, description = "All certificate-pinned clients", body = [PairedClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_paired_clients(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<PairedClient>> {
    let ders = st
        .app
        .paired
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(ders.iter().map(|der| client_info(der)).collect())
}

pub(crate) fn client_info(der: &[u8]) -> PairedClient {
    let fingerprint = hex::encode(Sha256::digest(der));
    match x509_parser::parse_x509_certificate(der) {
        Ok((_, x509)) => PairedClient {
            fingerprint,
            subject: Some(x509.subject().to_string()),
            not_before_unix: Some(x509.validity().not_before.timestamp()),
            not_after_unix: Some(x509.validity().not_after.timestamp()),
        },
        Err(_) => PairedClient {
            fingerprint,
            subject: None,
            not_before_unix: None,
            not_after_unix: None,
        },
    }
}

/// Unpair a client
///
/// Removes the client's certificate from the pairing store (persisted — the removal survives a
/// host restart). Revocation is complete: a LIVE GameStream session owned by this certificate is
/// ended (the client gets the standard TERMINATION+disconnect), and removing the last pairing
/// also closes the ENet control port (UDP 47999), which is only bound while at least one pairing
/// exists. The nvhttp TLS layer still completes a handshake with any well-formed client cert BY
/// DESIGN (authorization is per-request via the paired-fingerprint check) — an unpaired client
/// that reconnects is rejected at every post-pair endpoint.
#[utoipa::path(
    delete,
    path = "/clients/{fingerprint}",
    tag = "clients",
    operation_id = "unpairClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 fingerprint of the client certificate DER (64 chars, case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = BAD_REQUEST, description = "Malformed fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No paired client with that fingerprint", body = ApiError),
    )
)]
pub(crate) async fn unpair_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "fingerprint must be the 64-char hex SHA-256 of the client certificate DER",
        );
    }
    let mut paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    let before = paired.len();
    paired.retain(|der| !hex::encode(Sha256::digest(der)).eq_ignore_ascii_case(&fingerprint));
    if paired.len() < before {
        // Persist the removal — without this the unpair lasted only until the next host
        // restart, which now also matters below: a resurrected pairing would silently
        // re-open the control port.
        crate::gamestream::save_paired(&paired);
        drop(paired);
        // Revocation reaches a LIVE session too: a mid-stream client whose pairing was just
        // removed must not keep streaming until it chooses to leave. Clearing the launch makes
        // the ENet control thread give it the standard TERMINATION+disconnect farewell. (An
        // owner-less launch — the cert was unreadable at /launch — cannot be attributed and is
        // left to the port teardown below when this was the last pairing.)
        let removed_fp: Option<[u8; 32]> = hex::decode(&fingerprint)
            .ok()
            .and_then(|v| v.try_into().ok());
        let live_owner = st
            .app
            .launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .and_then(|l| l.owner_fp);
        if removed_fp.is_some() && removed_fp == live_owner {
            st.app.quit_session("client unpaired");
        }
        // The last pairing going away closes the ENet control port (rust-safety WP0). A
        // no-op while other pairings remain — or on a native-only host, where the gate is
        // never armed.
        if let Err(e) = crate::gamestream::sync_control(&st.app) {
            tracing::warn!(error = %format!("{e:#}"), "control port sync after unpair failed");
        }
        tracing::info!(fingerprint, "management API: client unpaired");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "no paired client with that fingerprint",
        )
    }
}

/// Pairing-flow status
///
/// Poll this to know when to prompt the user for the PIN Moonlight displays.
#[cfg(feature = "gamestream")]
#[utoipa::path(
    get,
    path = "/pair",
    tag = "pairing",
    operation_id = "getPairingStatus",
    responses(
        (status = OK, description = "Whether a pairing handshake is waiting for a PIN", body = PairingStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_pairing_status(State(st): State<Arc<MgmtState>>) -> Json<PairingStatus> {
    Json(PairingStatus {
        pin_pending: st.app.pairing.pin.awaiting_pin(),
    })
}

/// Submit the pairing PIN
///
/// Delivers the PIN the Moonlight client is displaying, completing the out-of-band half
/// of the pairing handshake.
#[cfg(feature = "gamestream")]
#[utoipa::path(
    post,
    path = "/pair/pin",
    tag = "pairing",
    operation_id = "submitPairingPin",
    request_body = SubmitPin,
    responses(
        (status = NO_CONTENT, description = "PIN delivered to the waiting handshake"),
        (status = BAD_REQUEST, description = "Malformed PIN or unparseable JSON body", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No pairing handshake is waiting for a PIN", body = ApiError),
        (status = UNSUPPORTED_MEDIA_TYPE, description = "Body is not application/json", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "JSON body does not match the schema", body = ApiError),
    )
)]
pub(crate) async fn submit_pairing_pin(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<SubmitPin>,
) -> Response {
    let pin = req.pin.trim();
    if pin.is_empty() || pin.len() > 16 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return api_error(StatusCode::BAD_REQUEST, "pin must be 1-16 ASCII digits");
    }
    if !st.app.pairing.pin.awaiting_pin() {
        // Refusing (rather than parking the PIN) prevents a stale PIN from silently
        // satisfying a *future* pairing attempt.
        return api_error(
            StatusCode::CONFLICT,
            "no pairing handshake is waiting for a PIN",
        );
    }
    st.app.pairing.pin.submit(pin.to_string());
    StatusCode::NO_CONTENT.into_response()
}
