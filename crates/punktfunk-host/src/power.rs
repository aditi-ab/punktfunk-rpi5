//! Machine power executors for the `power.*` host actions (`design/host-actions.md` §6):
//! sleep / reboot / shutdown, plus the per-verb availability probe the discovery route reports.
//!
//! Linux drives logind over zbus — the SAME privileged path the already-shipped polkit rule
//! (`packaging/linux/49-punktfunk-power.rules`) authorizes for members of group `punktfunk`,
//! and deliberately WITHOUT `-ignore-inhibit`/`-multiple-sessions`: a foreign block inhibitor
//! or a second local user makes logind refuse, and that refusal is surfaced honestly as a
//! `409 blocked` instead of being steamrolled. Windows uses the interactive user token's own
//! `SeShutdownPrivilege` (`InitiateSystemShutdownExW` / `SetSuspendState`). macOS has no
//! executor yet (the mgmt route answers `501`).
//!
//! Both [`probe`] and [`act`] BLOCK (a D-Bus round trip / a Win32 call) — call them via
//! `spawn_blocking` from async contexts. The zbus threading dance mirrors
//! [`crate::sleep_inhibit::acquire`]: zbus's blocking API cannot run on a tokio worker.

/// The three built-in machine-power verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerVerb {
    Sleep,
    Reboot,
    Shutdown,
}

/// One verb's platform answer: can this host do it right now, and if not, why (the honest
/// `unavailable_reason` the discovery route reports — the `SessionSettingsState::enforced`
/// pattern: say "unavailable, because X" instead of offering a dead switch).
pub struct Availability {
    pub available: bool,
    pub reason: Option<String>,
}

impl Availability {
    fn yes() -> Availability {
        Availability {
            available: true,
            reason: None,
        }
    }

    fn no(reason: impl Into<String>) -> Availability {
        Availability {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

/// Whether this platform has power executors at all — `false` answers the invoke route with
/// `501 unsupported` (macOS host, until that leg exists).
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

// ---------------------------------------------------------------------------- Linux (logind)

/// One logind `Manager` call on a dedicated plain thread (see the module header for why), the
/// reply deserialized as `T`. Errors come back as the D-Bus error text — which IS the honest
/// reason ("Interactive authentication required", "Operation inhibited by …").
#[cfg(target_os = "linux")]
fn logind_call<T, A>(method: &'static str, args: A) -> Result<T, String>
where
    T: for<'de> serde::Deserialize<'de> + ashpd::zbus::zvariant::Type + Send + 'static,
    A: serde::Serialize + ashpd::zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    std::thread::spawn(move || -> Result<T, String> {
        use ashpd::zbus;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let conn = zbus::Connection::system()
                .await
                .map_err(|e| e.to_string())?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    method,
                    &args,
                )
                .await
                .map_err(|e| e.to_string())?;
            reply.body().deserialize().map_err(|e| e.to_string())
        })
    })
    .join()
    .map_err(|_| "logind call thread panicked".to_string())?
}

/// Ask logind whether the verb can run: `CanSuspend`/`CanReboot`/`CanPowerOff` answer `"yes"`,
/// `"no"`, `"na"` (hardware can't) or `"challenge"` (polkit would need interactive auth —
/// typically the host user is not in group `punktfunk`, or a second local user is logged in).
/// Only `"yes"` is available; everything else carries its reason.
#[cfg(target_os = "linux")]
pub fn probe(verb: PowerVerb) -> Availability {
    let method = match verb {
        PowerVerb::Sleep => "CanSuspend",
        PowerVerb::Reboot => "CanReboot",
        PowerVerb::Shutdown => "CanPowerOff",
    };
    match logind_call::<String, ()>(method, ()) {
        Ok(ans) if ans == "yes" => Availability::yes(),
        Ok(ans) if ans == "challenge" => Availability::no(
            "the host would need interactive authorization — is the host user in group \
             'punktfunk' (and no second local user logged in)?",
        ),
        Ok(ans) if ans == "na" => Availability::no("this machine does not support it"),
        Ok(ans) => Availability::no(format!("logind answered {ans:?}")),
        Err(e) => Availability::no(format!("no logind: {e}")),
    }
}

