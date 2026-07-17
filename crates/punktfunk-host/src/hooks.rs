//! Operator hooks: commands and webhooks fired on host lifecycle events
//! (scripting-and-hooks RFC §6, M2).
//!
//! `<config_dir>/hooks.json` holds a list of [`HookEntry`]s — *what to run on which event* —
//! managed over `GET|PUT /api/v1/hooks` and applied immediately (the runner reads the store
//! per event). The runner subscribes to the [`crate::events`] bus and dispatches matching
//! entries **fire-and-forget**: a hook observes; it can never veto or delay a connection,
//! stream, or pairing decision (decisions are made asynchronously through the API — RFC §6).
//!
//! Two actions:
//! - **`run`** — a shell command, executed detached with the event JSON on stdin plus flat
//!   `PF_EVENT_*` env vars (the [`crate::stream_marker`] `PF_STREAM_*` vocabulary's sibling).
//!   Per-hook timeout (default 30 s) kills the whole process group on expiry; reaped
//!   off-thread (the `try_recover_session` recipe). On a SYSTEM-service Windows host the
//!   command runs **in the interactive user session** (never SYSTEM); that path cannot carry
//!   per-process env/stdin, so the event JSON lands in a temp file appended as the command's
//!   last argument (a console-mode Windows host gets env + stdin like Unix).
//! - **`webhook`** — POST the event JSON to an operator URL. TLS-verified, redirects are not
//!   followed, no punktfunk credentials are attached; an optional per-hook secret file yields
//!   an `X-Punktfunk-Signature: sha256=<hex HMAC>` header so the receiver can authenticate us.
//!
//! Bounds (RFC §9.6): at most [`MAX_CONCURRENT_HOOKS`] hook executions in flight (excess
//! firings are dropped with a warning, never queued unboundedly), per-hook `debounce_ms`, the
//! exec timeout + process-group kill. Trust model (RFC §9.1): `hooks.json` is
//! operator-privileged config in the DACL'd/0700 config dir; before executing a hook whose
//! command is a script *path*, the host verifies the file is owned by the operator (or root)
//! and not group/world-writable — the sshd/sudoers rule — and refuses loudly otherwise.

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
            }
            if h.timeout_s == 0 || h.timeout_s > MAX_TIMEOUT_S {
                return Err(at(&format!("`timeout_s` must be 1–{MAX_TIMEOUT_S}")));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------- store

/// The persisted hooks store — the [`crate::vdisplay::policy::DisplayPolicyStore`] recipe:
/// private dir, temp-write + atomic rename, in-memory value changes only if the write succeeds.
pub struct HooksStore {
    path: PathBuf,
    cur: Mutex<Option<HooksConfig>>,
}

impl HooksStore {
    /// Load from `path`. Missing file ⇒ no hooks; corrupt file ⇒ no hooks with a warning
    /// (never fail host startup over a settings file).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
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
        HooksStore {
            path,
            cur: Mutex::new(cur),
        }
    }

    /// The stored configuration (empty when unconfigured) — the mgmt GET and the dispatcher.
    pub fn get(&self) -> HooksConfig {
        self.cur.lock().unwrap().clone().unwrap_or_default()
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
        *self.cur.lock().unwrap() = Some(cfg);
        Ok(())
    }
}

/// The process-wide hooks store (`<config_dir>/hooks.json`), loaded once on first access.
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

fn fire_exec(
    cmd: String,
    ev: &crate::events::HostEvent,
    timeout_s: u32,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(cmd = %cmd, "hook dropped — too many hook executions in flight");
        return;
    };
    if let Err(e) = exec_path_check(&cmd) {
        tracing::error!(cmd = %cmd, "REFUSING hook command — {e}");
        return;
    }
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let env = flatten_env(ev);
    let kind = ev.kind.name();
    let timeout = Duration::from_secs(u64::from(timeout_s));
    tracing::info!(cmd = %cmd, kind, "hook: running command");
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

