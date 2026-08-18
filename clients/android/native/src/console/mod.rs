//! The Skia console UI on Android (design/android-skia-console-port.md, WP3): the same
//! `pf-console-ui` shell the Linux/Windows session binary shows, drawn by this crate onto
//! the `SurfaceView` Kotlin hands over, through Skia's GL backend on an EGL context this
//! module owns. Kotlin keeps the services (trust store, settings, discovery, the mTLS
//! library fetch, WoL, pairing) and feeds the console's models over these JNI seams; the
//! console's own asks — start a session, quit, copy text — come back as events.
//!
//! The seam in one breath: `nativeConsoleCreate` builds the console (a `jlong` handle),
//! the surface-lifecycle calls hand it somewhere to draw, `nativeConsoleSet*` /
//! `nativeConsoleLibrary*` push model snapshots (JSON, the model types' own serde shape),
//! `nativeConsoleMenu`/`PadSample`/`Pointer`/`Key`/`Text` are input,
//! `nativeConsoleNextEvent` is the blocking event poll (actions, haptic pulses, editing
//! state, settings to persist), `nativeConsoleDrainCmds` the command bus.
//!
//! JSON everywhere a struct crosses: `HostRow`, `PairPhase`, `WakeStatus`, `LibraryGame`,
//! `LibraryPhase`, `ConsoleCmd`, `OverlayAction`, `Settings`, `KnownHosts` all serialize with
//! serde in their defining crates — there is no second Android-side mirror to drift.

mod egl;
mod gpu;
mod host;

use host::{Cmd, ConsoleHost, Phase};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::EnvUnowned;
use pf_client_core::console::{PointerButton, PointerInput};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuSample, PadBattery, PadInfo};
use pf_console_ui::{
    ConsoleEntry, ConsoleOptions, HostRow, Insets, Key, LibraryGame, LibraryPhase, PairPhase,
    Platform, SnapshotStore, Stale, WakeStatus,
};
use punktfunk_core::config::GamepadPref;
use std::sync::Arc;
use std::time::Duration;

/// How long `nativeConsoleNextEvent` blocks at most — short enough that Kotlin's poll thread
/// notices `running = false` promptly on teardown (the rumble poll's cadence).
const EVENT_TIMEOUT: Duration = Duration::from_millis(100);

/// What Kotlin hands `nativeConsoleCreate`.
#[derive(serde::Deserialize)]
struct CreateOptions {
    device_name: String,
    /// Skia's resource budget, bytes (Kotlin sizes it from `ActivityManager.memoryClass`).
    gpu_cache_bytes: usize,
    /// The settings snapshot the shell starts from (`pf_client_core::trust::Settings` JSON).
    settings: pf_client_core::trust::Settings,
    /// The profile catalog as `[[id, name], …]`.
    #[serde(default)]
    profiles: Vec<(String, String)>,
    /// The known-hosts records (`KnownHosts` JSON) — for building `punktfunk://` links.
    #[serde(default)]
    known_hosts: pf_client_core::trust::KnownHosts,
    /// Where to start: `{"home": true}` or `{"library": <HostRow>}`.
    #[serde(default)]
    entry: EntryJson,
}

#[derive(serde::Deserialize, Default)]
struct EntryJson {
    #[serde(default)]
    library: Option<HostRow>,
}

impl EntryJson {
    fn into_entry(self) -> ConsoleEntry {
        match self.library {
            Some(h) => ConsoleEntry::Library(Box::new(h)),
            None => ConsoleEntry::Home,
        }
    }
}

/// One controller as Kotlin describes it — `PadInfo` with the pref as its wire byte.
#[derive(serde::Deserialize)]
struct PadJson {
    name: String,
    key: String,
    pref: u8,
    #[serde(default)]
    steam_virtual: bool,
    #[serde(default)]
    battery: Option<BatteryJson>,
}

#[derive(serde::Deserialize)]
struct BatteryJson {
    percent: u8,
    charging: bool,
}

