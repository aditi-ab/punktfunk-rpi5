//! Host status model + the poller thread feeding the platform tray implementations.
//!
//! Two sources, service manager FIRST: the SCM (Windows) / systemd user unit (Linux) decides
//! stopped-vs-running — a malicious local process squatting the mgmt port while the service is
//! down can never make the tray say Running. Only when the service manager reports Running does
//! the poller consult the host's loopback-only `GET /api/v1/local/summary` for streaming detail.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What the service manager reports for the host service.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    /// Linux `ActiveState=failed` (with the sub-state), or a Windows stop with a failure exit code.
    Failed(String),
}

/// `GET /api/v1/local/summary` — the non-sensitive counts/booleans the host serves to loopback
/// peers without authentication (mgmt.rs `LocalSummary`). Unknown fields are ignored so a newer
/// host can grow the summary without breaking an older tray.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Summary {
    pub version: String,
    pub video_streaming: bool,
    pub audio_streaming: bool,
    pub session: Option<SessionInfo>,
    pub paired_clients: u32,
    pub native_paired_clients: u32,
    pub pin_pending: bool,
    pub pending_approvals: u32,
    /// Virtual displays kept with no live session (lingering/pinned). `#[serde(default)]` so an older
    /// host that doesn't send it deserializes as 0.
    #[serde(default)]
    pub kept_displays: u32,
    /// Other Moonlight-compatible hosts (Sunshine/Apollo/…) the host detected on this machine at
    /// startup — side-by-side use is unsupported. `#[serde(default)]` so an older host omitting it
    /// deserializes as empty.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
pub struct SessionInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// What the icon shows.
#[derive(Clone, Debug, PartialEq)]
pub enum TrayStatus {
    NotInstalled,
    Stopped,
    /// Service starting, or running with the mgmt API not answering yet (within [`START_GRACE`]).
    Starting,
    Running(Summary),
    /// Service running but the summary unreachable past the grace period — amber, not red: a
    /// custom `PUNKTFUNK_HOST_CMD` (no mgmt API) or a relocated `--mgmt-bind` is legitimate.
    Degraded,
    Error(String),
}

impl TrayStatus {
    /// One-line headline for the tooltip / the disabled menu header.
    pub fn headline(&self) -> String {
        match self {
            TrayStatus::NotInstalled => "punktfunk host — not installed".into(),
            TrayStatus::Stopped => "punktfunk host — stopped".into(),
            TrayStatus::Starting => "punktfunk host — starting…".into(),
            TrayStatus::Degraded => "punktfunk host — running (status unavailable)".into(),
            TrayStatus::Error(e) => format!("punktfunk host — failed ({e})"),
            TrayStatus::Running(s) => {
                let base = match (&s.session, s.video_streaming) {
                    (Some(sess), true) => format!(
                        "punktfunk host {} — streaming {}×{}@{}",
                        s.version, sess.width, sess.height, sess.fps
                    ),
                    (_, true) => format!("punktfunk host {} — streaming", s.version),
                    // Idle, but surface a kept (lingering/pinned) display: it — and, under an
                    // exclusive topology, your physical monitors — is being held. Release it from
                    // the console.
                    _ if s.kept_displays > 0 => format!(
                        "punktfunk host {} — idle · {} display{} kept",
                        s.version,
                        s.kept_displays,
                        if s.kept_displays == 1 { "" } else { "s" }
                    ),
                    _ => format!("punktfunk host {} — idle", s.version),
                };
                // A conflicting Moonlight host (Sunshine/Apollo/…) is the loudest thing to say —
                // side-by-side use is unsupported, so lead the tooltip with it.
                if s.conflicts.is_empty() {
                    base
                } else {
                    format!("⚠ conflicting host: {} — {base}", s.conflicts.join(", "))
                }
            }
        }
    }

    /// The host detected another Moonlight-compatible host (Sunshine/Apollo/…) on this machine —
    /// unsupported side-by-side. Drives the tray's attention state.
    pub fn has_conflicts(&self) -> bool {
        matches!(self, TrayStatus::Running(s) if !s.conflicts.is_empty())
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self, TrayStatus::Running(s) if s.video_streaming)
    }

    /// A pairing attempt is waiting on the operator (shown as an extra menu entry).
    pub fn pairing_attention(&self) -> bool {
        matches!(self, TrayStatus::Running(s) if s.pin_pending || s.pending_approvals > 0)
    }
}