/// Run the verb: `Suspend`/`Reboot`/`PowerOff` with `interactive = false` — a polkit challenge
/// fails instead of prompting (there is nobody at a dialog on a streaming host). The caller has
/// already ended every session and released our own sleep inhibitor
/// ([`crate::sleep_inhibit::release_now`]) — a still-standing foreign inhibitor makes logind
/// refuse, and the error text says whose it is.
#[cfg(target_os = "linux")]
pub fn act(verb: PowerVerb) -> Result<(), String> {
    let method = match verb {
        PowerVerb::Sleep => "Suspend",
        PowerVerb::Reboot => "Reboot",
        PowerVerb::Shutdown => "PowerOff",
    };
    logind_call::<(), (bool,)>(method, (false,))
}

// ------------------------------------------------------------------------------------ Windows

/// Windows: reboot/shutdown are available whenever the interactive user token holds
/// `SeShutdownPrivilege` (it does by default — [`act`] enables and uses it); sleep asks the
/// power manager whether suspend is supported at all.
#[cfg(target_os = "windows")]
pub fn probe(verb: PowerVerb) -> Availability {
    match verb {
        PowerVerb::Sleep => {
            // SAFETY: no arguments, no aliasing — a pure capability query.
            if unsafe { windows::Win32::System::Power::IsPwrSuspendAllowed() } {
                Availability::yes()
            } else {
                Availability::no("this machine does not support sleep")
            }
        }
        PowerVerb::Reboot | PowerVerb::Shutdown => Availability::yes(),
    }
}

/// Enable this process's `SeShutdownPrivilege` (present-but-disabled by default on an
/// interactive user token), then run the verb. The reason string lands in the system event log.
#[cfg(target_os = "windows")]
pub fn act(verb: PowerVerb) -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
        SHTDN_REASON_MINOR_OTHER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard privilege-enable sequence on our own process token; the token handle is
    // closed on every path.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
        let mut luid = LUID::default();
        let looked_up = LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid);
        let adjusted = looked_up.and_then(|()| {
            let privs = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            AdjustTokenPrivileges(token, false, Some(&raw const privs), 0, None, None)
        });
        let _ = CloseHandle(token);
        adjusted.map_err(|e| format!("enabling SeShutdownPrivilege: {e}"))?;
    }

    match verb {
        PowerVerb::Sleep => {
            // SAFETY: plain suspend request — no hibernate, honor other apps' wake locks.
            if unsafe { SetSuspendState(false, false, false) } {
                Ok(())
            } else {
                Err(format!(
                    "SetSuspendState failed: {}",
                    windows::core::Error::from_thread()
                ))
            }
        }
        PowerVerb::Reboot | PowerVerb::Shutdown => {
            let reason = windows::core::HSTRING::from(
                "Requested from a Punktfunk client (host power action)",
            );
            // SAFETY: local machine (None), owned wide strings live across the call.
            unsafe {
                InitiateSystemShutdownExW(
                    None,
                    &reason,
                    0,    // no countdown dialog — sessions were already ended cleanly
                    true, // force apps closed; nobody is at the console to answer prompts
                    verb == PowerVerb::Reboot,
                    SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
                )
            }
            .map_err(|e| format!("InitiateSystemShutdownExW: {e}"))
        }
    }
}

// -------------------------------------------------------------------------- other platforms

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn probe(_verb: PowerVerb) -> Availability {
    Availability::no("not supported on this host platform")
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn act(_verb: PowerVerb) -> Result<(), String> {
    Err("not supported on this host platform".into())
}

// ------------------------------------------------------------------- the session close signal

/// The host-wide "a power action is ending every session" signal. Each paired session's
/// access-lifecycle task subscribes ([`closing_rx`]) and closes its connection with the typed
/// `RejectReason::HostPower` code when it fires — so the client renders "the host is going to
/// sleep" instead of a bare transport error. One-way: nothing un-fires it (sleep resumes with
/// no live sessions either way), but the flag resets after a completed sleep so the woken host
/// types future closes correctly.
static POWER_CLOSING: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
    std::sync::OnceLock::new();

fn power_closing() -> &'static tokio::sync::watch::Sender<bool> {
    POWER_CLOSING.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// Subscribe to the power-close signal (one receiver per session lifecycle task).
pub fn closing_rx() -> tokio::sync::watch::Receiver<bool> {
    power_closing().subscribe()
}

/// Fire (or reset) the power-close signal.
pub fn set_closing(closing: bool) {
    let _ = power_closing().send(closing);
}