#[derive(serde::Deserialize)]
struct PadsJson {
    #[serde(default)]
    label: Option<String>,
    /// The glyph style's pref as its wire byte; absent = keyboard glyphs.
    #[serde(default)]
    pref: Option<u8>,
    #[serde(default)]
    pads: Vec<PadJson>,
}

/// The `jlong` handle → the host. Every entry point takes the handle Kotlin got from
/// `nativeConsoleCreate` and returns it to `nativeConsoleDestroy` exactly once, never
/// concurrently with the destroy (Kotlin owns that ordering on its main thread; the event
/// poll thread is stopped and joined before destroy — same contract as the rumble poll).
fn host(handle: jlong) -> Option<&'static ConsoleHost> {
    if handle == 0 {
        return None;
    }
    // SAFETY: live handle per the create/destroy contract above.
    Some(unsafe { &*(handle as *const ConsoleHost) })
}

fn json_arg<T: serde::de::DeserializeOwned>(env: &mut jni::Env, s: &JString) -> Option<T> {
    let text = s.try_to_string(env).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            log::error!("console: bad JSON from Kotlin: {e} in {text:.200}");
            None
        }
    }
}

/// `NativeBridge.nativeConsoleCreate(optionsJson): Long` — build the console (shell + fonts on
/// the caller's thread, then its render thread parked until a surface arrives). `0` on failure
/// (logged); Kotlin then keeps its own console.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleCreate(
    mut env: EnvUnowned,
    _this: JObject,
    options: JString,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let Some(opts) = json_arg::<CreateOptions>(env, &options) else {
            return Ok(0);
        };
        let store = Arc::new(SnapshotStore::new(opts.settings, opts.profiles));
        store.set_known_hosts(opts.known_hosts);
        let console_opts = ConsoleOptions {
            device_name: opts.device_name,
            deck: false,
            store: Some(store.clone()),
            platform: Platform::Android,
            gpu_cache_bytes: opts.gpu_cache_bytes.max(16 << 20),
        };
        let host = ConsoleHost::start(console_opts, opts.entry.into_entry(), store);
        Ok(Box::into_raw(Box::new(host)) as jlong)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleDestroy(handle)` — stop the render thread (joined) and free.
///
/// # Safety contract
/// `handle` must be `0` or a live handle, destroyed once, after the event poll thread stopped.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleDestroy(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the contract; ownership returns here exactly once.
    let host = unsafe { Box::from_raw(handle as *mut ConsoleHost) };
    host.stop();
}

/// `NativeBridge.nativeConsoleSurfaceCreated(handle, surface)` — the `SurfaceView`'s surface is
/// up; the render thread wraps it in EGL and starts drawing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceCreated(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    surface: JObject,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        // SAFETY: `env`/`surface` are valid JNI pointers for this call; the raw casts bridge the
        // jni-sys version skew between the `jni` and vendored `ndk` crates (see nativeStartVideo).
        let window = unsafe {
            ndk::native_window::NativeWindow::from_surface(
                env.get_raw() as *mut _,
                surface.as_raw() as *mut _,
            )
        };
        match window {
            Some(w) => h.shared.send(Cmd::SurfaceCreated(w)),
            None => log::error!("console: no ANativeWindow from Surface"),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSurfaceChanged(handle)` — size changed; the thread re-reads it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceChanged(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    if let Some(h) = host(handle) {
        h.shared.send(Cmd::SurfaceChanged);
    }
}

/// `NativeBridge.nativeConsoleSurfaceDestroyed(handle)` — BLOCKS until the render thread has
/// released the EGL surface: Android forbids touching a `Surface` after `surfaceDestroyed`
/// returns, and the GL driver would otherwise still be presenting into it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceDestroyed(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    if let Some(h) = host(handle) {
        h.shared.destroy_surface_blocking();
    }
}

