//! `/emulators`: the emulators hermir knows, holds, or finds on this host. Listing is open to
//! plugins — a managed exe is what their launch templates point at — while installing and
//! removing are the operator's, like every install on this host. The work runs on the blocking
//! pool: a download takes as long as it takes.
use super::shared::*;
use crate::events::{emit, EventKind};

/// One copy of an emulator on this host.
#[derive(Serialize, ToSchema)]
pub(crate) struct EmulatorCopy {
    /// `managed`, `flatpak`, `native` or `portable`.
    pub kind: String,
    /// The program, or `flatpak run <app id>`.
    pub exe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_root: Option<String>,
}

/// What hermir installed, and from where.
#[derive(Serialize, ToSchema)]
pub(crate) struct ManagedEmulator {
    /// `flatpak`, `github` or `url`.
    pub channel: String,
    pub exe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// RFC 3339.
    pub installed_at: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EmulatorStatus {
    pub id: String,
    pub name: String,
    /// Platform ids the emulator plays.
    pub platforms: Vec<String>,
    /// Whether this host's OS has an install channel. A detect-only entry is never offered.
    pub offered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedEmulator>,
    pub detected: Vec<EmulatorCopy>,
    /// The folder a plugin is granted to reach the managed copy.
    pub home: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RemoveEmulatorRequest {
    /// Also delete the emulator's own data (config, saves).
    #[serde(default)]
    pub purge: bool,
}

async fn blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!("emulator worker panicked: {e}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The emulator manager stopped responding",
        )
    })
}

/// A hermir failure as an API error, by what went wrong rather than where.
pub(crate) fn hermir_err(e: &hermir::Error, what: &str) -> Response {
    let status = match e {
        hermir::Error::NotInCatalog(_) => StatusCode::NOT_FOUND,
        hermir::Error::Policy { .. } | hermir::Error::UnsupportedOs { .. } => {
            StatusCode::BAD_REQUEST
        }
        hermir::Error::Network { .. } | hermir::Error::Verify { .. } => StatusCode::BAD_GATEWAY,
        hermir::Error::Locked(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, &format!("{what} — {e}"))
}

fn statuses() -> hermir::Result<Vec<EmulatorStatus>> {
    let h = crate::emulators::open()?;
    let rows = h.status(None, false)?;
    Ok(rows
        .into_iter()
        .map(|s| {
            let platforms = h
                .catalog()
                .get(&s.emulator)
                .map(|e| e.platforms.clone())
                .unwrap_or_default();
            EmulatorStatus {
                home: crate::emulators::home_of(&s.emulator)
                    .to_string_lossy()
                    .into_owned(),
                id: s.emulator,
                name: s.name,
                platforms,
                offered: s.offered,
                managed: s.managed.map(|m| ManagedEmulator {
                    channel: m.channel,
                    exe: m.exe.to_string(),
                    version: m.version,
                    release: m.release,
                    installed_at: m.installed_at,
                }),
                detected: s
                    .detected
                    .into_iter()
                    .map(|d| EmulatorCopy {
                        kind: format!("{:?}", d.kind).to_lowercase(),
                        exe: d.exe.to_string(),
                        version: d.version,
                        config_root: d.config_root.map(|p| p.to_string_lossy().into_owned()),
                    })
                    .collect(),
            }
        })
        .collect())
}

/// List the emulators this host knows
///
/// Every catalog entry: whether this OS can install it, what hermir installed, and the copies
/// the user installed. A plugin may read this to find a managed copy's program.
#[utoipa::path(
    get,
    path = "/emulators",
    tag = "emulators",
    operation_id = "getEmulators",
    responses(
        (status = OK, description = "One row per catalog emulator", body = [EmulatorStatus]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The catalog or the prefix could not be read", body = ApiError),
    )
)]
pub(crate) async fn get_emulators() -> Response {
    match blocking(statuses).await {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(e)) => hermir_err(&e, "The emulators couldn't be listed"),
        Err(r) => r,
    }
}

/// Install an emulator
///
/// Fetches the emulator through its own release channel (Flatpak on Linux, the official
/// portable build on Windows), verifies it, and places it under the host's emulator prefix.
/// Reinstalls an existing copy. Admin lane only.
#[utoipa::path(
    post,
    path = "/emulators/{id}/install",
    tag = "emulators",
    operation_id = "installEmulator",
    params(("id" = String, Path, description = "The catalog id, like `pcsx2`")),
    responses(
        (status = OK, description = "Installed", body = ManagedEmulator),
        (status = BAD_REQUEST, description = "Not offered on this OS, or never installed by policy", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = BAD_GATEWAY, description = "The release could not be fetched or verified", body = ApiError),
        (status = CONFLICT, description = "Another install holds the prefix", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The files could not be placed", body = ApiError),
    )
)]
pub(crate) async fn install_emulator(Path(id): Path<String>) -> Response {
    let target = id.clone();
    match blocking(move || crate::emulators::install(&target)).await {
        Ok(Ok(row)) => {
            emit(EventKind::EmulatorsChanged { id });
            Json(ManagedEmulator {
                channel: row.channel,
                exe: row.exe.to_string(),
                version: row.version,
                release: row.release,
                installed_at: row.installed_at,
            })
            .into_response()
        }
        Ok(Err(e)) => hermir_err(&e, "The emulator didn't install"),
        Err(r) => r,
    }
}

/// Remove a managed emulator
///
/// Takes the release's files away and keeps the emulator's own data unless `purge` is set.
/// A Flatpak is uninstalled. Grants on its folder stay until the operator forgets them.
#[utoipa::path(
    post,
    path = "/emulators/{id}/remove",
    tag = "emulators",
    operation_id = "removeEmulator",
    params(("id" = String, Path, description = "The catalog id")),
    request_body = RemoveEmulatorRequest,
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = CONFLICT, description = "Another install holds the prefix", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Not installed by the host, or the files could not be removed", body = ApiError),
    )
)]
pub(crate) async fn remove_emulator(
    Path(id): Path<String>,
    ApiJson(req): ApiJson<RemoveEmulatorRequest>,
) -> Response {
    let target = id.clone();
    match blocking(move || crate::emulators::remove(&target, req.purge)).await {
        Ok(Ok(())) => {
            emit(EventKind::EmulatorsChanged { id });
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => hermir_err(&e, "The emulator wasn't removed"),
        Err(r) => r,
    }
}
