//! Fire-and-forget operator commands and webhooks for host lifecycle events.
//!
//! `hooks.json` is operator-privileged and managed through `/api/v1/hooks`. Commands receive
//! event JSON and `PF_EVENT_*` variables; Windows services run them in the interactive user
//! session. Webhooks use verified TLS, do not follow redirects or attach Punktfunk credentials,
//! and may carry an HMAC signature.
//!
//! Debouncing, timeouts, process-group termination, and [`MAX_CONCURRENT_HOOKS`] bound work.
//! Script paths must pass ownership and writability checks. Logs use sanitized labels rather
//! than raw command lines or webhook URLs, which may contain credentials.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use utoipa::ToSchema;

/// Concurrent hook executions in flight (exec + webhook combined). Excess firings are dropped
/// with a warning — hooks are best-effort observers, and unbounded queueing is the failure
/// mode this cap exists to prevent.
const MAX_CONCURRENT_HOOKS: usize = 8;

/// Default and ceiling for the exec timeout.
const DEFAULT_TIMEOUT_S: u32 = 30;
const MAX_TIMEOUT_S: u32 = 600;

/// Outbound webhook timeout (connect + response).
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

fn default_timeout_s() -> u32 {
    DEFAULT_TIMEOUT_S
}

/// The operator's hook configuration — the `hooks.json` document and the `/api/v1/hooks` body.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
}

/// One hook: fire `run` and/or `webhook` when an event matching `on` (+ `filter`) occurs.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct HookEntry {
    /// Which events fire this hook: an exact kind (`stream.started`) or a `domain.*` prefix
    /// (`pairing.*`) — the same vocabulary as the SSE `?kinds=` filter.
    pub on: String,
    /// Exact-match constraints on the event's fields; every present field must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<HookFilter>,
    /// Shell command to execute (detached, event JSON on stdin + `PF_EVENT_*` env).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// URL to POST the event JSON to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    /// Exec timeout in seconds (1–600, default 30); the process group is killed on expiry.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u32,
    /// Minimum interval between firings of this hook, in milliseconds. 0 = fire every time.
    #[serde(default)]
    pub debounce_ms: u64,
    /// File holding the webhook HMAC secret (`X-Punktfunk-Signature: sha256=<hex>`). The file
    /// should be operator-owned and private; a world-readable secret is warned about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub hmac_secret_file: Option<PathBuf>,
}

/// Exact-match filters against an event's identity fields (RFC open-question 3: exact match
/// only — anything richer is what the SDK is for). Absent fields don't constrain; a filter
/// field set on an event kind that doesn't carry it (e.g. `client` on `host.started`) never
/// matches.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default)]
pub struct HookFilter {
    /// Client/device name (for `session.*`: the short client label the Dashboard shows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Certificate fingerprint (hex, case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Protocol plane (`native` / `gamestream`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<crate::events::Plane>,
    /// Launched app id/title (`stream.*` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

impl HookFilter {
    fn matches(&self, kind: &crate::events::EventKind) -> bool {
        if let Some(want) = &self.client {
            if kind.client_name() != Some(want.as_str()) {
                return false;
            }
        }
        if let Some(want) = &self.fingerprint {
            match kind.fingerprint() {
                Some(fp) if fp.eq_ignore_ascii_case(want) => {}
                _ => return false,
            }
        }
        if let Some(want) = self.plane {
            if kind.plane() != Some(want) {
                return false;
            }
        }
        if let Some(want) = &self.app {
            if kind.app() != Some(want.as_str()) {
                return false;
            }
        }
        true
    }
}

impl HooksConfig {
    /// Validate for the mgmt PUT: structural errors are rejected (the config would silently do
    /// nothing or something surprising); unknown kinds are accepted (additive event catalog).
    pub fn validate(&self) -> Result<(), String> {
        for (i, h) in self.hooks.iter().enumerate() {
            let at = |msg: &str| format!("hooks[{i}]: {msg}");
            if h.on.trim().is_empty() {
                return Err(at("`on` must be an event kind or `domain.*` pattern"));
            }
            if h.run.as_deref().is_none_or(|r| r.trim().is_empty())
                && h.webhook.as_deref().is_none_or(|w| w.trim().is_empty())
            {
                return Err(at("needs `run` and/or `webhook`"));
            }
            if let Some(url) = h.webhook.as_deref().filter(|w| !w.trim().is_empty()) {
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(at("`webhook` must be an http(s):// URL"));
                }
                if webhook_host_is_internal(url) {
                    return Err(at(
                        "`webhook` must not target a loopback/link-local/metadata host",
                    ));
                }
                // A signed webhook over plaintext http:// sends the HMAC'd event body in the clear.
                // Warn rather than reject (an internal-only `http://` receiver may be intentional).
                if h.hmac_secret_file.is_some() && url.starts_with("http://") {
                    tracing::warn!(
                        url = %webhook_origin(url),
                        "webhook has an hmac_secret_file but is http:// — the signed body is sent in cleartext; prefer https://"
                    );
                }
            }
            if let Some(p) = h.hmac_secret_file.as_deref() {
                if let Some(why) = secret_file_complaint(p) {
                    tracing::warn!(path = %p.display(),
                        "webhook hmac_secret_file is {why} — it should be operator-owned and chmod 600");
                }
            }
            if h.timeout_s == 0 || h.timeout_s > MAX_TIMEOUT_S {
                return Err(at(&format!("`timeout_s` must be 1–{MAX_TIMEOUT_S}")));
            }
        }
        Ok(())
    }
}

/// The documented `hmac_secret_file` hygiene check (see [`HookEntry::hmac_secret_file`]): the
/// secret should be operator-owned and private. Returns the complaint to warn about, `None` when
/// the file is fine (or absent — an unreadable secret is [`post_webhook`]'s fail-closed case, not
/// this one's). A warning, not a refusal: the operator asked for signing, and refusing here would
/// silently drop it.
#[cfg(unix)]
fn secret_file_complaint(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid && meta.uid() != 0 {
        return Some(format!(
            "owned by uid {} (host runs as uid {euid})",
            meta.uid()
        ));
    }
    (meta.mode() & 0o077 != 0)
        .then(|| format!("group/world-accessible (mode {:o})", meta.mode() & 0o7777))
}

/// Windows: the SYSTEM/Admins-DACL'd config dir is the boundary (as for [`exec_path_check`]).
#[cfg(not(unix))]
fn secret_file_complaint(_path: &std::path::Path) -> Option<String> {
    None
}

