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
    ///
    /// Do not display this as a device name. Every moonlight-common-c client self-signs with that
    /// same fixed subject, so it identifies the *protocol*, not the device — a list of paired
    /// phones, TVs and handhelds all read identically. [`Self::label`] is the field to show.
    subject: Option<String>,
    /// Operator-assigned display name for this device, if one has been set (`PATCH /clients/{fp}`).
    ///
    /// This is the ONLY thing that can tell two paired Moonlight devices apart in a list, because
    /// their certificates cannot: see [`Self::subject`]. Absent until somebody names the device.
    #[schema(example = "Living Room TV")]
    label: Option<String>,
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
    /// The parked ceremonies. Show these next to the PIN prompt — the operator should answer a
    /// NAMED request, and echo the identity back in the submit so the PIN is addressed to the
    /// ceremony they saw (security-review 2026-08-31 H-4).
    pending: Vec<PendingCeremony>,
}

/// One pairing handshake parked waiting for its PIN.
#[cfg(feature = "gamestream")]
#[derive(Serialize, ToSchema)]
pub(crate) struct PendingCeremony {
    /// The `uniqueid` the Moonlight client sent.
    #[schema(example = "0123456789ABCDEF")]
    uniqueid: String,
    /// Lowercase hex SHA-256 of the client certificate offered for pinning.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: String,
}

/// The PIN Moonlight displays during pairing, optionally addressed to one parked ceremony.
#[cfg(feature = "gamestream")]
#[derive(Deserialize, ToSchema)]
pub(crate) struct SubmitPin {
    /// 1–16 ASCII digits (Moonlight shows 4).
    #[schema(example = "1234")]
    pin: String,
    /// Address the PIN to the ceremony with this `uniqueid` (from the pairing status). Without
    /// it the PIN goes to the sole parked ceremony, and is refused when several are parked.
    #[serde(default)]
    uniqueid: Option<String>,
    /// Address the PIN to the ceremony offering this client-cert fingerprint — disambiguates
    /// two ceremonies claiming one `uniqueid`.
    #[serde(default)]
    fingerprint: Option<String>,
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
    // One read of the label sidecar for the whole list, not one per row.
    let labels = crate::gamestream::load_client_labels();
    Json(ders.iter().map(|der| client_info(der, &labels)).collect())
}

pub(crate) fn client_info(
    der: &[u8],
    labels: &std::collections::BTreeMap<String, String>,
) -> PairedClient {
    let fingerprint = hex::encode(Sha256::digest(der));
    let label = labels.get(&fingerprint).cloned();
    match x509_parser::parse_x509_certificate(der) {
        Ok((_, x509)) => PairedClient {
            subject: Some(x509.subject().to_string()),
            not_before_unix: Some(x509.validity().not_before.timestamp()),
            not_after_unix: Some(x509.validity().not_after.timestamp()),
            label,
            fingerprint,
        },
        Err(_) => PairedClient {
            subject: None,
            not_before_unix: None,
            not_after_unix: None,
            label,
            fingerprint,
        },
    }
}

/// Body of `PATCH /clients/{fingerprint}` — the device's display name.
#[derive(Deserialize, ToSchema)]
pub(crate) struct RenameClient {
    /// The name to show for this device. `null` (or an empty/whitespace-only string) clears it and
    /// the device goes back to being listed by fingerprint alone.
    ///
    /// Scrubbed before storage by the same sanitizer the native plane runs on device names:
    /// control characters and Unicode bidi overrides are stripped (they could make one paired
    /// device impersonate another in this very list), whitespace collapsed, and the result capped
    /// at 64 characters.
    #[schema(example = "Living Room TV")]
    label: Option<String>,
}

/// Rename a paired client
///
/// Sets or clears the operator-visible display name for one paired Moonlight client. This is
/// purely cosmetic — it touches no certificate and no trust decision — but it is the only way to
/// tell paired devices apart: every moonlight-common-c client self-signs with the identical
/// subject `CN=NVIDIA GameStream Client`, so an unnamed list is a row of clones distinguishable
/// only by fingerprint. The name is stored beside the pairing store and survives host restarts;
/// unpairing the device forgets it.
#[utoipa::path(
    patch,
    path = "/clients/{fingerprint}",
    tag = "clients",
    operation_id = "renameClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 fingerprint of the client certificate DER (64 chars, case-insensitive)")
    ),
    request_body = RenameClient,
    responses(
        (status = OK, description = "The client as it now reads", body = PairedClient),
        (status = BAD_REQUEST, description = "Malformed fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No paired client with that fingerprint", body = ApiError),
    )
)]
pub(crate) async fn rename_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
    Json(body): Json<RenameClient>,
) -> Response {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "fingerprint must be the 64-char hex SHA-256 of the client certificate DER",
        );
    }
    // Only name a device that is actually paired: a label for an unknown fingerprint would be
    // invisible (nothing lists it) and would sit in the file forever, since the unpair cleanup
    // only ever removes labels whose device WAS paired.
    let paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    let Some(der) = paired
        .iter()
        .find(|der| hex::encode(Sha256::digest(der)).eq_ignore_ascii_case(&fingerprint))
        .cloned()
    else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no paired client with that fingerprint",
        );
    };
    drop(paired);
    // An all-whitespace name is a cleared name, not a device called "   ": the sanitizer would
    // otherwise turn it into the "device <fp8>" fallback and the row would look renamed.
    let wanted = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    crate::gamestream::set_client_label(&fingerprint, wanted);
    let labels = crate::gamestream::load_client_labels();
    (StatusCode::OK, Json(client_info(&der, &labels))).into_response()
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
        // Forget this device's display name with it, so the file can't grow without bound and a
        // later re-pairing of the same certificate starts unnamed.
        crate::gamestream::retain_client_labels(&paired);
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