/// `NativeBridge.nativeConsoleSetViewport(handle, left, top, right, bottom, scale)` — safe-area
/// insets in surface pixels and the design-unit scale (`0` = the shell's own couch formula).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetViewport(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    left: jfloat,
    top: jfloat,
    right: jfloat,
    bottom: jfloat,
    scale: jfloat,
) {
    if let Some(h) = host(handle) {
        h.shared.send(Cmd::Viewport {
            insets: Insets {
                left: left.max(0.0),
                top: top.max(0.0),
                right: right.max(0.0),
                bottom: bottom.max(0.0),
            },
            scale: (scale > 0.0).then_some(f64::from(scale)),
        });
    }
}

/// `NativeBridge.nativeConsolePadSample(handle, buttons, lx, ly, dpad)` — the raw pad, whenever
/// it changes: `buttons` bit i = a, b, x, y, l1, r1 held; `lx`/`ly` the left stick in wire
/// units (±32767, +y = down); `dpad` bit i = up, down, left, right held. The shared
/// `MenuNav` turns it into menu events with the same dead zone, repeat cadence and hysteresis
/// as the desktop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsolePadSample(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    buttons: jint,
    lx: jint,
    ly: jint,
    dpad: jint,
) {
    if let Some(h) = host(handle) {
        let bit = |v: jint, i: u32| v & (1 << i) != 0;
        h.shared.send(Cmd::PadSample(MenuSample {
            buttons: [
                bit(buttons, 0),
                bit(buttons, 1),
                bit(buttons, 2),
                bit(buttons, 3),
                bit(buttons, 4),
                bit(buttons, 5),
            ],
            lx: lx.clamp(-32767, 32767) as i16,
            ly: ly.clamp(-32767, 32767) as i16,
            dpad: [bit(dpad, 0), bit(dpad, 1), bit(dpad, 2), bit(dpad, 3)],
        }));
    }
}

/// `NativeBridge.nativeConsoleMenu(handle, event)` — a discrete menu event, for input that is
/// already an event on the Kotlin side (a TV remote's D-pad `KeyEvent`s, the touch escape hatch):
/// 0..3 = move up/down/left/right, 4 confirm, 5 back, 6 secondary (Y), 7 tertiary (X),
/// 8 jump back (L1), 9 jump forward (R1).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleMenu(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    event: jint,
) {
    let ev = match event {
        0 => MenuEvent::Move(MenuDir::Up),
        1 => MenuEvent::Move(MenuDir::Down),
        2 => MenuEvent::Move(MenuDir::Left),
        3 => MenuEvent::Move(MenuDir::Right),
        4 => MenuEvent::Confirm,
        5 => MenuEvent::Back,
        6 => MenuEvent::Secondary,
        7 => MenuEvent::Tertiary,
        8 => MenuEvent::JumpBack,
        9 => MenuEvent::JumpForward,
        _ => return,
    };
    if let Some(h) = host(handle) {
        h.shared.send(Cmd::Menu(ev));
    }
}

/// `NativeBridge.nativeConsolePointer(handle, kind, x, y, dy)` — touch/mouse in surface pixels:
/// kind 0 move, 1 primary down, 2 primary up, 3 secondary down (= Back), 4 wheel (`dy` steps,
/// + = up), 5 cancel.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsolePointer(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    kind: jint,
    x: jfloat,
    y: jfloat,
    dy: jfloat,
) {
    let input = match kind {
        0 => PointerInput::Move { x, y },
        1 => PointerInput::Down {
            x,
            y,
            button: PointerButton::Primary,
        },
        2 => PointerInput::Up {
            x,
            y,
            button: PointerButton::Primary,
        },
        3 => PointerInput::Down {
            x,
            y,
            button: PointerButton::Secondary,
        },
        4 => PointerInput::Wheel { x, y, dy },
        5 => PointerInput::Cancel,
        _ => return,
    };
    if let Some(h) = host(handle) {
        h.shared.send(Cmd::Pointer(input));
    }
}