// ------------------------------------------------------------------------- store

/// The persisted hooks store — the [`crate::vdisplay::policy::DisplayPolicyStore`] recipe:
/// private dir, temp-write + atomic rename, in-memory value changes only if the write succeeds.
///
/// A hand-edited `hooks.json` is honored WITHOUT a restart (the documented contract): [`get`]
/// re-stats the file and reloads when its identity (mtime + length) moved. The stat rides the
/// per-event dispatch, so the check costs one `metadata()` call per event, and a full re-read
/// happens only when the file actually changed.
///
/// [`get`]: HooksStore::get
pub struct HooksStore {
    path: PathBuf,
    cur: Mutex<StoreState>,
}

struct StoreState {
    cfg: Option<HooksConfig>,
    /// Identity of the file revision `cfg` was parsed from (mtime + length); `None` = the file
    /// did not exist. `get` compares against a fresh stat to detect hand edits.
    file_id: Option<(std::time::SystemTime, u64)>,
}

impl HooksStore {
    /// Load from `path`. Missing file ⇒ no hooks; corrupt file ⇒ no hooks with a warning
    /// (never fail host startup over a settings file).
    pub fn load_from(path: PathBuf) -> Self {
        let (cfg, file_id) = Self::read_disk(&path);
        HooksStore {
            path,
            cur: Mutex::new(StoreState { cfg, file_id }),
        }
    }

    /// The file's on-disk identity, `None` when it does not exist (or cannot be stat'd —
    /// indistinguishable on purpose: both mean "no usable hooks file").
    fn file_identity(path: &PathBuf) -> Option<(std::time::SystemTime, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    /// Read + validate the file. Same lenient contract as startup: missing ⇒ no hooks;
    /// invalid/unreadable ⇒ no hooks with a warning naming the problem.
    fn read_disk(path: &PathBuf) -> (Option<HooksConfig>, Option<(std::time::SystemTime, u64)>) {
        let file_id = Self::file_identity(path);
        let cfg = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<HooksConfig>(&bytes) {
                Ok(c) => {
                    if let Err(e) = c.validate() {
                        tracing::warn!(path = %path.display(),
                            "hooks.json invalid — hooks disabled until fixed: {e}");
                        None
                    } else {
                        Some(c)
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(),
                        "hooks.json unreadable — hooks disabled until fixed: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        (cfg, file_id)
    }

    /// The stored configuration (empty when unconfigured) — the mgmt GET and the dispatcher.
    /// Re-reads `hooks.json` first if it changed on disk since last load, so hand edits apply
    /// on the next event, no restart ("changes apply immediately" — docs/automation.md).
    pub fn get(&self) -> HooksConfig {
        let mut st = self.cur.lock().unwrap();
        let now_id = Self::file_identity(&self.path);
        if now_id != st.file_id {
            let (cfg, file_id) = Self::read_disk(&self.path);
            tracing::info!(path = %self.path.display(), hooks = cfg.as_ref().map_or(0, |c| c.hooks.len()),
                "hooks.json changed on disk — reloaded");
            st.cfg = cfg;
            st.file_id = file_id;
        }
        st.cfg.clone().unwrap_or_default()
    }

    /// Persist + adopt a new configuration (caller validates first). The in-memory value
    /// changes only if the disk write succeeds.
    pub fn set(&self, cfg: HooksConfig) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&cfg)?)?;
        std::fs::rename(&tmp, &self.path)?;
        let mut st = self.cur.lock().unwrap();
        st.file_id = Self::file_identity(&self.path);
        st.cfg = Some(cfg);
        Ok(())
    }
}

/// The process-wide hooks store (`<config_dir>/hooks.json`), loaded on first access and
/// re-loaded whenever the file changes on disk (see [`HooksStore::get`]).
pub fn store() -> &'static HooksStore {
    static STORE: OnceLock<HooksStore> = OnceLock::new();
    STORE.get_or_init(|| HooksStore::load_from(pf_paths::config_dir().join("hooks.json")))
}

// ------------------------------------------------------------------------- runner

