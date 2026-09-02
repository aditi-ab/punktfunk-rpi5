//! The Windows transcript's bookends: what was detected, what was chosen, what to do next.
//!
//! Sibling of `report.rs`, same `Reporter` vocabulary, Windows words. The outro carries the
//! D11 and D12 footnotes — the moved console port and the unreachable-host warning are the
//! two facts a user must leave the installer knowing.

use super::choices::{NetworkAnswer, WinChoices};
use super::plan::Artifact;
use super::{WinFacts, MGMT_PORT_MOVED};
use crate::facts::DOCS;
use crate::ui::Reporter;

pub fn detected(ui: &dyn Reporter, facts: &WinFacts, artifact: Artifact) {
    let what = match artifact {
        Artifact::Host => "host",
        Artifact::Client => "client",
    };
    ui.say(&format!(
        "Detected Windows (build {}), {} — {what} installer",
        facts.os_build, facts.arch
    ));
    if let Some(installed) = facts.installed_for(artifact) {
        ui.ok(&format!(
            "existing install: {} at {}",
            installed.version.as_deref().unwrap_or("unknown version"),
            installed.location.as_deref().unwrap_or("the default dir"),
        ));
    }
}

pub fn choices_summary(ui: &dyn Reporter, choices: &WinChoices, artifact: Artifact) {
    ui.say("Choices (nothing below has run yet)");
    let yn = |v: bool| if v { "yes" } else { "no" };
    match artifact {
        Artifact::Host => {
            ui.line(&format!(
                "  Virtual display driver: {}",
                yn(choices.install_driver)
            ));
            ui.line(&format!(
                "  Gamepad drivers: {}",
                yn(choices.install_gamepad)
            ));
            ui.line(&format!(
                "  HDR Vulkan layer: {}",
                yn(choices.install_hdr_layer)
            ));
            let opt = |v: Option<bool>| match v {
                Some(true) => "yes",
                Some(false) => "no",
                None => "keep the box's setting",
            };
            ui.line(&format!("  Moonlight compat: {}", opt(choices.gamestream)));
            ui.line(&format!(
                "  Public-network firewall rules: {}",
                opt(choices.allow_public_fw)
            ));
            ui.line(&format!(
                "  Start the service: {}",
                yn(choices.start_service)
            ));
            ui.line(&format!("  Tray autostart: {}", yn(choices.tray_autostart)));
        }
        Artifact::Client => {
            ui.line(&format!("  Desktop shortcut: {}", yn(choices.desktop_icon)));
        }
    }
}

pub fn outro(ui: &dyn Reporter, facts: &WinFacts, choices: &WinChoices, artifact: Artifact) {
    ui.blank();
    ui.line("  Done. Next:");
    match artifact {
        Artifact::Host => {
            ui.line("  1. Open the web console:  https://<this-PC>:47992  (the certificate is the host's own — continue past the warning)");
            if !facts.web_password_present {
                ui.line("     password: stored ACL'd in %ProgramData%\\punktfunk\\web-password (the wizard also shows it on its finish page)");
            }
            ui.line(&format!(
                "  2. Install a client on the device you stream to ({DOCS}/install-client), connect, and click Approve in the console."
            ));
            footnotes(ui, facts, choices);
        }
        Artifact::Client => {
            ui.line("  1. Open Punktfunk from the Start menu and pick your host.");
            ui.line(&format!("  2. Pairing help: {DOCS}/pairing"));
        }
    }
    ui.line(&format!("  Stuck? {DOCS}/troubleshooting"));
    ui.blank();
}

/// The D11/D12 footnotes: the moved console port and the unreachable-host warning — the
/// two facts a user must leave the installer knowing. The wizard's Done page renders these
/// alone; the transcript's outro embeds them.
pub fn footnotes(ui: &dyn Reporter, facts: &WinFacts, choices: &WinChoices) {
    if facts.needs_coexistence() {
        ui.line(&format!(
            "  Running next to Sunshine/Apollo: punktfunk's management API is on :{MGMT_PORT_MOVED} — {DOCS}/switching-from-sunshine"
        ));
    }
    if matches!(choices.network, NetworkAnswer::Skip)
        && choices.allow_public_fw != Some(true)
        && !facts.public_networks().is_empty()
    {
        ui.line(&format!(
            "  ⚠ This network is Public and the firewall rules don't apply there — the host is unreachable until that changes ({DOCS}/troubleshooting#windows-firewall)"
        ));
    }
}
