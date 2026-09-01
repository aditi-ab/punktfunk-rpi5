//! The CLI: flags, env twins, the TTY probe, and the mode dispatch.
//!
//! D5 is the contract this file keeps — same flags, same env twins, same exit codes
//! (0 done · 1 unsupported system or a step failed · 2 bad usage) as `scripts/install.sh`,
//! plus `--host`/`--client` and the demo flags that are outside it.
//!
//! No TTY behaves exactly like `--yes`, and the terminal is probed by *opening* `/dev/tty`:
//! in a container or under a service the node exists but opens ENXIO, so `-r`/`-w` lie.

use std::path::PathBuf;
use std::process::ExitCode;

use punktfunk_setup::choices::{Action, Choices, Pins};
use punktfunk_setup::exec::{Executor, Opts};
use punktfunk_setup::facts::{Facts, Floor, DOCS};
use punktfunk_setup::plan;
use punktfunk_setup::report;
use punktfunk_setup::seam::{BasePaths, CommandRunner, Env, SystemRunner};
use punktfunk_setup::ui::logo;
use punktfunk_setup::ui::summary::{Screen, Step};
use punktfunk_setup::ui::term::{ConsoleTerm, Terminal};
use punktfunk_setup::ui::theme::Caps;
use punktfunk_setup::ui::tui::Tui;
use punktfunk_setup::ui::{Plain, Reporter};

/// Long enough that a demo step reads as work happening, short enough to sit through.
const DEMO_LATENCY_MS: u64 = 140;

/// The mark needs a width to decide whether it fits; 80 is the safe assumption when the
/// terminal will not say.
fn terminal_width() -> u16 {
    ConsoleTerm::open().map_or(80, |t| t.width())
}

const USAGE: &str = r#"punktfunk guided installer (preview)

usage: punktfunk-setup [options]
  -y, --yes             no prompts: take every default (also the behaviour without a terminal)
  --host | --client     what to install (default: host; combinable)
  --channel stable|canary   package channel (default stable; canary = latest main build). On a box
                        that already has it this SWITCHES channel, either direction.
  --gamestream | --no-gamestream   Moonlight/Artemis/third-party clients (default depends on the box)
  --clipboard | --no-clipboard     shared clipboard (default no)
  --punktfunk-group | --no-punktfunk-group   full controller / virtual Steam Deck pad (default yes;
                        it joins the punktfunk group, which grants usbip attach)
  --linger | --no-linger           start at boot with nobody logged in (default depends on the box)
  --omarchy-setup | --no-omarchy-setup   run `punktfunk-omarchy setup` after an Omarchy install
  --mgmt-port N         port to move the management API to if Sunshine/Apollo holds 47990 (default 47991)
  --no-start            install and configure, but don't enable the services
  --uninstall           stop the services and remove the packages + repo (config stays)
  --dry-run             print every command it would run, change nothing
  --facts FILE          load a box description instead of probing this one
  --demo PRESET         walk the whole flow against a canned box, changing nothing
  --fail PHASE          with --demo: show how a failure in that phase renders
  -h, --help            this text

Every option has an environment twin for scripted installs: PUNKTFUNK_INSTALL_YES=1,
PUNKTFUNK_INSTALL_CHANNEL, PUNKTFUNK_INSTALL_GAMESTREAM, PUNKTFUNK_INSTALL_CLIPBOARD,
PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP, PUNKTFUNK_INSTALL_LINGER, PUNKTFUNK_INSTALL_OMARCHY_SETUP,
PUNKTFUNK_INSTALL_MGMT_PORT (1/0 for the flags)."#;

/// 2 is "bad usage", matching the sh installer's contract.
const BAD_USAGE: u8 = 2;

struct Cli {
    pins: Pins,
    yes: bool,
    dry: bool,
    facts_file: Option<PathBuf>,
    demo: Option<String>,
    fail: Option<String>,
}