/// The hook runner: a host-lifetime task consuming the live event tail and dispatching
/// matching hooks. Spawned by `serve()` before `host.started` is emitted, so hooks can
/// observe the full host lifetime. Lag (more events than the runner drained) skips the
/// missed events with a warning — fire-and-forget, never a queue that grows unboundedly.
pub async fn runner() {
    let mut rx = crate::events::bus().subscribe_live();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HOOKS));
    let mut debounce: HashMap<u64, Instant> = HashMap::new();
    loop {
        match rx.recv().await {
            Ok(ev) => dispatch(&ev, &sem, &mut debounce),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "hook runner lagged — skipped events fire no hooks"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Stable identity for a hook entry across config reloads (the debounce key): the hash of its
/// serialized form — an unchanged entry keeps its debounce window across a PUT.
fn entry_key(h: &HookEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(h)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn dispatch(
    ev: &crate::events::HostEvent,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
    debounce: &mut HashMap<u64, Instant>,
) {
    let kind = ev.kind.name();
    let cfg = store().get();
    for h in &cfg.hooks {
        if !crate::events::kind_matches(&h.on, kind) {
            continue;
        }
        if !h
            .filter
            .as_ref()
            .unwrap_or(&HookFilter::default())
            .matches(&ev.kind)
        {
            continue;
        }
        if h.debounce_ms > 0 {
            let key = entry_key(h);
            let now = Instant::now();
            if debounce
                .get(&key)
                .is_some_and(|t| now.duration_since(*t) < Duration::from_millis(h.debounce_ms))
            {
                tracing::debug!(on = %h.on, kind, "hook debounced");
                continue;
            }
            debounce.insert(key, now);
        }
        if let Some(cmd) = h.run.as_deref().filter(|c| !c.trim().is_empty()) {
            fire_exec(cmd.to_string(), ev, h.timeout_s, sem);
        }
        if let Some(url) = h.webhook.as_deref().filter(|u| !u.trim().is_empty()) {
            fire_webhook(url.to_string(), h.hmac_secret_file.clone(), ev, sem);
        }
    }
    // The two env-var mirrors (`PUNKTFUNK_ON_CONNECT_CMD` / `PUNKTFUNK_ON_DISCONNECT_CMD`) —
    // the zero-config siblings of `PUNKTFUNK_RECOVER_SESSION_CMD` for the simplest cases.
    let mirror = match kind {
        "client.connected" => pf_host_config::config().on_connect_cmd.clone(),
        "client.disconnected" => pf_host_config::config().on_disconnect_cmd.clone(),
        _ => None,
    };
    if let Some(cmd) = mirror {
        fire_exec(cmd, ev, DEFAULT_TIMEOUT_S, sem);
    }
}

// ------------------------------------------------------------------------- exec action

/// Short, stable id for one hook action (`#1a2b3c4d`) — all a log line keeps of the part that can
/// carry a secret. The same id on every line about one firing, a different one for two hooks that
/// share a program or a webhook host, so an operator can still tell which fired. Process-lifetime
/// stable, like [`entry_key`]'s hash.
fn short_id(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    format!("#{:08x}", hasher.finish() as u32)
}

/// What the `cmd` field of a log line carries about an operator command: the program's file name
/// plus its [`short_id`]. The arguments are dropped — a hook command line carries API tokens
/// (`curl -H "Authorization: …"`) as readily as a webhook URL does, and these lines land in the
/// tracing ring `GET /api/v1/logs` serves verbatim (security review 2026-08-24). A refusal still
/// names the offending *path*, through the [`exec_path_check`] error — that is what the operator
/// needs in order to fix it.
fn cmd_label(cmd: &str) -> String {
    let prog = cmd
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['"', '\'']);
    let file = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
    format!("{file} {}", short_id(cmd))
}

fn fire_exec(
    cmd: String,
    ev: &crate::events::HostEvent,
    timeout_s: u32,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let label = cmd_label(&cmd);
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(cmd = %label, "hook dropped — too many hook executions in flight");
        return;
    };
    if let Err(e) = exec_path_check(&cmd) {
        tracing::error!(cmd = %label, "REFUSING hook command — {e}");
        return;
    }
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let env = flatten_env(ev);
    let kind = ev.kind.name();
    let timeout = Duration::from_secs(u64::from(timeout_s));
    tracing::info!(cmd = %label, kind, "hook: running command");
    // Detached execution + off-thread reap (the `try_recover_session` recipe): the streaming
    // planes never wait on operator code. The permit rides along and frees on thread exit.
    std::thread::spawn(move || {
        run_hook_process(&cmd, &json, &env, timeout);
        drop(permit);
    });
}

/// The event flattened to `PF_EVENT_*` env vars: scalar leaves of the event JSON, path-joined
/// with `_` and uppercased (`client.name` → `PF_EVENT_CLIENT_NAME`), plus `PF_EVENT_JSON` with
/// the whole document. Values are control-char-stripped so a hostile device name can't smuggle
/// newlines into a naive shell consumer.
fn flatten_env(ev: &crate::events::HostEvent) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let key = k
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() {
                                c.to_ascii_uppercase()
                            } else {
                                '_'
                            }
                        })
                        .collect::<String>();
                    walk(&format!("{prefix}_{key}"), val, out);
                }
            }
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => {
                let clean: String = s.chars().filter(|c| !c.is_control()).collect();
                out.push((prefix.to_string(), clean));
            }
            other => out.push((prefix.to_string(), other.to_string())),
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::to_value(ev) {
        walk("PF_EVENT", &v, &mut out);
    }
    if let Ok(json) = serde_json::to_string(ev) {
        out.push(("PF_EVENT_JSON".to_string(), json));
    }
    out
}

/// The sshd/sudoers rule (RFC §9.1): refuse to run a command that references a script/binary which
/// is group/world-writable, or owned by neither the host user nor root — a world-writable hook
/// script is privilege-escalation bait. The same rule covers every directory above the script: a
/// writable parent is the same bait one level up, since whoever may rename an entry in it chooses
/// what runs. A bare command name (`systemctl`, `curl`) is left to PATH.
///
/// **This is a hygiene rule, not an authorization gate**, and the distinction matters: it
/// constrains *who owns the file being run*, never *what the command does*. `curl … | sh` and
/// `python3 -c '…'` are unconstrained by construction, and `/bin/sh -c '<anything>'` passes because
/// `/bin/sh` is root-owned. Whoever may WRITE a hook already has command execution as the host
/// user — which is why writing them is admin-only. A pass here does not mean "this command is
/// safe", and nothing should be granted on the strength of it.
///
/// It checks EVERY absolute-path token, not just the first (2026-08-05 review L-12). Looking only
/// at `cmd.split_whitespace().next()` meant `bash /opt/x/hook.sh`, `sh -c /tmp/x` and any quoted
/// path skipped the check entirely — so the interpreter was vetted and the script it ran was not,
/// which is backwards: the script is the part an attacker can plant.
#[cfg(unix)]
fn exec_path_check(cmd: &str) -> Result<(), String> {
    let tokens = shell_tokens(cmd);
    if tokens.is_empty() {
        return Err("empty command".into());
    }
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    for token in &tokens {
        if !token.starts_with('/') {
            continue;
        }
        let path = std::path::Path::new(token);
        if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
            continue; // not an existing file — the shell will report it
        }
        // The script, then every directory up to `/`: an unchecked writable parent lets the
        // attacker swap a perfectly-owned script out from under us, which is why sshd walks the
        // whole chain rather than stat'ing the file alone.
        for node in path.ancestors() {
            let Ok(meta) = std::fs::metadata(node) else {
                continue;
            };
            path_node_check(node, &meta, euid)?;
        }
    }
    Ok(())
}

/// The ownership/mode rule [`exec_path_check`] applies to the script and to each directory above
/// it. A world-writable *directory* with the sticky bit set (`/tmp`) passes: there only an entry's
/// own owner can replace it, so the swap this rule exists to block is already impossible.
#[cfg(unix)]
fn path_node_check(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
    euid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != euid && meta.uid() != 0 {
        return Err(format!(
            "{} is owned by uid {} (host runs as uid {euid}) — a hook script and the directories \
             holding it must be owned by the operator or root",
            path.display(),
            meta.uid()
        ));
    }
    let sticky_dir = meta.is_dir() && meta.mode() & 0o1000 != 0;
    if meta.mode() & 0o022 != 0 && !sticky_dir {
        return Err(format!(
            "{} is group/world-writable (mode {:o}) — chmod go-w it first",
            path.display(),
            meta.mode() & 0o7777
        ));
    }
    Ok(())
}

