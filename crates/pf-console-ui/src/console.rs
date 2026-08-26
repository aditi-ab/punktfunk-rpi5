//! The portable console driver — the one object a HOST holds (design
//! android-skia-console-port.md, WP2). It owns the shell and its fonts and speaks the
//! host-facing vocabulary only: a canvas + [`Viewport`] per frame, [`MenuEvent`]s,
//! [`PointerInput`], [`Key`]s and text in; [`OverlayAction`]s out; [`SessionPhase`] edges
//! back. The Vulkan session's [`crate::SkiaOverlay`] and the Android client's GL host both
//! sit on this; nothing in here knows what a `VkImage`, an SDL event or a JNI env is.

use crate::model::{ConsoleBus, ConsoleShared, HostRow};
use crate::screens::Screen;
use crate::shell::{ConsoleOptions, Shell};
use crate::theme::Fonts;
use anyhow::Result;
use pf_client_core::console::{OverlayAction, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use punktfunk_core::config::GamepadPref;
use skia_safe::Canvas;

pub use crate::input::Key;

/// What produced a menu event — the device family the hint legend should speak in.
/// Pointer input carries no source on purpose: a tap says nothing about which buttons
/// the user's OTHER hand holds, so it leaves the legend as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// A gamepad (the glyphs follow the active pad's family).
    Pad,
    /// Keys: a TV remote's D-pad on Android, a keyboard on the desktop.
    Keys,
}

/// Where the console starts.
pub enum ConsoleEntry {
    /// The host list (the session binary's bare `--browse`; the Android console's Home).
    Home,
    /// Home with this host's library already pushed (`--browse host` — the Decky
    /// per-host launch; Android's "come back to the shelf you launched from"; B backs
    /// out to Home). Boxed: `HostRow` outgrew the dataless `Home` variant when it learned
    /// its profile chips.
    Library(Box<HostRow>),
}

/// The host's ends of the console: models to write, commands to serve. Built by the host
/// BEFORE the console (so the host can keep them on one thread and build the console — which
/// holds Skia handles and is not `Send` — on another); every field is `Clone` + thread-safe.
#[derive(Clone, Default)]
pub struct ConsoleHandles {
    pub console: ConsoleShared,
    pub library: crate::library::LibraryShared,
    pub bus: ConsoleBus,
}

impl ConsoleHandles {
    pub fn new() -> ConsoleHandles {
        ConsoleHandles::default()
    }
}

/// Safe-area insets, device pixels — what the console must keep its CHROME out of. A
/// phone's display cutout in landscape is a left or right inset; a TV's overscan margin is
/// all four. The backdrop still paints edge to edge; the title, legend, controller chip
/// and screen content sit inside. Zero on the desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// The surface the console draws into this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub insets: Insets,
    /// Device pixels per design unit. `None` = the couch formula the shell has always used,
    /// `(height / 800).clamp(0.75, 3.0)` — a Deck is 1×, a 4K TV 2.7×. A host with a better
    /// idea of viewing distance (a phone in the hand wants a density floor) says so here.
    pub scale: Option<f64>,
}

impl Viewport {
    /// A plain full-surface viewport — no insets, the default scale. What the desktop
    /// passes, and what a test means by "render at w×h".
    pub fn plain(width: u32, height: u32) -> Viewport {
        Viewport {
            width,
            height,
            insets: Insets::default(),
            scale: None,
        }
    }
}

/// The console, host-facing. See the module doc.
pub struct Console {
    shell: Shell,
    fonts: Fonts,
}

impl Console {
    /// Build the console at `entry` over the host's `handles`. Not `Send` (Skia handles
    /// inside): build it on the thread that will draw it.
    pub fn new(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
        handles: &ConsoleHandles,
    ) -> Result<Console> {
        let stack = entry_stack(entry, &handles.library);
        let shell = Shell::new(
            handles.console.clone(),
            handles.library.clone(),
            handles.bus.clone(),
            opts,
            stack,
        )?;
        let fonts = crate::theme::build_fonts()?;
        Ok(Console { shell, fonts })
    }

