//! User-curated custom store: CRUD (add/update/delete) over the persisted custom entries the web
//! console manages, and their mapping onto the uniform `GameEntry`. Split out of the `library` facade (plan §W5).

use super::*;

/// A user-added title, persisted in the hardened host config dir's `library.json` (see
/// [`custom_path`]). Same shape the API returns and the web console edits.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CustomEntry {
    /// Host-assigned, stable for the life of the entry (the `{id}` in the CRUD path).
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// Per-title prep/undo steps (RFC §6): each `do` runs before this title launches, each
    /// `undo` at session end in reverse order (see [`crate::hooks::run_prep`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prep: Vec<crate::hooks::PrepCmd>,
    /// The external provider owning this entry (RFC §8), set ONLY by the provider reconcile
    /// API — `None` = a manual entry, which no provider operation ever touches, and which the
    /// manual CRUD alone may edit (the converse holds too: manual CRUD refuses provider-owned
    /// entries, so ownership is never ambiguous).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The provider's own stable key for this title — the reconcile diff key, so the
    /// host-assigned `id` stays stable across reconciles. Present iff `provider` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// The **store this entry was claimed under** (D2), stamped by a `?store=`-qualified reconcile.
    /// `None` = an unclaimed provider entry or a manual one, both of which surface as `custom`.
    ///
    /// Materialized onto the entry rather than looked up in [`Catalog::claims`] on every read so an
    /// entry is self-describing: its id and its `store` badge derive from the entry alone, and stay
    /// correct even while the claim map is being rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Whether this entry is a game or the launcher itself — see [`GameRole`].
    #[serde(default, skip_serializing_if = "GameRole::is_game")]
    pub role: GameRole,
    /// How to recognize this title's process once it is running (design §9) — the one thing a
    /// provider knows that the host cannot work out for itself.
    ///
    /// Optional: without it the entry is still tracked by the child the host spawns for it, which
    /// covers every command that stays in the foreground. It earns its keep for a command that hands
    /// off and exits — a launcher script, a `flatpak run`, a front-end that starts an emulator — where
    /// the host would otherwise lose the game the moment the shim returns.
    #[serde(default, skip_serializing_if = "DetectHint::is_empty")]
    pub detect: DetectHint,
    /// Descriptive metadata (platform, description, …), flattened — see [`GameMeta`].
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// Request body to create or replace a custom entry (no `id` — the host owns it).
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomInput {
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub launch: Option<LaunchSpec>,
    /// Per-title prep/undo steps — commands run as the host user; operator-privileged config.
    #[serde(default)]
    pub prep: Vec<crate::hooks::PrepCmd>,
    /// Whether this entry is a game or the launcher itself — see [`GameRole`]. A hand-added launcher
    /// entry is legal (an operator may want a "Steam" tile without installing the steam plugin).
    #[serde(default)]
    pub role: GameRole,
    /// How to recognize this title's process — see [`CustomEntry::detect`].
    #[serde(default)]
    pub detect: DetectHint,
    /// Descriptive metadata (platform, description, …), flattened — see [`GameMeta`]. Replaced
    /// wholesale on update, like `art`: an edit must round-trip every field it wants kept.
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// One title in a provider's declarative reconcile payload (RFC §8): [`CustomInput`] plus the
/// provider's required stable key.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct ProviderEntryInput {
    /// The provider's stable id for this title (the reconcile diff key).
    pub external_id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub launch: Option<LaunchSpec>,
    /// Per-title prep/undo steps — commands run as the host user; operator-privileged config.
    #[serde(default)]
    pub prep: Vec<crate::hooks::PrepCmd>,
    /// Whether this entry is a game or the launcher itself — see [`GameRole`]. A library plugin
    /// emits its `launchers(cfg)` entries with `role: "launcher"`.
    #[serde(default)]
    pub role: GameRole,
    /// How to recognize this title's process — see [`CustomEntry::detect`]. A provider that knows its
    /// titles' install directories (Playnite does) should send them: it is what lets a game launched
    /// through the provider's own client still end its session when the player quits.
    #[serde(default)]
    pub detect: DetectHint,
    /// Descriptive metadata (platform, description, …), flattened — see [`GameMeta`].
    #[serde(flatten)]
    pub meta: GameMeta,
}

