//! Portable console driver: the object a host holds (`android-skia-console-port.md`).
//! Owns the shell and fonts; host-facing vocabulary only: a canvas + [`Viewport`]
//! per frame, [`MenuEvent`]s, [`PointerInput`], [`Key`]s and text in;
//! [`OverlayAction`]s out; [`SessionPhase`] edges back. The Vulkan session's
//! [`crate::SkiaOverlay`] and the Android GL host both sit on this; nothing here
//! knows a `VkImage`, an SDL event, or a JNI env.

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

/// Device family for the hint legend. Pointer has no source: a tap does not say
/// which buttons the other hand holds, so the legend stays as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// Glyphs follow the active pad's family.
    Pad,
    /// TV remote D-pad on Android; keyboard on desktop.
    Keys,
}

pub enum ConsoleEntry {
    /// Host list (`--browse`; Android Home).
    Home,
    /// Home with this host's library pushed (`--browse host`). B pops to Home.
    /// `Box` because `HostRow` is larger than the other variant.
    Library(Box<HostRow>),
}

/// Host-side models and the command bus. Built before [`Console`]: handles are `Clone` +
/// thread-safe so the host can keep them on one thread and build the (not `Send`) console
/// on the draw thread.
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

/// Safe-area insets in device pixels. Chrome stays inside; the backdrop still paints
/// edge to edge. Zero on desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub insets: Insets,
    /// Device pixels per design unit. `None` uses `(height / 800).clamp(0.75, 3.0)`
    /// (Deck 1×, 4K TV 2.7×). A phone in hand should pass a density floor.
    pub scale: Option<f64>,
}

impl Viewport {
    pub fn plain(width: u32, height: u32) -> Viewport {
        Viewport {
            width,
            height,
            insets: Insets::default(),
            scale: None,
        }
    }
}

pub struct Console {
    shell: Shell,
    fonts: Fonts,
}

impl Console {
    /// Not `Send` (Skia). Build on the thread that will draw.
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

    /// `pad` is the chip label; `None` means no controller.
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

    pub fn menu(&mut self, event: MenuEvent, source: InputSource) -> Option<MenuPulse> {
        self.shell.note_input_source(source);
        self.shell.handle_menu(event)
    }

    /// Pointer in surface pixels; the shell subtracts insets.
    pub fn pointer(&mut self, input: PointerInput) -> bool {
        self.shell.pointer_input(input)
    }

    pub fn key(&mut self, key: Key, shift: bool, repeat: bool) -> bool {
        self.shell.key(key, shift, repeat)
    }

    pub fn text(&mut self, text: &str) {
        self.shell.text_input(text);
    }

    /// True while a field is being edited: keep IME / SDL text-input started, and
    /// route printable keys as text, not [`Key`]s.
    pub fn editing(&self) -> bool {
        self.shell.editing()
    }

    pub fn session_phase(&mut self, phase: SessionPhase) {
        self.shell.session_phase(phase);
    }

    /// Drain after every input and every frame.
    pub fn take_action(&mut self) -> Option<OverlayAction> {
        self.shell.take_action()
    }

    /// Console is off screen; the shell keeps its stack for return.
    pub fn in_stream(&self) -> bool {
        self.shell.in_stream
    }

    /// Replace the stack with `entry` (deep link, or return to the shelf a game launched from).
    pub fn navigate(&mut self, entry: ConsoleEntry) {
        let stack = entry_stack(entry, self.shell.library());
        self.shell.replace_stack(stack);
    }

    /// Skia resource-cache budget for the host `DirectContext`. The shell only carries it.
    pub fn gpu_cache_bytes(&self) -> usize {
        self.shell.gpu_cache_bytes
    }

    /// Shell and fonts for the Vulkan overlay: stream chrome uses the same fonts; the
    /// overlay holds the shell as `Option`.
    #[cfg(feature = "vulkan-overlay")]
    pub(crate) fn into_parts(self) -> (Shell, Fonts) {
        (self.shell, self.fonts)
    }
}

fn entry_stack(entry: ConsoleEntry, library: &crate::library::LibraryShared) -> Vec<Screen> {
    match entry {
        ConsoleEntry::Home => vec![Screen::Home(crate::screens::home::HomeScreen::new())],
        ConsoleEntry::Library(host) => vec![
            Screen::Home(crate::screens::home::HomeScreen::new()),
            // Snapshot the model's fetch epoch so the host's following `FetchLibrary`
            // is the first raise; that is how the shelf knows the result is its own.
            Screen::Library(crate::screens::library::LibraryScreen::new(
                &host,
                library.fetch_epoch(),
            )),
        ],
    }
}
