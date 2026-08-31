//! The text around the plan: the banner, the choices summary, and step 7's verify + next steps.
//!
//! Two honesty rules live here and are tested. The console URL is never printed as fact when
//! `punktfunk-web-server` is not on the box — that is what sent a Fedora user looking for a
//! page nothing was serving. And the NVIDIA warnings fire on a *successful* install, because
//! the install did succeed and streaming still will not work until the driver does.
//!
//! Under `--yes` the choices summary is the only place the punktfunk-group grant is stated,
//! so the row names it (`design/installer-v2.md` D4).

use crate::choices::{Action, Choices};
use crate::exec::{Opts, Outcome};
use crate::facts::{Facts, Family, Nvidia, DOCS};
use crate::seam::CommandRunner;
use crate::ui::Reporter;

pub fn banner(ui: &dyn Reporter) {
    ui.blank();
    ui.line("  punktfunk guided host installer — PREVIEW");
    ui.line(&format!(
        "  The per-distro pages stay the documented path; this automates them. Docs: {DOCS}/install"
    ));
    ui.line("  Re-running is safe. Ctrl-C stops between steps.");
    ui.blank();
}

pub fn detected(ui: &dyn Reporter, facts: &Facts) {
    ui.say(&format!(
        "Detected {} → {} (guide: {})",
        facts.os.pretty,
        facts.family.as_str(),
        facts.docs_page
    ));
}

/// Nothing here has run yet — this is the last screen before anything touches the box.
pub fn choices_summary(ui: &dyn Reporter, choices: &Choices) {
    if choices.action == Action::Uninstall {
        return;
    }
    ui.say("Choices (nothing below has run yet)");
    let rows: [(&str, bool, &Option<String>); 4] = [
        (
            "Full controller (joins the punktfunk group — grants usbip attach)",
            choices.punktfunk_group,
            &choices.group_why,
        ),
        (
            "Third-party clients (Moonlight, Artemis)",
            choices.gamestream,
            &choices.gamestream_why,
        ),
        ("Shared clipboard", choices.clipboard, &None),
        (
            "Start at boot with nobody logged in",
            choices.linger,
            &choices.linger_why,
        ),
    ];
    for (label, on, why) in rows {
        let value = if on { "yes" } else { "no" };
        match why {
            Some(why) if on => ui.line(&format!("  {label}: {value}  ({why})")),
            _ => ui.line(&format!("  {label}: {value}")),
        }
    }
}

/// What `--uninstall` deliberately left behind, so a reinstall picks it up.
pub fn uninstall_outro(ui: &dyn Reporter) {
    ui.blank();
    ui.line("  Removed. Left on purpose: ~/.config/punktfunk (identity, pairings, host.env, plugins — a reinstall");
    ui.line("  picks them up), the punktfunk / punktfunk-update groups, and any firewall rules you opened.");
    ui.line(&format!(
        "  The one-command cleanups for each are on {DOCS}/uninstall#linux-hosts"
    ));
}

pub fn verify(
    ui: &dyn Reporter,
    run: &dyn CommandRunner,
    facts: &Facts,
    choices: &Choices,
    outcome: &Outcome,
    opts: Opts,
) {
    ui.say("Checking");
    if outcome.started && !opts.dry {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if run
            .probe(
                "systemctl",
                &["--user", "is-active", "--quiet", "punktfunk-host"],
            )
            .is_some_and(|o| o.ok())
        {
            ui.ok("punktfunk-host is running");
        } else {
            ui.warn(&format!(
                "punktfunk-host is not active — journalctl --user -u punktfunk-host -e ({DOCS}/troubleshooting#the-linux-host-service-wont-start)"
            ));
        }
        let listening = run
            .probe("ss", &["-lun"])
            .is_some_and(|o| o.ok() && o.stdout.contains(":9777 "));
        if listening {
            ui.ok("listening on UDP 9777 (punktfunk/1)");
        } else {
            ui.warn(
                "nothing on UDP 9777 yet — give it a second, then: journalctl --user -u punktfunk-host -e",
            );
        }
    }
    nvidia_warnings(ui, facts);
    next_steps(ui, facts, choices, outcome, opts);
}

/// GPU drivers are the docs pages' job — but these two failures are silent, and the install
/// having succeeded is exactly what makes them worth saying out loud.
fn nvidia_warnings(ui: &dyn Reporter, facts: &Facts) {
    match facts.nvidia {
        Nvidia::Absent => return,
        Nvidia::NoDriver => ui.warn(&format!(
            "NVIDIA GPU without the NVIDIA driver — nothing can encode until it's installed: step 1 of {}",
            facts.docs_page
        )),
        Nvidia::ModuleNotLoaded => ui.warn(&format!(
            "NVIDIA GPU, but nvidia-smi can't talk to the driver — the kernel module didn't load (Secure Boot? run: mokutil --sb-state): {DOCS}/troubleshooting#nvidia-smi-says-it-cant-communicate-with-the-driver"
        )),
        Nvidia::Ok => {}
    }
    // Fedora's own ffmpeg has no NVENC; the RPM only Recommends RPM Fusion's build.
    if facts.family == Family::Dnf && !facts.has_rpmfusion_ffmpeg {
        ui.warn(&format!(
            "NVIDIA GPU, but RPM Fusion's ffmpeg-libs isn't installed — NVENC won't work until it is: step 1 of {}",
            facts.docs_page
        ));
    }
}

fn next_steps(ui: &dyn Reporter, facts: &Facts, choices: &Choices, outcome: &Outcome, opts: Opts) {
    let ip = facts.ip.clone().unwrap_or_else(|| "<host-ip>".to_string());
    ui.blank();
    ui.line("  Done. Next:");
    // --dry-run installs nothing by definition, so it shows the normal text.
    if facts.has_web_server || opts.dry {
        ui.line(&format!(
            "  1. Open the web console:  https://{ip}:47992  (the certificate is the host's own — continue past the warning)"
        ));
        ui.line(
            "     password:  sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password",
        );
    } else {
        ui.line("  1. Install the web console — it is NOT on this box, and pairing, approving a device and");
        ui.line(&format!(
            "     every setting live there. The install line for your distro is on {}",
            facts.docs_page
        ));
    }
    ui.line(&format!(
        "  2. Install a client on the device you stream to ({DOCS}/install-client), connect, and click"
    ));
    ui.line(&format!(
        "     Approve in the console — or Pair a device for a PIN ({DOCS}/pairing)."
    ));
    ui.line("  3. Stream. Ctrl+Alt+Shift+Q hands mouse and keyboard back on desktop clients.");
    if outcome.relogin {
        ui.line("  Group changes apply after you log out and back in (controllers won't work until then).");
    }
    if choices.move_mgmt_port || facts.sunshine_active {
        ui.line(&format!(
            "  Running next to Sunshine/Apollo: {DOCS}/switching-from-sunshine"
        ));
    }
    ui.line(&format!(
        "  Stuck? {DOCS}/troubleshooting · this installer is a preview — the full guide is {}",
        facts.docs_page
    ));
    ui.blank();
}
