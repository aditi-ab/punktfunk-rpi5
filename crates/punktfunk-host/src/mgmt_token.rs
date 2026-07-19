//! Management-API bearer token resolution.
//!
//! The mgmt API always serves HTTPS (the host's identity cert) and now always requires auth — even
//! on a loopback bind. This module guarantees the tokens always exist: an explicit env var wins
//! (operator override, not persisted); otherwise the persisted file under the config dir is used;
//! otherwise a fresh 32-byte hex token is generated and persisted. Files are written in
//! `KEY=<hex>` form (0600) so the bundled web console can source them directly as a systemd
//! `EnvironmentFile` — a single source of truth shared between the host and its consumers.
//!
//! Two tokens, two authorities:
//! - **`mgmt-token`** (`PUNKTFUNK_MGMT_TOKEN`) — the operator/console token; authorizes the full
//!   admin surface.
//! - **`plugin-token`** (`PUNKTFUNK_PLUGIN_TOKEN`) — the scripting runner's capability-limited
//!   credential (`mgmt::auth::plugin_may_access`): everything a plugin legitimately needs, but not
//!   hook registration or pairing administration. The SDK's `connect()` prefers this file, so a
//!   defect in an operator plugin can't rewrite `hooks.json` or admit new devices.

use anyhow::{Context, Result};
use rand::RngCore;
use std::fs;
use std::path::Path;

const ENV_VAR: &str = "PUNKTFUNK_MGMT_TOKEN";
const FILE: &str = "mgmt-token";
const PLUGIN_ENV_VAR: &str = "PUNKTFUNK_PLUGIN_TOKEN";
const PLUGIN_FILE: &str = "plugin-token";

/// Resolve the mgmt (full-admin) token (env > persisted file > generate+persist). Hex (not base64)
/// so the persisted `KEY=VALUE` line is safe to source from a shell / systemd `EnvironmentFile`.
pub fn load_or_generate() -> Result<String> {
    load_or_generate_impl(ENV_VAR, FILE)
}

/// Resolve the scripting runner's scoped plugin token, same precedence as [`load_or_generate`].
/// Persisted to `plugin-token` next to `mgmt-token`; on Windows `plugins enable` grants the
/// runner's LocalService principal read on exactly this file (and `cert.pem`) — never `mgmt-token`.
pub fn load_or_generate_plugin() -> Result<String> {
    load_or_generate_impl(PLUGIN_ENV_VAR, PLUGIN_FILE)
}

fn load_or_generate_impl(env_var: &str, file: &str) -> Result<String> {
    if let Ok(v) = std::env::var(env_var) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    let path = pf_paths::config_dir().join(file);
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Some(tok) = parse_token(&contents, env_var) {
            return Ok(tok);
        }
    }
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let token = hex::encode(buf);
    let dir = pf_paths::config_dir();
    // Owner-private dir (0700 Unix / DACL-locked Windows) so the token can't leak via the config path.
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    write_token(&path, env_var, &token)?;
    tracing::info!(path = %path.display(), "generated and persisted API token (owner-only)");
    Ok(token)
}

/// Parse the token from the persisted file: accept either a bare token line or a
/// `<KEY>=<token>` line (the form we write, also valid as an EnvironmentFile).
fn parse_token(contents: &str, env_var: &str) -> Option<String> {
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let tok = line
        .strip_prefix(env_var)
        .and_then(|rest| rest.strip_prefix('='))
        .unwrap_or(line)
        .trim();
    (!tok.is_empty()).then(|| tok.to_string())
}

/// Write `<KEY>=<token>` to `path` as an owner-only secret — 0600 on Unix AND DACL-locked to
/// SYSTEM/Administrators on Windows. Routes through the shared `write_secret_file` so both bearer
/// tokens get the SAME Windows lockdown as the host key; the bespoke `cfg(unix)`-only writer used
/// to leave the mgmt token readable by any local user (security-review 2026-06-28 #2).
fn write_token(path: &Path, env_var: &str, token: &str) -> Result<()> {
    let line = format!("{env_var}={token}\n");
    pf_paths::write_secret_file(path, line.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_keyvalue_forms() {
        assert_eq!(parse_token("abc123\n", ENV_VAR).as_deref(), Some("abc123"));
        assert_eq!(
            parse_token("PUNKTFUNK_MGMT_TOKEN=deadbeef\n", ENV_VAR).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(
            parse_token("PUNKTFUNK_PLUGIN_TOKEN=deadbeef\n", PLUGIN_ENV_VAR).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(parse_token("\n  \n", ENV_VAR), None);
        assert_eq!(parse_token("PUNKTFUNK_MGMT_TOKEN=\n", ENV_VAR), None);
    }

    #[test]
    fn generated_token_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("pf-mgmt-token-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(FILE);
        write_token(&path, ENV_VAR, "cafef00d").unwrap();
        let read = fs::read_to_string(&path).unwrap();
        assert_eq!(parse_token(&read, ENV_VAR).as_deref(), Some("cafef00d"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
