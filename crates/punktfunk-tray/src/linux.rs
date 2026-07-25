//! Linux tray: a StatusNotifierItem (ksni/zbus) fed by the status poller. The host runs as the
//! systemd **user** unit `punktfunk-host.service`, so start/stop/restart are plain
//! `systemctl --user` calls — no polkit, no elevation. KDE (the project's primary Linux desktop)
//! renders SNI natively; GNOME needs the AppIndicator extension (without it the icon is invisible
//! — `--autostart` exits silently rather than erroring at every login).

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::status::{self, Poller, TrayStatus};

/// The tray's D-Bus/menu model. `status` + `web_console` are the mutable state; the poller
/// rewrites them via `Handle::update`, which re-emits the SNI properties (icon, tooltip, menu).
struct HostTray {
    status: TrayStatus,
    web_port: u16,
    /// The console answered the poller's live loopback probe — labels the (always present) "Open
    /// web console" entry; it never hides it.
    web_console: bool,
    /// Filled right after `spawn` (the poller needs the tray handle first) — lets menu actions
    /// force an immediate re-poll instead of waiting out the cadence.
    poller: Arc<OnceLock<Poller>>,
}

impl HostTray {
    fn systemctl(&self, verb: &str) {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", verb, status::UNIT_NAME])
            .status();
        if let Some(p) = self.poller.get() {
            p.poke();
        }
    }

