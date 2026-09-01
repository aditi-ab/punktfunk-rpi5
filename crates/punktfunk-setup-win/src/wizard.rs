//! The D9 step frame over the engine's `WinScreen` (WP2.1): Welcome → Configure → Network →
//! Install → Done, every rule of which lives in `punktfunk_setup::platform::windows::screen`
//! — this module only renders it.
//!
//! State discipline is the client shell's, measured not folklore (`clients/windows/src/app/
//! mod.rs`): everything event- or thread-driven is ROOT `use_async_state`, passed down as
//! values — the WinUI backend wires handlers straight in, and only root async state reliably
//! re-renders. Pages are plain functions (no hooks), so the root's hook order is fixed.
//!
//! The install page is a `Reporter`: the executor runs on a worker thread and every
//! `say`/`plus`/`ok`/`warn` lands as a line of root state — the same dim-command-echo
//! transparency contract as the TUI, phase checklist included.

use std::sync::Mutex;

use punktfunk_setup::platform::windows::choices::NetworkAnswer;
use punktfunk_setup::platform::windows::demo::{sandbox_app_dir, WinDemoRunner, WinPreset};
use punktfunk_setup::platform::windows::exec::{FakePayload, Subst, WinExecutor};
use punktfunk_setup::platform::windows::plan::{self, Artifact};
use punktfunk_setup::platform::windows::screen::{Editor, Field, WinScreen, WizStep};
use punktfunk_setup::platform::windows::{choices::WinChoices, report as win_report, FakeNet};
use punktfunk_setup::ui::Reporter;
use windows_reactor::*;

use crate::brand;

/// Long enough that a demo step reads as work happening — the Linux demo's value.
const DEMO_LATENCY_MS: u64 = 140;

#[derive(Clone, PartialEq)]
pub enum LineKind {
    Phase,
    Cmd,
    Ok,
    Warn,
    Fail,
    Detail,
    Text,
}

#[derive(Clone, PartialEq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum InstallPhase {
    Idle,
    Running,
    Failed,
    Finished,
}

/// The install page as a `Reporter`: lines accumulate here and mirror into root state.
struct ChannelReporter {
    lines: Mutex<Vec<LogLine>>,
    set: AsyncSetState<Vec<LogLine>>,
}

impl ChannelReporter {
    fn new(set: AsyncSetState<Vec<LogLine>>) -> ChannelReporter {
        ChannelReporter {
            lines: Mutex::new(Vec::new()),
            set,
        }
    }

    fn push(&self, kind: LineKind, text: &str) {
        let mut lines = self.lines.lock().unwrap();
        lines.push(LogLine {
            kind,
            text: text.to_string(),
        });
        self.set.call(lines.clone());
    }
}

impl Reporter for ChannelReporter {
    fn say(&self, msg: &str) {
        self.push(LineKind::Phase, msg);
    }
    fn ok(&self, msg: &str) {
        self.push(LineKind::Ok, msg);
    }
    fn warn(&self, msg: &str) {
        self.push(LineKind::Warn, msg);
    }
    fn die(&self, msg: &str) {
        self.push(LineKind::Fail, msg);
    }
    fn plus(&self, cmd: &str) {
        self.push(LineKind::Cmd, cmd);
    }
    fn detail(&self, msg: &str) {
        self.push(LineKind::Detail, msg);
    }
    fn line(&self, msg: &str) {
        self.push(LineKind::Text, msg);
    }
    fn blank(&self) {
        self.push(LineKind::Text, "");
    }
}

/// A render-time collector for the Done page's outro text (single-threaded, no channel).
#[derive(Default)]
struct VecReporter(std::cell::RefCell<Vec<String>>);

impl Reporter for VecReporter {
    fn say(&self, msg: &str) {
        self.0.borrow_mut().push(msg.to_string());
    }
    fn ok(&self, msg: &str) {
        self.say(msg);
    }
    fn warn(&self, msg: &str) {
        self.say(msg);
    }
    fn die(&self, msg: &str) {
        self.say(msg);
    }
    fn plus(&self, cmd: &str) {
        self.say(cmd);
    }
    fn detail(&self, msg: &str) {
        self.say(msg);
    }
    fn line(&self, msg: &str) {
        self.say(msg);
    }
    fn blank(&self) {}
}