/// Split a command line into tokens the way `/bin/sh` would *for the purpose of finding paths*:
/// whitespace separates, but a quoted or backslash-escaped run stays one token. Plain
/// `split_whitespace` turned `"/opt/my hooks/run.sh"` into two nonexistent tokens, so the check
/// above silently passed the case it most needs to catch — a script the shell really does run, at
/// a path with a space in it (security review 2026-08-24).
#[cfg(unix)]
fn shell_tokens(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            // Inside '' a backslash is literal; inside "" and unquoted it escapes the next char.
            Some('"') if c == '\\' => cur.extend(chars.next()),
            Some(_) => cur.push(c),
            None if c == '\\' => cur.extend(chars.next()),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether this process is running as `NT AUTHORITY\SYSTEM` (S-1-5-18) — i.e. as the SCM service
/// rather than as the operator's own console process.
///
/// Used to decide whether the in-process hook fallback is acceptable: as the operator it is the
/// privilege they already have, as SYSTEM it is an elevation the hook contract forbids
/// (2026-08-05 review L-13). Fails CLOSED — an unreadable token is treated as SYSTEM, because the
/// consequence of guessing wrong in that direction is a skipped hook, and in the other direction
/// it is a SYSTEM command.
#[cfg(windows)]
fn running_as_system() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinLocalSystemSid, PSID,
        SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle from GetCurrentProcess; `token` is a live out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return true; // fail closed
    }
    // TOKEN_USER is align-8; a bare `[u8; 256]` is align-1, and forming `&TOKEN_USER` out of it
    // below would be UB by the language rule whenever the stack slot happens to land misaligned.
    // (Shipped codegen happens to 8-align it today — that is luck, not a guarantee.) The wrapper
    // keeps the buffer at 256 BYTES: redeclaring as `[u64; 32]` would silently turn the length
    // argument below into 32 — `len()` counts elements — and a console operator's 44-byte
    // TOKEN_USER+SID would then fail with ERROR_INSUFFICIENT_BUFFER, misclassifying every
    // hand-run host as SYSTEM (it fits exactly for SYSTEM's own 16-byte S-1-5-18, so a
    // SYSTEM-side test would not catch it).
    #[repr(align(8))]
    struct TokenUserBuf([u8; 256]);
    let mut buf = TokenUserBuf([0u8; 256]);
    let mut len = 0u32;
    // SAFETY: `buf` is a writable local of the length passed; `len` is a live out-param.
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.0.as_mut_ptr().cast()),
            std::mem::size_of_val(&buf) as u32,
            &mut len,
        )
    };
    // SAFETY: the token handle came from OpenProcessToken and is not used after this.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(token);
    }
    if got.is_err() {
        return true; // fail closed
    }
    let mut system = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut cb = system.len() as u32;
    // SAFETY: the buffer is SECURITY_MAX_SID_SIZE, the documented maximum SID size.
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(system.as_mut_ptr().cast())),
            &mut cb,
        )
    }
    .is_err()
    {
        return true; // fail closed
    }
    // SAFETY: `buf` holds a TOKEN_USER written by GetTokenInformation (align guaranteed by
    // TokenUserBuf); its `User.Sid` points into the same buffer, and both SIDs are valid for
    // this comparison.
    unsafe {
        let tu = &*(buf.0.as_ptr() as *const TOKEN_USER);
        // windows-rs maps EqualSid's BOOL(0) to Err BOTH for "SIDs differ" and for a genuine
        // failure, telling them apart only via GetLastError — so clear it first (a stale value
        // from an earlier call would otherwise read as failure) and split three ways. `.is_ok()`
        // here previously meant an EqualSid ERROR yielded "not SYSTEM" — the fail-OPEN
        // direction, contradicting the contract in the doc comment above.
        windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
        match EqualSid(tu.User.Sid, PSID(system.as_mut_ptr().cast())) {
            Ok(()) => true,                      // equal: we are SYSTEM
            Err(e) if e.code().is_ok() => false, // BOOL(0), last-error 0: genuinely not equal
            Err(_) => true,                      // EqualSid itself failed: fail closed
        }
    }
}

#[cfg(not(unix))]
fn exec_path_check(_cmd: &str) -> Result<(), String> {
    // Windows: hooks.json lives in the SYSTEM/Admins-DACL'd config dir and the command runs in
    // the interactive user session (never SYSTEM) — the config itself is the trust boundary.
    // A per-script ACL check is a hardening follow-up.
    Ok(())
}

/// Run one hook command to completion (or timeout), blocking the reaper thread it runs on.
/// Returns whether the command ran to completion successfully (exit 0) — the prep machinery
/// gates each step's `undo` on it.
#[cfg(unix)]
fn run_hook_process(
    cmd: &str,
    event_json: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> bool {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let label = cmd_label(cmd);
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Its own process group, so the timeout can kill the whole tree the shell spawned.
        .process_group(0);
    c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::error!(cmd = %label, error = %e, "hook command failed to launch");
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(event_json.as_bytes());
        // stdin drops (closes) here — a hook that never reads it is unaffected.
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::warn!(cmd = %label, %status, "hook command exited non-zero");
                }
                return status.success();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    tracing::warn!(cmd = %label, timeout_s = timeout.as_secs(),
                        "hook command timed out — killing its process group");
                    #[cfg(target_os = "linux")]
                    {
                        // SAFETY: kill(2) with a negative pid signals the process group we
                        // created via process_group(0); no memory is touched.
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    }
                    #[cfg(not(target_os = "linux"))]
                    let _ = child.kill();
                    let _ = child.wait(); // reap — never leave a zombie
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(cmd = %label, error = %e, "hook command wait failed");
                return false;
            }
        }
    }
}

