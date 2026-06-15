//! Management-API bearer token resolution.
//!
//! The mgmt API always serves HTTPS (the host's identity cert) and now always requires auth — even
//! on a loopback bind. This module guarantees a token always exists: an explicit
//! `PUNKTFUNK_MGMT_TOKEN` env wins (operator override, not persisted); otherwise the persisted
//! `~/.config/punktfunk/mgmt-token` is used; otherwise a fresh 32-byte hex token is generated and
//! persisted. The file is written in `PUNKTFUNK_MGMT_TOKEN=<hex>` form (0600) so the bundled web
//! console can source it directly as a systemd `EnvironmentFile` — a single source of truth shared
//! between the host and the console with no copying.

use anyhow::{Context, Result};
use rand::RngCore;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

const ENV_VAR: &str = "PUNKTFUNK_MGMT_TOKEN";
const FILE: &str = "mgmt-token";

/// Resolve the mgmt token (env > persisted file > generate+persist). Hex (not base64) so the
/// persisted `KEY=VALUE` line is safe to source from a shell / systemd `EnvironmentFile`.
pub fn load_or_generate() -> Result<String> {
    if let Ok(v) = std::env::var(ENV_VAR) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    let path = crate::gamestream::config_dir().join(FILE);
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Some(tok) = parse_token(&contents) {
            return Ok(tok);
        }
    }
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let token = hex::encode(buf);
    let dir = crate::gamestream::config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    write_token(&path, &token)?;
    tracing::info!(path = %path.display(), "generated and persisted management API token (0600)");
    Ok(token)
}

/// Parse the token from the persisted file: accept either a bare token line or a
/// `PUNKTFUNK_MGMT_TOKEN=<token>` line (the form we write, also valid as an EnvironmentFile).
fn parse_token(contents: &str) -> Option<String> {
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let tok = line
        .strip_prefix("PUNKTFUNK_MGMT_TOKEN=")
        .unwrap_or(line)
        .trim();
    (!tok.is_empty()).then(|| tok.to_string())
}

/// Write `PUNKTFUNK_MGMT_TOKEN=<token>` to `path`, mode 0600 (never briefly world-readable).
fn write_token(path: &Path, token: &str) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    writeln!(f, "PUNKTFUNK_MGMT_TOKEN={token}")?;
    #[cfg(unix)]
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_keyvalue_forms() {
        assert_eq!(parse_token("abc123\n").as_deref(), Some("abc123"));
        assert_eq!(
            parse_token("PUNKTFUNK_MGMT_TOKEN=deadbeef\n").as_deref(),
            Some("deadbeef")
        );
        assert_eq!(parse_token("\n  \n"), None);
        assert_eq!(parse_token("PUNKTFUNK_MGMT_TOKEN=\n"), None);
    }

    #[test]
    fn generated_token_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("pf-mgmt-token-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(FILE);
        write_token(&path, "cafef00d").unwrap();
        let read = fs::read_to_string(&path).unwrap();
        assert_eq!(parse_token(&read).as_deref(), Some("cafef00d"));
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