/// Everything a navigation or edit handler needs — one clone per handler.
#[derive(Clone)]
struct Ctx {
    preset: WinPreset,
    screen: WinScreen,
    install: InstallPhase,
    latency_ms: u64,
    set_screen: AsyncSetState<WinScreen>,
    set_step: AsyncSetState<WizStep>,
    set_install: AsyncSetState<InstallPhase>,
    set_log: AsyncSetState<Vec<LogLine>>,
}

pub struct WizardRoot {
    preset: WinPreset,
    initial: WinScreen,
    latency_ms: u64,
}

impl WizardRoot {
    pub fn new(preset: WinPreset, latency_ms: u64) -> WizardRoot {
        let mut choices = WinChoices::derive(&preset.facts);
        // The fresh-host password row arrives pre-filled (D9): real RNG, 24 hex chars — the
        // PowerShell RNG hack dies here. It travels via an ACL'd temp file, never argv.
        if preset.artifact == Artifact::Host
            && preset.facts.installed.is_none()
            && !preset.facts.web_password_present
            && let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12)
        {
            choices.web_password = Some(pw);
        }
        let initial = WinScreen::new(preset.facts.clone(), choices, preset.artifact);
        WizardRoot {
            preset,
            initial,
            latency_ms,
        }
    }
}

impl Component for WizardRoot {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (screen, set_screen) = cx.use_async_state(self.initial.clone());
        let (step, set_step) = cx.use_async_state(WizStep::Welcome);
        let (install, set_install) = cx.use_async_state(InstallPhase::Idle);
        let (log, set_log) = cx.use_async_state(Vec::<LogLine>::new());

        let ctx = Ctx {
            preset: self.preset.clone(),
            screen: screen.clone(),
            install,
            latency_ms: self.latency_ms,
            set_screen,
            set_step,
            set_install,
            set_log,
        };
        let steps = run_steps(&ctx.preset, &ctx.screen);
        let pos = steps.iter().position(|s| *s == step).unwrap_or(0);

        let body = match step {
            WizStep::Welcome => welcome_page(&ctx, pos, steps.len()),
            WizStep::Configure => configure_page(&ctx, pos, steps.len()),
            WizStep::Network => network_page(&ctx, pos, steps.len()),
            WizStep::Install => install_page(&ctx, &log, pos, steps.len()),
            WizStep::Done => done_page(&ctx, pos, steps.len()),
        };
        body
    }
}

/// This run's step list. An uninstall run has nothing to configure — Welcome states what is
/// about to happen and Install is the teardown (the full manage screen is WP2.2).
fn run_steps(preset: &WinPreset, screen: &WinScreen) -> Vec<WizStep> {
    if preset.uninstall {
        vec![WizStep::Welcome, WizStep::Install, WizStep::Done]
    } else {
        screen.steps()
    }
}

/// Continue: move to the step after `cur` on this run's real path.
fn advance(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(&ctx.preset, &ctx.screen);
    let next = steps
        .iter()
        .skip_while(|s| **s != cur)
        .nth(1)
        .copied()
        .unwrap_or(WizStep::Done);
    if next == WizStep::Network && ctx.screen.choices.network == NetworkAnswer::Skip {
        // The step opens on its recommended answer (D12: recommended first); the radio
        // edits it from there.
        let mut s = ctx.screen.clone();
        let name = s
            .facts
            .public_networks()
            .first()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        s.set_network(NetworkAnswer::MakePrivate(name));
        ctx.set_screen.call(s);
    }
    if next == WizStep::Install {
        if ctx.install != InstallPhase::Idle {
            return;
        }
        start_install(ctx);
    }
    ctx.set_step.call(next);
}

fn back(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(&ctx.preset, &ctx.screen);
    if let Some(i) = steps.iter().position(|s| *s == cur)
        && i > 0
    {
        ctx.set_step.call(steps[i - 1]);
    }
}

/// The demo's filesystem: everything the executor may write goes under the same per-process
/// sandbox root as `demo::sandbox_paths` — including the "install dir" the uninstall preset
/// removes, staged here with a marker so the removal has something honest to do.
fn stage_demo_tree() -> String {
    let app = sandbox_app_dir();
    let _ = std::fs::create_dir_all(&app);
    let _ = std::fs::write(
        std::path::Path::new(&app).join("installed-by-demo.txt"),
        "demo install marker\n",
    );
    let tmp = std::env::temp_dir()
        .join(format!("punktfunk-setup-demo-{}", std::process::id()))
        .join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    tmp.display().to_string()
}