/// How long a running service may leave the summary unreachable before Starting turns Degraded.
/// Also re-applied mid-life: the SYSTEM supervisor relaunching a crashed host child looks like
/// "running, briefly unreachable" — that shows as Starting again, not an alarming flicker to red.
pub const START_GRACE: Duration = Duration::from_secs(15);

/// Pure status mapping (unit-tested): service-manager state first, summary second, grace third.
pub fn map_status(svc: &ServiceState, summary: Option<Summary>, grace_expired: bool) -> TrayStatus {
    match svc {
        ServiceState::NotInstalled => TrayStatus::NotInstalled,
        ServiceState::Stopped | ServiceState::StopPending => TrayStatus::Stopped,
        ServiceState::StartPending => TrayStatus::Starting,
        ServiceState::Failed(e) => TrayStatus::Error(e.clone()),
        ServiceState::Running => match summary {
            Some(s) => TrayStatus::Running(s),
            None if !grace_expired => TrayStatus::Starting,
            None => TrayStatus::Degraded,
        },
    }
}

// ── Poller ──────────────────────────────────────────────────────────────────────────────────────

pub struct Poller {
    shared: Arc<Shared>,
}

struct Shared {
    poked: Mutex<bool>,
    cv: Condvar,
}

impl Poller {
    /// Spawn the poll thread; `on_change(status, console_up)` fires (from that thread) whenever
    /// either changes. `console_up` is a live loopback probe of the web console on `web_port` —
    /// ground truth for the "Open web console" menu entry (a layout sniff would miss consoles run
    /// from a repo checkout, and shows a dead entry while an installed console is still starting).
    pub fn spawn(
        mgmt_addr: String,
        mgmt_port: u16,
        web_port: u16,
        on_change: Box<dyn Fn(TrayStatus, bool) + Send>,
    ) -> Poller {
        let shared = Arc::new(Shared {
            poked: Mutex::new(false),
            cv: Condvar::new(),
        });
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("status-poll".into())
            .spawn(move || poll_loop(&thread_shared, &mgmt_addr, mgmt_port, web_port, on_change))
            .expect("spawn status-poll thread");
        Poller { shared }
    }

    /// Force an immediate re-poll (right after a start/stop/restart menu action).
    pub fn poke(&self) {
        *self.shared.poked.lock().unwrap() = true;
        self.shared.cv.notify_one();
    }
}

fn poll_loop(
    shared: &Shared,
    mgmt_addr: &str,
    mgmt_port: u16,
    web_port: u16,
    on_change: Box<dyn Fn(TrayStatus, bool) + Send>,
) {
    // IPv6 literals bracketed, like the Linux client's `base_url`.
    let url = if mgmt_addr.contains(':') {
        format!("https://[{mgmt_addr}]:{mgmt_port}/api/v1/local/summary")
    } else {
        format!("https://{mgmt_addr}:{mgmt_port}/api/v1/local/summary")
    };
    let console_url = format!("https://127.0.0.1:{web_port}/");
    let agent = agent(load_pin());
    let mut last: Option<(TrayStatus, bool)> = None;
    // When the summary became unreachable while the service was running (grace anchor).
    // Runs for the process lifetime (the tray exits by process exit; nothing to unwind).
    let mut unreachable_since: Option<Instant> = None;
    loop {
        let svc = probe_service();
        let summary = if svc == ServiceState::Running {
            let s = fetch_summary(&agent, &url);
            match s {
                Some(_) => unreachable_since = None,
                None if unreachable_since.is_none() => unreachable_since = Some(Instant::now()),
                None => {}
            }
            s
        } else {
            unreachable_since = None;
            None
        };
        let grace_expired = unreachable_since.is_some_and(|t| t.elapsed() >= START_GRACE);
        let status = map_status(&svc, summary, grace_expired);
        let console_up = probe_console(&agent, &console_url);
        if last.as_ref() != Some(&(status.clone(), console_up)) {
            on_change(status.clone(), console_up);
            last = Some((status, console_up));
        }
        // 3 s while there is anything to watch; back off when the box just doesn't run a host.
        let cadence = match last.as_ref().map(|(s, _)| s) {
            Some(TrayStatus::Stopped) | Some(TrayStatus::NotInstalled) => Duration::from_secs(10),
            _ => Duration::from_secs(3),
        };
        let mut poked = shared.poked.lock().unwrap();
        if !*poked {
            (poked, _) = shared.cv.wait_timeout(poked, cadence).unwrap();
        }
        *poked = false;
    }
}