/// Unpair every client
///
/// The collection form of [`unpair_client`]: empties the pairing store in ONE persisted write,
/// carrying the same revocation guarantees across the whole set. A LIVE GameStream session is
/// ended (its owning certificate is necessarily one of those just removed), and the ENet control
/// port (UDP 47999) closes, because no pairing is left to hold it open.
///
/// Idempotent, and so a 200 rather than the single unpair's 204/404 pair: "unpair everything" is
/// satisfied by an already-empty store, and the operator still wants to know whether that meant
/// three devices or none.
#[utoipa::path(
    delete,
    path = "/clients",
    tag = "clients",
    operation_id = "unpairAllClients",
    responses(
        (status = OK, description = "Every client unpaired (possibly none)", body = UnpairAllResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn unpair_all_clients(State(st): State<Arc<MgmtState>>) -> Response {
    let mut paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    if paired.is_empty() {
        // Nothing to persist, no port to sync — an empty store is already the requested state.
        return Json(UnpairAllResult { unpaired: 0 }).into_response();
    }
    let removed: Vec<[u8; 32]> = paired
        .iter()
        .map(|der| Sha256::digest(der).into())
        .collect();
    paired.clear();
    // Persist under the lock, as the single unpair does: a pairing resurrected by a restart would
    // silently re-open the control port.
    crate::gamestream::save_paired(&paired);
    // Nothing is paired any more, so no label can still belong to anyone.
    crate::gamestream::retain_client_labels(&paired);
    drop(paired);
    // A mid-stream client must not keep streaming once its pairing is gone. Clearing the launch
    // makes the ENet control thread send the standard TERMINATION+disconnect. (An owner-less
    // launch — the cert was unreadable at /launch — cannot be attributed, and is left to the port
    // teardown below, which here always fires: no pairing remains.)
    let live_owner = st
        .app
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .and_then(|l| l.owner_fp);
    if live_owner.is_some_and(|fp| removed.contains(&fp)) {
        st.app.quit_session("client unpaired");
    }
    if let Err(e) = crate::gamestream::sync_control(&st.app) {
        tracing::warn!(error = %format!("{e:#}"), "control port sync after unpair-all failed");
    }
    let unpaired = removed.len() as u32;
    tracing::info!(unpaired, "management API: all clients unpaired");
    Json(UnpairAllResult { unpaired }).into_response()
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
    let pending: Vec<PendingCeremony> = st
        .app
        .pairing
        .pin
        .pending()
        .into_iter()
        .map(|c| PendingCeremony {
            uniqueid: c.uniqueid,
            fingerprint: c.fingerprint,
        })
        .collect();
    Json(PairingStatus {
        pin_pending: !pending.is_empty(),
        pending,
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
    use crate::gamestream::pairing::SubmitOutcome;
    match st.app.pairing.pin.submit(
        pin.to_string(),
        req.uniqueid.as_deref(),
        req.fingerprint.as_deref(),
    ) {
        SubmitOutcome::Delivered(_) => StatusCode::NO_CONTENT.into_response(),
        // Refusing (rather than parking the PIN) prevents a stale PIN from silently
        // satisfying a *future* pairing attempt.
        SubmitOutcome::NoWaiter => api_error(
            StatusCode::CONFLICT,
            "no pairing handshake is waiting for a PIN",
        ),
        SubmitOutcome::NoMatch => api_error(
            StatusCode::CONFLICT,
            "no waiting handshake matches the given uniqueid/fingerprint — re-read the pairing status",
        ),
        // Which ceremony the operator means is ambiguous; the PIN must be addressed, never
        // handed to whichever racer polls first (security-review 2026-08-31 H-4).
        SubmitOutcome::Ambiguous(_) => api_error(
            StatusCode::CONFLICT,
            "more than one client is waiting to pair — name the target uniqueid/fingerprint from the pairing status",
        ),
    }
}
