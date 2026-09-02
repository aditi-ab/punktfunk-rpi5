//! The text around the plan: banner, choices summary, verify, next steps.
//!
//! Two honesty rules, both tested. Never print the console URL as fact when
//! `punktfunk-web-server` is not on the box. NVIDIA warnings fire on a
//! successful install: the install succeeded, and streaming still will not
//! work until the driver does.
//!
//! Under `--yes` the choices summary is the only place the punktfunk-group
//! grant is stated, so the row names it (`design/installer-v2.md`).

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
    // A client listens on nothing fixed and joins no groups. Host rows would be
    // four questions with no consequence.
    if !choices.components.host {
        ui.line(&format!("  Channel: {}", choices.channel.as_str()));
        return;
    }
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
    next_steps(ui, run, facts, choices, outcome, opts);
}

/// The install succeeded; these two NVIDIA failures are silent and still block encode.
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

fn next_steps(
    ui: &dyn Reporter,
    run: &dyn CommandRunner,
    facts: &Facts,
    choices: &Choices,
    outcome: &Outcome,
    opts: Opts,
) {
    let ip = facts.ip.clone().unwrap_or_else(|| "<host-ip>".to_string());
    ui.blank();
    ui.line("  Done. Next:");
    // This box serves nothing. Console URL and pairing belong on the host, not localhost.
    if !choices.components.host {
        ui.line(
            "  Open punktfunk and pick a host — then approve this device in that host's console.",
        );
        ui.line(&format!("  {DOCS}/install-client · {DOCS}/pairing"));
        ui.blank();
        return;
    }
    // Probe again, not Facts: the install that just ran is usually what put the
    // console on the box. `--dry-run` shows the same text (it installs nothing).
    if run.which("punktfunk-web-server") || opts.dry {
        ui.line("  Connect with a client — a notification appears here to approve it.");
        ui.line(&format!(
            "  Console: https://{ip}:47992  (its certificate is this host's own)"
        ));
        ui.line(
            "  Password: sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password",
        );
    } else {
        ui.line("  The web console is NOT on this box, and pairing and every setting live there.");
        ui.line(&format!(
            "  Install line for your distro: {}",
            facts.docs_page
        ));
    }
    if outcome.relogin {
        ui.line("  Group changes apply after you log out and back in (controllers won't work until then).");
    }
    if choices.move_mgmt_port || facts.sunshine_active {
        ui.line(&format!(
            "  Running next to Sunshine/Apollo: {DOCS}/switching-from-sunshine"
        ));
    }
    ui.line(&format!("  {DOCS}/troubleshooting · {}", facts.docs_page));
    ui.blank();
}