impl From<CustomEntry> for GameEntry {
    fn from(c: CustomEntry) -> Self {
        // A custom/provider entry is spawned by the host itself, so its own child process is the
        // primary lifetime signal; the spec is the fallback for a command that hands off and exits (a
        // launcher script, a `flatpak run`). An absolute exe in the command line is all that can be
        // *inferred*; anything sharper has to be stated, which is what `detect` is for (design §9).
        // The inferred exe wins where both exist — it is derived from the very command being run.
        let detect = c
            .launch
            .as_ref()
            .filter(|l| l.kind == "command")
            .map(|l| crate::library::spec_from_command(&l.value))
            .unwrap_or_default()
            .or_hint(&c.detect);
        GameEntry {
            id: library_id_for(&c),
            // A claimed entry wears its store's badge; everything else is `custom`. `provider` rides
            // along either way, so attribution ("synced by the steam plugin") survives the claim.
            store: c.store.clone().unwrap_or_else(|| "custom".into()),
            title: c.title,
            art: c.art,
            role: c.role,
            launch: c.launch,
            provider: c.provider,
            detect,
            meta: c.meta,
        }
    }
}

fn custom_path() -> PathBuf {
    // The shared, hardened host config dir (`%ProgramData%\punktfunk` / `~/.config/punktfunk`, with
    // the `PUNKTFUNK_CONFIG_DIR` override) — NOT a bespoke XDG/HOME resolver with a CWD-relative
    // fallback. This file drives operator `prep`/`launch` command execution, so it must live where the
    // rest of the privileged host config does and be DACL/0600-locked against a non-privileged local
    // user planting one (security-review 2026-07-17). Matches hooks.json / the mgmt token.
    pf_paths::config_dir().join("library.json")
}

/// The persisted catalog (`library.json` **v2**): the entries plus the store-claim map (D2).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub entries: Vec<CustomEntry>,
    /// `store id → provider id`. One provider per store; a second claimant is refused (409).
    ///
    /// The map — not the entries — is the authority for a claim, which is exactly why it survives an
    /// **empty reconcile**: a store the plugin legitimately owns can have zero installed titles, and
    /// the built-in scanner it suppresses must stay suppressed anyway. Releasing is explicit
    /// (`DELETE /library/provider/{p}`, or the plugin claiming a different store).
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
}

/// What `library.json` may contain on disk. v1 was a bare array of entries; v2 is the [`Catalog`]
/// object. Untagged, so an existing v1 file loads unchanged — and the host always WRITES v2, so the
/// first mutation after an upgrade migrates the file in place with no separate migration step.
#[derive(Deserialize)]
#[serde(untagged)]
enum LibraryFile {
    V2(Catalog),
    Legacy(Vec<CustomEntry>),
}

/// Load the whole catalog (default + non-fatal if the file is absent or malformed).
pub fn load_catalog() -> Catalog {
    match std::fs::read_to_string(custom_path()) {
        Ok(raw) => match serde_json::from_str::<LibraryFile>(&raw) {
            Ok(LibraryFile::V2(c)) => c,
            Ok(LibraryFile::Legacy(entries)) => Catalog {
                entries,
                claims: BTreeMap::new(),
            },
            Err(e) => {
                tracing::warn!(error = %e, "library.json malformed — ignoring custom entries");
                Catalog::default()
            }
        },
        Err(_) => Catalog::default(),
    }
}

/// Load just the entries — the read path every library surface uses.
pub fn load_custom() -> Vec<CustomEntry> {
    load_catalog().entries
}

/// The active store claims (`store → provider`). Read per library scan to suppress the built-in
/// scanner a plugin has taken over (D2).
pub fn claimed_stores() -> BTreeMap<String, String> {
    load_catalog().claims
}

/// The library id a stored entry surfaces as. **The single source of truth for the mapping** —
/// [`From<CustomEntry> for GameEntry`] and every id→entry lookup go through it, so the id scheme
/// can't drift between the catalog, the art proxy and the launch resolver.
///
/// A **claimed** entry (D2) gets the deterministic `<store>:<external_id>` its built-in scanner used
/// to produce — `steam:440`, `heroic:legendary:Quail` — so entry ids, GameStream FNV-1a app ids,
/// client art caches and Moonlight pins all survive the migration to a plugin untouched. That is the
/// whole point of the claim: extraction must be invisible to everything downstream. An unclaimed
/// entry keeps the opaque host-assigned `custom:<id>`.
pub(crate) fn library_id_for(e: &CustomEntry) -> String {
    match (e.store.as_deref(), e.external_id.as_deref()) {
        (Some(store), Some(external)) => format!("{store}:{external}"),
        _ => format!("custom:{}", e.id),
    }
}

