//! Stage three, Windows: `(WinFacts, WinChoices) → WinPlan`. Pure — no I/O, no spawns.
//!
//! A sibling of `plan.rs` rather than an extension of it: the Linux `StepAction` and its
//! executor thread Linux `Facts`/`Choices` everywhere and are golden-locked, so Windows gets
//! its own data types over the same `Reporter` vocabulary and the same golden harness
//! (`design/installer-v2-windows.md` D4's sibling-seam rule).
//!
//! Every path in a step is a literal string, never a `PathBuf::join` — the plans must render
//! byte-identically on every OS that runs the goldens. The phase order is the `.iss` order,
//! which is load-bearing (§5): stop → files → registry → network → coexistence → drivers →
//! service → web → plugin runner → restore → tray. `<staging>` and `<temp>` are placeholders
//! the executor substitutes at run time; dry-run renders them verbatim.

use serde::{Deserialize, Serialize};

use super::choices::{NetworkAnswer, WinChoices};
use super::{TaskState, WinFacts, MGMT_PORT_MOVED};
use crate::facts::DOCS;
use crate::plan::Level;

/// Where the host lands without `/DIR`. Load-bearing: `pf-update-check` classifies an install
/// by this path, and a different dir silently downgrades the box to notify-only updates.
pub const DEFAULT_HOST_DIR: &str = r"C:\Program Files\punktfunk";

/// Rendered, not resolved: the client is per-user and the real profile path is the
/// executor's business.
pub const DEFAULT_CLIENT_DIR: &str = r"%LocalAppData%\Programs\Punktfunk";

/// The client installer's ARP key — Inno's `_is1` kept, same reasoning as the host's.
pub const CLIENT_ARP_KEY: &str = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{52464E61-68A1-4621-B6B3-5B8BBB823D1A}_is1";

/// Which payload this exe carries; the embedded manifest decides (design D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Artifact {
    Host,
    Client,
}

