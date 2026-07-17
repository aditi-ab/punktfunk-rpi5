//! GPU-tagged management endpoints: inventory + automatic/preferred selection. Split out of the
//! `mgmt` facade (plan §W5).

use super::shared::*;

/// One hardware GPU on the host (software/WARP adapters are never listed).
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiGpu {
    /// Stable identifier (`vendorid-deviceid-occurrence`, hex PCI ids) — pass to `setGpuPreference`.
    /// Stable across reboots and driver updates, unlike an adapter index or LUID.
    #[schema(example = "10de-2c05-0")]
    id: String,
    /// Adapter/marketing name.
    #[schema(example = "NVIDIA GeForce RTX 5070 Ti")]
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// Dedicated VRAM in MiB (0 where the platform doesn't expose it).
    vram_mb: u64,
}

/// The GPU the **next** session's pipeline will be created on, and why. (A preference change
/// applies to the next session; a running session keeps the GPU it opened on.)
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiSelectedGpu {
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// Why this GPU was selected: `preference` (the manual choice), `env`
    /// (`PUNKTFUNK_RENDER_ADAPTER`), `auto` (max dedicated VRAM / platform default), or
    /// `preference_missing` (a manual choice is set but that GPU is absent — auto-selected
    /// instead so the host keeps streaming).
    source: String,
}

/// The GPU live sessions are encoding on right now.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiActiveGpu {
    /// Stable id matching an entry of `gpus` (empty for the CPU/software encoder).
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// The encode backend in use (`nvenc` | `amf` | `qsv` | `vaapi` | `software`).
    backend: String,
    /// Number of live encode sessions on it.
    sessions: u32,
}

/// Full GPU-selection state for the console: inventory, the persisted preference, what the next
/// session will use, and what is in use right now.
#[derive(Serialize, ToSchema)]
pub(crate) struct GpuState {
    /// The host's hardware GPUs.
    gpus: Vec<ApiGpu>,
    /// `auto` or `manual`.
    mode: String,
    /// The manually preferred GPU's stable id, when one is stored (kept while `mode` is `auto` so
    /// a console can offer returning to it). May reference a GPU that is currently absent.
    preferred_id: Option<String>,
    /// The stored name of the preferred GPU (a usable label even when it is absent).
    preferred_name: Option<String>,
    /// Whether the preferred GPU is currently present.
    preferred_available: bool,
    /// `PUNKTFUNK_RENDER_ADAPTER` (the host.env pin), when set — it applies while `mode` is
    /// `auto`; a manual preference overrides it.
    env_override: Option<String>,
    /// The GPU the next session will use.
    selected: Option<ApiSelectedGpu>,
    /// The GPU live sessions use right now (absent while nothing is streaming).
    active: Option<ApiActiveGpu>,
}

/// Request body for `setGpuPreference`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SetGpuPreference {
    /// `auto` (env pin, else max dedicated VRAM — the default) or `manual`.
    #[schema(example = "manual")]
    mode: String,
    /// Required when `mode` is `manual`: the stable `id` of a currently listed GPU
    /// (see `listGpus`).
    #[schema(example = "10de-2c05-0")]
    gpu_id: Option<String>,
}