/// The **source id** an entry is toggled by (WP2.6): its claimed store when it has one, else its
/// provider id. `None` for a manual entry — the custom store is not a source and can never be
/// switched off. Since the claimed store id, the provider id and the old scanner id are all the same
/// string by construction, a user's existing disabled state carries over verbatim.
pub(crate) fn source_id_for(e: &CustomEntry) -> Option<&str> {
    e.store.as_deref().or(e.provider.as_deref())
}

/// The stored entry a full **library id** refers to, or `None`. The art proxy resolves *any* id this
/// way before falling back to the legacy per-store branches (WP1.2), which is what lets a plugin's
/// entries be served regardless of what their ids look like.
pub fn entry_for_library_id(library_id: &str) -> Option<CustomEntry> {
    load_custom()
        .into_iter()
        .find(|e| library_id_for(e) == library_id)
}

/// Serve a stored entry's **local** art file for one [`ArtKind`] — the `library.json` branch of the
/// art proxy (`GET /library/art/<library id>/<kind>`). `None` if the id names no stored entry, it has
/// no art of that kind, or that art value isn't a servable local file (e.g. an `http` URL the client
/// fetches itself). Blocking IO — call off the async runtime.
pub fn library_local_art_bytes(library_id: &str, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    let field = art_field(&entry_for_library_id(library_id)?.art, kind)?;
    is_local_art_path(&field)
        .then(|| local_art_bytes(&field))
        .flatten()
}

/// One [`Artwork`] field by kind — the tiny mapping the proxy and the box-art ladder share.
pub(crate) fn art_field(art: &Artwork, kind: ArtKind) -> Option<String> {
    match kind {
        ArtKind::Portrait => art.portrait.clone(),
        ArtKind::Hero => art.hero.clone(),
        ArtKind::Logo => art.logo.clone(),
        ArtKind::Header => art.header.clone(),
    }
}

/// Persist the catalog in the **v2** shape (write-then-rename, restrictive perms). Every mutation
/// path funnels through here, so a v1 file is upgraded by the first write.
fn save_catalog(catalog: &Catalog) -> Result<()> {
    let dir = pf_paths::config_dir();
    // Owner-private dir (0700 / SYSTEM+Admins DACL) so a non-privileged local user can't plant a
    // library.json whose `prep`/`launch` commands the host would later execute — the same trust
    // boundary hooks.json and the mgmt token already use.
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(catalog)?;
    // Write-then-rename so a crash mid-write never truncates the catalog; `write_secret_file` gives
    // the temp file its restrictive perms (0600 / SYSTEM+Admins DACL) before the rename carries them
    // to the final path.
    let tmp = custom_path().with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, custom_path()).context("rename library.json")?;
    Ok(())
}

/// 12 hex chars from the title + wall-clock nanos — collision-free in practice, no uuid dep.
fn new_id(title: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(&Sha256::digest(format!("{title}:{nanos}").as_bytes())[..6])
}

/// Outcome of a mutation — distinguishes "no such entry" from the two conflict cases the mgmt
/// layer maps to 409 rather than 404.
pub enum MutateOutcome<T> {
    Done(T),
    NotFound,
    /// The entry belongs to this provider — mutate it through the provider reconcile API
    /// (or remove the whole provider set); manual edits would be clobbered at the next sync.
    ProviderOwned(String),
    /// The requested store claim is already held by a DIFFERENT provider (D2: one provider per
    /// store). Refusing is the point — two plugins both emitting `steam:440` would collide on entry
    /// ids, so the second claimant is told who holds it instead of silently taking over.
    StoreClaimed {
        store: String,
        provider: String,
    },
}

/// Create a custom (manual) entry, returning it with its assigned id.
pub fn add_custom(input: CustomInput) -> Result<CustomEntry> {
    let mut catalog = load_catalog();
    let entry = CustomEntry {
        id: new_id(&input.title),
        title: input.title,
        art: input.art,
        launch: input.launch,
        prep: input.prep,
        provider: None,
        external_id: None,
        store: None,
        role: input.role,
        detect: input.detect,
        meta: input.meta,
    };
    catalog.entries.push(entry.clone());
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(entry)
}

/// Replace a manual entry's fields (id preserved). Provider-owned entries are refused —
/// their state belongs to the provider's reconcile (RFC §8 ownership rule).
pub fn update_custom(id: &str, input: CustomInput) -> Result<MutateOutcome<CustomEntry>> {
    let mut catalog = load_catalog();
    let Some(slot) = catalog.entries.iter_mut().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &slot.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    slot.title = input.title;
    slot.art = input.art;
    slot.launch = input.launch;
    slot.prep = input.prep;
    slot.role = input.role;
    slot.detect = input.detect;
    slot.meta = input.meta;
    let updated = slot.clone();
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(MutateOutcome::Done(updated))
}