/// `NativeBridge.nativeConsoleKey(handle, key, shift, repeat)` — a hardware key the console
/// understands: 0..3 left/right/up/down, 4 return, 5 space, 6 escape, 7 backspace, 8 page up,
/// 9 page down, 10 tab, 11 the letter Y, 12 the letter X. Anything else is Kotlin's to keep.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleKey(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    key: jint,
    shift: jboolean,
    repeat: jboolean,
) {
    let key = match key {
        0 => Key::Left,
        1 => Key::Right,
        2 => Key::Up,
        3 => Key::Down,
        4 => Key::Return,
        5 => Key::Space,
        6 => Key::Escape,
        7 => Key::Backspace,
        8 => Key::PageUp,
        9 => Key::PageDown,
        10 => Key::Tab,
        11 => Key::Y,
        12 => Key::X,
        _ => return,
    };
    if let Some(h) = host(handle) {
        h.shared.send(Cmd::Key { key, shift, repeat });
    }
}

/// `NativeBridge.nativeConsoleText(handle, text)` — typed characters while the console reports
/// `editing` (see the `editing` event).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleText(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(t)) = (host(handle), text.try_to_string(env)) {
            h.shared.send(Cmd::Text(t));
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSessionPhase(handle, phase, message)` — where the session the
/// console asked for stands: 0 connecting, 1 streaming, 2 failed(message), 3 ended(message or
/// empty = clean), 4 reconnecting(message).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSessionPhase(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    phase: jint,
    message: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        let msg = message.try_to_string(env).unwrap_or_default();
        let ph = match phase {
            0 => Phase::Connecting,
            1 => Phase::Streaming,
            2 => Phase::Failed(msg),
            3 => Phase::Ended((!msg.is_empty()).then_some(msg)),
            4 => Phase::Reconnecting(msg),
            _ => return Ok(()),
        };
        h.shared.send(Cmd::Phase(ph));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleNavigate(handle, entryJson)` — re-root the console (`{"library":
/// <HostRow>}` opens that host's shelf over Home; `{}` is Home).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNavigate(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    entry: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(e)) = (host(handle), json_arg::<EntryJson>(env, &entry)) {
            h.shared.send(Cmd::Navigate(e.into_entry()));
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetPads(handle, padsJson)` — the connected controllers for the
/// chip + settings rows: `{"label": "DualSense", "pref": 1, "pads": [{name, key, pref,
/// steam_virtual, battery: {percent, charging} | null}]}`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPads(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pads: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let (Some(h), Some(p)) = (host(handle), json_arg::<PadsJson>(env, &pads)) else {
            return Ok(());
        };
        let pads = p
            .pads
            .into_iter()
            .map(|j| PadInfo {
                name: j.name,
                key: j.key,
                pref: GamepadPref::from_u8(j.pref),
                steam_virtual: j.steam_virtual,
                battery: j.battery.map(|b| PadBattery {
                    percent: b.percent.min(100),
                    charging: b.charging,
                }),
            })
            .collect();
        h.shared.send(Cmd::Pads {
            label: p.label,
            pref: p.pref.map(GamepadPref::from_u8),
            pads,
        });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleNextEvent(handle): String` — block up to ~100 ms for the next
/// event: `{"action": <OverlayAction>}`, `{"pulse": "move"|"confirm"|"boundary"}`,
/// `{"editing": bool}`, `{"settings": <Settings>}` (persist it), `{"gles": 2|3}`,
/// `{"dead": "<why>"}`. Empty string on timeout / no handle. Run from a Kotlin poll thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNextEvent<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = match host(handle).and_then(|h| h.shared.next_event(EVENT_TIMEOUT)) {
            Some(ev) => ev.to_json(),
            None => String::new(),
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleDrainCmds(handle): String` — every `ConsoleCmd` queued since the
/// last call, as a JSON array (`[]` when none). Poll on a short cadence from the service side.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleDrainCmds<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = match host(handle) {
            Some(h) => {
                let cmds = h.handles.bus.drain();
                serde_json::to_string(&cmds).unwrap_or_else(|_| "[]".into())
            }
            None => "[]".into(),
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

// ---- model pushers -----------------------------------------------------------------------

/// `NativeBridge.nativeConsoleSetHosts(handle, json)` — the home carousel's rows (`[HostRow]`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetHosts(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(rows)) = (host(handle), json_arg::<Vec<HostRow>>(env, &json)) {
            h.handles.console.set_hosts(rows);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetPair(handle, json)` — the pairing ceremony's phase
/// (`"Idle"`, `"Busy"`, `{"Failed": "why"}`, `{"Paired": {"key": "…"}}`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPair(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(p)) = (host(handle), json_arg::<PairPhase>(env, &json)) {
            h.handles.console.set_pair(p);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetWake(handle, json)` — the wake-and-wait card's status
/// (`WakeStatus` JSON, or `null` to clear).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetWake(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(w)) = (host(handle), json_arg::<Option<WakeStatus>>(env, &json)) {
            h.handles.console.set_wake(w);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleNotice(handle, text)` — a one-shot toast from a service worker.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNotice(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(t)) = (host(handle), text.try_to_string(env)) {
            h.handles.console.set_notice(t);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryBegin(handle)` — a fetch is starting for the shelf on
/// screen: bumps the fetch epoch and sets `Loading`. Call this — not a bare `Loading` phase —
/// so the shelf can tell its own result from a previous host's cached one.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryBegin(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    if let Some(h) = host(handle) {
        h.handles.library.begin_fetch();
    }
}

/// `NativeBridge.nativeConsoleLibraryPhase(handle, json)` — `"Loading"`, `"Empty"`, `"Ready"`,
/// or `{"Error": {"title", "body", "can_retry"}}`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryPhase(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(p)) = (host(handle), json_arg::<LibraryPhase>(env, &json)) {
            h.handles.library.set_phase(p);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryGames(handle, json, cached)` — the catalog (`[LibraryGame]`);
/// `cached` = this is the last-known list from the cache, shown while the fetch runs.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryGames(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
    cached: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(games)) = (host(handle), json_arg::<Vec<LibraryGame>>(env, &json)) {
            if cached {
                h.handles.library.set_games_cached(games);
            } else {
                h.handles.library.set_games(games);
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryArt(handle, id, bytes)` — one title's poster, encoded
/// (JPEG/PNG); the shell decodes at the size it draws.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryArt(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    id: JString,
    bytes: JByteArray,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        let id = id.try_to_string(env)?;
        let bytes = env.convert_byte_array(&bytes)?;
        h.handles.library.push_art(id, bytes);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryRunning(handle, json)` — the ids the host has up
/// (`["steam:570", …]`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryRunning(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(ids)) = (host(handle), json_arg::<Vec<String>>(env, &json)) {
            let up: std::collections::HashSet<String> = ids.into_iter().collect();
            h.handles.library.set_running(&up);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryStale(handle, stale)` — 0 fresh, 1 waking, 2 offline.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryStale(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    stale: jint,
) {
    if let Some(h) = host(handle) {
        h.handles.library.set_stale(match stale {
            1 => Stale::Waking,
            2 => Stale::Offline,
            _ => Stale::No,
        });
    }
}

/// `NativeBridge.nativeConsoleSetSettings(handle, json)` — a settings change made elsewhere
/// (the touch UI, a deep link): the shell reads this on its next mutation. Not a save.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetSettings(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(s)) = (
            host(handle),
            json_arg::<pf_client_core::trust::Settings>(env, &json),
        ) {
            h.store.set(s);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetProfiles(handle, json)` — the profile catalog `[[id, name]]`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetProfiles(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(p)) = (host(handle), json_arg::<Vec<(String, String)>>(env, &json)) {
            h.store.set_profiles(p);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetKnownHosts(handle, json)` — the known-hosts records
/// (`KnownHosts` JSON) the console builds `punktfunk://` links from.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetKnownHosts(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(k)) = (
            host(handle),
            json_arg::<pf_client_core::trust::KnownHosts>(env, &json),
        ) {
            h.store.set_known_hosts(k);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}
