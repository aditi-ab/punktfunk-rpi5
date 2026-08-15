//! On-disk store for log bundles PAIRED CLIENTS upload to this host.
//!
//! Why this exists: on locked-down client platforms (a Steam Deck in Gaming Mode, tvOS, webOS)
//! the user has no realistic way to get the client's own log off the device — so every field
//! report from those platforms arrives host-log-only, and the client half of the story
//! (de-jitter, decode rungs, playout underruns) is invisible. "Send logs to host" inverts that:
//! the client POSTs its recent log over the management API with its paired mTLS cert, the bundle
//! lands here, and the web console lists it next to the host's own log export — one place to
//! collect both halves.
//!
//! Deliberately a FILE store, not `log_capture::ring()`: the ring holds the host's newest ~4096
//! entries, and ingesting a multi-thousand-line client bundle there would evict the host log —
//! destroying the other half of the very report this feature exists to complete. The host log
//! gets one INFO breadcrumb per received bundle instead.
//!
//! Quota: a paired device may upload at will, so the store must be bounded without operator
//! attention — per device (by fingerprint prefix) only the newest [`KEEP_PER_DEVICE`] bundles
//! survive, and a single bundle is capped at [`MAX_BUNDLE_BYTES`] by the endpoint. Paired
//! devices are operator-admitted and enumerable, so the total is bounded too.

use serde::Serialize;
use std::path::PathBuf;
use utoipa::ToSchema;

/// Newest bundles kept per device; older ones are pruned on the next upload from that device.
pub const KEEP_PER_DEVICE: usize = 5;

/// Upload size cap, enforced by the endpoint before the store sees the body. Client rings render
/// to a few hundred KiB; anything past this is not a log bundle.
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

/// The default store directory: `<config-dir>/client-logs/`, beside `captures/`.
pub fn default_dir() -> PathBuf {
    pf_paths::config_dir().join("client-logs")
}

/// One stored bundle, as the console lists it.
#[derive(Clone, Serialize, ToSchema)]
pub struct ClientLogMeta {
    /// The bundle id (its filename stem) — pass to the fetch/delete endpoints.
    pub id: String,
    /// The paired device's name at upload time (sanitized for the filesystem).
    pub device_name: String,
    /// First 16 hex chars of the device's pairing fingerprint — enough to correlate with the
    /// paired-devices roster without repeating the full identity in every filename.
    pub fingerprint_prefix: String,
    /// Upload time (unix ms, from the file's mtime).
    pub received_ms: u64,
    /// Bundle size in bytes.
    pub size_bytes: u64,
}

/// The store: a flat directory of `<ts>_<fp16>_<name>.log` files.
pub struct ClientLogStore {
    dir: PathBuf,
}

