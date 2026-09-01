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
//!
//! WP2.1b: the stepper draws `run_steps()` — this run's real path, never a ghost Network
//! dot — and each navigation slides the page in directionally via the client shell's manual
//! tween (a worker stepping root state under a generation guard); animations off ⇒ a cut.
//! WP2.2: an installed box opens in manage mode — Welcome re-titled, Reconfigure or
//! Uninstall — and the payload-less `unins000.exe` (D6) is the same page offering only the
//! teardown. Uninstall is run state, chosen there; the executor thread reads it from `Ctx`.

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};

use punktfunk_setup::platform::windows::choices::NetworkAnswer;
use punktfunk_setup::platform::windows::demo::{sandbox_app_dir, WinDemoRunner, WinPreset};
use punktfunk_setup::platform::windows::exec::{FakePayload, PayloadSource, Subst, WinExecutor};
use punktfunk_setup::platform::windows::plan::{self, Artifact};
use punktfunk_setup::platform::windows::screen::{Editor, Field, WinScreen, WizStep};
use punktfunk_setup::platform::windows::{
    base_paths, choices::WinChoices, report as win_report, FakeNet, NetProbe, SystemNet,
};
use punktfunk_setup::seam::{CommandRunner, Env, SystemRunner};
use punktfunk_setup::ui::Reporter;
use windows_reactor::*;

use crate::brand;
use crate::real::{self, DirPayload, Seams};

/// Long enough that a demo step reads as work happening — the Linux demo's value.
pub const DEMO_LATENCY_MS: u64 = 140;

/// 14 × 16 ms ≈ 220 ms: over before a fast clicker's next click lands (the generation guard
/// cuts a superseded tween regardless).
const SLIDE_FRAMES: u32 = 14;
const SLIDE_TRAVEL: f64 = 36.0;

/// Upcoming dots and the line ahead: a translucent grey that reads on both themes — shapes
/// take a `Color`, not a `ThemeRef`.
const MUTED: Color = Color {
    a: 0x55,
    r: 0x80,
    g: 0x80,
    b: 0x80,
};

/// Stepper dot diameter; the line is 2 px and meets the dot's edge.
const DOT: f64 = 10.0;

/// Command echo and the password: a monospace face that ships with Windows.
const MONO: &str = "Consolas";

/// Where the wizard stands and which way it got there (D9: Continue enters from the right,
/// Back from the left). The install thread's jump to Done is a forward move.
#[derive(Clone, Copy, PartialEq)]
pub struct Nav {
    pub step: WizStep,
    pub forward: bool,
}

/// The page's slide-in: 0 just after a navigation (off to the side, transparent) → 1 settled.
#[derive(Clone, Copy, PartialEq)]
struct Slide {
    progress: f64,
    forward: bool,
}

/// `UISettings.AnimationsEnabled`, the system "show animations" switch: off ⇒ every slide is
/// a cut (D9). Read once — the setting does not change mid-install.
fn animations_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        windows::UI::ViewManagement::UISettings::new()
            .and_then(|s| s.AnimationsEnabled())
            .unwrap_or(true)
    })
}

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
    /// This run tears down instead of installing: the uninstaller exe, or Uninstall chosen
    /// on the manage Welcome.
    uninstall: bool,
    seams: Seams,
    slide: Slide,
    /// The password row's Show/Hide state.
    reveal: bool,
    /// The install page's progress bar, tweened 0 → 1 across the plan's phases.
    bar: f64,
    set_reveal: AsyncSetState<bool>,
    set_screen: AsyncSetState<WinScreen>,
    set_step: AsyncSetState<Nav>,
    set_install: AsyncSetState<InstallPhase>,
    set_uninstall: AsyncSetState<bool>,
    set_log: AsyncSetState<Vec<LogLine>>,
}

pub struct WizardRoot {
    preset: WinPreset,
    initial: WinScreen,
    seams: Seams,
}

impl WizardRoot {
    pub fn new(preset: WinPreset, seams: Seams) -> WizardRoot {
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
            seams,
        }
    }
}