fn env_flag(env: &Env, key: &str) -> Option<bool> {
    env.get(key).map(|v| v == "1")
}

fn parse(args: Vec<String>, env: &Env) -> Result<Cli, (u8, String)> {
    // Env is the default; a flag on the command line overwrites it, exactly as the sh script
    // reads its variables first and lets the arg loop assign over them.
    let mut cli = Cli {
        pins: Pins {
            channel: env
                .get("PUNKTFUNK_INSTALL_CHANNEL")
                .and_then(|v| v.parse().ok()),
            gamestream: env_flag(env, "PUNKTFUNK_INSTALL_GAMESTREAM"),
            clipboard: env_flag(env, "PUNKTFUNK_INSTALL_CLIPBOARD"),
            punktfunk_group: env_flag(env, "PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP"),
            linger: env_flag(env, "PUNKTFUNK_INSTALL_LINGER"),
            omarchy_setup: env_flag(env, "PUNKTFUNK_INSTALL_OMARCHY_SETUP"),
            mgmt_port: env
                .get("PUNKTFUNK_INSTALL_MGMT_PORT")
                .map(|v| v.parse().unwrap_or(0)),
            ..Pins::default()
        },
        yes: env.get("PUNKTFUNK_INSTALL_YES") == Some("1"),
        dry: env.get("PUNKTFUNK_INSTALL_DRY_RUN") == Some("1"),
        facts_file: None,
        demo: None,
        fail: None,
    };
    if env.get("PUNKTFUNK_INSTALL_CHANNEL").is_some() && cli.pins.channel.is_none() {
        return Err((BAD_USAGE, "--channel must be stable or canary".into()));
    }

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next());
        match flag.as_str() {
            "-y" | "--yes" => cli.yes = true,
            "--host" => cli.pins.host = true,
            "--client" => cli.pins.client = true,
            "--channel" => {
                let raw = value().unwrap_or_default();
                cli.pins.channel =
                    Some(raw.parse().map_err(|()| {
                        (BAD_USAGE, "--channel must be stable or canary".to_string())
                    })?);
            }
            "--gamestream" => cli.pins.gamestream = Some(true),
            "--no-gamestream" => cli.pins.gamestream = Some(false),
            "--clipboard" => cli.pins.clipboard = Some(true),
            "--no-clipboard" => cli.pins.clipboard = Some(false),
            "--punktfunk-group" => cli.pins.punktfunk_group = Some(true),
            "--no-punktfunk-group" => cli.pins.punktfunk_group = Some(false),
            "--linger" => cli.pins.linger = Some(true),
            "--no-linger" => cli.pins.linger = Some(false),
            "--omarchy-setup" => cli.pins.omarchy_setup = Some(true),
            "--no-omarchy-setup" => cli.pins.omarchy_setup = Some(false),
            "--mgmt-port" => {
                let raw = value().unwrap_or_default();
                cli.pins.mgmt_port = Some(
                    raw.parse()
                        .map_err(|_| (BAD_USAGE, "--mgmt-port must be a number".to_string()))?,
                );
            }
            "--no-start" => cli.pins.no_start = true,
            "--uninstall" => cli.pins.action = Action::Uninstall,
            "--dry-run" => cli.dry = true,
            "--facts" => cli.facts_file = value().map(PathBuf::from),
            "--demo" => cli.demo = value(),
            "--fail" => cli.fail = value(),
            "-h" | "--help" => return Err((0, USAGE.to_string())),
            other => {
                return Err((BAD_USAGE, format!("unknown option: {other}\n\n{USAGE}")));
            }
        }
    }
    if cli.pins.mgmt_port == Some(0) {
        return Err((BAD_USAGE, "--mgmt-port must be a number".into()));
    }
    Ok(cli)
}