fn start_install(ctx: &Ctx) {
    let preset = ctx.preset.clone();
    let screen = ctx.screen.clone();
    let latency_ms = ctx.latency_ms;
    let set_install = ctx.set_install.clone();
    let set_step = ctx.set_step.clone();
    let set_log = ctx.set_log.clone();
    set_install.call(InstallPhase::Running);
    std::thread::spawn(move || {
        let tmp = stage_demo_tree();
        let choices = screen.effective_choices();
        let built = plan::build(&screen.facts, &choices, preset.artifact, preset.uninstall);
        let ui = ChannelReporter::new(set_log);
        let run = WinDemoRunner::new(latency_ms, None);
        let net = FakeNet {
            networks: screen.facts.networks.clone(),
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
            dry: false,
            silent: false,
            web_password: choices.web_password.clone(),
            subst: Subst {
                version: concat!(env!("CARGO_PKG_VERSION"), "-demo").to_string(),
                staging: format!("{tmp}\\staging"),
                temp: tmp,
            },
        };
        match exec.execute(&built) {
            Ok(()) => {
                set_install.call(InstallPhase::Finished);
                set_step.call(WizStep::Done);
            }
            Err(failed) => {
                ui.die(&failed.0);
                set_install.call(InstallPhase::Failed);
            }
        }
    });
}

// --- rendering ---------------------------------------------------------------------------

