//! Raspberry Pi status and remote access use the appliance helper.
use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::{Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::kiosk::{Action, Request, Snapshot};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Ssh,
    Information,
    VirtualHere,
    VirtualHereLicense,
    Update,
}

pub(crate) struct KioskScreen {
    list: MenuList,
    page: Page,
    state: Snapshot,
    request: Request,
    refreshed: Instant,
    password: Option<String>,
    error: Option<String>,
}
impl KioskScreen {
    pub(crate) fn new(page: Page) -> Self {
        let mut request = Request::default();
        request.start(Action::Status);
        Self {
            list: MenuList::new(),
            page,
            state: Snapshot::default(),
            request,
            refreshed: Instant::now(),
            password: None,
            error: None,
        }
    }
    fn sync(&mut self) {
        let action = self.request.action();
        if let Some(result) = self.request.poll() {
            self.refreshed = Instant::now();
            match result {
                Ok(mut state) => {
                    if let Some(password) = state.password.take() {
                        self.password = Some(password);
                    }
                    if !state.ssh_enabled {
                        self.password = None;
                    }
                    self.state = state;
                    if action != Some(Action::Status) {
                        self.error = None;
                    }
                }
                Err(error) => self.error = Some(error),
            }
        }
        if self.refreshed.elapsed() >= Duration::from_secs(5) && !self.request.busy() {
            self.request.start(Action::Status);
        }
    }
    fn rows(&self) -> Vec<RowSpec> {
        let info = |label: &str, value: String| RowSpec {
            value: Some(value),
            ..RowSpec::action(label, false)
        };
        if self.page == Page::Update {
            let busy = matches!(
                self.state.update_phase.as_str(),
                "queued" | "downloading" | "verifying" | "switching" | "testing"
            );
            let ready = self.state.available
                && self.state.update_supported
                && !self.request.busy()
                && !busy;
            return vec![
                RowSpec::action("Check for updates", ready),
                RowSpec::action(
                    "Install app update",
                    ready && self.state.update_available && self.state.update_phase == "ready",
                ),
                info("Installed", self.state.version.clone()),
                info(
                    "Available",
                    if self.state.update_version.is_empty() {
                        "Not checked".into()
                    } else {
                        self.state.update_version.clone()
                    },
                ),
                info("Kiosk OS", self.state.os_version.clone()),
            ];
        }
        if self.page == Page::VirtualHereLicense {
            return vec![
                RowSpec::action("1. Open VirtualHere Client on your gaming PC", false),
                RowSpec::action("2. Right-click USB Servers and choose License", false),
                RowSpec::action("3. Choose Enter License(s) and paste your key", false),
                RowSpec::action("Free mode needs the graphical PC client", false),
                info(
                    "License",
                    if self.state.virtualhere_license_saved {
                        "Key saved on this Pi"
                    } else {
                        "No key saved"
                    }
                    .into(),
                ),
            ];
        }
        if self.page == Page::VirtualHere {
            let ready =
                self.state.available && self.state.virtualhere_supported && !self.request.busy();
            let mut rows = vec![
                RowSpec::action(
                    if self.state.virtualhere_installed {
                        "Installed"
                    } else {
                        "Download and install"
                    },
                    ready && !self.state.virtualhere_installed,
                ),
                RowSpec {
                    value: Some(
                        if self.state.virtualhere_running {
                            "On"
                        } else {
                            "Off"
                        }
                        .into(),
                    ),
                    ..RowSpec::action("USB sharing", ready && self.state.virtualhere_installed)
                },
                RowSpec {
                    value: Some(
                        if self.state.virtualhere_license_saved {
                            "Key saved"
                        } else {
                            "Free: one USB device"
                        }
                        .into(),
                    ),
                    ..RowSpec::action("License setup", true)
                },
            ];
            for address in &self.state.ip_addresses {
                rows.push(info("Server address", address.clone()));
            }
            rows.push(info("Port", "7575".into()));
            return rows;
        }
        if self.page == Page::Ssh {
            let ready = self.state.available && !self.request.busy();
            let mut rows = vec![
                RowSpec {
                    value: Some(if self.state.ssh_enabled { "On" } else { "Off" }.into()),
                    ..RowSpec::action("SSH", ready)
                },
                RowSpec::action("Generate a new password", ready && self.state.ssh_enabled),
            ];
            if self.state.ssh_enabled {
                rows.push(info("User", self.state.ssh_user.clone()));
                for address in &self.state.ip_addresses {
                    rows.push(RowSpec {
                        header: Some("Connect"),
                        ..RowSpec::action(ssh_command(&self.state, address), false)
                    });
                }
                if let Some(password) = &self.password {
                    rows.push(info("Password", password.clone()));
                }
            }
            return rows;
        }
        let mut rows = vec![
            info("Hostname", self.state.hostname.clone()),
            info("Punktfunk", self.state.version.clone()),
            info("Kiosk OS", self.state.os_version.clone()),
            info("Connection", self.state.connection.clone()),
        ];
        for address in &self.state.ip_addresses {
            rows.push(info("IP address", address.clone()));
        }
        if self.state.ip_addresses.is_empty() {
            rows.push(info("IP address", "Not connected".into()));
        }
        if let Some(temp) = self.state.temperature_c {
            rows.push(info("Temperature", format!("{temp:.1} °C")));
        }
        let minutes = (self.state.uptime_seconds.max(0.0) / 60.0) as u64;
        rows.push(info(
            "Uptime",
            format!("{} h {} min", minutes / 60, minutes % 60),
        ));
        rows.push(info(
            "SSH",
            if self.state.ssh_enabled { "On" } else { "Off" }.into(),
        ));
        rows
    }
    fn activate(&mut self, msg: ListMsg, pulse: Option<MenuPulse>) -> Option<MenuPulse> {
        if matches!(msg, ListMsg::Adjust(_)) {
            return Some(MenuPulse::Boundary);
        }
        if !matches!(msg, ListMsg::Activate) {
            return pulse;
        }
        let rows = self.rows();
        if !rows.get(self.list.cursor).is_some_and(|r| r.enabled) {
            return Some(MenuPulse::Boundary);
        }
        let action = if self.page == Page::Update {
            match self.list.cursor {
                0 => Action::CheckUpdate,
                1 => Action::InstallUpdate,
                _ => return pulse,
            }
        } else if self.page == Page::VirtualHere {
            match self.list.cursor {
                0 => Action::InstallVirtualHere,
                1 if self.state.virtualhere_running => Action::DisableVirtualHere,
                1 => Action::EnableVirtualHere,
                2 => {
                    self.page = Page::VirtualHereLicense;
                    self.list = MenuList::new();
                    return pulse;
                }
                _ => return pulse,
            }
        } else {
            match self.list.cursor {
                0 if self.state.ssh_enabled => Action::Disable,
                0 => Action::Enable,
                1 => Action::ResetPassword,
                _ => return pulse,
            }
        };
        if self.request.start(action) {
            self.password = None;
            self.error = None;
        }
        pulse
    }
    fn detail(&self) -> &str {
        if let Some(error) = &self.error {
            return error;
        }
        if self.request.busy() && !self.state.available {
            return "Reading device status…";
        }
        if !self.state.available {
            return "Device information is unavailable.";
        }
        if self.page == Page::VirtualHereLicense {
            return "One USB device is free. Buy a license at virtualhere.com for more devices.";
        }
        if self.page == Page::Update {
            if !self.state.update_supported {
                return "The kiosk needs updater setup before it can install app updates.";
            }
            if self.request.action() == Some(Action::CheckUpdate) {
                return "Checking GitHub for a compatible app update…";
            }
            return &self.state.update_message;
        }
        if self.page == Page::VirtualHere {
            if !self.state.virtualhere_supported {
                return "Update the kiosk image to install VirtualHere.";
            }
            if self.request.action() == Some(Action::InstallVirtualHere) {
                return "Downloading and installing VirtualHere… This may take up to two minutes.";
            }
            if matches!(
                self.request.action(),
                Some(Action::EnableVirtualHere | Action::DisableVirtualHere)
            ) {
                return "Updating VirtualHere… This may take up to two minutes.";
            }
            return "Downloads from virtualhere.com. Turn on USB sharing, then connect using VirtualHere Client on your PC.";
        }
        if self.page == Page::Information {
            return "Network and device status refresh automatically.";
        }
        if self.request.busy() {
            return "Updating remote access…";
        }
        if self.password.is_some() {
            return "Use this password to connect. It is hidden when you leave this screen.";
        }
        if self.state.ssh_enabled {
            return "SSH is on. Generate a new password if you no longer have it.";
        }
        "Turn on SSH to connect from another computer on your network."
    }
}
fn ssh_command(state: &Snapshot, address: &str) -> String {
    if state.ssh_port == 22 {
        format!("ssh {}@{address}", state.ssh_user)
    } else {
        format!("ssh -p {} {}@{address}", state.ssh_port, state.ssh_user)
    }
}
impl ScreenView for KioskScreen {
    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        self.sync();
        if ev == MenuEvent::Back {
            if self.page == Page::VirtualHereLicense {
                self.page = Page::VirtualHere;
                self.list = MenuList::new();
                return None;
            }
            self.password = None;
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.rows().len());
        self.activate(msg, pulse)
    }
    fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        self.sync();
        let (msg, pulse) = self.list.pointer(p, self.rows().len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse);
        true
    }
    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let mut hints = vec![Hint::new(HintKey::Back, "Back")];
        if self.rows().get(self.list.cursor).is_some_and(|r| r.enabled) {
            hints.insert(0, Hint::new(HintKey::Confirm, "Choose"));
        }
        hints
    }
    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.sync();
        let rows = self.rows();
        self.list.cursor = self.list.cursor.min(rows.len().saturating_sub(1));
        self.list.render(
            canvas,
            ctx.ui,
            Rect::from_ltrb(
                rect.left,
                rect.top + (48.0 * k) as f32,
                rect.right,
                rect.bottom,
            ),
            &rows,
            fonts,
            k,
            dt,
            true,
        );
        fonts.centered(
            canvas,
            self.detail(),
            W::Regular,
            13.0 * k,
            ctx.ui.fg(0.65),
            f64::from(rect.center_x()),
            f64::from(rect.top) + 6.0 * k,
            f64::from(rect.width()) * 0.9,
        );
    }
    fn title(&self) -> String {
        match self.page {
            Page::Ssh => "SSH",
            Page::Information => "System information",
            Page::VirtualHere => "VirtualHere",
            Page::VirtualHereLicense => "VirtualHere license",
            Page::Update => "Punktfunk updates",
        }
        .into()
    }
    fn press(&mut self) {
        self.list.dip();
    }
    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        self.rows().get(self.list.cursor).map(|row| {
            format!(
                "{}: {}",
                row.label,
                row.value.as_deref().unwrap_or_default()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connect_command_uses_reported_address_and_port() {
        let mut state = Snapshot {
            ssh_user: "punktfunk-admin".into(),
            ssh_port: 22,
            ..Default::default()
        };
        assert_eq!(
            ssh_command(&state, "192.168.1.3"),
            "ssh punktfunk-admin@192.168.1.3"
        );
        state.ssh_port = 2222;
        assert_eq!(
            ssh_command(&state, "192.168.1.3"),
            "ssh -p 2222 punktfunk-admin@192.168.1.3"
        );
    }
    #[test]
    fn unavailable_controls_cannot_be_activated() {
        let mut screen = KioskScreen {
            list: MenuList::new(),
            page: Page::Ssh,
            state: Snapshot::default(),
            request: Request::default(),
            refreshed: Instant::now(),
            password: None,
            error: None,
        };
        assert!(screen.rows().iter().all(|row| !row.enabled));
        screen.activate(ListMsg::Activate, None);
        assert!(!screen.request.busy());
        screen.state.available = true;
        assert!(screen.rows()[0].enabled);
        assert!(!screen.rows()[1].enabled);
        screen.state.ssh_enabled = true;
        assert!(screen.rows()[1].enabled);
        screen.page = Page::VirtualHere;
        assert!(
            !screen.rows()[0].enabled,
            "older helpers cannot install USB sharing"
        );
        screen.state.virtualhere_supported = true;
        assert!(screen.rows()[0].enabled);
        assert!(
            !screen.rows()[1].enabled,
            "sharing needs an installed server"
        );
        screen.state.virtualhere_installed = true;
        assert!(!screen.rows()[0].enabled);
        assert!(screen.rows()[1].enabled);
        screen.list.cursor = 2;
        screen.activate(ListMsg::Activate, None);
        assert!(screen.page == Page::VirtualHereLicense);
        assert!(
            !screen.request.busy(),
            "license help does not start sharing"
        );
        screen.page = Page::Update;
        assert!(!screen.rows()[0].enabled);
        screen.state.update_supported = true;
        assert!(screen.rows()[0].enabled);
        assert!(!screen.rows()[1].enabled);
        screen.state.update_available = true;
        screen.state.update_phase = "ready".into();
        assert!(screen.rows()[1].enabled);
        screen.state.update_phase = "downloading".into();
        assert!(!screen.rows()[0].enabled);
        assert!(!screen.rows()[1].enabled);
    }
    #[test]
    fn appliance_screens_render_connection_details() {
        let fonts = crate::theme::build_fonts().unwrap();
        let library = crate::library::LibraryShared::default();
        let mut settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let password = "ABCDEFGHIJKLMNOPQRST";
        for page in [
            Page::Ssh,
            Page::Information,
            Page::VirtualHere,
            Page::VirtualHereLicense,
            Page::Update,
        ] {
            let mut screen = KioskScreen {
                list: MenuList::new(),
                page,
                state: Snapshot {
                    available: true,
                    hostname: "punktfunk-pi".into(),
                    version: "v0.46.0-rpi5.2".into(),
                    connection: "Wi-Fi: Living room wireless network (wlan0)".into(),
                    ip_addresses: vec![
                        "192.168.1.123".into(),
                        "2001:db8:1234:5678:9abc:def0:1234:5678".into(),
                    ],
                    temperature_c: Some(51.2),
                    uptime_seconds: 8123.0,
                    ssh_enabled: true,
                    ssh_user: "punktfunk-admin".into(),
                    ssh_port: 22,
                    virtualhere_supported: true,
                    virtualhere_installed: true,
                    update_supported: true,
                    update_available: true,
                    update_phase: "ready".into(),
                    update_version: "0.46.0-rpi5.29".into(),
                    update_message:
                        "An app update is available. Punktfunk restarts; your settings are kept."
                            .into(),
                    os_version: "Raspberry Pi OS · Debian 13 (64-bit)".into(),
                    ..Default::default()
                },
                request: Request::default(),
                refreshed: Instant::now(),
                password: Some(password.into()),
                error: None,
            };
            let (w, h) = (1920, 1080);
            let k = f64::from(h) / 800.0;
            let rect = Rect::from_ltrb(0.0, 100.0, w as f32, h as f32 - 100.0);
            let col = crate::widgets::column(ctx.ui, rect, k);
            assert!(
                f64::from(fonts.measure(password, W::Medium, 15.0 * k))
                    < f64::from(col.width()) * 0.55,
                "password must fit without truncation"
            );
            let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
            for _ in 0..15 {
                screen.refreshed = Instant::now();
                surface
                    .canvas()
                    .clear(skia_safe::Color::from_rgb(18, 20, 28));
                screen.render(surface.canvas(), rect, k, 1.0 / 15.0, &fonts, &mut ctx);
            }
            assert!(screen.list.row_rect(0).is_some());
            if let Ok(dir) = std::env::var("PF_CONSOLE_DUMP") {
                std::fs::create_dir_all(&dir).unwrap();
                let png = surface
                    .image_snapshot()
                    .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                    .unwrap();
                let name = match page {
                    Page::Ssh => "kiosk-ssh",
                    Page::Information => "kiosk-information",
                    Page::VirtualHere => "kiosk-virtualhere",
                    Page::VirtualHereLicense => "kiosk-virtualhere-license",
                    Page::Update => "kiosk-update",
                };
                std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
            }
            let mut fx = Outbox::default();
            screen.menu(MenuEvent::Back, &mut ctx, &mut fx);
            if page == Page::VirtualHereLicense {
                assert!(screen.page == Page::VirtualHere);
                screen.menu(MenuEvent::Back, &mut ctx, &mut fx);
            }
            assert!(
                screen.password.is_none(),
                "leaving hides generated credentials"
            );
        }
    }
}
