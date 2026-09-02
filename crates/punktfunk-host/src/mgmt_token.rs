//! Management-API bearer token resolution.
//!
//! HTTPS always, auth always (including loopback). Precedence: env (operator
//! override, not persisted) → `<config-dir>` file → generate 32-byte hex and
//! persist. Files are `KEY=<hex>` at 0600 so the console can source them as a
//! systemd `EnvironmentFile`.
//!
//! Two tokens:
//! - **`mgmt-token`** (`PUNKTFUNK_MGMT_TOKEN`) — full admin.
//! - **`plugin-token`** (`PUNKTFUNK_PLUGIN_TOKEN`) — `mgmt::auth::plugin_may_access`.
//!   The SDK `connect()` prefers this file so a plugin cannot rewrite
//!   `hooks.json` or admit devices.

use anyhow::{Context, Result};
use rand::RngCore;
use std::fs;
use std::path::Path;

const ENV_VAR: &str = "PUNKTFUNK_MGMT_TOKEN";
const FILE: &str = "mgmt-token";
const PLUGIN_ENV_VAR: &str = "PUNKTFUNK_PLUGIN_TOKEN";
const PLUGIN_FILE: &str = "plugin-token";

/// Admin token: env > file > generate+persist. Hex so `KEY=VALUE` is safe
/// to source from a shell or systemd `EnvironmentFile`.
pub fn load_or_generate() -> Result<String> {
    load_or_generate_impl(ENV_VAR, FILE)
}

/// Plugin-lane token, same precedence as [`load_or_generate`].
///
/// On Windows, `plugins enable` grants LocalService read on this file and
/// `cert.pem` — never `mgmt-token`.
pub fn load_or_generate_plugin() -> Result<String> {
    load_or_generate_impl(PLUGIN_ENV_VAR, PLUGIN_FILE)
}

/// Persisted operator token in `dir`, or `None`. Never mints.
///
/// `ctl` must not generate: a client-minted file would become the host
/// credential. Ignores `PUNKTFUNK_MGMT_TOKEN` so a consumer does not publish
/// the token in `/proc/<pid>/environ`.
pub(crate) fn read_persisted(dir: &Path) -> Option<String> {
    let contents = fs::read_to_string(dir.join(FILE)).ok()?;
    parse_token(&contents, ENV_VAR)
}

fn load_or_generate_impl(env_var: &str, file: &str) -> Result<String> {
    if let Ok(v) = std::env::var(env_var) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    let dir = pf_paths::config_dir();
    // Lock the dir (0700 / DACL) before the read. A world-writable config
    // dir would let a local user plant the admin token this then adopts.
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(file);
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Some(tok) = parse_token(&contents, env_var) {
            return Ok(tok);
        }
    }
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let token = hex::encode(buf);
    write_token(&path, env_var, &token)?;
    tracing::info!(path = %path.display(), "generated and persisted API token (owner-only)");
    Ok(token)
}

/// First non-empty line: bare token or `<KEY>=<token>` (EnvironmentFile).
fn parse_token(contents: &str, env_var: &str) -> Option<String> {
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let tok = line
        .strip_prefix(env_var)
        .and_then(|rest| rest.strip_prefix('='))
        .unwrap_or(line)
        .trim();
    (!tok.is_empty()).then(|| tok.to_string())
}

/// Owner-only `KEY=token` via `pf_paths::write_secret_file` (0600 Unix;
/// SYSTEM/Administrators DACL on Windows). Same lockdown as the host key.
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