    /// Open the web console at `path` ("" = dashboard). Deep links land the operator on the page
    /// the menu entry promised — the pairing queue, the virtual displays.
    fn open_console(&self, path: &str) {
        let url = format!("https://127.0.0.1:{}/{path}", self.web_port);
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

impl ksni::Tray for HostTray {
    fn id(&self) -> String {
        "punktfunk-tray".into()
    }

    fn title(&self) -> String {
        "punktfunk host".into()
    }

    fn status(&self) -> ksni::Status {
        match &self.status {
            TrayStatus::Error(_) => ksni::Status::NeedsAttention,
            s if s.pairing_attention() => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    /// Hicolor theme names (installed by the packages); `icon_pixmap` below is the fallback so a
    /// `cargo run` from the repo shows an icon too.
    fn icon_name(&self) -> String {
        match &self.status {
            TrayStatus::Running(_) if self.status.is_streaming() => {
                "punktfunk-tray-streaming".into()
            }
            TrayStatus::Running(_) => "punktfunk-tray".into(),
            TrayStatus::Starting | TrayStatus::Degraded => "punktfunk-tray-degraded".into(),
            TrayStatus::Error(_) => "punktfunk-tray-error".into(),
            TrayStatus::Stopped | TrayStatus::NotInstalled => "punktfunk-tray-stopped".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Same dot palette as scripts/gen-tray-icons.py.
        let rgb = match &self.status {
            TrayStatus::Running(_) if self.status.is_streaming() => (0xb4, 0x4c, 0xf0), // violet
            TrayStatus::Running(_) => (0x2e, 0xcc, 0x71),                               // green
            TrayStatus::Starting | TrayStatus::Degraded => (0xf0, 0xa0, 0x30),          // amber
            TrayStatus::Error(_) => (0xe7, 0x4c, 0x3c),                                 // red
            TrayStatus::Stopped | TrayStatus::NotInstalled => (0x8a, 0x8a, 0x8a),       // gray
        };
        vec![dot_icon(22, rgb), dot_icon(48, rgb)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.status.headline(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let running = matches!(
            self.status,
            TrayStatus::Running(_) | TrayStatus::Starting | TrayStatus::Degraded
        );
        let startable = matches!(
            self.status,
            TrayStatus::Stopped | TrayStatus::Error(_) | TrayStatus::NotInstalled
        );
        vec![
            StandardItem {
                label: self.status.headline(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // Always present — it is the reason most people open this menu. When the loopback
            // probe says the console isn't answering, the label says so rather than the entry
            // disappearing (a menu missing its main action reads as a broken tray).
            StandardItem {
                label: if self.web_console {
                    "Open web console".to_string()
                } else {
                    "Open web console (not responding)".to_string()
                },
                activate: Box::new(|t: &mut Self| t.open_console("")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Approve pairing request…".into(),
                visible: self.status.pairing_attention(),
                activate: Box::new(|t: &mut Self| t.open_console("pairing")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: match self.status.kept_displays() {
                    1 => "Release kept display…".to_string(),
                    n => format!("Release {n} kept displays…"),
                },
                visible: self.status.kept_displays() > 0,
                activate: Box::new(|t: &mut Self| t.open_console("displays")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Start host".into(),
                visible: startable && !matches!(self.status, TrayStatus::NotInstalled),
                activate: Box::new(|t: &mut Self| t.systemctl("start")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop host".into(),
                visible: running,
                activate: Box::new(|t: &mut Self| t.systemctl("stop")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Restart host".into(),
                visible: running || matches!(self.status, TrayStatus::Error(_)),
                activate: Box::new(|t: &mut Self| t.systemctl("restart")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Exit tray".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// Keep waiting when the watcher drops (plasmashell restart, GNOME shell reload) — the item
    /// re-registers when it returns. Only `--autostart` runs get here with SNI truly absent, and
    /// lingering invisibly is the documented trade-off (see `assume_sni_available` below).
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
}

/// A flat antialiased status dot — the pixmap fallback when the hicolor icons aren't installed
/// (dev runs from `target/`). ARGB32, network byte order (per the SNI spec).
fn dot_icon(size: i32, (r, g, b): (u8, u8, u8)) -> ksni::Icon {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.38;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            // 1 px antialiasing ramp at the rim.
            let alpha = ((radius - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            data.extend_from_slice(&[alpha, r, g, b]);
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

/// Does this user's box run (or intend to run) a punktfunk host? Gates `--autostart` so the
/// packaged autostart entry doesn't put an icon in every desktop user's tray.
fn host_present() -> bool {
    if status::punktfunk_config_dir().is_some_and(|d| d.exists()) {
        return true;
    }
    std::process::Command::new("systemctl")
        .args(["--user", "--quiet", "is-enabled", status::UNIT_NAME])
        .status()
        .is_ok_and(|s| s.success())
}

/// One tray per session: `flock` on a runtime-dir lockfile (held for the process lifetime).
fn acquire_instance_lock() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("punktfunk-tray.lock"))
        .ok()?;
    // SAFETY: `file` is an open, owned fd for the duration of the call; LOCK_NB makes this a
    // non-blocking advisory lock attempt with no other side effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

pub fn run(args: crate::Args) -> anyhow::Result<()> {
    if args.quit {
        // Windows-only convenience for the uninstaller; nothing to do here.
        return Ok(());
    }
    if args.autostart && !host_present() {
        return Ok(()); // not a host box — stay out of this user's tray
    }
    let Some(_lock) = acquire_instance_lock() else {
        return Ok(()); // another instance already runs in this session
    };

    let poller_slot = Arc::new(OnceLock::new());
    let tray = HostTray {
        status: TrayStatus::Stopped, // placeholder; the poller fires within its first cycle
        web_port: args.web_port,
        web_console: false, // live-probed by the poller within its first cycle
        poller: poller_slot.clone(),
    };
    // Autostart races the desktop (the watcher may register after us) → be lenient and wait for
    // it. A manual launch should fail loudly instead (e.g. GNOME without the AppIndicator
    // extension) so the user learns why there is no icon.
    use ksni::blocking::TrayMethods;
    let handle = match tray.assume_sni_available(args.autostart).spawn() {
        Ok(h) => h,
        Err(e) if args.autostart => {
            eprintln!("punktfunk-tray: no StatusNotifier host ({e}); exiting");
            return Ok(());
        }
        Err(e) => anyhow::bail!(
            "no StatusNotifier tray available ({e}) — on GNOME, install the AppIndicator extension"
        ),
    };

    let dead = Arc::new(AtomicBool::new(false));
    let dead_flag = dead.clone();
    let update_handle = handle.clone();
    let poller = Poller::spawn(
        args.mgmt_addr.clone(),
        args.mgmt_port,
        args.web_port,
        Box::new(move |st, console_up| {
            let updated = update_handle.update(|t: &mut HostTray| {
                t.status = st;
                t.web_console = console_up;
            });
            if updated.is_none() {
                dead_flag.store(true, Ordering::SeqCst); // tray service shut down
            }
        }),
    );
    let _ = poller_slot.set(poller);

    // The SNI service runs on its own thread; park here until it dies (shell logout etc.).
    while !dead.load(Ordering::SeqCst) && !handle.is_closed() {
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Ok(())
}