/// Delete a manual entry. Provider-owned entries are refused (see [`update_custom`]).
pub fn delete_custom(id: &str) -> Result<MutateOutcome<()>> {
    let mut catalog = load_catalog();
    let Some(entry) = catalog.entries.iter().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &entry.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    catalog.entries.retain(|e| e.id != id);
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(MutateOutcome::Done(()))
}

// ------------------------------------------------------------------ providers (RFC §8)

/// Provider ids are path segments, event sources, and console labels: keep them tame.
/// `manual` is reserved (it is the no-provider sentinel in `library.changed`).
pub fn validate_provider_name(provider: &str) -> Result<(), String> {
    if provider == "manual" {
        return Err("provider id `manual` is reserved".into());
    }
    let ok = !provider.is_empty()
        && provider.len() <= 64
        && provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && provider.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err("provider id must be 1–64 chars of [a-z0-9._-], starting alphanumeric".into())
    }
}

/// Store claims become the **prefix of every claimed entry's library id**, so they are far more
/// constrained than a provider name: no dots (an id is split on the first `:`, and a dotted store
/// would read as a hostname in logs), and the two host-owned namespaces are off-limits — `custom` is
/// the unclaimed-entry namespace and `manual` is the no-provider sentinel in `library.changed`.
pub fn validate_store_claim(store: &str) -> Result<(), String> {
    if store == "custom" || store == "manual" {
        return Err(format!("store id `{store}` is reserved"));
    }
    let ok = !store.is_empty()
        && store.len() <= 32
        && store
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err("store id must be 1–32 chars of [a-z0-9_-]".into())
    }
}

/// Validate a reconcile payload: non-empty titles and unique, non-empty external ids (the
/// diff key — a duplicate would make ownership of the surviving entry ambiguous).
pub fn validate_provider_payload(inputs: &[ProviderEntryInput]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for (i, e) in inputs.iter().enumerate() {
        if e.external_id.trim().is_empty() {
            return Err(format!("entries[{i}]: `external_id` must not be empty"));
        }
        if e.title.trim().is_empty() {
            return Err(format!("entries[{i}]: `title` must not be empty"));
        }
        if !seen.insert(e.external_id.as_str()) {
            return Err(format!(
                "entries[{i}]: duplicate `external_id` \"{}\"",
                e.external_id
            ));
        }
        // Closed-vocabulary launch kinds are checked on the way IN as well as at launch time, so a
        // plugin gets a 400 it can act on rather than a tile that silently refuses to start.
        if let Some(launch) = &e.launch {
            if launch.kind == "steam_ui" && !valid_steam_ui(&launch.value) {
                return Err(format!(
                    "entries[{i}]: `launch.value` for kind `steam_ui` must be `bigpicture` or `desktop`"
                ));
            }
        }
        if let Some(marker) = &e.detect.env_marker {
            if !valid_env_key(&marker.key) {
                return Err(format!(
                    "entries[{i}]: `detect.env_marker.key` must be 1–64 chars of [A-Za-z0-9_]"
                ));
            }
            if marker
                .value
                .as_ref()
                .is_some_and(|v| v.len() > MAX_ENV_VALUE)
            {
                return Err(format!(
                    "entries[{i}]: `detect.env_marker.value` must be at most {MAX_ENV_VALUE} chars"
                ));
            }
        }
    }
    Ok(())
}