    /// Draw one frame. `pad` is the controller chip's text (`None` = "no controller"),
    /// `pad_pref` picks the button-glyph style, `pads` feeds the chip's battery and the
    /// settings' controller rows.
    pub fn frame(
        &mut self,
        canvas: &Canvas,
        viewport: &Viewport,
        pad: Option<&str>,
        pad_pref: Option<GamepadPref>,
        pads: &[PadInfo],
    ) {
        self.shell
            .render_in(canvas, viewport, &self.fonts, pad, pad_pref, pads);
    }

    /// A menu event, with WHERE it came from — a controller, or keys (a TV remote's
    /// D-pad, a keyboard). The source is what keeps the hint legend speaking the language
    /// of the device actually in the user's hand; the pulse, if any, is what a pad should
    /// feel.
    pub fn menu(&mut self, event: MenuEvent, source: InputSource) -> Option<MenuPulse> {
        self.shell.note_input_source(source);
        self.shell.handle_menu(event)
    }

    /// Pointer input (mouse, touch, a TV remote's pointer). Coordinates are surface pixels
    /// — the shell subtracts the viewport's insets itself. Returns whether it was consumed.
    pub fn pointer(&mut self, input: PointerInput) -> bool {
        self.shell.pointer_input(input)
    }

    /// A key the console understands (see [`Key`]). Returns whether it was consumed.
    pub fn key(&mut self, key: Key, shift: bool, repeat: bool) -> bool {
        self.shell.key(key, shift, repeat)
    }

    /// Typed text, while [`Self::editing`] — the host's text-input machinery feeds this.
    pub fn text(&mut self, text: &str) {
        self.shell.text_input(text);
    }

    /// A text field is being edited: the host should keep its text input (SDL text input,
    /// the IME) started, and route printable keys as text rather than as [`Key`]s.
    pub fn editing(&self) -> bool {
        self.shell.editing()
    }

    /// The host reports where the session it was asked to start now stands.
    pub fn session_phase(&mut self, phase: SessionPhase) {
        self.shell.session_phase(phase);
    }

    /// The next action the host must act on, if any (starting/canceling a session,
    /// quitting, copying text). Drain after every input and every frame.
    pub fn take_action(&mut self) -> Option<OverlayAction> {
        self.shell.take_action()
    }

    /// True while a session is streaming — the host is presenting video and the console
    /// is off screen (the shell keeps its stack; the shelf is where it was on return).
    pub fn in_stream(&self) -> bool {
        self.shell.in_stream
    }

    /// Re-root the console at `entry` — a deep link into a host's library, or a "back to
    /// the shelf you launched from" after a game exits. The current stack is replaced.
    pub fn navigate(&mut self, entry: ConsoleEntry) {
        let stack = entry_stack(entry, self.shell.library());
        self.shell.replace_stack(stack);
    }

    /// Skia's resource-cache budget for the host's `DirectContext` (see
    /// [`ConsoleOptions::gpu_cache_bytes`]) — the host sets it, the shell only carries it.
    pub fn gpu_cache_bytes(&self) -> usize {
        self.shell.gpu_cache_bytes
    }

    /// Split into the shell and its fonts — for the Vulkan overlay, which draws stream
    /// chrome with the same fonts and holds the shell as an `Option`.
    #[cfg(feature = "vulkan-overlay")]
    pub(crate) fn into_parts(self) -> (Shell, Fonts) {
        (self.shell, self.fonts)
    }
}

/// The screen stack an entry point means.
fn entry_stack(entry: ConsoleEntry, library: &crate::library::LibraryShared) -> Vec<Screen> {
    match entry {
        ConsoleEntry::Home => vec![Screen::Home(crate::screens::home::HomeScreen::new())],
        ConsoleEntry::Library(host) => vec![
            Screen::Home(crate::screens::home::HomeScreen::new()),
            // The library screen records the model's CURRENT fetch epoch, so the entry's
            // own `FetchLibrary` (queued by the host right after this) is the first to
            // raise it — that is how the shelf knows a result is its own.
            Screen::Library(crate::screens::library::LibraryScreen::new(
                &host,
                library.fetch_epoch(),
            )),
        ],
    }
}
