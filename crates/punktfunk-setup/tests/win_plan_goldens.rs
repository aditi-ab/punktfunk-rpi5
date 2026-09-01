//! The Windows core suite: every WinFacts preset renders its WinPlan to dry-run text, pinned
//! under `tests/golden/win-*.txt`. Regenerate with `UPDATE_GOLDEN=1 cargo test`. Runs on
//! every OS — that is the whole point of the plans being pure data.
//!
//! Below the goldens, one named test per `design/installer-v2-windows.md` §5 trap. Those
//! assert on the command list and step order, so they keep meaning when wording moves.

use std::path::Path;

use punktfunk_setup::plan::Level;
use punktfunk_setup::platform::windows::args::InnoArgs;
use punktfunk_setup::platform::windows::choices::{NetworkAnswer, WinChoices};
use punktfunk_setup::platform::windows::plan::{self, Artifact, WinAction, WinPlan};
use punktfunk_setup::platform::windows::{
    exec, report, NetCategory, NetProfile, TaskState, WinFacts, WinInstall,
};
use punktfunk_setup::seam::Env;
use punktfunk_setup::ui::Plain;

// ------------------------------------------------------------------------------- presets

fn fresh() -> WinFacts {
    WinFacts {
        os_build: 26200,
        arch: "x64".into(),
        installed: None,
        host_env_present: false,
        web_password_present: false,
        mgmt_bind_set: false,
        competing_hosts: vec![],
        mgmt_port_in_use: false,
        networks: vec![NetProfile {
            name: "Home".into(),
            category: NetCategory::Private,
        }],
        steam_audio_drivers: true,
        tray_autostart: false,
        vulkan_layer_registered: false,
        web_task: TaskState::Absent,
        scripting_task: TaskState::Absent,
    }
}

fn upgrade() -> WinFacts {
    WinFacts {
        installed: Some(WinInstall {
            version: Some("0.34.0".into()),
            location: Some(r"C:\Program Files\punktfunk\".into()),
        }),
        host_env_present: true,
        web_password_present: true,
        tray_autostart: true,
        vulkan_layer_registered: true,
        web_task: TaskState::Disabled,
        scripting_task: TaskState::Enabled,
        ..fresh()
    }
}

fn sunshine() -> WinFacts {
    WinFacts {
        competing_hosts: vec!["SunshineService".into()],
        mgmt_port_in_use: true,
        ..fresh()
    }
}

fn public_network() -> WinFacts {
    WinFacts {
        networks: vec![NetProfile {
            name: "Netzwerk 2".into(),
            category: NetCategory::Public,
        }],
        ..fresh()
    }
}

// -------------------------------------------------------------------------- the mechanism

fn render(facts: &WinFacts, choices: &WinChoices, artifact: Artifact, uninstall: bool) -> String {
    let (ui, buf) = Plain::capture();
    report::detected(&ui, facts, artifact);
    if !uninstall {
        report::choices_summary(&ui, choices, artifact);
    }
    let plan = plan::build(facts, choices, artifact, uninstall);
    exec::render(&plan, &ui);
    if !uninstall {
        report::outro(&ui, facts, choices, artifact);
    }
    buf.borrow().clone()
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("no golden for {name} — run UPDATE_GOLDEN=1 cargo test -p punktfunk-setup")
    });
    assert_eq!(
        actual, expected,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}

fn host_plan(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    plan::build(facts, choices, Artifact::Host, false)
}

// --------------------------------------------------------------------------------- goldens

