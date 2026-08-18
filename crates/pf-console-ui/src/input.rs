//! The console's own keyboard vocabulary — the dozen keys the shell acts on, named without
//! reference to any windowing toolkit. The Vulkan session's overlay maps SDL scancodes onto
//! it; the Android host maps `KeyEvent` codes; the tests build it directly. Text itself
//! (typed characters) never travels here — that is [`crate::shell::Shell::text_input`]'s
//! job, fed by whatever text-input machinery the host has (SDL text input, an IME).

/// A physical key the console reacts to. Everything not listed is not the console's
/// business and a host should not bother forwarding it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    /// Enter and the keypad Enter — one key to the console.
    Return,
    Space,
    Escape,
    Backspace,
    PageUp,
    PageDown,
    Tab,
    /// The letter Y — the keyboard's stand-in for the pad's Secondary (Y) while not editing.
    Y,
    /// The letter X — the keyboard's stand-in for the pad's Tertiary (X) while not editing.
    X,
}