/// Same id gate as `stats_recorder::valid_id`: the exact charset [`bundle_id`] emits, and the
/// charset excludes `/` and `\`, so `dir.join(id + ".log")` is always a single child of `dir`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Squeeze a paired device's display name into the id charset (never empty — the id must parse
/// back into its three fields, and an all-symbols name would otherwise leave a dangling `_`).
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .take(24)
        .collect();
    if cleaned.is_empty() {
        "device".into()
    } else {
        cleaned
    }
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-08-15T10-22-33Z_9785592d05ef1234_SteamDeck` — timestamp first so a plain directory sort
/// is newest-last, dashes (not colons) in the time so the stem is a valid Windows filename.
/// Underscores separate the three fields, so name/fp keep to `[A-Za-z0-9.-]`.
fn bundle_id(unix_ms: u64, fp_hex: &str, name: &str) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, mo, d) = crate::stats_recorder::civil_from_days(days);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let fp16: String = fp_hex
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z_{fp16}_{}",
        sanitize_name(name)
    )
}

impl ClientLogStore {
    /// Open the store, creating `dir` (owner-private, best-effort) if missing.
    pub fn new(dir: PathBuf) -> std::sync::Arc<Self> {
        if let Err(e) = pf_paths::create_private_dir(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not create client-logs dir");
        }
        std::sync::Arc::new(ClientLogStore { dir })
    }

    /// Store a bundle from the paired device `fp_hex` named `device_name`; returns the new id.
    /// Prunes that device's older bundles past [`KEEP_PER_DEVICE`] (best-effort).
    pub fn save(&self, fp_hex: &str, device_name: &str, body: &[u8]) -> std::io::Result<String> {
        let id = bundle_id(unix_ms_now(), fp_hex, device_name);
        std::fs::write(self.dir.join(format!("{id}.log")), body)?;
        // Prune this device's older bundles. The fp16 field is position 2 of the stem, and ids
        // sort chronologically because the timestamp leads.
        let fp16: String = fp_hex
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        let mut mine: Vec<String> = self
            .stems()
            .into_iter()
            .filter(|stem| stem.split('_').nth(1) == Some(fp16.as_str()))
            .collect();
        mine.sort();
        if mine.len() > KEEP_PER_DEVICE {
            for stale in &mine[..mine.len() - KEEP_PER_DEVICE] {
                let _ = std::fs::remove_file(self.dir.join(format!("{stale}.log")));
            }
        }
        Ok(id)
    }

    /// Every stored bundle's metadata, newest first.
    pub fn list(&self) -> Vec<ClientLogMeta> {
        let mut out: Vec<ClientLogMeta> = self
            .stems()
            .into_iter()
            .filter_map(|stem| {
                let mut parts = stem.splitn(3, '_');
                let _ts = parts.next()?;
                let fp = parts.next()?.to_string();
                let name = parts.next()?.to_string();
                let md = std::fs::metadata(self.dir.join(format!("{stem}.log"))).ok()?;
                let received_ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some(ClientLogMeta {
                    id: stem,
                    device_name: name,
                    fingerprint_prefix: fp,
                    received_ms,
                    size_bytes: md.len(),
                })
            })
            .collect();
        out.sort_by(|a, b| b.id.cmp(&a.id)); // timestamp-led stems ⇒ lexicographic = chronological
        out
    }

    /// The full bundle body for `id`. `NotFound` for unknown AND invalid ids — an id that fails
    /// the charset gate must be indistinguishable from an absent one.
    pub fn load(&self, id: &str) -> std::io::Result<Vec<u8>> {
        if !valid_id(id) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::read(self.dir.join(format!("{id}.log")))
    }

    /// Delete the bundle `id` (same invalid-id handling as [`Self::load`]).
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        if !valid_id(id) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::remove_file(self.dir.join(format!("{id}.log")))
    }

    /// Filename stems of every `.log` in the store (unsorted).
    fn stems(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| {
            let name = e.ok()?.file_name();
            let name = name.to_str()?;
            let stem = name.strip_suffix(".log")?;
            valid_id(stem).then(|| stem.to_string())
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (std::sync::Arc<ClientLogStore>, PathBuf) {
        // pid+timestamp alone COLLIDED: the tests run in parallel in one process and both can
        // land on the same millisecond — then one test's cleanup deletes the other's live dir.
        // A process-wide counter makes each call unique regardless of timing.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pf-client-logs-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        (ClientLogStore::new(dir.clone()), dir)
    }

    #[test]
    fn save_list_load_delete_roundtrip() {
        let (s, dir) = store();
        let id = s
            .save("abcdef0123456789ff", "Steam Deck!", b"hello log")
            .unwrap();
        assert!(id.contains("abcdef0123456789"), "{id}");
        assert!(id.ends_with("SteamDeck"), "sanitized name: {id}");
        let listed = s.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].device_name, "SteamDeck");
        assert_eq!(listed[0].size_bytes, 9);
        assert_eq!(s.load(&id).unwrap(), b"hello log");
        s.delete(&id).unwrap();
        assert!(s.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn traversal_ids_read_as_absent() {
        let (s, dir) = store();
        for bad in ["../cert", "..", ".", "a/b", "a\\b", ""] {
            assert_eq!(
                s.load(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "{bad:?}"
            );
            assert_eq!(
                s.delete(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "{bad:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn per_device_quota_prunes_oldest() {
        let (s, dir) = store();
        // Same device, KEEP_PER_DEVICE + 2 uploads. Ids share a second-resolution timestamp in a
        // fast test, so disambiguate chronology by writing distinct bodies and checking survival
        // by count + the other device's bundle being untouched.
        let other = s.save("ffff00000000000000", "other", b"keep me").unwrap();
        let mut ids = Vec::new();
        for i in 0..(KEEP_PER_DEVICE + 2) {
            // Distinct mtime-independent ids: the timestamp field has 1 s resolution, so append
            // uniqueness through the name (the id embeds it).
            ids.push(
                s.save("abcdef0123456789", &format!("dev{i}"), b"x")
                    .unwrap(),
            );
        }
        let listed = s.list();
        let mine = listed
            .iter()
            .filter(|m| m.fingerprint_prefix == "abcdef0123456789")
            .count();
        assert_eq!(mine, KEEP_PER_DEVICE);
        assert!(listed.iter().any(|m| m.id == other), "other device pruned");
        let _ = std::fs::remove_dir_all(dir);
    }
}