/// Windows: on a SYSTEM host the command must run in the interactive user session
/// ([`crate::interactive::spawn_in_active_session`], never SYSTEM) — that path can't carry
/// per-process env or stdin, so the event JSON is written to a private temp file whose path is
/// appended as the command's last argument. A console-mode host (dev) falls back to a plain
/// spawn with the full Unix-style context (env + stdin).
#[cfg(windows)]
fn run_hook_process(
    cmd: &str,
    event_json: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> bool {
    use std::io::Write;
    let label = cmd_label(cmd);
    let stamp = format!(
        "pf-hook-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let json_path = std::env::temp_dir().join(stamp);
    if std::fs::write(&json_path, event_json).is_err() {
        tracing::warn!(cmd = %label, "hook: could not write event JSON temp file");
    }
    let cmdline = format!("{cmd} \"{}\"", json_path.display());
    match crate::interactive::spawn_in_active_session(&cmdline, None) {
        Ok(pid) => {
            tracing::debug!(cmd = %label, pid, "hook command launched in the interactive session");
            // No child handle on this path — wait out the timeout, then clean the temp file.
            std::thread::sleep(timeout);
            let _ = std::fs::remove_file(&json_path);
            // Detached in the user session: completion/exit status is unobservable here —
            // report "ran" (prep `undo`s stay armed).
            true
        }
        Err(e) if running_as_system() => {
            // NO in-process fallback when we are SYSTEM.
            //
            // `spawn_in_active_session` fails whenever there is no interactive user — pre-login, at
            // boot, on a logged-off box — and the fallback below then ran the operator's command
            // line through `cmd.exe /C` IN THIS PROCESS. As the SCM service that process is
            // LocalSystem, so a hook the module contract promises runs "in the interactive session,
            // never SYSTEM" quietly became a SYSTEM command, at the exact moments nobody is watching
            // the screen, with no ownership check on the script (`exec_path_check` is a no-op on
            // Windows) — 2026-08-05 review L-13.
            //
            // Refusing is the honest behaviour: the contract says these run as the user, and if
            // there is no user there is nothing to run them as. A hook that must run without a
            // logged-in user belongs in a service, not here.
            tracing::warn!(
                cmd = %label,
                error = %format!("{e:#}"),
                "hook SKIPPED: no interactive user session to run it in, and this host is SYSTEM — \
                 hooks run as the logged-in user by design and are never elevated to SYSTEM"
            );
            let _ = std::fs::remove_file(&json_path);
            false
        }
        Err(e) => {
            // Not SYSTEM (a hand-run `punktfunk-host serve` in the operator's own console): running
            // in-process is the same privilege the operator already has, which is the whole trust
            // model for hooks.
            tracing::debug!(error = %format!("{e:#}"),
                "interactive-session spawn unavailable — running hook in-console");
            let mut ok = false;
            let mut c = std::process::Command::new("cmd.exe");
            c.arg("/C")
                .arg(cmd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            match c.spawn() {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(event_json.as_bytes());
                    }
                    let deadline = Instant::now() + timeout;
                    loop {
                        match child.try_wait().ok().flatten() {
                            Some(status) => {
                                ok = status.success();
                                break;
                            }
                            None if Instant::now() >= deadline => {
                                tracing::warn!(cmd = %label, "hook command timed out — killing it");
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                            None => std::thread::sleep(Duration::from_millis(100)),
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(cmd = %label, error = %e, "hook command failed to launch")
                }
            }
            let _ = std::fs::remove_file(&json_path);
            ok
        }
    }
}

// ------------------------------------------------------------------------- webhook action

fn fire_webhook(
    url: String,
    secret_file: Option<PathBuf>,
    ev: &crate::events::HostEvent,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let origin = webhook_origin(&url);
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(url = %origin, "webhook dropped — too many hook executions in flight");
        return;
    };
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let kind = ev.kind.name();
    tracing::info!(url = %origin, kind, "hook: posting webhook");
    std::thread::spawn(move || {
        post_webhook(&url, &json, secret_file.as_deref());
        drop(permit);
    });
}

/// True if `url`'s host is a clearly-illegitimate webhook target — loopback, link-local (which
/// includes the `169.254.169.254` cloud-metadata endpoint), the unspecified address, or `localhost`
/// — so a tampered/misguided hooks.json can't make the privileged host POST event data to its own
/// services or a metadata endpoint (direct-SSRF guard; security-review 2026-07-17). Deliberately does
/// NOT block RFC-1918 / ULA / `.local` — a webhook to another box on the operator's own LAN is a
/// legitimate self-hosting config. A best-effort textual + IP-literal check (no DNS resolution, so
/// not a full anti-rebinding defense; the operator-gated config already limits the threat).
fn webhook_host_is_internal(url: &str) -> bool {
    let hostport = webhook_authority(url);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // [::1]:443 → ::1
    } else {
        hostport
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(hostport)
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            // Loopback (::1), unspecified (::), or link-local fe80::/10.
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false, // a resolvable hostname — not statically classifiable here
    }
}

/// `scheme://[userinfo@]host[:port]/...` → the bare `host[:port]`. Textual, no DNS.
fn webhook_authority(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority)
}

/// What the `url` field of a log line carries about a webhook: `scheme://host[:port]` plus the
/// URL's [`short_id`]. The path, query and any userinfo are dropped — for Slack, Discord, ntfy,
/// Teams, Zapier and Home Assistant the token IS a path segment, and these lines land in the
/// tracing ring `GET /api/v1/logs` serves verbatim (security review 2026-08-24).
fn webhook_origin(url: &str) -> String {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| format!("{s}://"))
        .unwrap_or_default();
    format!("{scheme}{} {}", webhook_authority(url), short_id(url))
}

fn post_webhook(url: &str, json: &str, secret_file: Option<&std::path::Path>) {
    let origin = webhook_origin(url);
    // TLS is verified (ureq's default rustls roots); redirects are never followed, so a
    // compromised receiver can't bounce the POST cross-origin (RFC §9.5).
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_global(Some(WEBHOOK_TIMEOUT))
        .build()
        .into();
    let mut req = agent.post(url).header("Content-Type", "application/json");
    if let Some(path) = secret_file {
        match std::fs::read(path) {
            Ok(secret) => {
                // `new_from_slice` moved from `Mac` to `KeyInit` in the digest 0.11 wave.
                use hmac::{Hmac, KeyInit, Mac};
                let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(&secret) {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::error!(path = %path.display(), "webhook HMAC secret unusable");
                        return;
                    }
                };
                mac.update(json.as_bytes());
                let sig = hex::encode(mac.finalize().into_bytes());
                req = req.header("X-Punktfunk-Signature", &format!("sha256={sig}"));
            }
            Err(e) => {
                // A configured-but-unreadable secret means the operator WANTS signing —
                // failing open (unsigned POST) would defeat the receiver's authentication.
                tracing::error!(path = %path.display(), error = %e,
                    "webhook HMAC secret unreadable — NOT posting unsigned");
                return;
            }
        }
    }
    match req.send(json) {
        Ok(resp) => {
            tracing::debug!(url = %origin, status = resp.status().as_u16(), "webhook delivered")
        }
        Err(ureq::Error::StatusCode(code)) => {
            tracing::warn!(url = %origin, status = code, "webhook rejected by receiver")
        }
        Err(e) => tracing::warn!(url = %origin, error = %e, "webhook delivery failed"),
    }
}

// ------------------------------------------------------------------------- per-app prep/undo