/// The pure reconcile (unit-tested without the filesystem): replace `provider`'s entry set
/// with `inputs` inside `entries` — keeping each surviving title's host id stable (keyed on
/// `external_id`), dropping the provider's orphans, and never touching manual entries or
/// other providers'. Returns the provider's resulting entries, payload order.
fn reconcile_entries(
    entries: &mut Vec<CustomEntry>,
    provider: &str,
    store: Option<&str>,
    inputs: Vec<ProviderEntryInput>,
) -> Vec<CustomEntry> {
    // The provider's current entries, keyed by its own stable id.
    let mut existing: std::collections::HashMap<String, CustomEntry> = entries
        .iter()
        .filter(|e| e.provider.as_deref() == Some(provider))
        .filter_map(|e| e.external_id.clone().map(|x| (x, e.clone())))
        .collect();
    // Everything the provider does NOT own survives untouched.
    entries.retain(|e| e.provider.as_deref() != Some(provider));
    let mut result = Vec::with_capacity(inputs.len());
    for input in inputs {
        let id = existing
            .remove(&input.external_id)
            .map(|prev| prev.id) // same title as last sync → keep its host id
            .unwrap_or_else(|| new_id(&format!("{provider}:{}", input.external_id)));
        result.push(CustomEntry {
            id,
            title: input.title,
            art: input.art,
            launch: input.launch,
            prep: input.prep,
            provider: Some(provider.to_string()),
            external_id: Some(input.external_id),
            // Stamping the claim per entry is what makes the surfaced id deterministic
            // (`<store>:<external_id>`) — see `library_id_for`.
            store: store.map(str::to_string),
            role: input.role,
            detect: input.detect,
            meta: input.meta,
        });
    }
    // `existing`'s leftovers are the orphans — deliberately dropped (declarative reconcile).
    entries.extend(result.iter().cloned());
    result
}

/// Atomically replace `provider`'s entry set (RFC §8: `PUT /library/provider/{provider}`), optionally
/// under a **store claim** (D2: `?store=steam`). The caller validates the name and payload first.
/// Emits `library.changed` with the provider as the source.
///
/// Claiming is idempotent for the holder and refused for anyone else. A provider holds at most one
/// store, so claiming a new one releases whatever it held before — otherwise an abandoned claim would
/// go on suppressing a built-in scanner with nothing to replace it.
pub fn reconcile_provider(
    provider: &str,
    store: Option<&str>,
    inputs: Vec<ProviderEntryInput>,
) -> Result<MutateOutcome<Vec<CustomEntry>>> {
    let mut catalog = load_catalog();
    if let Some(store) = store {
        if let Some(holder) = catalog.claims.get(store) {
            if holder != provider {
                return Ok(MutateOutcome::StoreClaimed {
                    store: store.to_string(),
                    provider: holder.clone(),
                });
            }
        }
        let previous: Vec<String> = catalog
            .claims
            .iter()
            .filter(|(s, p)| p.as_str() == provider && s.as_str() != store)
            .map(|(s, _)| s.clone())
            .collect();
        for stale in previous {
            tracing::info!(provider, released = %stale, claimed = store, "library: provider moved its store claim");
            catalog.claims.remove(&stale);
        }
        if catalog
            .claims
            .insert(store.to_string(), provider.to_string())
            .is_none()
        {
            tracing::info!(provider, store, "library: store claimed by a provider");
        }
    }
    let result = reconcile_entries(&mut catalog.entries, provider, store, inputs);
    save_catalog(&catalog)?;
    emit_changed(provider);
    Ok(MutateOutcome::Done(result))
}

/// Remove every entry of `provider` **and release its store claim** (RFC §8:
/// `DELETE /library/provider/{provider}` — the clean-uninstall path). Returns how many entries were
/// removed; no event when nothing changed at all.
///
/// Releasing here — and only here — is what makes uninstalling a library plugin bring its built-in
/// scanner straight back, with no restart and nothing to undo by hand.
pub fn delete_provider(provider: &str) -> Result<usize> {
    let mut catalog = load_catalog();
    let before = catalog.entries.len();
    catalog
        .entries
        .retain(|e| e.provider.as_deref() != Some(provider));
    let removed = before - catalog.entries.len();
    let claims_before = catalog.claims.len();
    catalog.claims.retain(|_, p| p != provider);
    let released = claims_before - catalog.claims.len();
    if removed > 0 || released > 0 {
        if released > 0 {
            tracing::info!(provider, released, "library: store claim released");
        }
        save_catalog(&catalog)?;
        emit_changed(provider);
    }
    Ok(removed)
}

/// The prep/undo steps for a library id — any **stored** entry (the in-host scanners have no
/// per-title config surface; a GameStream `apps.json` entry carries its own `prep` instead).
///
/// Resolved through [`entry_for_library_id`] rather than by stripping a `custom:` prefix, so a
/// claimed entry's prep still runs: after extraction a `steam:440` entry is a stored one, and
/// per-title prep is exactly the kind of thing an operator sets on a game they play.
pub fn prep_for(library_id: &str) -> Vec<crate::hooks::PrepCmd> {
    entry_for_library_id(library_id)
        .map(|e| e.prep)
        .unwrap_or_default()
}

/// Every library mutation announces itself (RFC §4): `source` is `"manual"` for the operator
/// CRUD, the provider id for a reconcile/uninstall — hooks and the SDK filter on it.
fn emit_changed(source: &str) {
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: source.to_string(),
    });
}