/// Is the web console answering on loopback? Any HTTP response (incl. the login redirect / 401)
/// counts as up — only a transport failure (nothing listening, TLS handshake dead) means down.
fn probe_console(agent: &ureq::Agent, url: &str) -> bool {
    match agent.get(url).call() {
        Ok(_) => true,
        Err(ureq::Error::Status(..)) => true,
        Err(_) => false,
    }
}

// ── Summary fetch (loopback HTTPS) ──────────────────────────────────────────────────────────────

fn fetch_summary(agent: &ureq::Agent, url: &str) -> Option<Summary> {
    let body = agent.get(url).call().ok()?.into_string().ok()?;
    serde_json::from_str(&body).ok()
}

/// The host identity cert's SHA-256, when `cert.pem` is readable (Linux: same-user file). On
/// Windows the file is SYSTEM/Administrators-DACL'd, so the per-user tray can't pin — `None` =
/// accept any cert. That is acceptable here: the connection is loopback, carries no credentials,
/// and only *reads* non-sensitive data; stopped-vs-running is decided by the service manager, so
/// a port-squatter gains nothing but a fake "streaming" tooltip on an already-compromised box.
fn load_pin() -> Option<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let pem = std::fs::read(punktfunk_config_dir()?.join("cert.pem")).ok()?;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).ok()?;
    Some(punktfunk_core::tls::cert_fingerprint(der.as_ref()))
}

/// The host's config dir, mirroring `gamestream::config_dir()` without linking the host crate:
/// `PUNKTFUNK_CONFIG_DIR` override, else `$XDG_CONFIG_HOME`/`~/.config` + `punktfunk` (Linux).
/// `None` on Windows — everything the tray would read there is SYSTEM/Admins-DACL'd anyway.
pub fn punktfunk_config_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("PUNKTFUNK_CONFIG_DIR") {
        if !d.is_empty() {
            return Some(std::path::PathBuf::from(d));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            if !x.is_empty() {
                return Some(std::path::PathBuf::from(x).join("punktfunk"));
            }
        }
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join(".config")
                .join("punktfunk")
        })
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// A sync HTTPS agent over the same rustls(ring) stack the rest of the workspace uses, with a
/// pin-or-accept-any verifier (the Linux client's `PinVerify` pattern, `library.rs`).
fn agent(pin: Option<[u8; 32]>) -> ureq::Agent {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::new(pin)))
        .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(Arc::new(cfg))
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
}

// ── Service-manager probe ───────────────────────────────────────────────────────────────────────

/// The SCM name registered by `punktfunk-host service install` (windows/service.rs SERVICE_NAME).
#[cfg(windows)]
pub const SERVICE_NAME: &str = "PunktfunkHost";

#[cfg(windows)]
pub fn probe_service() -> ServiceState {
    use windows_service::service::{ServiceAccess, ServiceExitCode, ServiceState as Scm};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    // CONNECT + QUERY_STATUS work unprivileged. Re-opened every poll on purpose: a reinstall
    // (delete + create) invalidates old handles, and this picks the new service up within a poll.
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return ServiceState::NotInstalled;
    };
    let Ok(svc) = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) else {
        return ServiceState::NotInstalled; // ERROR_SERVICE_DOES_NOT_EXIST et al.
    };
    let Ok(status) = svc.query_status() else {
        return ServiceState::NotInstalled;
    };
    match status.current_state {
        Scm::StartPending => ServiceState::StartPending,
        Scm::StopPending => ServiceState::StopPending,
        Scm::Running | Scm::ContinuePending | Scm::PausePending | Scm::Paused => {
            ServiceState::Running
        }
        Scm::Stopped => match status.exit_code {
            // 0 = clean; 1077 = never started since boot (ERROR_SERVICE_NEVER_HAS_BEEN_RUN? no —
            // "no attempts to start have been made"): both are an ordinary Stopped, not a failure.
            ServiceExitCode::Win32(0) | ServiceExitCode::Win32(1077) => ServiceState::Stopped,
            ServiceExitCode::Win32(code) => ServiceState::Failed(format!("exit code {code}")),
            ServiceExitCode::ServiceSpecific(code) => {
                ServiceState::Failed(format!("service error {code}"))
            }
        },
    }
}