fn main() -> ExitCode {
    let env = Env::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(args, &env) {
        Ok(cli) => cli,
        Err((0, text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err((code, text)) => {
            eprintln!("{text}");
            return ExitCode::from(code);
        }
    };

    // Probe the terminal by opening it: no terminal behaves like --yes.
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok();
    let demo = cli.demo.clone();
    let yes = cli.yes || !tty;
    let plain = Plain::stdio(env.get("NO_COLOR").is_none());
    let paths = match &demo {
        Some(_) => punktfunk_setup::demo::sandbox_paths(),
        None => BasePaths::from_env(),
    };
    let caps = Caps::detect(&env, tty, terminal_width());

    let mut runner = SystemRunner::new();
    let user = env
        .get("USER")
        .map(str::to_string)
        .or_else(|| runner.first_line("id", &["-un"]))
        .unwrap_or_default();
    runner.exports.push(("USER".to_string(), user));

    // A demo box is canned, so none of the preflight applies — and nothing may be probed.
    let facts = if let Some(name) = &demo {
        match punktfunk_setup::demo::preset(name) {
            Some(facts) => facts,
            None => {
                plain.die(&format!(
                    "unknown --demo preset '{name}'. Try: {}",
                    punktfunk_setup::demo::PRESETS.join(", ")
                ));
                return ExitCode::FAILURE;
            }
        }
    } else {
        if let Err(msg) = preflight(&env, &paths, &mut runner) {
            report::banner(&plain);
            plain.die(&msg);
            return ExitCode::FAILURE;
        }
        match load_facts(&cli, &paths, &runner, &env) {
            Ok(facts) => facts,
            Err(msg) => {
                report::banner(&plain);
                plain.die(&msg);
                return ExitCode::FAILURE;
            }
        }
    };

    // The TUI is the interactive surface only. No terminal or --yes stays on today's output,
    // byte for byte, which is what CI containers and scripts already depend on.
    let interactive = tty && !yes;
    let mut console = if interactive {
        ConsoleTerm::open()
    } else {
        None
    };
    let tui = console
        .as_mut()
        .map(|term| Tui::new(term as &mut dyn Terminal, caps, logo::FRAME_MS));
    let ui: &dyn Reporter = match &tui {
        Some(tui) => tui,
        None => &plain,
    };

    let mut choices = Choices::derive(&facts, &cli.pins);
    let mut opts = Opts {
        dry: cli.dry,
        yes,
        tty,
    };

    if tui.is_none() {
        report::banner(ui);
        report::detected(ui, &facts);
    }

    // The floors sit after the uninstall dispatch on purpose: a box below them must still be
    // able to clean itself up.
    if choices.action != Action::Uninstall
        && let Some(floor) = &facts.floor
    {
        match floor {
            Floor::Die(msg) => {
                ui.die(msg);
                return ExitCode::FAILURE;
            }
            Floor::Confirm(msg) => {
                ui.warn(msg);
                if !ask(interactive, "Continue anyway?") {
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    if let Some(tui) = &tui {
        let parts = logo::Parts {
            host: choices.components.host,
            client: choices.components.client,
        };
        let drawn = tui.intro(logo::intro_level(&caps, yes), parts);
        let mut screen = Screen::new(facts.clone(), choices.clone());
        match tui.settings(&mut screen, drawn) {
            Step::Cancel => {
                tui.outro(&["Nothing was changed.".to_string()]);
                return ExitCode::SUCCESS;
            }
            Step::DryRun => opts.dry = true,
            Step::Run(action) => choices.action = action,
            Step::Idle | Step::Edit(_) => unreachable!("the settings loop only ends on a choice"),
        }
        // Every edit the user made lives on the screen, not in the pins.
        let action = choices.action;
        choices = screen.choices;
        choices.action = action;
    } else {
        report::choices_summary(ui, &choices);
    }

    // A distro with no punktfunk repo stops a HOST install and nothing else: a client install
    // there takes the flatpak line instead of dying (§5). Checked after the screen, because
    // switching Components to client-only is exactly how a user gets past it.
    if choices.action != Action::Uninstall
        && choices.components.host
        && let Some(msg) = &facts.host_punt
    {
        ui.die(msg);
        return ExitCode::FAILURE;
    }

    let plan = plan::build(&facts, &choices);
    let demo_runner = demo.as_ref().map(|_| {
        let at = cli
            .fail
            .as_deref()
            .and_then(|phase| punktfunk_setup::demo::fail_index(&plan, phase));
        punktfunk_setup::demo::DemoRunner::new(DEMO_LATENCY_MS, at)
    });
    let run: &dyn CommandRunner = match &demo_runner {
        Some(demo) => demo,
        None => &runner,
    };
    let exec = Executor {
        paths: &paths,
        run,
        ui,
        opts,
    };
    let outcome = match exec.execute(&plan, &facts, &choices) {
        Ok(outcome) => outcome,
        Err(failed) => {
            ui.die(&failed.0);
            return ExitCode::FAILURE;
        }
    };

    if choices.action == Action::Uninstall {
        report::uninstall_outro(ui);
        return ExitCode::SUCCESS;
    }
    // `punktfunk-omarchy setup` did the wiring and prints its own outro.
    if !outcome.ended_early {
        report::verify(ui, run, &facts, &choices, &outcome, opts);
    }
    ExitCode::SUCCESS
}

fn preflight(env: &Env, paths: &BasePaths, runner: &mut SystemRunner) -> Result<(), String> {
    let linux =
        std::env::consts::OS == "linux" || env.get("PUNKTFUNK_INSTALL_OS_RELEASE").is_some();
    if !linux {
        return Err(format!(
            "this installer is for Linux hosts — Windows: {DOCS}/windows-host"
        ));
    }
    let root = runner.first_line("id", &["-u"]).as_deref() == Some("0");
    if env.get("SUDO_USER").is_some() && root {
        return Err("run this as your normal user, not under sudo — it calls sudo itself where needed, and the host runs as you (host.env, the services)".into());
    }
    if !runner.which("curl") {
        return Err("curl is required (install it with your package manager first)".into());
    }
    // Running as root without sudo (a minimal Debian container): a shim so the verbatim
    // `sudo …` lines from platforms.json still work.
    if root && !runner.which("sudo") {
        let dir = std::env::temp_dir().join(format!("punktfunk-setup-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(dir.join("sudo"), "#!/bin/sh\nexec \"$@\"\n").is_ok()
        {
            set_executable(&dir.join("sudo"));
            runner.path_prefix = Some(dir);
        }
    }
    if !paths.os_release.exists() {
        return Err(format!(
            "no /etc/os-release — can't tell which distro this is: {DOCS}/install"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) {}

fn load_facts(
    cli: &Cli,
    paths: &BasePaths,
    runner: &SystemRunner,
    env: &Env,
) -> Result<Facts, String> {
    if let Some(file) = &cli.facts_file {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        return serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a Facts document: {e}", file.display()));
    }
    Facts::probe(paths, runner, env).map_err(|punt| punt.message())
}

/// The sh installer's `ask`, with the default behind Enter. Without a terminal the default
/// stands — which is why a version-floor confirm defaulting to *no* aborts under `--yes`.
fn ask(interactive: bool, question: &str) -> bool {
    if !interactive {
        return false;
    }
    use std::io::{BufRead, Write};
    let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return false;
    };
    let _ = write!(tty, "? {question} [y/N] ");
    let _ = tty.flush();
    let mut answer = String::new();
    let Ok(tty_read) = std::fs::File::open("/dev/tty") else {
        return false;
    };
    if std::io::BufReader::new(tty_read)
        .read_line(&mut answer)
        .is_err()
    {
        return false;
    }

    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_setup::facts::Channel;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn an_unknown_option_is_bad_usage() {
        let err = parse(args(&["--nope"]), &Env::default()).err().unwrap();
        assert_eq!(err.0, BAD_USAGE);
        assert!(err.1.starts_with("unknown option: --nope"));
    }

    #[test]
    fn help_exits_zero_and_names_every_env_twin() {
        let err = parse(args(&["--help"]), &Env::default()).err().unwrap();
        assert_eq!(err.0, 0);
        for twin in [
            "PUNKTFUNK_INSTALL_YES",
            "PUNKTFUNK_INSTALL_CHANNEL",
            "PUNKTFUNK_INSTALL_GAMESTREAM",
            "PUNKTFUNK_INSTALL_CLIPBOARD",
            "PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP",
            "PUNKTFUNK_INSTALL_LINGER",
            "PUNKTFUNK_INSTALL_OMARCHY_SETUP",
            "PUNKTFUNK_INSTALL_MGMT_PORT",
        ] {
            assert!(err.1.contains(twin), "--help does not mention {twin}");
        }
    }

    #[test]
    fn a_bad_channel_or_port_is_bad_usage() {
        assert_eq!(
            parse(args(&["--channel", "beta"]), &Env::default())
                .err()
                .unwrap()
                .0,
            BAD_USAGE
        );
        assert_eq!(
            parse(args(&["--mgmt-port", "http"]), &Env::default())
                .err()
                .unwrap()
                .0,
            BAD_USAGE
        );
    }

    #[test]
    fn both_the_split_and_the_equals_spelling_parse() {
        let split = parse(args(&["--channel", "canary"]), &Env::default()).unwrap();
        let equals = parse(args(&["--channel=canary"]), &Env::default()).unwrap();
        assert_eq!(split.pins.channel, Some(Channel::Canary));
        assert_eq!(equals.pins.channel, Some(Channel::Canary));
    }

    #[test]
    fn a_flag_overrides_its_env_twin() {
        let env = Env::of(&[
            ("PUNKTFUNK_INSTALL_CHANNEL", "canary"),
            ("PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP", "1"),
        ]);
        let cli = parse(args(&["--channel", "stable", "--no-punktfunk-group"]), &env).unwrap();
        assert_eq!(cli.pins.channel, Some(Channel::Stable));
        assert_eq!(cli.pins.punktfunk_group, Some(false));
    }

    #[test]
    fn env_twins_stand_when_no_flag_contradicts_them() {
        let env = Env::of(&[
            ("PUNKTFUNK_INSTALL_YES", "1"),
            ("PUNKTFUNK_INSTALL_LINGER", "0"),
            ("PUNKTFUNK_INSTALL_OMARCHY_SETUP", "0"),
            ("PUNKTFUNK_INSTALL_MGMT_PORT", "48000"),
        ]);
        let cli = parse(vec![], &env).unwrap();
        assert!(cli.yes);
        assert_eq!(cli.pins.linger, Some(false));
        assert_eq!(cli.pins.omarchy_setup, Some(false));
        assert_eq!(cli.pins.mgmt_port, Some(48000));
    }

    // The smoke needs to decline the Omarchy hand-off; today's sh question has no env twin.
    #[test]
    fn the_omarchy_hand_off_has_a_flag_and_an_env_twin() {
        let flag = parse(args(&["--no-omarchy-setup"]), &Env::default()).unwrap();
        assert_eq!(flag.pins.omarchy_setup, Some(false));
        let env = Env::of(&[("PUNKTFUNK_INSTALL_OMARCHY_SETUP", "1")]);
        assert_eq!(parse(vec![], &env).unwrap().pins.omarchy_setup, Some(true));
    }

    #[test]
    fn components_default_to_host_and_client_is_opt_in() {
        assert!(!parse(vec![], &Env::default()).unwrap().pins.client);
        assert!(
            parse(args(&["--client"]), &Env::default())
                .unwrap()
                .pins
                .client
        );
        let both = parse(args(&["--host", "--client"]), &Env::default()).unwrap();
        assert!(both.pins.host && both.pins.client);
    }
}