// `valid_steam_appid` moved to `launch.rs` (WP1.1) — it validates a launch value, not a store entry.

#[cfg(test)]
mod tests {
    use super::*;

    fn manual(id: &str, title: &str) -> CustomEntry {
        CustomEntry {
            id: id.into(),
            title: title.into(),
            art: Artwork::default(),
            launch: None,
            prep: Vec::new(),
            provider: None,
            external_id: None,
            store: None,
            role: GameRole::Game,
            detect: DetectHint::default(),
            meta: GameMeta::default(),
        }
    }

    fn input(external_id: &str, title: &str) -> ProviderEntryInput {
        ProviderEntryInput {
            external_id: external_id.into(),
            title: title.into(),
            art: Artwork::default(),
            launch: None,
            prep: Vec::new(),
            role: GameRole::Game,
            detect: DetectHint::default(),
            meta: GameMeta::default(),
        }
    }

    #[test]
    fn custom_entry_maps_to_game_entry_with_provider() {
        let g: GameEntry = manual("abc123", "My ROM").into();
        assert_eq!(g.id, "custom:abc123");
        assert_eq!(g.store, "custom");
        assert_eq!(g.provider, None);

        let mut e = manual("def456", "Synced");
        e.provider = Some("romm".into());
        e.external_id = Some("rom-1".into());
        e.meta.platform = Some("PS2".into());
        let g: GameEntry = e.into();
        assert_eq!(g.provider.as_deref(), Some("romm"));
        assert_eq!(g.meta.platform.as_deref(), Some("PS2"));
    }

    /// D2's core promise: a **claimed** entry is indistinguishable from what the built-in scanner
    /// produced. Same id, same store badge — plus the provider attribution the scanner never had.
    #[test]
    fn a_claimed_entry_reproduces_the_scanner_identity() {
        let mut e = manual("host-assigned", "Portal 2");
        e.provider = Some("steam".into());
        e.external_id = Some("620".into());
        e.store = Some("steam".into());
        assert_eq!(library_id_for(&e), "steam:620");
        let g: GameEntry = e.clone().into();
        assert_eq!(g.id, "steam:620", "exactly what the scanner emitted");
        assert_eq!(g.store, "steam", "the store badge, not `custom`");
        assert_eq!(
            g.provider.as_deref(),
            Some("steam"),
            "attribution rides along too"
        );

        // Unclaimed provider entries are untouched by any of this — rom-manager/playnite keep the
        // opaque host id they have always had.
        let mut u = manual("abc", "Chrono Trigger");
        u.provider = Some("romm".into());
        u.external_id = Some("rom-1".into());
        assert_eq!(library_id_for(&u), "custom:abc");
        assert_eq!(GameEntry::from(u).store, "custom");

        // The source a toggle addresses: the claimed store when there is one, else the provider.
        assert_eq!(source_id_for(&e), Some("steam"));
        let mut r = manual("z", "T");
        r.provider = Some("romm".into());
        assert_eq!(source_id_for(&r), Some("romm"));
        assert_eq!(
            source_id_for(&manual("m", "Manual")),
            None,
            "never hideable"
        );
    }

    /// A claimed entry keeps its `<store>:<external_id>` id across reconciles no matter what the
    /// host-assigned id does — which is what keeps GameStream's FNV-1a app ids, client art caches
    /// and Moonlight pins valid through the migration (the whole point of D2).
    #[test]
    fn claimed_ids_are_deterministic_across_reconciles() {
        let mut entries = Vec::new();
        let r1 = reconcile_entries(
            &mut entries,
            "steam",
            Some("steam"),
            vec![input("440", "Team Fortress 2"), input("620", "Portal 2")],
        );
        let ids: Vec<String> = r1.iter().map(library_id_for).collect();
        assert_eq!(ids, ["steam:440", "steam:620"]);

        // Re-sync with a renamed title and a new entry: the surfaced ids for surviving titles are
        // byte-identical, and a brand-new title's id is derived, not random.
        let r2 = reconcile_entries(
            &mut entries,
            "steam",
            Some("steam"),
            vec![
                input("440", "Team Fortress 2 (2026)"),
                input("70", "Half-Life"),
            ],
        );
        let ids2: Vec<String> = r2.iter().map(library_id_for).collect();
        assert_eq!(ids2, ["steam:440", "steam:70"]);

        // Dropping the claim on a later reconcile reverts them to opaque custom ids — the entries
        // are the same rows, so this is exactly the "plugin stopped claiming" degradation.
        let r3 = reconcile_entries(&mut entries, "steam", None, vec![input("440", "TF2")]);
        assert!(library_id_for(&r3[0]).starts_with("custom:"));
    }