/// One per-app preparation step (RFC §6 — deliberate Sunshine `prep-cmd` parity): `do` runs
/// **synchronously before the app launches** (an HDR toggle or a MangoHud env change must land
/// first), `undo` runs at session end — reverse order across steps, best-effort, on every exit
/// path including a crash-unwind (RAII via [`PrepGuard`]).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq)]
pub struct PrepCmd {
    /// Command run before launch. Same execution recipe and ownership checks as hook `run`
    /// commands (event-less: stdin is empty JSON, env carries the `PF_APP_*` context).
    #[serde(rename = "do")]
    pub run: String,
    /// Command run after the session ends. Skipped when its `do` failed (it never took effect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
}

/// The negotiated stream mode as env for prep `do`/`undo` commands — the same `PF_STREAM_*`
/// vocabulary as the [`crate::stream_marker`] file (and the same rule: keys only ever get
/// added), so a script written against either sees one spelling. Exists because prep commands
/// are how operators do per-mode setup (an RTSS/driver FPS cap wants the refresh rate), and
/// until now their whole environment was the app identity — every mode value had to be
/// hard-coded per device. One definition, used by BOTH serving planes, so they can't drift.
pub fn prep_mode_env(width: u32, height: u32, refresh_hz: u32, hdr: bool) -> [(String, String); 4] {
    [
        ("PF_STREAM_WIDTH".to_string(), width.to_string()),
        ("PF_STREAM_HEIGHT".to_string(), height.to_string()),
        ("PF_STREAM_REFRESH".to_string(), refresh_hz.to_string()),
        ("PF_STREAM_HDR".to_string(), u8::from(hdr).to_string()),
    ]
}

/// Holds the armed `undo` commands for one session's prep steps; dropping it (session end,
/// error return, panic-unwind) runs them in reverse order on a detached thread — teardown
/// never blocks on operator code.
#[must_use = "dropping the guard immediately runs the undo commands"]
pub struct PrepGuard {
    undo: Vec<String>,
    env: Vec<(String, String)>,
}

/// Run a title's prep steps **synchronously, in order** (the caller is a launch path — this is
/// the one deliberate exception to fire-and-forget, because prep exists to happen *before* the
/// game). Each step gets the default hook timeout and the same ownership gate as hook
/// commands; a failed/refused `do` logs and continues (best-effort), and its `undo` stays
/// disarmed. Returns the guard that runs the armed `undo`s at drop.
pub fn run_prep(cmds: &[PrepCmd], env: &[(String, String)]) -> PrepGuard {
    let timeout = Duration::from_secs(u64::from(DEFAULT_TIMEOUT_S));
    let mut undo = Vec::new();
    for c in cmds {
        let cmd = c.run.trim();
        if cmd.is_empty() {
            continue;
        }
        let label = cmd_label(cmd);
        if let Err(e) = exec_path_check(cmd) {
            tracing::error!(cmd = %label, "REFUSING prep command — {e}");
            continue;
        }
        tracing::info!(cmd = %label, "prep: running");
        if run_hook_process(cmd, "{}", env, timeout) {
            if let Some(u) = c.undo.as_deref().filter(|u| !u.trim().is_empty()) {
                undo.push(u.to_string());
            }
        } else if c.undo.is_some() {
            tracing::warn!(cmd = %label, "prep step failed — its undo is skipped");
        }
    }
    PrepGuard {
        undo,
        env: env.to_vec(),
    }
}

impl Drop for PrepGuard {
    fn drop(&mut self) {
        if self.undo.is_empty() {
            return;
        }
        let undo = std::mem::take(&mut self.undo);
        let env = std::mem::take(&mut self.env);
        let timeout = Duration::from_secs(u64::from(DEFAULT_TIMEOUT_S));
        // Detached: the drop site may be an async task or a panic-unwind — session teardown
        // must not block on operator commands. Order (reverse of `do`) is preserved because
        // the one thread runs them sequentially.
        std::thread::spawn(move || {
            for cmd in undo.iter().rev() {
                let label = cmd_label(cmd);
                if let Err(e) = exec_path_check(cmd) {
                    tracing::error!(cmd = %label, "REFUSING prep undo command — {e}");
                    continue;
                }
                tracing::info!(cmd = %label, "prep: running undo");
                run_hook_process(cmd, "{}", &env, timeout);
            }
        });
    }
}

