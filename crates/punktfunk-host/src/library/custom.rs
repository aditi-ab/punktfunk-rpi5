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
            id: format!("custom:{}", c.id),
            store: "custom".into(),
            title: c.title,
            art: c.art,
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

/// Load the custom entries (empty + non-fatal if the file is absent or malformed).
pub fn load_custom() -> Vec<CustomEntry> {
    match std::fs::read_to_string(custom_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "library.json malformed — ignoring custom entries");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Serve a custom/provider entry's stored **local** art file for one [`ArtKind`] — the non-Steam
/// branch of the art proxy (`GET /library/art/custom:<id>/<kind>`). `id` is the bare custom id (the
/// `custom:` prefix already stripped by the handler). `None` if the entry is unknown, has no art of
/// that kind, or that art value isn't a servable local file (e.g. an `http` URL the client fetches
/// itself). Blocking IO — call off the async runtime.
pub fn custom_local_art_bytes(id: &str, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    let entry = load_custom().into_iter().find(|e| e.id == id)?;
    let field = match kind {
        ArtKind::Portrait => entry.art.portrait,
        ArtKind::Hero => entry.art.hero,
        ArtKind::Logo => entry.art.logo,
        ArtKind::Header => entry.art.header,
    }?;
    is_local_art_path(&field)
        .then(|| local_art_bytes(&field))
        .flatten()
}

fn save_custom(entries: &[CustomEntry]) -> Result<()> {
    let dir = pf_paths::config_dir();
    // Owner-private dir (0700 / SYSTEM+Admins DACL) so a non-privileged local user can't plant a
    // library.json whose `prep`/`launch` commands the host would later execute — the same trust
    // boundary hooks.json and the mgmt token already use.
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(entries)?;
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

/// Outcome of a manual mutation against an id — distinguishes "no such entry" from "exists,
/// but a provider owns it" (the mgmt layer maps the latter to 409, not 404).
pub enum MutateOutcome<T> {
    Done(T),
    NotFound,
    /// The entry belongs to this provider — mutate it through the provider reconcile API
    /// (or remove the whole provider set); manual edits would be clobbered at the next sync.
    ProviderOwned(String),
}

/// Create a custom (manual) entry, returning it with its assigned id.
pub fn add_custom(input: CustomInput) -> Result<CustomEntry> {
    let mut entries = load_custom();
    let entry = CustomEntry {
        id: new_id(&input.title),
        title: input.title,
        art: input.art,
        launch: input.launch,
        prep: input.prep,
        provider: None,
        external_id: None,
        detect: input.detect,
        meta: input.meta,
    };
    entries.push(entry.clone());
    save_custom(&entries)?;
    emit_changed("manual");
    Ok(entry)
}

/// Replace a manual entry's fields (id preserved). Provider-owned entries are refused —
/// their state belongs to the provider's reconcile (RFC §8 ownership rule).
pub fn update_custom(id: &str, input: CustomInput) -> Result<MutateOutcome<CustomEntry>> {
    let mut entries = load_custom();
    let Some(slot) = entries.iter_mut().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &slot.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    slot.title = input.title;
    slot.art = input.art;
    slot.launch = input.launch;
    slot.prep = input.prep;
    slot.detect = input.detect;
    slot.meta = input.meta;
    let updated = slot.clone();
    save_custom(&entries)?;
    emit_changed("manual");
    Ok(MutateOutcome::Done(updated))
}

/// Delete a manual entry. Provider-owned entries are refused (see [`update_custom`]).
pub fn delete_custom(id: &str) -> Result<MutateOutcome<()>> {
    let mut entries = load_custom();
    let Some(entry) = entries.iter().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &entry.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    entries.retain(|e| e.id != id);
    save_custom(&entries)?;
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
            detect: input.detect,
            meta: input.meta,
        });
    }
    // `existing`'s leftovers are the orphans — deliberately dropped (declarative reconcile).
    entries.extend(result.iter().cloned());
    result
}

/// Atomically replace `provider`'s entry set (RFC §8: `PUT /library/provider/{provider}`).
/// The caller validates the name and payload first. Emits `library.changed` with the provider
/// as the source.
pub fn reconcile_provider(
    provider: &str,
    inputs: Vec<ProviderEntryInput>,
) -> Result<Vec<CustomEntry>> {
    let mut entries = load_custom();
    let result = reconcile_entries(&mut entries, provider, inputs);
    save_custom(&entries)?;
    emit_changed(provider);
    Ok(result)
}

/// Remove every entry of `provider` (RFC §8: `DELETE /library/provider/{provider}` — the
/// clean-uninstall path). Returns how many were removed; no event when nothing was.
pub fn delete_provider(provider: &str) -> Result<usize> {
    let mut entries = load_custom();
    let before = entries.len();
    entries.retain(|e| e.provider.as_deref() != Some(provider));
    let removed = before - entries.len();
    if removed > 0 {
        save_custom(&entries)?;
        emit_changed(provider);
    }
    Ok(removed)
}

/// The prep/undo steps for a library id — `custom:<id>` entries only (the other stores have no
/// per-title config surface; a GameStream `apps.json` entry carries its own `prep` instead).
pub fn prep_for(library_id: &str) -> Vec<crate::hooks::PrepCmd> {
    let Some(id) = library_id.strip_prefix("custom:") else {
        return Vec::new();
    };
    load_custom()
        .into_iter()
        .find(|e| e.id == id)
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

/// A digits-only Steam appid: the sole client-influenced part of a Steam launch, validated before it
/// is interpolated into any command / URI (so a client-sent id can never carry shell or URI syntax).
/// Cross-platform — used by the Linux shell mapping ([`command_for`]) and the Windows spawn mapping
/// ([`windows_launch_for`]).
pub(crate) fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

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
        let r4 = reconcile_entries(&mut entries, "romm", Vec::new());
        assert!(r4.is_empty());
        assert_eq!(
            entries.len(),
            2,
            "only the manual + other-provider entries remain"
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