    /// The metadata contract on the wire and on disk: fields serialize FLAT (no `meta` nesting —
    /// clients and plugins see `platform` beside `title`), absent fields vanish entirely, and a
    /// pre-metadata `library.json` / payload still parses (all-optional).
    #[test]
    fn meta_is_flat_and_optional_on_the_wire() {
        let mut e = manual("abc123", "Shadow of the Colossus");
        e.meta = GameMeta {
            platform: Some("PS2".into()),
            release_year: Some(2005),
            genres: vec!["Adventure".into()],
            ..Default::default()
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["platform"], "PS2");
        assert_eq!(v["release_year"], 2005);
        assert_eq!(v["genres"][0], "Adventure");
        assert!(v.get("meta").is_none(), "flattened, not nested");
        assert!(v.get("description").is_none(), "absent fields are omitted");
        assert!(v.get("tags").is_none(), "empty lists are omitted");

        // A pre-metadata entry (what an existing library.json holds) still deserializes.
        let old = r#"{"id":"abc","title":"Old"}"#;
        let e: CustomEntry = serde_json::from_str(old).unwrap();
        assert!(e.meta.platform.is_none());

        // And a provider payload can carry the fields flat, next to `title` (RFC §8 shape).
        let payload = r#"{"external_id":"rom-1","title":"OoT","platform":"N64",
                          "region":"NTSC-U","players":1,"tags":["kids"]}"#;
        let p: ProviderEntryInput = serde_json::from_str(payload).unwrap();
        assert_eq!(p.meta.platform.as_deref(), Some("N64"));
        assert_eq!(p.meta.players, Some(1));
    }

    /// The RFC §8 contract in one walk: add keeps ids stable across re-syncs, updates flow,
    /// orphans drop, and neither manual entries nor other providers are ever touched.
    #[test]
    fn reconcile_is_declarative_with_stable_ids() {
        let mut entries = vec![manual("man1", "Hand-added")];
        // Another provider's entry must survive every romm reconcile.
        let mut other = manual("oth1", "Other title");
        other.provider = Some("itch".into());
        other.external_id = Some("x1".into());
        entries.push(other);

        // First sync: two titles appear.
        let r1 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A"), input("rom-b", "Game B")],
        );
        assert_eq!(r1.len(), 2);
        assert!(r1.iter().all(|e| e.provider.as_deref() == Some("romm")));
        let id_a = r1[0].id.clone();
        assert_eq!(entries.len(), 4);

        // Second sync: A renamed, B gone, C new — A's host id must be STABLE.
        let r2 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A (v2)"), input("rom-c", "Game C")],
        );
        assert_eq!(r2.len(), 2);
        assert_eq!(r2[0].id, id_a, "same external_id keeps its host id");
        assert_eq!(r2[0].title, "Game A (v2)");
        assert_ne!(r2[1].id, id_a);
        assert!(
            !entries
                .iter()
                .any(|e| e.external_id.as_deref() == Some("rom-b")),
            "orphan dropped"
        );

        // Idempotence: an identical re-PUT changes nothing.
        let snapshot: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        let r3 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A (v2)"), input("rom-c", "Game C")],
        );
        assert_eq!(
            r3.iter().map(|e| &e.id).collect::<Vec<_>>(),
            r2.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            snapshot
        );

        // The bystanders never moved.
        assert!(entries
            .iter()
            .any(|e| e.id == "man1" && e.provider.is_none()));
        assert!(entries
            .iter()
            .any(|e| e.id == "oth1" && e.provider.as_deref() == Some("itch")));

        // Empty payload = remove everything the provider owns (same as DELETE).
        let r4 = reconcile_entries(&mut entries, "romm", None, Vec::new());
        assert!(r4.is_empty());
        assert_eq!(
            entries.len(),
            2,
            "only the manual + other-provider entries remain"
        );
    }

    /// `library.json` v1 (a bare array) must keep loading, and v2 (the claims object) must round
    /// trip. This is the only migration in the whole program — get it wrong and an existing host
    /// silently loses its manual entries on upgrade.
    #[test]
    fn v1_and_v2_library_files_both_load() {
        // v1: exactly what a shipped host has on disk today.
        let v1 = r#"[{"id":"abc","title":"Old Manual"}]"#;
        let c = match serde_json::from_str::<LibraryFile>(v1).unwrap() {
            LibraryFile::Legacy(entries) => Catalog {
                entries,
                claims: BTreeMap::new(),
            },
            LibraryFile::V2(_) => panic!("an array must not parse as v2"),
        };
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].title, "Old Manual");
        assert!(c.claims.is_empty());

        // v2, including a claim.
        let v2 = r#"{"entries":[{"id":"abc","title":"New"}],"claims":{"steam":"steam"}}"#;
        let c = match serde_json::from_str::<LibraryFile>(v2).unwrap() {
            LibraryFile::V2(c) => c,
            LibraryFile::Legacy(_) => panic!("an object must not parse as v1"),
        };
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.claims.get("steam").map(String::as_str), Some("steam"));

        // A v2 file with no claims key at all (what the first write after upgrade produces before
        // anything is claimed) still loads.
        let bare = r#"{"entries":[]}"#;
        assert!(matches!(
            serde_json::from_str::<LibraryFile>(bare).unwrap(),
            LibraryFile::V2(_)
        ));

        // And what we WRITE is v2, so one mutation upgrades the file in place.
        let written = serde_json::to_string(&Catalog::default()).unwrap();
        assert!(written.contains("\"entries\""));
        assert!(written.contains("\"claims\""));
    }

    #[test]
    fn store_claim_validation() {
        assert!(validate_store_claim("steam").is_ok());
        assert!(validate_store_claim("epic-games").is_ok());
        assert!(validate_store_claim("xbox_pc").is_ok());
        // The two host-owned namespaces are off-limits.
        assert!(validate_store_claim("custom").is_err());
        assert!(validate_store_claim("manual").is_err());
        assert!(validate_store_claim("").is_err());
        assert!(validate_store_claim("Steam").is_err()); // no uppercase
                                                         // A dot would read as a hostname in a log line and muddies the `store:id` split.
        assert!(validate_store_claim("my.store").is_err());
        assert!(validate_store_claim(&"s".repeat(33)).is_err());
    }

    /// The closed-vocabulary fields are rejected at the door, so a plugin gets a 400 rather than a
    /// tile that silently refuses to launch.
    #[test]
    fn payload_validation_covers_the_new_closed_vocabularies() {
        let with_launch = |kind: &str, value: &str| {
            let mut i = input("a", "A");
            i.launch = Some(LaunchSpec {
                kind: kind.into(),
                value: value.into(),
            });
            i
        };
        assert!(validate_provider_payload(&[with_launch("steam_ui", "bigpicture")]).is_ok());
        assert!(validate_provider_payload(&[with_launch("steam_ui", "desktop")]).is_ok());
        assert!(validate_provider_payload(&[with_launch("steam_ui", "gamepad")]).is_err());
        assert!(validate_provider_payload(&[with_launch("steam_ui", "")]).is_err());
        // Other kinds are unconstrained here (the host validates them per-kind at launch).
        assert!(validate_provider_payload(&[with_launch("command", "anything")]).is_ok());

        let with_env = |key: &str, value: Option<&str>| {
            let mut i = input("a", "A");
            i.detect.env_marker = Some(EnvMarker {
                key: key.into(),
                value: value.map(str::to_string),
            });
            i
        };
        assert!(validate_provider_payload(&[with_env("HEROIC_APP_NAME", Some("Quail"))]).is_ok());
        assert!(validate_provider_payload(&[with_env("BAD-KEY", None)]).is_err());
        assert!(validate_provider_payload(&[with_env("", None)]).is_err());
        assert!(
            validate_provider_payload(&[with_env("K", Some(&"x".repeat(MAX_ENV_VALUE + 1)))])
                .is_err()
        );
    }

    #[test]
    fn provider_name_and_payload_validation() {
        assert!(validate_provider_name("romm").is_ok());
        assert!(validate_provider_name("my-provider.v2").is_ok());
        assert!(validate_provider_name("manual").is_err(), "reserved");
        assert!(validate_provider_name("").is_err());
        assert!(validate_provider_name("Bad/Name").is_err());
        assert!(validate_provider_name("-lead").is_err());
        assert!(validate_provider_name(&"x".repeat(65)).is_err());

        assert!(validate_provider_payload(&[input("a", "A")]).is_ok());
        assert!(validate_provider_payload(&[input("", "A")]).is_err());
        assert!(validate_provider_payload(&[input("a", " ")]).is_err());
        assert!(
            validate_provider_payload(&[input("a", "A"), input("a", "B")]).is_err(),
            "duplicate external_id"
        );
    }
}
