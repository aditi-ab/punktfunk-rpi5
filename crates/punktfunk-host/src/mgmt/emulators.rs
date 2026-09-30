//! `/emulators`: the emulators hermir knows, holds, or finds on this host. Listing and
//! preparing are open to plugins — a managed exe is what their launch templates point at, and a
//! launch needs the emulator past its first-run questions — while installing and removing are
//! the operator's, like every install on this host. The work runs on the blocking pool: a
//! download takes as long as it takes.
use super::auth::{AuthLane, PluginIdentity};
use super::shared::*;
use crate::events::{emit, EventKind};
use axum::Extension;

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
pub(crate) struct PrepareEmulatorRequest {
    /// The platform about to play: a catalog id like `ps2`, or an alias (RomM slug, ES-DE
    /// folder, libretro name). Without it only first-run questions are answered.
    #[serde(default)]
    pub platform: Option<String>,
    /// A folder whose files are that platform's firmware. From a plugin, a path relative to its
    /// own state directory (`firmware/ps2`); from the operator, an absolute path.
    #[serde(default)]
    pub firmware_dir: Option<String>,
}

/// One copy of the emulator, and what preparing it did.
#[derive(Serialize, ToSchema)]
pub(crate) struct PreparedCopy {
    /// The program, or `flatpak run <app id>`, as the emulator list names it.
    pub exe: String,
    pub steps: Vec<PreparedStep>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PreparedStep {
    /// `first_run`, `firmware`, `firmware_install`, `players` or `config_root`.
    pub kind: String,
    /// The file the step is about.
    pub target: String,
    /// `applied`, `present`, `skipped` or `failed`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

/// Prepare an emulator for a launch
///
/// Every copy of the emulator on this host answers its first-run questions (a setup wizard, a
/// welcome box) the way clicking through would, gets the platform's firmware — copied into
/// its firmware folder, or installed by the emulator itself — and has this session's pads
/// bound in seat order, which the host undoes when the game exits. Idempotent; each step says
/// what it did, and a platform still missing its firmware says so.
#[utoipa::path(
    post,
    path = "/emulators/{id}/prepare",
    tag = "emulators",
    operation_id = "prepareEmulator",
    params(("id" = String, Path, description = "The catalog id, like `pcsx2`")),
    request_body = PrepareEmulatorRequest,
    responses(
        (status = OK, description = "What each copy's preparation did", body = [PreparedCopy]),
        (status = BAD_REQUEST, description = "A malformed platform, or a firmware folder that isn't one", body = ApiError),
        (status = FORBIDDEN, description = "The firmware folder is outside the calling plugin's state directory", body = ApiError),
        (status = NOT_FOUND, description = "Not in the catalog", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The catalog or the firmware folder could not be read", body = ApiError),
    )
)]
pub(crate) async fn prepare_emulator(
    Path(id): Path<String>,
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<PluginIdentity>>,
    ApiJson(req): ApiJson<PrepareEmulatorRequest>,
) -> Response {
    if let Some(p) = req.platform.as_deref()
        && !valid_platform(p)
    {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a platform id");
    }
    let dir = match req.firmware_dir.as_deref() {
        None => None,
        Some(d) => match firmware_dir_for(
            lane,
            who.map(|Extension(w)| w.0),
            d,
            &pf_paths::config_dir().join("plugin-state"),
        ) {
            Ok(p) => Some(p),
            Err((status, why)) => return api_error(status, why),
        },
    };
    let platform = req.platform;
    let prepared =
        blocking(move || crate::emulators::prepare(&id, platform.as_deref(), dir.as_deref())).await;
    match prepared {
        Ok(Ok(copies)) => Json(
            copies
                .into_iter()
                .map(|(exe, p)| PreparedCopy {
                    exe,
                    steps: p
                        .steps
                        .into_iter()
                        .map(|s| PreparedStep {
                            kind: s.kind,
                            target: s.target.to_string_lossy().into_owned(),
                            outcome: format!("{:?}", s.outcome).to_lowercase(),
                            note: s.note,
                        })
                        .collect(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Ok(Err(e)) => hermir_err(&e, "The emulator wasn't prepared"),
        Err(r) => r,
    }
}

/// A platform id or alias: `ps2`, `ngc`, `Sony - PlayStation 2`.
fn valid_platform(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
}

/// The firmware folder a caller may hand over, resolved. The operator's is absolute and may be
/// anywhere. A plugin's is relative to its own state directory, which its sandbox mounts
/// elsewhere, and must still resolve inside it: the host copies only what that plugin could
/// already write, never a file it merely names.
fn firmware_dir_for(
    lane: AuthLane,
    plugin: Option<String>,
    dir: &str,
    plugin_states: &std::path::Path,
) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
    let missing = (StatusCode::BAD_REQUEST, "The firmware folder doesn't exist");
    let resolve = |p: &std::path::Path| p.canonicalize().ok().filter(|p| p.is_dir()).ok_or(missing);
    let path = std::path::Path::new(dir);
    if lane.is_operator() {
        return if path.is_absolute() {
            resolve(path)
        } else {
            Err(missing)
        };
    }
    let own = plugin
        .and_then(|id| plugin_states.join(id).canonicalize().ok())
        .ok_or((
            StatusCode::FORBIDDEN,
            "Handing over firmware needs a plugin's own token",
        ))?;
    let inside = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !inside {
        return Err((
            StatusCode::FORBIDDEN,
            "A plugin names its firmware folder relative to its own state folder",
        ));
    }
    let real = resolve(&own.join(path))?;
    if !real.starts_with(&own) {
        return Err((
            StatusCode::FORBIDDEN,
            "A plugin may hand over only firmware from its own state folder",
        ));
    }
    Ok(real)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_hands_over_firmware_only_from_its_own_state_folder() {
        let states = tempfile::tempdir().unwrap();
        let own = states.path().join("rom-manager");
        std::fs::create_dir_all(own.join("firmware/ps2")).unwrap();
        std::fs::create_dir_all(states.path().join("other/firmware")).unwrap();
        let plugin = |dir: &str| {
            firmware_dir_for(
                AuthLane::Plugin,
                Some("rom-manager".into()),
                dir,
                states.path(),
            )
            .map_err(|(s, _)| s)
        };
        assert_eq!(
            plugin("firmware/ps2").unwrap(),
            own.join("firmware/ps2").canonicalize().unwrap()
        );
        assert_eq!(plugin("../other/firmware"), Err(StatusCode::FORBIDDEN));
        let absolute = own.join("firmware/ps2").to_string_lossy().into_owned();
        assert_eq!(plugin(&absolute), Err(StatusCode::FORBIDDEN));
        assert_eq!(plugin("firmware/none"), Err(StatusCode::BAD_REQUEST));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(states.path().join("other"), own.join("out")).unwrap();
            assert_eq!(plugin("out/firmware"), Err(StatusCode::FORBIDDEN));
        }
        let shared = firmware_dir_for(AuthLane::Plugin, None, "firmware/ps2", states.path());
        assert_eq!(shared.map_err(|(s, _)| s), Err(StatusCode::FORBIDDEN));
        let operator = firmware_dir_for(AuthLane::Admin, None, &absolute, states.path());
        assert!(operator.is_ok());
    }
}