/// One Windows step. `Run` is an argv, never a shell string — there is no `sh` here, and
/// re-deriving argv from a display string is a quoting bug farm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WinAction {
    Run(Vec<String>),
    Note(Level, String),
    /// One `KEY=VALUE` line in `%ProgramData%\punktfunk\host.env`.
    SetEnv {
        key: String,
        value: String,
    },
    /// Unpack the embedded payload into `dest`.
    DeployFiles {
        dest: String,
    },
    /// Delete the install dir. The uninstaller lives inside it — the executor owns the
    /// classic self-delete dance; the plan only states the intent.
    RemoveFiles {
        dir: String,
    },
    /// The surgical PATH edit: containment-checked append / entry-by-entry rebuild,
    /// `REG_EXPAND_SZ`, never a substring delete. HKLM when `machine`, else HKCU.
    PathAdd {
        machine: bool,
        dir: String,
    },
    PathRemove {
        machine: bool,
        dir: String,
    },
    /// The Add/Remove Programs entry, key name frozen (winget ProductCode).
    ArpRegister {
        key: String,
        display_name: String,
        version: String,
        location: String,
    },
    ArpRemove {
        key: String,
    },
    /// A `.lnk` via `IShellLink` — no compiled tool writes one.
    Shortcut {
        link: String,
        target: String,
    },
    /// D12's recommended fix, per network, consent given in the wizard. Never in silent runs.
    MakeNetworkPrivate {
        network: String,
    },
    /// Stop the service (SCM, waited), every tray, and the bun tasks — capturing nothing:
    /// the restore data was captured into Facts before the plan existed.
    StopHostRuntime,
    /// Re-enable only what was enabled before the stop. `None` = the task did not exist.
    RestoreTasks {
        web_enabled: Option<bool>,
        scripting_enabled: Option<bool>,
    },
    /// `punktfunk-host web setup`; the password travels via an ACL'd temp file, never argv.
    WebSetup {
        app_dir: String,
        fresh_password: bool,
    },
    /// Task Scheduler registration needs XML (restart backoff is inexpressible in flags);
    /// the executor generates it. `start_now` only on a fresh registration.
    RegisterScriptingTask {
        start_now: bool,
    },
    /// Non-elevated, current user, skipped in silent installs (the host supervises one).
    LaunchTray {
        exe: String,
    },
    /// Download + run `windowsappruntimeinstall --quiet` when the runtime is missing.
    /// Best-effort, exactly as shipped: a failure warns and points at the docs.
    EnsureAppRuntime {
        arch: String,
    },
    /// Reap listeners a stop may have missed (the legacy console migration).
    KillPortListeners {
        ports: Vec<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinPhase {
    pub title: String,
    pub steps: Vec<WinAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinPlan {
    pub phases: Vec<WinPhase>,
}

impl WinPlan {
    pub fn steps(&self) -> impl Iterator<Item = &WinAction> {
        self.phases.iter().flat_map(|p| p.steps.iter())
    }

    /// Every argv this plan would spawn, joined for display — what the trap tests assert on.
    pub fn commands(&self) -> Vec<String> {
        self.steps()
            .filter_map(|s| match s {
                WinAction::Run(argv) => Some(join_argv(argv)),
                _ => None,
            })
            .collect()
    }

    fn push(&mut self, title: impl Into<String>, steps: Vec<WinAction>) {
        let steps: Vec<WinAction> = steps;
        if steps.is_empty() {
            return;
        }
        self.phases.push(WinPhase {
            title: title.into(),
            steps,
        });
    }
}

/// Args with spaces are quoted for display; execution uses the vector, not this string.
pub fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run(argv: &[&str]) -> WinAction {
    WinAction::Run(argv.iter().map(|s| (*s).to_string()).collect())
}

fn note(level: Level, text: impl Into<String>) -> WinAction {
    WinAction::Note(level, text.into())
}

/// The whole Windows engine: probe results in, ordered work out.
pub fn build(
    facts: &WinFacts,
    choices: &WinChoices,
    artifact: Artifact,
    uninstall: bool,
) -> WinPlan {
    match (artifact, uninstall) {
        (Artifact::Host, false) => host_install(facts, choices),
        (Artifact::Host, true) => host_uninstall(facts, choices),
        (Artifact::Client, false) => client_install(facts, choices),
        (Artifact::Client, true) => client_uninstall(choices),
    }
}

fn app_dir(choices: &WinChoices, artifact: Artifact) -> String {
    choices
        .dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| {
            match artifact {
                Artifact::Host => DEFAULT_HOST_DIR,
                Artifact::Client => DEFAULT_CLIENT_DIR,
            }
            .to_string()
        })
        .trim_end_matches('\\')
        .to_string()
}

fn host_install(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let mut plan = WinPlan::default();
    let app = app_dir(choices, Artifact::Host);
    let host_exe = format!("{app}\\punktfunk-host.exe");
    let upgrade = facts.installed.is_some();

    if upgrade {
        plan.push(
            "Stopping the running host for the upgrade",
            vec![WinAction::StopHostRuntime],
        );
    }

    plan.push(
        format!("Files → {app}"),
        vec![WinAction::DeployFiles { dest: app.clone() }],
    );

    plan.push("Registry", registry_steps(facts, choices, &app));
    plan.push("Network", network_steps(facts, choices));
    plan.push(
        "Checking for Sunshine / Apollo / Vibeshine",
        coexist_steps(facts, choices),
    );

    let mut drivers = vec![];
    if choices.install_driver {
        drivers.push(run(&[
            &host_exe,
            "driver",
            "install",
            "--dir",
            r"<staging>\pfvdisplay",
        ]));
    }
    if choices.install_gamepad {
        drivers.push(run(&[
            &host_exe,
            "driver",
            "install",
            "--gamepad",
            "--dir",
            r"<staging>\gamepad",
        ]));
    }
    plan.push("Drivers (a hiccup warns and never aborts)", drivers);

    // Fresh installs pass the two persisted options explicitly; None passes nothing and the
    // box keeps its state — the `.iss`'s fresh-only params, ported as data.
    let mut service = vec![host_exe.clone(), "service".into(), "install".into()];
    if let Some(on) = choices.gamestream {
        service.push(format!("--gamestream={}", if on { "on" } else { "off" }));
    }
    if let Some(on) = choices.allow_public_fw {
        service.push(format!(
            "--allow-public-network={}",
            if on { "on" } else { "off" }
        ));
    }
    let mut service_steps = vec![WinAction::Run(service)];
    if choices.start_service {
        service_steps.push(run(&[&host_exe, "service", "start"]));
    }
    plan.push("Service", service_steps);

    plan.push(
        "Web console",
        vec![WinAction::WebSetup {
            app_dir: app.clone(),
            fresh_password: !facts.web_password_present,
        }],
    );

    plan.push(
        "Plugin runner",
        vec![WinAction::RegisterScriptingTask {
            start_now: facts.scripting_task == TaskState::Absent,
        }],
    );

    if upgrade {
        plan.push(
            "Restoring what the stop disabled",
            vec![WinAction::RestoreTasks {
                web_enabled: enabled(facts.web_task),
                scripting_enabled: enabled(facts.scripting_task),
            }],
        );
    }

    plan.push(
        "Register the uninstaller",
        vec![WinAction::ArpRegister {
            key: super::HOST_ARP_KEY.into(),
            display_name: "Punktfunk Host".into(),
            version: "<version>".into(),
            location: app.clone(),
        }],
    );

    if choices.tray_autostart {
        plan.push(
            "Tray",
            vec![WinAction::LaunchTray {
                exe: format!("{app}\\punktfunk-tray.exe"),
            }],
        );
    }
    plan
}

fn enabled(state: TaskState) -> Option<bool> {
    match state {
        TaskState::Absent => None,
        TaskState::Enabled => Some(true),
        TaskState::Disabled => Some(false),
    }
}

fn registry_steps(facts: &WinFacts, choices: &WinChoices, app: &str) -> Vec<WinAction> {
    let mut steps = vec![];
    let run_key = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    if choices.tray_autostart {
        steps.push(run(&[
            "reg",
            "add",
            run_key,
            "/v",
            "PunktfunkTray",
            "/t",
            "REG_SZ",
            "/d",
            &format!("{app}\\punktfunk-tray.exe"),
            "/f",
        ]));
    } else if facts.tray_autostart {
        // Better than Inno's unchecked-box-changes-nothing: turning the row off removes it.
        steps.push(run(&[
            "reg",
            "delete",
            run_key,
            "/v",
            "PunktfunkTray",
            "/f",
        ]));
    }
    // The tray's toast identity — must stay in lockstep with punktfunk-tray/src/win.rs.
    let aumid = r"HKLM\SOFTWARE\Classes\AppUserModelId\unom.punktfunk.tray";
    steps.push(run(&[
        "reg",
        "add",
        aumid,
        "/v",
        "DisplayName",
        "/t",
        "REG_SZ",
        "/d",
        "Punktfunk",
        "/f",
    ]));
    steps.push(run(&[
        "reg",
        "add",
        aumid,
        "/v",
        "IconUri",
        "/t",
        "REG_SZ",
        "/d",
        &format!("{app}\\punktfunk.ico"),
        "/f",
    ]));
    let layers = r"HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers";
    let layer_json = format!("{app}\\vklayer\\pf_vkhdr_layer.json");
    if choices.install_hdr_layer {
        steps.push(run(&[
            "reg",
            "add",
            layers,
            "/v",
            &layer_json,
            "/t",
            "REG_DWORD",
            "/d",
            "0",
            "/f",
        ]));
    } else if facts.vulkan_layer_registered {
        steps.push(run(&["reg", "delete", layers, "/v", &layer_json, "/f"]));
    }
    steps.push(WinAction::PathAdd {
        machine: true,
        dir: app.to_string(),
    });
    steps
}

/// D12. Silent installs never touch a profile: the Skip arm's warning is all they render.
fn network_steps(facts: &WinFacts, choices: &WinChoices) -> Vec<WinAction> {
    match &choices.network {
        NetworkAnswer::MakePrivate(name) => vec![
            WinAction::MakeNetworkPrivate {
                network: name.clone(),
            },
            note(
                Level::Ok,
                format!("network '{name}' becomes Private — the standard firewall rules apply there"),
            ),
        ],
        NetworkAnswer::OpenPublicRules => vec![note(
            Level::Ok,
            "keeping the network Public and opening the firewall for it (--allow-public-network=on)",
        )],
        NetworkAnswer::Skip => {
            let public: Vec<String> = facts
                .public_networks()
                .iter()
                .map(|n| n.name.clone())
                .collect();
            if public.is_empty() || choices.allow_public_fw == Some(true) {
                return vec![];
            }
            vec![note(
                Level::Warn,
                format!(
                    "network '{}' is set to Public, and the firewall rules below don't apply there — the host will be unreachable until the network is Private or the public rules are added ({DOCS}/troubleshooting#windows-firewall)",
                    public.join("', '")
                ),
            )]
        }
    }
}

/// D11: mirror of the Linux conflict steps, same wording where the situation is the same.
fn coexist_steps(facts: &WinFacts, choices: &WinChoices) -> Vec<WinAction> {
    if !facts.needs_coexistence() {
        if facts.mgmt_bind_set {
            return vec![note(
                Level::Ok,
                "management port already moved in host.env — leaving the operator's value alone",
            )];
        }
        return vec![note(
            Level::Ok,
            "No conflicting game-streaming host detected.",
        )];
    }
    let who = facts
        .competing_hosts
        .first()
        .map(String::as_str)
        .unwrap_or("another streaming host");
    vec![
        note(
            Level::Warn,
            format!("{who} is active on this box — both want TCP 47990 (its web UI, punktfunk's management API)"),
        ),
        WinAction::SetEnv {
            key: "PUNKTFUNK_MGMT_BIND".into(),
            value: format!("0.0.0.0:{MGMT_PORT_MOVED}"),
        },
        note(
            Level::Ok,
            format!("Clients learn the port from discovery; the console and plugins read it from mgmt-endpoint. Details: {DOCS}/switching-from-sunshine"),
        ),
        if choices.gamestream == Some(true) {
            note(
                Level::Warn,
                "with another GameStream host running, only one can bind the Moonlight ports — stop the other first or skip this",
            )
        } else {
            note(
                Level::Ok,
                "Moonlight compatibility stays off — the other host owns those ports while it runs",
            )
        },
    ]
}

/// The `[UninstallRun]` order, verbatim — each position is load-bearing (§5).
fn host_uninstall(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let app = facts
        .installed
        .as_ref()
        .and_then(|i| i.location.clone())
        .unwrap_or_else(|| app_dir(choices, Artifact::Host))
        .trim_end_matches('\\')
        .to_string();
    let host_exe = format!("{app}\\punktfunk-host.exe");
    let mut plan = WinPlan::default();
    plan.push(
        format!("Uninstalling the host ({DOCS}/uninstall)"),
        vec![
            // Service first: the host supervises the tray and would respawn it.
            run(&[&host_exe, "service", "uninstall"]),
            run(&[&format!("{app}\\punktfunk-tray.exe"), "--quit"]),
            run(&["taskkill", "/F", "/IM", "punktfunk-tray.exe"]),
            // All three legs unconditionally: an upgrade may have dropped a payload an
            // earlier install laid down. `driver uninstall` also purges the trusted certs.
            run(&[&host_exe, "driver", "uninstall"]),
            run(&[&host_exe, "driver", "uninstall", "--gamepad"]),
            run(&[&host_exe, "driver", "uninstall", "--audio"]),
            run(&["schtasks", "/End", "/TN", "PunktfunkWeb"]),
            run(&["schtasks", "/Delete", "/TN", "PunktfunkWeb", "/F"]),
            run(&["schtasks", "/End", "/TN", "PunktfunkScripting"]),
            run(&["schtasks", "/Delete", "/TN", "PunktfunkScripting", "/F"]),
            WinAction::KillPortListeners {
                ports: vec![47992, 3000],
            },
            run(&[
                "netsh", "advfirewall", "firewall", "delete", "rule",
                "name=Punktfunk web console (TCP 47992)",
            ]),
            run(&[
                "netsh", "advfirewall", "firewall", "delete", "rule",
                "name=Punktfunk plugin UIs (TCP 47993)",
            ]),
            WinAction::PathRemove {
                machine: true,
                dir: app.clone(),
            },
            WinAction::ArpRemove {
                key: super::HOST_ARP_KEY.into(),
            },
            WinAction::RemoveFiles { dir: app.clone() },
            note(
                Level::Ok,
                r"kept on purpose: %ProgramData%\punktfunk (identity, passwords, update cache) and any VB-CABLE install",
            ),
        ],
    );
    plan
}

fn client_install(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let mut plan = WinPlan::default();
    let app = app_dir(choices, Artifact::Client);
    let client_exe = format!("{app}\\punktfunk-client.exe");

    plan.push(
        "Stopping running punktfunk apps",
        [
            "punktfunk-client.exe",
            "punktfunk-session.exe",
            "punktfunk-console.exe",
            "punktfunk.exe",
        ]
        .iter()
        .map(|exe| run(&["taskkill", "/F", "/IM", exe]))
        .collect(),
    );
    plan.push(
        format!("Files → {app}"),
        vec![WinAction::DeployFiles { dest: app.clone() }],
    );

    let proto = r"HKCU\Software\Classes\punktfunk";
    let mut registry = vec![
        run(&[
            "reg",
            "add",
            proto,
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            "URL:Punktfunk stream link",
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            proto,
            "/v",
            "URL Protocol",
            "/t",
            "REG_SZ",
            "/d",
            "",
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            &format!(r"{proto}\DefaultIcon"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &format!("{client_exe},0"),
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            &format!(r"{proto}\shell\open\command"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &format!("\"{client_exe}\" \"%1\""),
            "/f",
        ]),
        WinAction::PathAdd {
            machine: false,
            dir: app.clone(),
        },
    ];
    plan.push("Registry", std::mem::take(&mut registry));

    let mut shortcuts = vec![
        WinAction::Shortcut {
            link: r"<start menu>\Punktfunk.lnk".into(),
            target: client_exe.clone(),
        },
        WinAction::Shortcut {
            link: r"<start menu>\Punktfunk Console.lnk".into(),
            target: format!("{app}\\punktfunk-console.exe"),
        },
    ];
    if choices.desktop_icon {
        shortcuts.push(WinAction::Shortcut {
            link: r"<desktop>\Punktfunk.lnk".into(),
            target: client_exe.clone(),
        });
    }
    plan.push("Shortcuts", shortcuts);

    plan.push(
        "Windows App Runtime",
        vec![WinAction::EnsureAppRuntime {
            arch: facts.arch.clone(),
        }],
    );
    plan.push(
        "Register the uninstaller",
        vec![WinAction::ArpRegister {
            key: CLIENT_ARP_KEY.into(),
            display_name: "Punktfunk".into(),
            version: "<version>".into(),
            location: app,
        }],
    );
    plan
}

fn client_uninstall(choices: &WinChoices) -> WinPlan {
    let app = app_dir(choices, Artifact::Client);
    let mut plan = WinPlan::default();
    plan.push(
        format!("Uninstalling the client ({DOCS}/uninstall)"),
        vec![
            run(&["taskkill", "/F", "/IM", "punktfunk-client.exe"]),
            run(&["taskkill", "/F", "/IM", "punktfunk-session.exe"]),
            run(&["reg", "delete", r"HKCU\Software\Classes\punktfunk", "/f"]),
            WinAction::PathRemove {
                machine: false,
                dir: app.clone(),
            },
            WinAction::ArpRemove {
                key: CLIENT_ARP_KEY.into(),
            },
            WinAction::RemoveFiles { dir: app },
        ],
    );
    plan
}