/// The sshd/sudoers rule (RFC §9.1): when the command's first token is a path to an existing
/// file, refuse to run it unless it is owned by the host user (or root) and not
/// group/world-writable — a world-writable hook script is privilege escalation bait. A bare
/// command name (`systemctl`, `curl`) is left to PATH.
#[cfg(unix)]
fn exec_path_check(cmd: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let Some(first) = cmd.split_whitespace().next() else {
        return Err("empty command".into());
    };
    if !first.starts_with('/') {
        return Ok(());
    }
    let meta = match std::fs::metadata(first) {
        Ok(m) => m,
        Err(_) => return Ok(()), // not an existing file — the shell will report it
    };
    if !meta.is_file() {
        return Ok(());
    }
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid && meta.uid() != 0 {
        return Err(format!(
            "{first} is owned by uid {} (host runs as uid {euid}) — hook scripts must be \
             owned by the operator or root",
            meta.uid()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{first} is group/world-writable (mode {:o}) — chmod go-w it first",
            meta.mode() & 0o7777
        ));
    }
    Ok(())
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
            tracing::error!(cmd = %cmd, error = %e, "hook command failed to launch");
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
                    tracing::warn!(cmd = %cmd, %status, "hook command exited non-zero");
                }
                return status.success();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    tracing::warn!(cmd = %cmd, timeout_s = timeout.as_secs(),
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
                tracing::warn!(cmd = %cmd, error = %e, "hook command wait failed");
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
        tracing::warn!(cmd = %cmd, "hook: could not write event JSON temp file");
    }
    let cmdline = format!("{cmd} \"{}\"", json_path.display());
    match crate::interactive::spawn_in_active_session(&cmdline, None) {
        Ok(pid) => {
            tracing::debug!(cmd = %cmd, pid, "hook command launched in the interactive session");
            // No child handle on this path — wait out the timeout, then clean the temp file.
            std::thread::sleep(timeout);
            let _ = std::fs::remove_file(&json_path);
            // Detached in the user session: completion/exit status is unobservable here —
            // report "ran" (prep `undo`s stay armed).
            true
        }
        Err(e) => {
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
                                tracing::warn!(cmd = %cmd, "hook command timed out — killing it");
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                            None => std::thread::sleep(Duration::from_millis(100)),
                        }
                    }
                }
                Err(e) => tracing::error!(cmd = %cmd, error = %e, "hook command failed to launch"),
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
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(url = %url, "webhook dropped — too many hook executions in flight");
        return;
    };
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let kind = ev.kind.name();
    tracing::info!(url = %url, kind, "hook: posting webhook");
    std::thread::spawn(move || {
        post_webhook(&url, &json, secret_file.as_deref());
        drop(permit);
    });
}

fn post_webhook(url: &str, json: &str, secret_file: Option<&std::path::Path>) {
    // TLS is verified (ureq's default rustls roots); redirects are never followed, so a
    // compromised receiver can't bounce the POST cross-origin (RFC §9.5).
    let agent = ureq::builder()
        .redirects(0)
        .timeout(WEBHOOK_TIMEOUT)
        .build();
    let mut req = agent.post(url).set("Content-Type", "application/json");
    if let Some(path) = secret_file {
        match std::fs::read(path) {
            Ok(secret) => {
                use hmac::{Hmac, Mac};
                let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(&secret) {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::error!(path = %path.display(), "webhook HMAC secret unusable");
                        return;
                    }
                };
                mac.update(json.as_bytes());
                let sig = hex::encode(mac.finalize().into_bytes());
                req = req.set("X-Punktfunk-Signature", &format!("sha256={sig}"));
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
    match req.send_string(json) {
        Ok(resp) => tracing::debug!(url, status = resp.status(), "webhook delivered"),
        Err(ureq::Error::Status(code, _)) => {
            tracing::warn!(url, status = code, "webhook rejected by receiver")
        }
        Err(e) => tracing::warn!(url, error = %e, "webhook delivery failed"),
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
        if let Err(e) = exec_path_check(cmd) {
            tracing::error!(cmd = %cmd, "REFUSING prep command — {e}");
            continue;
        }
        tracing::info!(cmd = %cmd, "prep: running");
        if run_hook_process(cmd, "{}", env, timeout) {
            if let Some(u) = c.undo.as_deref().filter(|u| !u.trim().is_empty()) {
                undo.push(u.to_string());
            }
        } else if c.undo.is_some() {
            tracing::warn!(cmd = %cmd, "prep step failed — its undo is skipped");
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
                if let Err(e) = exec_path_check(cmd) {
                    tracing::error!(cmd = %cmd, "REFUSING prep undo command — {e}");
                    continue;
                }
                tracing::info!(cmd = %cmd, "prep: running undo");
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
}