/// Build the [`GpuState`] snapshot (shared by the GET and the PUT's response).
pub(crate) fn gpu_state() -> GpuState {
    let gpus = pf_gpu::enumerate();
    let pref = pf_gpu::prefs().get();
    let (preferred_id, preferred_name, preferred_available) = match &pref.gpu {
        Some(want) => {
            let found = pf_gpu::find_preferred(&gpus, want);
            let id = match found {
                // Canonical: the present GPU's id (identity may have matched loosely).
                Some(i) => gpus[i].id.clone(),
                None => format!(
                    "{:04x}-{:04x}-{}",
                    want.vendor_id, want.device_id, want.occurrence
                ),
            };
            let name = match found {
                Some(i) => gpus[i].name.clone(),
                None => want.name.clone(),
            };
            (Some(id), Some(name), found.is_some())
        }
        None => (None, None, false),
    };
    let selected = pf_gpu::selected_gpu().map(|sel| ApiSelectedGpu {
        vendor: sel.info.vendor_tag().into(),
        id: sel.info.id,
        name: sel.info.name,
        source: sel.source.tag().into(),
    });
    let active = pf_gpu::active().and_then(|(g, sessions)| {
        (sessions > 0).then(|| ApiActiveGpu {
            vendor: pf_gpu::vendor_tag(g.vendor_id).into(),
            id: g.id,
            name: g.name,
            backend: g.backend.into(),
            sessions,
        })
    });
    GpuState {
        gpus: gpus
            .into_iter()
            .map(|g| ApiGpu {
                vendor: g.vendor_tag().into(),
                vram_mb: g.vram_bytes / (1024 * 1024),
                id: g.id,
                name: g.name,
            })
            .collect(),
        mode: match pref.mode {
            pf_gpu::GpuMode::Auto => "auto".into(),
            pf_gpu::GpuMode::Manual => "manual".into(),
        },
        preferred_id,
        preferred_name,
        preferred_available,
        env_override: pf_host_config::config()
            .render_adapter
            .clone()
            .filter(|s| !s.is_empty()),
        selected,
        active,
    }
}

/// GPU inventory and selection
///
/// Lists the host's hardware GPUs, the persisted auto/manual preference, the GPU the next session
/// will use (and why), and the GPU live sessions encode on right now.
#[utoipa::path(
    get,
    path = "/gpus",
    tag = "gpu",
    operation_id = "listGpus",
    responses(
        (status = OK, description = "GPU inventory + selection state", body = GpuState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_gpus() -> Json<GpuState> {
    Json(gpu_state())
}

/// Set the GPU preference
///
/// `auto` restores automatic selection (`PUNKTFUNK_RENDER_ADAPTER` pin, else max dedicated VRAM);
/// `manual` pins capture + encode to the given GPU. Persisted across restarts; applies to the
/// **next** session (a running session keeps its GPU). If the preferred GPU is absent at session
/// start the host falls back to automatic selection rather than failing.
#[utoipa::path(
    put,
    path = "/gpus/preference",
    tag = "gpu",
    operation_id = "setGpuPreference",
    request_body = SetGpuPreference,
    responses(
        (status = OK, description = "Preference stored; the new selection state", body = GpuState),
        (status = BAD_REQUEST, description = "Unknown mode, or `gpu_id` missing / not a listed GPU", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Preference could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_gpu_preference(ApiJson(req): ApiJson<SetGpuPreference>) -> Response {
    let pref = match req.mode.to_ascii_lowercase().as_str() {
        "auto" => {
            // Keep the stored manual pick so the console can offer switching back to it.
            let mut p = pf_gpu::prefs().get();
            p.mode = pf_gpu::GpuMode::Auto;
            p
        }
        "manual" => {
            let Some(id) = req
                .gpu_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return api_error(StatusCode::BAD_REQUEST, "mode `manual` requires `gpu_id`");
            };
            let Some(g) = pf_gpu::enumerate().into_iter().find(|g| g.id == id) else {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "gpu_id does not match a present GPU (see GET /gpus)",
                );
            };
            pf_gpu::GpuPreference {
                mode: pf_gpu::GpuMode::Manual,
                gpu: Some(pf_gpu::PreferredGpu {
                    vendor_id: g.vendor_id,
                    device_id: g.device_id,
                    occurrence: g.occurrence,
                    name: g.name,
                }),
            }
        }
        other => {
            return api_error(
                StatusCode::BAD_REQUEST,
                &format!("unknown mode {other:?} — use `auto` or `manual`"),
            )
        }
    };
    if let Err(e) = pf_gpu::prefs().set(pref) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist GPU preference: {e:#}"),
        );
    }
    tracing::info!(mode = %req.mode, gpu_id = ?req.gpu_id, "management API: GPU preference updated");
    Json(gpu_state()).into_response()
}