// ------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ClientRef, EventKind, HostEvent, Plane, StreamRef};

    fn sample_event() -> HostEvent {
        HostEvent {
            seq: 7,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::StreamStarted {
                stream: StreamRef {
                    mode: "2560x1440@120".into(),
                    hdr: true,
                    client: "Living Room TV".into(),
                    app: Some("steam:570".into()),
                    plane: Plane::Native,
                },
            },
        }
    }

    #[test]
    fn validation_rejects_structural_errors() {
        let ok = HooksConfig {
            hooks: vec![HookEntry {
                on: "stream.*".into(),
                filter: None,
                run: Some("echo hi".into()),
                webhook: None,
                timeout_s: 30,
                debounce_ms: 0,
                hmac_secret_file: None,
            }],
        };
        assert!(ok.validate().is_ok());

        let mut bad = ok.clone();
        bad.hooks[0].on = " ".into();
        assert!(bad.validate().is_err(), "empty `on`");

        let mut bad = ok.clone();
        bad.hooks[0].run = None;
        assert!(bad.validate().is_err(), "no action");

        let mut bad = ok.clone();
        bad.hooks[0].webhook = Some("ftp://nope".into());
        assert!(bad.validate().is_err(), "non-http webhook");

        let mut bad = ok.clone();
        bad.hooks[0].timeout_s = 0;
        assert!(bad.validate().is_err(), "zero timeout");
        bad.hooks[0].timeout_s = 601;
        assert!(bad.validate().is_err(), "over-ceiling timeout");
    }

    #[test]
    fn store_roundtrips_and_survives_corruption() {
        let path = std::env::temp_dir().join(format!(
            "pf-hooks-test-{}-{:p}.json",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&path);

        let store = HooksStore::load_from(path.clone());
        assert!(store.get().hooks.is_empty(), "unconfigured = no hooks");

        let cfg = HooksConfig {
            hooks: vec![HookEntry {
                on: "pairing.pending".into(),
                filter: Some(HookFilter {
                    plane: Some(Plane::Native),
                    ..Default::default()
                }),
                run: None,
                webhook: Some("https://ha.local/api/webhook/punktfunk".into()),
                timeout_s: 30,
                debounce_ms: 500,
                hmac_secret_file: None,
            }],
        };
        store.set(cfg).unwrap();
        assert_eq!(store.get().hooks.len(), 1);

        // A fresh load sees the persisted value.
        let reload = HooksStore::load_from(path.clone());
        assert_eq!(reload.get().hooks.len(), 1);
        assert_eq!(reload.get().hooks[0].on, "pairing.pending");

        // Corruption never breaks startup — it just disables hooks loudly.
        std::fs::write(&path, b"{ not json").unwrap();
        let corrupt = HooksStore::load_from(path.clone());
        assert!(corrupt.get().hooks.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hand_edited_file_reloads_without_restart() {
        let path = std::env::temp_dir().join(format!(
            "pf-hooks-reload-test-{}-{:p}.json",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&path);

        let store = HooksStore::load_from(path.clone());
        assert!(store.get().hooks.is_empty());

        // The documented flow: the operator writes hooks.json by hand and the SAME running
        // store honors it on the next event — no restart, no PUT.
        std::fs::write(
            &path,
            br#"{"hooks":[{"on":"stream.started","run":"true"}]}"#,
        )
        .unwrap();
        assert_eq!(store.get().hooks.len(), 1, "hand edit applies on next read");
        assert_eq!(store.get().hooks[0].on, "stream.started");

        // A second edit applies too (length differs, so same-second mtime granularity can't
        // mask it).
        std::fs::write(
            &path,
            br#"{"hooks":[{"on":"stream.started","run":"true"},{"on":"client.*","run":"true"}]}"#,
        )
        .unwrap();
        assert_eq!(store.get().hooks.len(), 2, "second hand edit applies too");

        // Deleting the file removes the hooks.
        std::fs::remove_file(&path).unwrap();
        assert!(store.get().hooks.is_empty(), "deleted file = no hooks");
    }

    #[test]
    fn filters_constrain_and_missing_fields_never_match() {
        let ev = sample_event();
        let f = HookFilter {
            client: Some("Living Room TV".into()),
            app: Some("steam:570".into()),
            plane: Some(Plane::Native),
            ..Default::default()
        };
        assert!(f.matches(&ev.kind));

        let f = HookFilter {
            client: Some("Bedroom".into()),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        let f = HookFilter {
            plane: Some(Plane::Gamestream),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        // stream.* events carry no fingerprint — a fingerprint filter can't match them.
        let f = HookFilter {
            fingerprint: Some("ab12".into()),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        // Fingerprint matching is case-insensitive where the field exists.
        let connected = EventKind::ClientConnected {
            client: ClientRef {
                name: "Deck".into(),
                fingerprint: Some("AB12CD".into()),
                plane: Plane::Native,
            },
        };
        let f = HookFilter {
            fingerprint: Some("ab12cd".into()),
            ..Default::default()
        };
        assert!(f.matches(&connected));
    }

    #[test]
    fn env_flattening_is_shell_safe_and_complete() {
        let ev = sample_event();
        let env = flatten_env(&ev);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PF_EVENT_KIND"), Some("stream.started"));
        assert_eq!(get("PF_EVENT_SEQ"), Some("7"));
        assert_eq!(get("PF_EVENT_STREAM_MODE"), Some("2560x1440@120"));
        assert_eq!(get("PF_EVENT_STREAM_HDR"), Some("true"));
        assert_eq!(get("PF_EVENT_STREAM_CLIENT"), Some("Living Room TV"));
        assert_eq!(get("PF_EVENT_STREAM_APP"), Some("steam:570"));
        assert_eq!(get("PF_EVENT_STREAM_PLANE"), Some("native"));
        assert!(get("PF_EVENT_JSON").unwrap().contains("\"seq\":7"));

        // A hostile client name can't smuggle control chars into env consumers.
        let mut evil = sample_event();
        if let EventKind::StreamStarted { stream } = &mut evil.kind {
            stream.client = "evil\nname\r\t".into();
        }
        let env = flatten_env(&evil);
        let v = env
            .iter()
            .find(|(k, _)| k == "PF_EVENT_STREAM_CLIENT")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(v, "evilname");
    }

    #[test]
    fn prep_mode_env_speaks_the_marker_vocabulary() {
        // The names are the stream-marker file's — a script written against either sees one
        // spelling — and HDR is 1/0 like the marker, not true/false like PF_EVENT_*.
        let env = prep_mode_env(2560, 1440, 120, true);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PF_STREAM_WIDTH"), Some("2560"));
        assert_eq!(get("PF_STREAM_HEIGHT"), Some("1440"));
        assert_eq!(get("PF_STREAM_REFRESH"), Some("120"));
        assert_eq!(get("PF_STREAM_HDR"), Some("1"));
        assert_eq!(prep_mode_env(1, 1, 1, false)[3].1, "0");
    }

    #[cfg(unix)]
    #[test]
    fn exec_runs_with_stdin_and_env_and_timeout_kills() {
        // A hook that proves stdin + env delivery by writing both to a file.
        let out = std::env::temp_dir().join(format!(
            "pf-hook-exec-{}-{:p}.txt",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&out);
        let ev = sample_event();
        let env = flatten_env(&ev);
        let json = serde_json::to_string(&ev).unwrap();
        run_hook_process(
            &format!(
                "printf '%s|' \"$PF_EVENT_KIND\" > {p}; cat >> {p}",
                p = out.display()
            ),
            &json,
            &env,
            Duration::from_secs(5),
        );
        let text = std::fs::read_to_string(&out).expect("hook wrote its file");
        assert!(text.starts_with("stream.started|"), "env delivered: {text}");
        assert!(text.contains("\"seq\":7"), "stdin delivered: {text}");
        let _ = std::fs::remove_file(&out);

        // Timeout: a sleeping hook is killed (process group) well before its sleep ends.
        let started = Instant::now();
        run_hook_process("sleep 30", &json, &env, Duration::from_secs(1));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must kill the hook, not wait it out"
        );
    }

    /// Prep semantics end to end: `do`s run in order before the guard exists, armed `undo`s run
    /// in REVERSE order at drop, and a failed `do` disarms its own `undo` only.
    #[cfg(unix)]
    #[test]
    fn prep_runs_do_in_order_and_undo_in_reverse() {
        let out = std::env::temp_dir().join(format!(
            "pf-prep-test-{}-{:p}.txt",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&out);
        let step = |do_tag: &str, undo_tag: Option<&str>| PrepCmd {
            run: format!("echo {do_tag} >> {}", out.display()),
            undo: undo_tag.map(|t| format!("echo {t} >> {}", out.display())),
        };
        let cmds = vec![
            step("do-a", Some("undo-a")),
            step("do-b", Some("undo-b")),
            // A failing `do` must not arm its undo.
            PrepCmd {
                run: "false".into(),
                undo: Some(format!("echo undo-never >> {}", out.display())),
            },
        ];
        let guard = run_prep(&cmds, &[]);
        let text = std::fs::read_to_string(&out).expect("prep steps ran synchronously");
        assert_eq!(text, "do-a\ndo-b\n", "dos run in order, before launch");

        drop(guard);
        // The undo thread is detached — poll for its completion.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            if text.lines().count() >= 4 {
                assert_eq!(
                    text, "do-a\ndo-b\nundo-b\nundo-a\n",
                    "undos run in reverse; the failed step's undo is skipped"
                );
                break;
            }
            assert!(Instant::now() < deadline, "undo thread never ran: {text}");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Give the skipped-undo a beat to (wrongly) appear, then assert it didn't.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!std::fs::read_to_string(&out)
            .unwrap()
            .contains("undo-never"));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn prep_cmd_wire_shape() {
        // The RFC's `{ "do": …, "undo": … }` spelling is the wire contract.
        let c: PrepCmd = serde_json::from_str(r#"{"do":"a","undo":"b"}"#).unwrap();
        assert_eq!(c.run, "a");
        assert_eq!(c.undo.as_deref(), Some("b"));
        let c: PrepCmd = serde_json::from_str(r#"{"do":"a"}"#).unwrap();
        assert!(c.undo.is_none());
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"do":"a"}"#);
    }

    #[cfg(unix)]
    #[test]
    fn ownership_check_refuses_world_writable_scripts() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "pf-hook-own-{}-{:p}.sh",
            std::process::id(),
            &0u8 as *const u8
        ));
        std::fs::write(&path, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(exec_path_check(&format!("{} arg", path.display())).is_ok());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            exec_path_check(&format!("{} arg", path.display())).is_err(),
            "world-writable script must be refused"
        );
        let _ = std::fs::remove_file(&path);

        // Bare command names are left to PATH; nonexistent paths are the shell's problem.
        assert!(exec_path_check("systemctl suspend").is_ok());
        assert!(exec_path_check("/nonexistent/definitely-not-here").is_ok());
    }

    /// The two holes the check used to have: a path with a space in it (whitespace splitting made
    /// the check a no-op for exactly the paths the shell still runs), and a writable directory
    /// above an otherwise-fine script.
    #[cfg(unix)]
    #[test]
    fn ownership_check_sees_quoted_paths_and_writable_parents() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode;
        let dir = std::env::temp_dir().join(format!(
            "pf-hook-parent-{}-{:p}",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, mode(0o755)).unwrap();
        let script = dir.join("my hook.sh");
        std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&script, mode(0o700)).unwrap();
        let quoted = format!("\"{}\" arg", script.display());

        assert!(exec_path_check(&quoted).is_ok(), "a sane quoted path runs");
        std::fs::set_permissions(&script, mode(0o777)).unwrap();
        assert!(
            exec_path_check(&quoted).is_err(),
            "world-writable script behind a quoted, space-bearing path must be refused"
        );
        assert!(
            exec_path_check(&format!("'{}'", script.display())).is_err(),
            "single quotes too"
        );
        assert!(
            exec_path_check(&script.display().to_string().replace(' ', "\\ ")).is_err(),
            "backslash-escaped spaces too"
        );

        // A writable parent defeats a perfectly-owned script — the attacker replaces the file.
        std::fs::set_permissions(&script, mode(0o700)).unwrap();
        std::fs::set_permissions(&dir, mode(0o777)).unwrap();
        let err = exec_path_check(&quoted).expect_err("world-writable parent must be refused");
        assert!(err.contains(&dir.display().to_string()), "names it: {err}");
        std::fs::set_permissions(&dir, mode(0o755)).unwrap();
        assert!(exec_path_check(&quoted).is_ok(), "chmod go-w fixes it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `hmac_secret_file` warning the field doc promises.
    #[cfg(unix)]
    #[test]
    fn secret_file_permissions_are_complained_about() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "pf-hook-secret-{}-{:p}.key",
            std::process::id(),
            &0u8 as *const u8
        ));
        std::fs::write(&path, b"s3cret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(secret_file_complaint(&path).is_none(), "0600 is the ask");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let why = secret_file_complaint(&path).expect("a world-readable secret is warned about");
        assert!(why.contains("644"), "the complaint names the mode: {why}");

        // Missing/unreadable is post_webhook's fail-closed case, not a permissions complaint.
        std::fs::remove_file(&path).unwrap();
        assert!(secret_file_complaint(&path).is_none());
    }

    /// `GET /api/v1/logs` serves the tracing ring verbatim, so no log line may carry a webhook URL's
    /// path (the bearer credential for Slack/Discord/ntfy) or a command's arguments — while still
    /// saying which hook fired.
    #[test]
    fn log_labels_drop_the_credential_and_stay_identifiable() {
        let slack = "https://hooks.slack.com/services/T0000/B0000/XXXXsecretXXXX";
        let shown = webhook_origin(slack);
        assert!(
            shown.starts_with("https://hooks.slack.com "),
            "origin: {shown}"
        );
        assert!(!shown.contains("XXXXsecretXXXX"), "token dropped: {shown}");
        assert_ne!(
            shown,
            webhook_origin("https://hooks.slack.com/services/T1/B1/OTHER"),
            "two hooks to one host stay distinguishable"
        );

        // userinfo, path and query all go; the port stays (it names the receiver, not the secret).
        let creds = webhook_origin("https://user:pw@ha.local:8123/api/webhook/zzz?token=qqq");
        assert!(creds.starts_with("https://ha.local:8123 "), "{creds}");
        for secret in ["pw", "zzz", "qqq"] {
            assert!(!creds.contains(secret), "{secret} leaked: {creds}");
        }

        // Command lines: the program's file name survives, its arguments don't.
        let cmd = "/usr/local/bin/notify.sh --token=SEKRIT-zz 'Living Room'";
        let label = cmd_label(cmd);
        assert!(
            label.starts_with("notify.sh #"),
            "says which script ran: {label}"
        );
        assert!(!label.contains("SEKRIT"), "arguments dropped: {label}");
        assert_eq!(
            label,
            cmd_label(cmd),
            "stable across one firing's log lines"
        );
        assert_ne!(
            label,
            cmd_label("/usr/local/bin/notify.sh --token=SEKRIT-yy")
        );
        assert!(cmd_label("curl -H \"Authorization: Bearer t\" https://x/y").starts_with("curl #"));
    }
}