#[test]
fn golden_win11_fresh() {
    let facts = fresh();
    let choices = WinChoices::derive(&facts);
    golden(
        "win-fresh",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_upgrade() {
    let facts = upgrade();
    let choices = WinChoices::derive(&facts);
    golden(
        "win-upgrade",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_sunshine() {
    let facts = sunshine();
    let choices = WinChoices::derive(&facts);
    golden(
        "win-sunshine",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_public_all_three_branches() {
    let facts = public_network();
    let mut choices = WinChoices::derive(&facts);
    golden(
        "win-public-skip",
        &render(&facts, &choices, Artifact::Host, false),
    );
    choices.network = NetworkAnswer::MakePrivate("Netzwerk 2".into());
    golden(
        "win-public-private",
        &render(&facts, &choices, Artifact::Host, false),
    );
    choices.network = NetworkAnswer::OpenPublicRules;
    choices.allow_public_fw = Some(true);
    golden(
        "win-public-open",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_uninstall() {
    let facts = upgrade();
    let choices = WinChoices::derive(&facts);
    golden(
        "win-uninstall",
        &render(&facts, &choices, Artifact::Host, true),
    );
}

#[test]
fn golden_client_fresh() {
    let facts = fresh();
    let choices = WinChoices::derive(&facts);
    golden(
        "win-client-fresh",
        &render(&facts, &choices, Artifact::Client, false),
    );
}

// ------------------------------------------------------------------- named traps (§5, D11, D12)

// D11: coexistence is a SetEnv the host was built for, never an abort — and the value is
// exactly the one shipped Linux behaviour writes.
#[test]
fn a_sunshine_box_coexists_by_moving_the_management_port() {
    let facts = sunshine();
    let plan = host_plan(&facts, &WinChoices::derive(&facts));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::SetEnv { key, value }
            if key == "PUNKTFUNK_MGMT_BIND" && value == "0.0.0.0:47991"
    )));
}

// The D11 guard: an operator's own value is never rewritten.
#[test]
fn an_operator_mgmt_bind_is_never_rewritten() {
    let facts = WinFacts {
        mgmt_bind_set: true,
        ..sunshine()
    };
    let plan = host_plan(&facts, &WinChoices::derive(&facts));
    assert!(!plan
        .steps()
        .any(|s| matches!(s, WinAction::SetEnv { key, .. } if key == "PUNKTFUNK_MGMT_BIND")));
}

// Silent installs never touch a network profile: the Skip default renders a warning only.
#[test]
fn silent_with_a_public_network_warns_but_never_touches_the_profile() {
    let facts = public_network();
    let plan = host_plan(&facts, &WinChoices::derive(&facts));
    assert!(!plan
        .steps()
        .any(|s| matches!(s, WinAction::MakeNetworkPrivate { .. })));
    assert!(plan
        .steps()
        .any(|s| matches!(s, WinAction::Note(Level::Warn, text) if text.contains("Public"))));
}

// Fresh-only params, ported as data: a fresh install pins both; an upgrade passes neither.
#[test]
fn service_install_params_are_fresh_only_by_default() {
    let fresh_cmds = host_plan(&fresh(), &WinChoices::derive(&fresh())).commands();
    let install = fresh_cmds
        .iter()
        .find(|c| c.contains("service install"))
        .unwrap();
    assert!(install.contains("--gamestream=off"));
    assert!(install.contains("--allow-public-network=off"));

    let up_cmds = host_plan(&upgrade(), &WinChoices::derive(&upgrade())).commands();
    let install = up_cmds
        .iter()
        .find(|c| c.contains("service install"))
        .unwrap();
    assert!(!install.contains("--gamestream"));
    assert!(!install.contains("--allow-public-network"));
}

// ...but an explicit ask reaches the plan even on an upgrade — the Reconfigure fix for the
// fielded Public-network boxes.
#[test]
fn an_explicit_public_fw_task_reaches_an_upgrade_plan() {
    let facts = upgrade();
    let mut choices = WinChoices::derive(&facts);
    let args = InnoArgs::parse(&[r#"/MERGETASKS="allowpublicfw""#.to_string()]);
    choices.apply(&args, &Env::default());
    let cmds = host_plan(&facts, &choices).commands();
    let install = cmds.iter().find(|c| c.contains("service install")).unwrap();
    assert!(install.contains("--allow-public-network=on"));
}

// §5: the uninstall order is load-bearing — service first (it supervises the tray), then the
// tray, then all three driver legs unconditionally.
#[test]
fn uninstall_order_is_service_tray_then_all_three_driver_legs() {
    let facts = upgrade();
    let cmds = plan::build(&facts, &WinChoices::derive(&facts), Artifact::Host, true).commands();
    let pos = |needle: &str| {
        cmds.iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("missing: {needle}"))
    };
    assert!(pos("service uninstall") < pos("--quit"));
    assert!(pos("--quit") < pos("taskkill"));
    assert!(pos("taskkill") < pos("driver uninstall"));
    assert!(pos("driver uninstall --gamepad") < pos("driver uninstall --audio"));
}

// §5: stop/restore honesty — the restore step carries the pre-install task states, so an
// upgrade puts back exactly what was there (here: web stays disabled, scripting enabled).
#[test]
fn restore_carries_the_pre_install_task_states() {
    let facts = upgrade();
    let plan = host_plan(&facts, &WinChoices::derive(&facts));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::RestoreTasks {
            web_enabled: Some(false),
            scripting_enabled: Some(true),
        }
    )));
    // And a fresh install has nothing to stop or restore.
    let fresh_plan = host_plan(&fresh(), &WinChoices::derive(&fresh()));
    assert!(!fresh_plan.steps().any(|s| matches!(
        s,
        WinAction::StopHostRuntime | WinAction::RestoreTasks { .. }
    )));
}

// §5: turning a row off on an upgrade removes what the box has — better than Inno's
// unchecked-box-changes-nothing, and the registry line proves it.
#[test]
fn deselecting_tray_on_an_upgrade_deletes_the_run_key() {
    let facts = upgrade();
    let mut choices = WinChoices::derive(&facts);
    choices.tray_autostart = false;
    let cmds = host_plan(&facts, &choices).commands();
    assert!(cmds
        .iter()
        .any(|c| c.starts_with("reg delete") && c.contains("PunktfunkTray")));
}

// The web password travels via a temp file on fresh installs only.
#[test]
fn web_password_file_is_fresh_only() {
    let plan = host_plan(&fresh(), &WinChoices::derive(&fresh()));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::WebSetup {
            fresh_password: true,
            ..
        }
    )));
    let plan = host_plan(&upgrade(), &WinChoices::derive(&upgrade()));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::WebSetup {
            fresh_password: false,
            ..
        }
    )));
}