impl Component for WizardRoot {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (screen, set_screen) = cx.use_async_state(self.initial.clone());
        let start = Nav {
            step: WizStep::Welcome,
            forward: true,
        };
        let (nav, set_step) = cx.use_async_state(start);
        let (install, set_install) = cx.use_async_state(InstallPhase::Idle);
        let (uninstall, set_uninstall) = cx.use_async_state(self.preset.uninstall);
        let (log, set_log) = cx.use_async_state(Vec::<LogLine>::new());
        let (reveal, set_reveal) = cx.use_async_state(false);

        // The slide is a manual tween (the client shell's): reactor's one-shot animations run
        // from the visual's CURRENT value, and a freshly mounted page has nothing to fade from.
        // A worker steps 0 → 1 after each navigation; the generation guard stops a superseded
        // one so rapid clicks never fight. A value for another `Nav` reads as 0: no flash.
        let generation = cx.use_ref(Arc::new(AtomicU64::new(0)));
        let (anim, set_anim) = cx.use_async_state((start, 0.0f64));
        cx.use_effect(nav, {
            let (set_anim, generation) = (set_anim.clone(), generation.borrow().clone());
            move || {
                let mine = generation.fetch_add(1, SeqCst) + 1;
                if !animations_enabled() {
                    set_anim.call((nav, 1.0));
                    return;
                }
                std::thread::spawn(move || {
                    for i in 0..=SLIDE_FRAMES {
                        if generation.load(SeqCst) != mine {
                            return;
                        }
                        let p = f64::from(i) / f64::from(SLIDE_FRAMES);
                        set_anim.call((nav, 1.0 - (1.0 - p).powi(3))); // ease-out cubic
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }
        });
        let progress = if anim.0 == nav { anim.1 } else { 0.0 };

        // The install bar: one phase heading per plan phase reaches the log, so the bar's
        // target is the phase count over the plan's; the same worker-tween eases it there.
        let phases_seen = log.iter().filter(|l| l.kind == LineKind::Phase).count();
        let total = plan::build(
            &screen.facts,
            &screen.effective_choices(),
            self.preset.artifact,
            uninstall,
        )
        .phases
        .len()
        .max(1);
        let target = match install {
            InstallPhase::Idle => 0.0,
            InstallPhase::Finished => 1.0,
            _ => (phases_seen as f64 - 0.5).max(0.0) / total as f64,
        };
        let bar_generation = cx.use_ref(Arc::new(AtomicU64::new(0)));
        let (bar, set_bar) = cx.use_async_state(0.0f64);
        cx.use_effect((phases_seen, install == InstallPhase::Finished, total), {
            let (set_bar, generation, from) =
                (set_bar.clone(), bar_generation.borrow().clone(), bar);
            move || {
                let mine = generation.fetch_add(1, SeqCst) + 1;
                if !animations_enabled() {
                    set_bar.call(target);
                    return;
                }
                std::thread::spawn(move || {
                    for i in 0..=SLIDE_FRAMES {
                        if generation.load(SeqCst) != mine {
                            return;
                        }
                        let p = f64::from(i) / f64::from(SLIDE_FRAMES);
                        let eased = 1.0 - (1.0 - p).powi(3);
                        set_bar.call(from + (target - from) * eased);
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }
        });

        let ctx = Ctx {
            preset: self.preset.clone(),
            screen: screen.clone(),
            install,
            uninstall,
            seams: self.seams.clone(),
            slide: Slide {
                progress,
                forward: nav.forward,
            },
            reveal,
            bar,
            set_reveal,
            set_screen,
            set_step,
            set_install,
            set_uninstall,
            set_log,
        };

        match nav.step {
            WizStep::Welcome => welcome_page(&ctx),
            WizStep::Configure => configure_page(&ctx),
            WizStep::Network => network_page(&ctx),
            WizStep::Install => install_page(&ctx, &log),
            WizStep::Done => done_page(&ctx),
        }
    }
}

/// This run's step list. An uninstall run has nothing to configure — Welcome states what is
/// about to happen and Install is the teardown.
fn run_steps(ctx: &Ctx) -> Vec<WizStep> {
    if ctx.uninstall {
        vec![WizStep::Welcome, WizStep::Install, WizStep::Done]
    } else {
        ctx.screen.steps()
    }
}

/// The Install step is the teardown on an uninstall run, and its dot says so.
fn step_title(step: WizStep, uninstall: bool) -> &'static str {
    match step {
        WizStep::Install if uninstall => "Uninstall",
        _ => step.title(),
    }
}

/// Continue: move to the step after `cur` on this run's real path.
fn advance(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(ctx);
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
    ctx.set_step.call(Nav {
        step: next,
        forward: true,
    });
}

fn back(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(ctx);
    if let Some(i) = steps.iter().position(|s| *s == cur)
        && i > 0
    {
        ctx.set_step.call(Nav {
            step: steps[i - 1],
            forward: false,
        });
    }
}

/// The demo's filesystem: everything the executor may write goes under the same per-process
/// sandbox root as `demo::sandbox_paths` — including the "install dir" the uninstall preset
/// removes, staged here with a marker so the removal has something honest to do.
pub(crate) fn stage_demo_tree() -> String {
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

/// The demo's seams — the same objects `silent` builds, owned for the thread's lifetime.
pub struct DemoSeams {
    pub run: WinDemoRunner,
    pub net: FakeNet,
    pub payload: FakePayload,
    pub paths: punktfunk_setup::seam::BasePaths,
    pub subst: Subst,
}

impl DemoSeams {
    pub fn new(preset: &WinPreset, latency_ms: u64) -> DemoSeams {
        let tmp = stage_demo_tree();
        DemoSeams {
            run: WinDemoRunner::new(latency_ms, None),
            net: FakeNet {
                networks: preset.facts.networks.clone(),
                ..FakeNet::default()
            },
            payload: FakePayload::default(),
            paths: punktfunk_setup::demo::sandbox_paths(),
            subst: Subst {
                version: concat!(env!("CARGO_PKG_VERSION"), "-demo").to_string(),
                staging: format!("{tmp}\\staging"),
                temp: tmp,
            },
        }
    }
}

/// The real box's seams (WP3.1): system runner + NLA probe, `%ProgramData%` paths, the
/// extracted tree as payload (a `FakePayload` when there is none — dry runs only).
pub struct RealSeams {
    pub run: SystemRunner,
    pub net: SystemNet,
    pub payload: Box<dyn PayloadSource>,
    pub paths: punktfunk_setup::seam::BasePaths,
    pub subst: Subst,
}

impl RealSeams {
    pub fn new(root: Option<&std::path::Path>, version: &str) -> RealSeams {
        RealSeams {
            run: SystemRunner::new(),
            net: SystemNet,
            payload: match root {
                Some(root) => Box::new(DirPayload {
                    root: root.to_path_buf(),
                }),
                None => Box::new(FakePayload::default()),
            },
            paths: base_paths(&Env::from_env()),
            subst: real::subst(root, version),
        }
    }
}

fn start_install(ctx: &Ctx) {
    let preset = ctx.preset.clone();
    let screen = ctx.screen.clone();
    let uninstall = ctx.uninstall;
    let seams = ctx.seams.clone();
    let set_install = ctx.set_install.clone();
    let set_step = ctx.set_step.clone();
    let set_log = ctx.set_log.clone();
    set_install.call(InstallPhase::Running);
    std::thread::spawn(move || {
        let choices = screen.effective_choices();
        let built = plan::build(&screen.facts, &choices, preset.artifact, uninstall);
        let ui = ChannelReporter::new(set_log);
        let (demo, real) = match &seams {
            Seams::Demo { latency_ms } => (Some(DemoSeams::new(&preset, *latency_ms)), None),
            Seams::Real { root, version } => (None, Some(RealSeams::new(root.as_deref(), version))),
        };
        let (run, net, payload, paths, subst): (
            &dyn CommandRunner,
            &dyn NetProbe,
            &dyn PayloadSource,
            &punktfunk_setup::seam::BasePaths,
            Subst,
        ) = match (&demo, &real) {
            (Some(d), _) => (&d.run, &d.net, &d.payload, &d.paths, d.subst.clone()),
            (_, Some(r)) => (
                &r.run,
                &r.net,
                r.payload.as_ref(),
                &r.paths,
                r.subst.clone(),
            ),
            (None, None) => unreachable!("one seam set per run"),
        };
        let exec = WinExecutor {
            run,
            net,
            payload,
            paths,
            ui: &ui,
            dry: false,
            silent: false,
            web_password: choices.web_password.clone(),
            subst,
        };
        match exec.execute(&built) {
            Ok(()) => {
                set_install.call(InstallPhase::Finished);
                set_step.call(Nav {
                    step: WizStep::Done,
                    forward: true,
                });
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
        .corner_radius(10.0)
        .padding(edges(18.0, 12.0, 18.0, 12.0))
}

/// The D9 stepper: dots joined by a line, filled through the current step in brand violet,
/// hollow ahead. Each step owns one equal column: a half-segment either side of its dot,
/// stopping at the dot's edge, so the line meets every dot and the first/last halves are
/// simply invisible. `steps` is this run's real path — no ghost Network dot.
fn stepper(steps: &[WizStep], pos: usize, uninstall: bool) -> Element {
    let n = steps.len();
    let half = |lit: bool, hidden: bool| {
        Shape::rectangle()
            .fill(if lit { brand::VIOLET } else { MUTED })
            .height(2.0)
            .opacity(if hidden { 0.0 } else { 1.0 })
            .vertical_alignment(VerticalAlignment::Center)
    };
    let cells: Vec<Element> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let dot = if i <= pos {
                Shape::ellipse().fill(brand::VIOLET)
            } else {
                Shape::ellipse().stroke(MUTED).stroke_thickness(1.5)
            };
            let line = grid((
                half(i > 0 && i <= pos, i == 0).grid_column(0),
                dot.width(DOT).height(DOT).grid_column(1),
                half(i < pos, i + 1 == n).grid_column(2),
            ))
            .columns([
                GridLength::Star(1.0),
                GridLength::Auto,
                GridLength::Star(1.0),
            ]);
            let label = text_block(step_title(*step, uninstall)).font_size(11.0);
            let label = if i == pos {
                label.semibold()
            } else {
                label.foreground(ThemeRef::SecondaryText)
            };
            vstack((
                line,
                label.horizontal_alignment(HorizontalAlignment::Center),
            ))
            .spacing(4.0)
            .grid_column(i as i32)
            .into()
        })
        .collect();
    grid(cells)
        .columns(std::iter::repeat_n(GridLength::Star(1.0), n))
        .into()
}

/// The incoming page's offset at `progress`: forward starts to the right and travels left,
/// back the mirror. Opposite margins keep the width constant, so nothing reflows mid-tween.
pub fn slide_margin(forward: bool, progress: f64) -> Thickness {
    let off = (1.0 - progress) * SLIDE_TRAVEL;
    let (left, right) = if forward { (off, -off) } else { (-off, off) };
    edges(left, 0.0, right, 0.0)
}

/// Stepper · (title · content · button bar). The stepper stays put; the rest is the page
/// that slides in.
fn frame(ctx: &Ctx, step: WizStep, content: Element, buttons: Vec<Element>) -> Element {
    let steps = run_steps(ctx);
    let pos = steps.iter().position(|s| *s == step).unwrap_or(0);
    let head = stepper(&steps, pos, ctx.uninstall).margin(edges(0.0, 0.0, 0.0, 22.0));
    let title = text_block(step_title(step, ctx.uninstall))
        .font_size(26.0)
        .semibold()
        .margin(edges(0.0, 0.0, 0.0, 14.0));
    let bar = border(
        hstack(buttons)
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right),
    )
    .border_brush(ThemeRef::DividerStroke)
    .border_thickness(edges(0.0, 1.0, 0.0, 0.0))
    .padding(edges(0.0, 14.0, 0.0, 0.0))
    .margin(edges(0.0, 16.0, 0.0, 0.0));
    let page = grid((title.grid_row(0), content.grid_row(1), bar.grid_row(2))).rows([
        GridLength::Auto,
        GridLength::Star(1.0),
        GridLength::Auto,
    ]);
    let slide = ctx.slide;
    let page = border(page)
        .opacity(slide.progress)
        .margin(slide_margin(slide.forward, slide.progress));
    grid((head.grid_row(0), page.grid_row(1)))
        .rows([GridLength::Auto, GridLength::Star(1.0)])
        .margin(edges(36.0, 26.0, 36.0, 24.0))
        .into()
}

fn continue_button(ctx: &Ctx, cur: WizStep, label: &str) -> Element {
    let ctx = ctx.clone();
    button(label)
        .accent()
        .min_width(124.0)
        .on_click(move || advance(&ctx, cur))
        .into()
}

/// Uninstall from Welcome: flips this run to the teardown path, then advances exactly like
/// Continue — the install thread reads the flag through the same `Ctx`.
fn uninstall_button(ctx: &Ctx) -> Element {
    let mut ctx = ctx.clone();
    ctx.uninstall = true;
    button("Uninstall")
        .on_click(move || {
            ctx.set_uninstall.call(true);
            advance(&ctx, WizStep::Welcome);
        })
        .into()
}

fn back_button(ctx: &Ctx, cur: WizStep) -> Element {
    let ctx = ctx.clone();
    button("Back")
        .min_width(92.0)
        .on_click(move || back(&ctx, cur))
        .into()
}

fn welcome_page(ctx: &Ctx) -> Element {
    let what = match ctx.preset.artifact {
        Artifact::Host => "host",
        Artifact::Client => "client",
    };
    // Manage mode (D9): an installed box re-titles Welcome. The uninstaller exe (D6) offers
    // the teardown only; the installer offers Reconfigure — the upgrade path — or Uninstall.
    let wordmark = match &ctx.screen.facts.installed {
        Some(inst) => match &inst.version {
            Some(v) => format!("punktfunk {v} · {what} installed"),
            None => format!("punktfunk · {what} installed"),
        },
        None => "punktfunk".to_string(),
    };
    let (sentence, buttons): (&str, Vec<Element>) = if ctx.preset.uninstall {
        (
            "This removes punktfunk from this PC. Identity, pairings and passwords stay — a reinstall picks them up.",
            vec![uninstall_button(ctx)],
        )
    } else if ctx.screen.facts.installed.is_some() {
        (
            "Reconfigure keeps what is on this PC and applies your changes. Uninstall removes it — identity, pairings and passwords stay.",
            vec![
                uninstall_button(ctx),
                continue_button(ctx, WizStep::Welcome, "Reconfigure"),
            ],
        )
    } else {
        (
            match ctx.preset.artifact {
                Artifact::Host => {
                    "This installs the punktfunk host — it streams this PC's screen, audio and games to your devices."
                }
                Artifact::Client => {
                    "This installs the punktfunk client — it plays streams from a punktfunk host."
                }
            },
            vec![continue_button(ctx, WizStep::Welcome, "Continue")],
        )
    };
    let mut children: Vec<Element> = Vec::new();
    if let Some(uri) = brand::mark_uri() {
        children.push(
            Image::new_with_uri(uri)
                .stretch(Stretch::Uniform)
                .width(120.0)
                .height(120.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    children.push(
        text_block(wordmark)
            .font_size(34.0)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    );
    children.push(
        text_block(sentence)
            .wrap()
            .font_size(15.0)
            .foreground(ThemeRef::SecondaryText)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    );
    let content = vstack(children)
        .spacing(18.0)
        .max_width(500.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    frame(ctx, WizStep::Welcome, content, buttons)
}

fn configure_page(ctx: &Ctx) -> Element {
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
    let content = scroll_view(vstack(items).spacing(8.0).max_width(760.0)).into();
    // The go button says what Continue will do: install now, or one more step first.
    let steps = run_steps(ctx);
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
        ctx,
        WizStep::Configure,
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
            // WinUI's own eye only appears while the user types, so a pre-generated password
            // would never be revealable — this toggle drives the reveal mode instead.
            let reveal = ctx.reveal;
            let edit = ctx.clone();
            let toggle = ctx.clone();
            let regen = ctx.clone();
            hstack((
                PasswordBox::new()
                    .value(value.unwrap_or_default())
                    .password_reveal_mode(if reveal {
                        PasswordRevealMode::Visible
                    } else {
                        PasswordRevealMode::Hidden
                    })
                    .reveal_button_enabled(false)
                    .font_family(MONO)
                    .min_width(280.0)
                    .on_password_changed(move |pw: String| {
                        let mut s = edit.screen.clone();
                        s.set_password(pw);
                        edit.set_screen.call(s);
                    }),
                button(if reveal { "Hide" } else { "Show" })
                    .min_width(72.0)
                    .on_click(move || toggle.set_reveal.call(!reveal)),
                button("Regenerate").subtle().on_click(move || {
                    if let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12) {
                        let mut s = regen.screen.clone();
                        s.set_password(pw);
                        regen.set_screen.call(s);
                    }
                }),
            ))
            .spacing(6.0)
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

fn network_page(ctx: &Ctx) -> Element {
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
        radio,
    ))
    .spacing(12.0)
    .max_width(640.0)
    .into();
    frame(
        ctx,
        WizStep::Network,
        content,
        vec![
            back_button(ctx, WizStep::Network),
            continue_button(ctx, WizStep::Network, "Install"),
        ],
    )
}

fn install_page(ctx: &Ctx, log: &[LogLine]) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for line in log {
        items.push(match line.kind {
            LineKind::Phase => text_block(line.text.as_str())
                .semibold()
                .font_size(13.5)
                .margin(edges(0.0, 10.0, 0.0, 2.0))
                .into(),
            LineKind::Cmd => text_block(format!("  + {}", line.text))
                .font_size(12.0)
                .font_family(MONO)
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
    let status = match (ctx.install, ctx.uninstall) {
        (InstallPhase::Failed, _) => "Something went wrong — the log below has the step.",
        (_, true) => "Removing punktfunk…",
        (_, false) => "Installing punktfunk…",
    };
    let mut head: Vec<Element> = Vec::new();
    if ctx.install == InstallPhase::Running {
        head.push(
            ProgressRing::indeterminate()
                .width(16.0)
                .height(16.0)
                .into(),
        );
    }
    head.push(
        text_block(status)
            .foreground(if ctx.install == InstallPhase::Failed {
                ThemeRef::SystemCritical
            } else {
                ThemeRef::SecondaryText
            })
            .into(),
    );
    children.push(hstack(head).spacing(8.0).into());
    // Range 0–100 is the widget's default; the value tweens in root state.
    children.push(ProgressBar::new(ctx.bar * 100.0).into());
    children.push(
        card(scroll_view(vstack(items).spacing(2.0)))
            .padding(edges(16.0, 10.0, 16.0, 12.0))
            .into(),
    );
    let content = vstack(children).spacing(12.0).into();
    let buttons = if ctx.install == InstallPhase::Failed {
        vec![button("Close").on_click(|| std::process::exit(1)).into()]
    } else {
        vec![] // no cancel mid-plan: the executor's steps are not interruptible-safe
    };
    frame(ctx, WizStep::Install, content, buttons)
}

fn done_page(ctx: &Ctx) -> Element {
    let mut children: Vec<Element> = Vec::new();
    if ctx.uninstall {
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
                        text_block(pw.clone())
                            .font_size(22.0)
                            .font_family(MONO)
                            .selectable(),
                        text_block(
                            r"Generated for you at install. Also stored (ACL'd) in %ProgramData%\punktfunk\web-password — change it any time from the console.",
                        )
                        .wrap()
                        .font_size(12.0)
                        .foreground(ThemeRef::SecondaryText),
                    ))
                    .spacing(6.0),
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
    let content = scroll_view(vstack(children).spacing(10.0).max_width(760.0)).into();
    frame(
        ctx,
        WizStep::Done,
        content,
        vec![button("Finish")
            .accent()
            .on_click(|| std::process::exit(0))
            .into()],
    )
}

/// The real window over a preset (canned or probed) and the seams its install runs on.
pub fn run(preset: WinPreset, seams: Seams) -> windows_reactor::Result<()> {
    brand::install();
    let root = WizardRoot::new(preset, seams);
    // Self-contained: the runtime DLLs sit beside the exe (build.rs), so there is
    // deliberately NO windows_reactor::bootstrap() call — that is the framework path (S1).
    App::new()
        .title("Punktfunk Setup")
        .inner_size(980.0, 700.0)
        .backdrop(Backdrop::Mica)
        .render(move |cx| root.render(&(), cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_enters_from_the_right_and_back_from_the_left() {
        let fwd = slide_margin(true, 0.0);
        assert!(fwd.left > 0.0 && fwd.right == -fwd.left);
        let back = slide_margin(false, 0.0);
        assert!(back.left < 0.0 && back.right == -back.left);
        let settled = slide_margin(true, 1.0);
        assert_eq!((settled.left, settled.right), (0.0, 0.0));
    }
}