/// The systemd user unit the Linux packages install (scripts/punktfunk-host.service).
#[cfg(target_os = "linux")]
pub const UNIT_NAME: &str = "punktfunk-host.service";

#[cfg(target_os = "linux")]
pub fn probe_service() -> ServiceState {
    // `systemctl show` exits 0 even for unknown units (LoadState=not-found) — parse, don't rely
    // on the exit code.
    let Ok(out) = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            UNIT_NAME,
            "--property=LoadState,ActiveState,SubState",
        ])
        .output()
    else {
        return ServiceState::NotInstalled; // no systemctl → nothing to watch
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let prop = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or("")
            .to_string()
    };
    if prop("LoadState") == "not-found" {
        return ServiceState::NotInstalled;
    }
    match prop("ActiveState").as_str() {
        "active" | "reloading" => ServiceState::Running,
        "activating" => ServiceState::StartPending,
        "deactivating" => ServiceState::StopPending,
        "failed" => ServiceState::Failed(prop("SubState")),
        _ => ServiceState::Stopped, // "inactive" and anything new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(streaming: bool) -> Summary {
        Summary {
            version: "0.5.1".into(),
            video_streaming: streaming,
            audio_streaming: streaming,
            session: streaming.then_some(SessionInfo {
                width: 2560,
                height: 1440,
                fps: 120,
            }),
            paired_clients: 1,
            native_paired_clients: 2,
            pin_pending: false,
            pending_approvals: 0,
            kept_displays: 0,
            conflicts: Vec::new(),
        }
    }

    /// The full (service state × summary × grace) table.
    #[test]
    fn status_mapping_table() {
        use ServiceState as S;
        use TrayStatus as T;
        let cases: Vec<(S, Option<Summary>, bool, T)> = vec![
            (S::NotInstalled, None, false, T::NotInstalled),
            (S::Stopped, None, false, T::Stopped),
            (S::StopPending, None, false, T::Stopped),
            (S::StartPending, None, false, T::Starting),
            (
                S::Failed("code 3".into()),
                None,
                false,
                T::Error("code 3".into()),
            ),
            // Running + summary → Running regardless of grace.
            (
                S::Running,
                Some(summary(false)),
                true,
                T::Running(summary(false)),
            ),
            // Running + unreachable: Starting within grace, Degraded past it.
            (S::Running, None, false, T::Starting),
            (S::Running, None, true, T::Degraded),
            // A summary while the SCM says Stopped is impossible by construction (the poller only
            // fetches when Running) — but the mapping must still trust the service manager.
            (S::Stopped, Some(summary(true)), false, T::Stopped),
        ];
        for (svc, sum, grace, want) in cases {
            assert_eq!(
                map_status(&svc, sum.clone(), grace),
                want,
                "{svc:?} {sum:?} grace={grace}"
            );
        }
    }

    #[test]
    fn conflicts_drive_attention_and_lead_the_tooltip() {
        let mut s = summary(false);
        assert!(!TrayStatus::Running(s.clone()).has_conflicts());
        s.conflicts = vec!["Sunshine (running)".into(), "Apollo".into()];
        let st = TrayStatus::Running(s);
        assert!(st.has_conflicts());
        let head = st.headline();
        assert!(head.starts_with("⚠ conflicting host: Sunshine (running), Apollo"));
    }

    #[test]
    fn headline_shows_session_and_reason() {
        assert_eq!(
            TrayStatus::Running(summary(true)).headline(),
            "punktfunk host 0.5.1 — streaming 2560×1440@120"
        );
        assert_eq!(
            TrayStatus::Running(summary(false)).headline(),
            "punktfunk host 0.5.1 — idle"
        );
        assert!(TrayStatus::Error("exit code 3".into())
            .headline()
            .contains("exit code 3"));
        assert!(TrayStatus::Degraded
            .headline()
            .contains("status unavailable"));
    }

    #[test]
    fn pairing_attention_flags() {
        let mut s = summary(false);
        assert!(!TrayStatus::Running(s.clone()).pairing_attention());
        s.pending_approvals = 1;
        assert!(TrayStatus::Running(s.clone()).pairing_attention());
        s.pending_approvals = 0;
        s.pin_pending = true;
        assert!(TrayStatus::Running(s).pairing_attention());
        assert!(!TrayStatus::Degraded.pairing_attention());
    }
}
