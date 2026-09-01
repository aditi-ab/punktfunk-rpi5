//! The wizard exe's silent entry (WP2.4): no window. The engine's `silent::run` over the real
//! box or a `--demo` preset, `Plain` into `/LOG=` and — when a terminal launched us — the
//! parent console. A GUI-subsystem exe owns no console: `sys::attach_parent_console` binds
//! the parent's and hands back `CONOUT$`, because the std handles were fixed at startup and
//! stdout is not it.
//!
//! No payload ships in this build (WP3.1), so a real run refuses with exit 1 — never a silent
//! no-op exiting 0 (D5). `--dry-run` and `--demo` are the two honest modes until then.

use std::process::ExitCode;

use punktfunk_setup::platform::windows::args::InnoArgs;
use punktfunk_setup::platform::windows::demo::{WinDemoRunner, WinPreset};
use punktfunk_setup::platform::windows::exec::{FakePayload, Subst, WinExecutor};
use punktfunk_setup::platform::windows::plan::Artifact;
use punktfunk_setup::platform::windows::{base_paths, silent, sys, FakeNet, SystemNet, WinFacts};
use punktfunk_setup::seam::{Env, SystemRunner};
use punktfunk_setup::ui::{Plain, Reporter};

pub fn main(inno: &InnoArgs, demo: Option<WinPreset>, uninstaller: bool, dry: bool) -> ExitCode {
    let log = inno
        .log
        .as_ref()
        .and_then(|path| {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::File::create(path).ok()
        })
        .map(|file| Plain::to_writer(file, false));
    let console = sys::attach_parent_console().map(|file| Plain::to_writer(file, false));
    let ui = silent::Tee(
        log.iter()
            .chain(console.iter())
            .map(|p| p as &dyn Reporter)
            .collect(),
    );

    if demo.is_none() && !dry {
        ui.die("no payload in this build — nothing was changed (add --dry-run, or wait for the packed installer)");
        return ExitCode::FAILURE;
    }
    let env = Env::from_env();
    let outcome = match demo {
        Some(preset) => {
            let tmp = crate::wizard::stage_demo_tree();
            let run = WinDemoRunner::new(0, None);
            let net = FakeNet {
                networks: preset.facts.networks.clone(),
                ..FakeNet::default()
            };
            let payload = FakePayload::default();
            let paths = punktfunk_setup::demo::sandbox_paths();
            let exec = WinExecutor {
                run: &run,
                net: &net,
                payload: &payload,
                paths: &paths,
                ui: &ui,
                dry,
                silent: true,
                web_password: None,
                subst: Subst {
                    version: concat!(env!("CARGO_PKG_VERSION"), "-demo").to_string(),
                    staging: format!("{tmp}\\staging"),
                    temp: tmp,
                },
            };
            silent::run(
                &exec,
                &preset.facts,
                preset.artifact,
                preset.uninstall || uninstaller,
                inno,
                &env,
            )
        }
        None => {
            // Dry by construction (checked above): the real seams probe, nothing deploys.
            let run = SystemRunner::new();
            let net = SystemNet;
            let paths = base_paths(&env);
            let facts = WinFacts::probe(&paths, &run, &env, &net);
            let payload = FakePayload::default();
            let exec = WinExecutor {
                run: &run,
                net: &net,
                payload: &payload,
                paths: &paths,
                ui: &ui,
                dry: true,
                silent: true,
                web_password: None,
                subst: Subst {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    ..Subst::default()
                },
            };
            silent::run(&exec, &facts, Artifact::Host, uninstaller, inno, &env)
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