fn edges(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

/// A rounded, bordered surface in the theme's card colours — the client shell's card.
fn card(child: impl Into<Element>) -> Border {
    border(child.into())
        .background(ThemeRef::CardBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(8.0)
        .padding(edges(16.0, 10.0, 16.0, 10.0))
}

/// Header · content · button bar. The dots-and-line stepper replaces the plain step counter
/// with WP2.1b; the counter already honours the materialize rule (it counts this run's path).
fn frame(
    step: WizStep,
    pos: usize,
    total: usize,
    content: Element,
    buttons: Vec<Element>,
) -> Element {
    let head = vstack((
        text_block(step.title()).font_size(24.0).semibold(),
        text_block(format!("Step {} of {total}", pos + 1))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(2.0)
    .margin(edges(0.0, 0.0, 0.0, 12.0));
    let bar = hstack(buttons)
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Right)
        .margin(edges(0.0, 14.0, 0.0, 0.0));
    grid((head.grid_row(0), content.grid_row(1), bar.grid_row(2)))
        .rows([GridLength::Auto, GridLength::Star(1.0), GridLength::Auto])
        .margin(edges(28.0, 22.0, 28.0, 20.0))
        .into()
}

fn continue_button(ctx: &Ctx, cur: WizStep, label: &str) -> Element {
    let ctx = ctx.clone();
    button(label)
        .accent()
        .on_click(move || advance(&ctx, cur))
        .into()
}

fn back_button(ctx: &Ctx, cur: WizStep) -> Element {
    let ctx = ctx.clone();
    button("Back").on_click(move || back(&ctx, cur)).into()
}

fn welcome_page(ctx: &Ctx, pos: usize, total: usize) -> Element {
    let sentence = if ctx.preset.uninstall {
        "This removes punktfunk from this PC. Identity, pairings and passwords stay — a reinstall picks them up."
    } else {
        match ctx.preset.artifact {
            Artifact::Host => {
                "This installs the punktfunk host — it streams this PC's screen, audio and games to your devices."
            }
            Artifact::Client => {
                "This installs the punktfunk client — it plays streams from a punktfunk host."
            }
        }
    };
    let mut children: Vec<Element> = Vec::new();
    if let Some(uri) = brand::mark_uri() {
        children.push(
            Image::new_with_uri(uri)
                .stretch(Stretch::Uniform)
                .width(96.0)
                .height(96.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    children.push(
        text_block("punktfunk")
            .font_size(32.0)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    );
    children.push(
        text_block(sentence)
            .wrap()
            .foreground(ThemeRef::SecondaryText)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    );
    let content = vstack(children)
        .spacing(14.0)
        .max_width(420.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    frame(
        WizStep::Welcome,
        pos,
        total,
        content,
        vec![continue_button(ctx, WizStep::Welcome, "Continue")],
    )
}

fn configure_page(ctx: &Ctx, pos: usize, total: usize) -> Element {
    let mut items: Vec<Element> = Vec::new();
    // The D11 coexistence row: a visible row, never a dialog.
    if let Some(note) = ctx.screen.coexistence_note() {
        items.push(
            card(
                vstack((
                    text_block("Runs next to your other streaming host")
                        .semibold()
                        .foreground(ThemeRef::SystemAttention),
                    text_block(note).wrap().foreground(ThemeRef::SecondaryText),
                ))
                .spacing(2.0),
            )
            .into(),
        );
    }
    for field in ctx.screen.rows() {
        items.push(config_row(ctx, field));
    }
    let content = scroll_view(vstack(items).spacing(6.0).max_width(640.0)).into();
    // The go button says what Continue will do: install now, or one more step first.
    let steps = run_steps(&ctx.preset, &ctx.screen);
    let next_is_network = steps
        .iter()
        .skip_while(|s| **s != WizStep::Configure)
        .nth(1)
        == Some(&WizStep::Network);
    let label = if next_is_network {
        "Continue"
    } else {
        "Install"
    };
    frame(
        WizStep::Configure,
        pos,
        total,
        content,
        vec![
            back_button(ctx, WizStep::Configure),
            continue_button(ctx, WizStep::Configure, label),
        ],
    )
}

fn config_row(ctx: &Ctx, field: Field) -> Element {
    let editor: Element = match ctx.screen.editor(field) {
        Editor::Toggle(on) => {
            let ctx = ctx.clone();
            ToggleSwitch::new(on)
                .on_toggled(move |v: bool| {
                    let mut s = ctx.screen.clone();
                    s.set_bool(field, v);
                    ctx.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
        Editor::TriState(value) => {
            let ctx = ctx.clone();
            ComboBox::new(["Keep the box's setting", "On", "Off"])
                .selected_index(match value {
                    None => 0,
                    Some(true) => 1,
                    Some(false) => 2,
                })
                .on_selection_changed(move |i: i32| {
                    let mut s = ctx.screen.clone();
                    match i {
                        0 => s.keep_box_setting(field),
                        1 => s.set_bool(field, true),
                        _ => s.set_bool(field, false),
                    }
                    ctx.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
        Editor::Password(value) => {
            let ctx = ctx.clone();
            PasswordBox::new()
                .value(value.unwrap_or_default())
                .reveal_button_enabled(true)
                .on_password_changed(move |pw: String| {
                    let mut s = ctx.screen.clone();
                    s.set_password(pw);
                    ctx.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
    };
    card(
        grid((
            vstack((
                text_block(WinScreen::label(field)).semibold(),
                text_block(ctx.screen.why(field))
                    .wrap()
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(1.0)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(0),
            editor.grid_column(1),
        ))
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .column_spacing(16.0),
    )
    .into()
}

fn network_page(ctx: &Ctx, pos: usize, total: usize) -> Element {
    let name = match &ctx.screen.choices.network {
        NetworkAnswer::MakePrivate(n) if !n.is_empty() => n.clone(),
        _ => ctx
            .screen
            .facts
            .public_networks()
            .first()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| "this network".into()),
    };
    let selected = match &ctx.screen.choices.network {
        NetworkAnswer::MakePrivate(_) => 0,
        NetworkAnswer::OpenPublicRules => 1,
        NetworkAnswer::Skip => 2,
    };
    let radio = {
        let ctx = ctx.clone();
        let name = name.clone();
        RadioButtons::new([
            format!("This is my home network — make '{name}' Private (recommended)"),
            "Keep it Public, open the firewall for it".to_string(),
            "Skip — I'll fix it later".to_string(),
        ])
        .selected_index(selected)
        .on_selection_changed(move |i: i32| {
            let mut s = ctx.screen.clone();
            s.set_network(match i {
                0 => NetworkAnswer::MakePrivate(name.clone()),
                1 => NetworkAnswer::OpenPublicRules,
                _ => NetworkAnswer::Skip,
            });
            ctx.set_screen.call(s);
        })
    };
    let content = vstack((
        text_block(format!("'{name}' is set to Public"))
            .font_size(16.0)
            .semibold(),
        text_block(
            "Windows scopes the standard firewall rules to private networks, so on this network the host would be silently unreachable. VPN and virtual adapters are routinely Public by design — this changes only the named network, never a global setting.",
        )
        .wrap()
        .foreground(ThemeRef::SecondaryText),
        radio.into(),
    ))
    .spacing(12.0)
    .max_width(640.0)
    .into();
    frame(
        WizStep::Network,
        pos,
        total,
        content,
        vec![
            back_button(ctx, WizStep::Network),
            continue_button(ctx, WizStep::Network, "Install"),
        ],
    )
}

fn install_page(ctx: &Ctx, log: &[LogLine], pos: usize, total: usize) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for line in log {
        items.push(match line.kind {
            LineKind::Phase => text_block(line.text.as_str())
                .semibold()
                .margin(edges(0.0, 8.0, 0.0, 0.0))
                .into(),
            LineKind::Cmd => text_block(format!("  + {}", line.text))
                .font_size(12.0)
                .wrap()
                .foreground(ThemeRef::TertiaryText)
                .into(),
            LineKind::Ok => text_block(format!("✓ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SecondaryText)
                .into(),
            LineKind::Warn => text_block(format!("⚠ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SystemCaution)
                .into(),
            LineKind::Fail => text_block(format!("✗ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SystemCritical)
                .into(),
            LineKind::Detail => text_block(format!("  {}", line.text))
                .font_size(12.0)
                .wrap()
                .foreground(ThemeRef::TertiaryText)
                .into(),
            LineKind::Text => text_block(line.text.as_str()).wrap().into(),
        });
    }
    let mut children: Vec<Element> = Vec::new();
    if ctx.install == InstallPhase::Running {
        children.push(
            hstack((
                ProgressRing::indeterminate().width(18.0).height(18.0),
                text_block("Installing…").foreground(ThemeRef::SecondaryText),
            ))
            .spacing(8.0)
            .into(),
        );
    }
    children.push(
        card(scroll_view(vstack(items).spacing(2.0)))
            .padding(edges(14.0, 10.0, 14.0, 10.0))
            .into(),
    );
    let content = vstack(children).spacing(10.0).into();
    let buttons = if ctx.install == InstallPhase::Failed {
        vec![button("Close").on_click(|| std::process::exit(1)).into()]
    } else {
        vec![] // no cancel mid-plan: the executor's steps are not interruptible-safe
    };
    frame(WizStep::Install, pos, total, content, buttons)
}

fn done_page(ctx: &Ctx, pos: usize, total: usize) -> Element {
    let mut children: Vec<Element> = Vec::new();
    if ctx.preset.uninstall {
        children.push(
            text_block("punktfunk was removed from this PC.")
                .wrap()
                .into(),
        );
        children.push(
            text_block(
                r"Kept on purpose: %ProgramData%\punktfunk (identity, passwords, update cache) — a reinstall picks it up.",
            )
            .wrap()
            .foreground(ThemeRef::SecondaryText)
            .into(),
        );
    } else {
        // Fresh host: the password card, the one thing the user must leave with (D9).
        let fresh = ctx.screen.fresh();
        if ctx.preset.artifact == Artifact::Host
            && fresh
            && let Some(pw) = &ctx.screen.choices.web_password
        {
            children.push(
                card(
                    vstack((
                        text_block("Web console password").semibold(),
                        text_block(pw.clone()).font_size(18.0).selectable(),
                        text_block(r"Also stored (ACL'd) in %ProgramData%\punktfunk\web-password")
                            .font_size(12.0)
                            .foreground(ThemeRef::SecondaryText),
                    ))
                    .spacing(4.0),
                )
                .into(),
            );
        }
        // The transcript outro carries the D11/D12 footnotes — same words, this surface.
        let collector = VecReporter::default();
        win_report::outro(
            &collector,
            &ctx.screen.facts,
            &ctx.screen.effective_choices(),
            ctx.preset.artifact,
        );
        for line in collector.0.into_inner() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            children.push(text_block(trimmed).wrap().into());
        }
    }
    let content = scroll_view(vstack(children).spacing(8.0).max_width(640.0)).into();
    frame(
        WizStep::Done,
        pos,
        total,
        content,
        vec![button("Finish")
            .accent()
            .on_click(|| std::process::exit(0))
            .into()],
    )
}

/// The real window. `--demo` is the only mode this build has (main.rs enforces it).
pub fn run(preset: WinPreset) -> windows_reactor::Result<()> {
    brand::install();
    stage_demo_tree();
    let root = WizardRoot::new(preset, DEMO_LATENCY_MS);
    // Self-contained: the runtime DLLs sit beside the exe (build.rs), so there is
    // deliberately NO windows_reactor::bootstrap() call — that is the framework path (S1).
    App::new()
        .title("Punktfunk Setup")
        .inner_size(760.0, 660.0)
        .backdrop(Backdrop::Mica)
        .render(move |cx| root.render(&(), cx))
}
