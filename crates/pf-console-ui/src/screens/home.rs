//! The console home: a center-snapping carousel of host tiles (saved, discovered, and
//! the trailing Add Host action) — the Swift `GamepadHomeView` re-homed onto Skia. A
//! connects (or wakes, or routes to pairing), Y opens a paired host's library, X opens
//! settings, B quits to Gaming Mode. The cursor is the authority; the sprung position
//! chases it, and the focus pop (scale/brightness/fade) reads off the LIVE sprung
//! distance so the look always matches the strip mid-motion.

use crate::anim::Spring;
use crate::glyphs::{Hint, HintKey};
use crate::library::{step_cursor, StepResult, BUMP_C, BUMP_K, BUMP_PX, SPRING_C, SPRING_K};
use crate::model::{ConsoleCmd, HostRow};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{brand, white, Fonts, PanelStroke, BRAND, DIM, ONLINE_GREEN, W, WHITE};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, MaskFilter, Paint, Path, Point, RRect, Rect};

const TILE_W: f64 = 340.0;
const TILE_H: f64 = 224.0;
const TILE_GAP: f64 = 30.0;
const TILE_CORNER: f64 = 26.0;

/// The Add Host tile's synthetic key (host keys are fingerprints or `addr:port`,
/// neither starts with `\0`).
const ADD_KEY: &str = "\0add";

pub(crate) struct HomeScreen {
    cursor: i32,
    anim: Spring,
    bump: Spring,
    /// Last-seen tile keys — hosts churn under discovery; focus follows the KEY.
    keys: Vec<String>,
}

impl HomeScreen {
    pub(crate) fn new() -> HomeScreen {
        HomeScreen {
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            keys: Vec::new(),
        }
    }

    /// Keep the cursor on "the same tile" across host-list churn.
    fn reconcile(&mut self, hosts: &[HostRow]) {
        let keys: Vec<String> = hosts
            .iter()
            .map(|h| h.key.clone())
            .chain(std::iter::once(ADD_KEY.to_string()))
            .collect();
        if keys != self.keys {
            let followed = self
                .keys
                .get(self.cursor as usize)
                .and_then(|old| keys.iter().position(|k| k == old));
            self.cursor = followed.unwrap_or(self.cursor as usize).min(keys.len() - 1) as i32;
            // A follow that moved the tile shifts the spring target; the chase animates
            // the strip to its new berth instead of snapping.
            self.keys = keys;
        }
    }

    fn focused<'h>(&self, hosts: &'h [HostRow]) -> Option<&'h HostRow> {
        hosts.get(self.cursor as usize)
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.reconcile(ctx.hosts);
        let len = ctx.hosts.len() + 1;
        match ev {
            MenuEvent::Move(MenuDir::Left) => self.step(-1, len, false),
            MenuEvent::Move(MenuDir::Right) => self.step(1, len, false),
            MenuEvent::JumpBack => self.step(-5, len, true),
            MenuEvent::JumpForward => self.step(5, len, true),
            MenuEvent::Confirm => {
                match self.focused(ctx.hosts) {
                    None => fx.push(Screen::AddHost(super::add_host::AddHostScreen::new())),
                    Some(h) if !h.paired => fx.push(Screen::Pair(super::pair::PairScreen::new(
                        h,
                        ctx.device_name,
                    ))),
                    Some(h) if !h.online && h.can_wake => {
                        // Wake first; the wake overlay connects once it answers.
                        fx.cmds.push(ConsoleCmd::Wake {
                            key: h.key.clone(),
                            then_connect: true,
                        });
                    }
                    Some(h) => {
                        // Dial-first even when the presence pips say offline — a
                        // routed/VPN host is mDNS-blind and probe-shy but dials fine.
                        fx.connect = Some(ConnectIntent {
                            addr: h.addr.clone(),
                            port: h.port,
                            fp_hex: h.fp_hex.clone(),
                            launch: None,
                            title: h.name.clone(),
                        });
                    }
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Secondary => match self.focused(ctx.hosts) {
                Some(h) if h.paired && h.saved => {
                    fx.cmds.push(ConsoleCmd::FetchLibrary {
                        addr: h.addr.clone(),
                        mgmt: h.mgmt_port,
                        fp_hex: h.fp_hex.clone(),
                    });
                    fx.push(Screen::Library(super::library::LibraryScreen::new(h)));
                    Some(MenuPulse::Confirm)
                }
                Some(_) => {
                    fx.toast = Some("Pair with this host to browse its library".into());
                    Some(MenuPulse::Boundary)
                }
                None => None,
            },
            MenuEvent::Tertiary => {
                fx.push(Screen::Settings(super::settings::SettingsScreen::new()));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop(); // popping the root = quit (the shell's rule)
                None
            }
            MenuEvent::Move(_) => None,
        }
    }

    fn step(&mut self, delta: i32, len: usize, clamp: bool) -> Option<MenuPulse> {
        match step_cursor(self.cursor, len, delta, clamp) {
            StepResult::Moved(to) => {
                self.cursor = to;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: -BUMP_PX * f64::from(delta.signum()),
                    vel: 0.0,
                };
                Some(MenuPulse::Boundary)
            }
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let mut hints = Vec::new();
        match self.focused(ctx.hosts) {
            None => hints.push(Hint::new(HintKey::Confirm, "Add Host")),
            Some(h) if !h.paired => hints.push(Hint::new(HintKey::Confirm, "Pair…")),
            Some(h) if !h.online && h.can_wake => {
                hints.push(Hint::new(HintKey::Confirm, "Wake & Connect"))
            }
            Some(_) => hints.push(Hint::new(HintKey::Confirm, "Connect")),
        }
        if self.focused(ctx.hosts).is_some_and(|h| h.paired && h.saved) {
            hints.push(Hint::new(HintKey::Secondary, "Library"));
        }
        hints.push(Hint::new(HintKey::Tertiary, "Settings"));
        hints.push(Hint::new(HintKey::Back, "Quit"));
        hints
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.reconcile(ctx.hosts);
        self.anim
            .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
        self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);

        let w = f64::from(rect.width());
        let tile_w = (TILE_W * k).min(w * 0.84);
        let tile_h = (TILE_H * k)
            .min(f64::from(rect.height()) - 48.0 * k)
            .max(118.0 * k);
        let pitch = tile_w + TILE_GAP * k;
        let cx0 = f64::from(rect.left) + w / 2.0 + self.bump.pos * k;
        let cy = f64::from(rect.top) + f64::from(rect.height()) / 2.0;

        let len = ctx.hosts.len() + 1;
        for i in 0..len {
            let d = i as f64 - self.anim.pos;
            if d.abs() > 2.6 {
                continue;
            }
            let f = 1.0 - d.abs().min(1.0); // 1 at focus → 0 one slot out
            let scale = 0.88 + 0.12 * f;
            let alpha = 0.78 + 0.22 * f;
            let cx = cx0 + d * pitch;
            let tile = Rect::from_xywh(
                (cx - tile_w / 2.0) as f32,
                (cy - tile_h / 2.0) as f32,
                tile_w as f32,
                tile_h as f32,
            );
            canvas.save();
            canvas.translate((cx as f32, cy as f32));
            canvas.scale((scale as f32, scale as f32));
            canvas.translate((-cx as f32, -cy as f32));
            canvas.save_layer_alpha_f(None, alpha as f32);
            if f > 0.4 {
                crate::theme::drop_shadow(
                    canvas,
                    tile,
                    TILE_CORNER as f32,
                    k as f32,
                    0.45 * f as f32,
                );
            }
            match ctx.hosts.get(i) {
                Some(h) => draw_host_tile(canvas, fonts, h, tile, k, ctx.t),
                None => draw_add_tile(canvas, fonts, tile, k),
            }
            // The brightness recede: an opaque veil, matching the coverflow's rule.
            if f < 1.0 {
                let veil = (1.0 - f) as f32 * 0.24;
                canvas.draw_rrect(
                    RRect::new_rect_xy(tile, (TILE_CORNER * k) as f32, (TILE_CORNER * k) as f32),
                    &Paint::new(Color4f::new(0.0, 0.0, 0.0, veil), None),
                );
            }
            canvas.restore(); // layer
            canvas.restore(); // transform
        }

        if ctx.hosts.is_empty() {
            fonts.centered(
                canvas,
                "Hosts on this network appear automatically — add one by address for everything else.",
                W::Regular,
                13.0 * k,
                DIM,
                f64::from(rect.left) + w / 2.0,
                cy + tile_h / 2.0 + 24.0 * k,
                w * 0.7,
            );
        }
    }
}

fn draw_host_tile(canvas: &Canvas, fonts: &Fonts, h: &HostRow, rect: Rect, k: f64, _t: f64) {
    crate::theme::panel(
        canvas,
        rect,
        TILE_CORNER as f32,
        h.saved.then(|| brand(0.20)),
        if h.saved {
            PanelStroke::Gradient
        } else {
            PanelStroke::GradientDashed
        },
        k as f32,
    );
    let pad = 20.0 * k;
    let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
    draw_monogram(canvas, fonts, &h.name, h.saved, l, t, k);

    // Top-right status cluster: a lock for a paired identity, a glowing pip when live.
    let mut sx = f64::from(rect.right) - pad;
    if h.online {
        let r = 4.5 * k;
        let center = Point::new((sx - r) as f32, (t + 9.0 * k) as f32);
        let mut glow = Paint::new(
            Color4f::new(ONLINE_GREEN.r, ONLINE_GREEN.g, ONLINE_GREEN.b, 0.7),
            None,
        );
        glow.set_mask_filter(MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            (5.0 * k) as f32,
            None,
        ));
        canvas.draw_circle(center, r as f32, &glow);
        canvas.draw_circle(center, r as f32, &Paint::new(ONLINE_GREEN, None));
        sx -= 2.0 * r + 9.0 * k;
    }
    if h.paired {
        draw_lock(canvas, sx - 9.0 * k, t + 4.0 * k, k);
    }

    let max_w = f64::from(rect.width()) - 2.0 * pad;
    let sub_base = f64::from(rect.bottom) - pad;
    fonts.draw_clipped(
        canvas,
        &format!("{}:{}", h.addr, h.port),
        l,
        sub_base,
        W::Regular,
        13.0 * k,
        white(0.55),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        &h.name,
        l,
        sub_base - 22.0 * k,
        W::Bold,
        23.0 * k,
        WHITE,
        max_w,
    );
}

fn draw_add_tile(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64) {
    crate::theme::panel(
        canvas,
        rect,
        TILE_CORNER as f32,
        None,
        PanelStroke::GradientDashed,
        k as f32,
    );
    let pad = 20.0 * k;
    let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
    // The badge with a + instead of a monogram.
    let badge = Rect::from_xywh(l as f32, t as f32, (52.0 * k) as f32, (52.0 * k) as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32),
        &Paint::new(brand(0.16), None),
    );
    let mut ring = Paint::new(brand(0.5), None);
    ring.set_style(skia_safe::PaintStyle::Stroke);
    ring.set_stroke_width(1.0);
    ring.set_anti_alias(true);
    canvas.draw_rrect(
        RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32),
        &ring,
    );
    let (bcx, bcy) = (l + 26.0 * k, t + 26.0 * k);
    let mut p = Paint::new(BRAND, None);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width((3.0 * k) as f32);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_anti_alias(true);
    let r = 9.0 * k;
    canvas.draw_line(
        ((bcx - r) as f32, bcy as f32),
        ((bcx + r) as f32, bcy as f32),
        &p,
    );
    canvas.draw_line(
        (bcx as f32, (bcy - r) as f32),
        (bcx as f32, (bcy + r) as f32),
        &p,
    );

    let max_w = f64::from(rect.width()) - 2.0 * pad;
    let sub_base = f64::from(rect.bottom) - pad;
    fonts.draw_clipped(
        canvas,
        "Register a host by address",
        l,
        sub_base,
        W::Regular,
        13.0 * k,
        white(0.55),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        "Add Host",
        l,
        sub_base - 22.0 * k,
        W::Bold,
        23.0 * k,
        WHITE,
        max_w,
    );
}

fn draw_monogram(canvas: &Canvas, fonts: &Fonts, name: &str, filled: bool, x: f64, y: f64, k: f64) {
    let badge = Rect::from_xywh(x as f32, y as f32, (52.0 * k) as f32, (52.0 * k) as f32);
    let rr = RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32);
    if filled {
        let mut p = Paint::default();
        p.set_shader(skia_safe::gradient_shader::linear(
            (
                Point::new(badge.left, badge.top),
                Point::new(badge.left, badge.bottom),
            ),
            skia_safe::gradient_shader::GradientShaderColors::Colors(&[
                BRAND.to_color(),
                brand(0.68).to_color(),
            ]),
            None,
            skia_safe::TileMode::Clamp,
            None,
            None,
        ));
        canvas.draw_rrect(rr, &p);
    } else {
        canvas.draw_rrect(rr, &Paint::new(brand(0.16), None));
        let mut ring = Paint::new(brand(0.5), None);
        ring.set_style(skia_safe::PaintStyle::Stroke);
        ring.set_stroke_width(1.0);
        ring.set_anti_alias(true);
        canvas.draw_rrect(rr, &ring);
    }
    let letter: String = name
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "•".to_string());
    let size = 25.0 * k;
    let tw = fonts.measure(&letter, W::Bold, size) as f64;
    fonts.draw(
        canvas,
        &letter,
        x + 26.0 * k - tw / 2.0,
        y + 26.0 * k + size * 0.36,
        W::Bold,
        size,
        if filled { WHITE } else { BRAND },
    );
}

/// A small padlock: filled body + stroked shackle (the paired-identity mark).
fn draw_lock(canvas: &Canvas, x: f64, y: f64, k: f64) {
    let ink = white(0.5);
    let body_w = 11.0 * k;
    let body_h = 8.0 * k;
    let body_top = y + 5.0 * k;
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(x as f32, body_top as f32, body_w as f32, body_h as f32),
            (2.0 * k) as f32,
            (2.0 * k) as f32,
        ),
        &Paint::new(ink, None),
    );
    let mut p = Paint::new(ink, None);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width((1.6 * k) as f32);
    p.set_anti_alias(true);
    let mut shackle = Path::new();
    let (cx, r) = (x + body_w / 2.0, 3.2 * k);
    shackle.move_to(((cx - r) as f32, body_top as f32));
    shackle.arc_to(
        Rect::from_xywh(
            (cx - r) as f32,
            (body_top - r) as f32,
            (2.0 * r) as f32,
            (2.0 * r) as f32,
        ),
        180.0,
        180.0,
        false,
    );
    canvas.draw_path(&shackle, &p);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str, paired: bool, online: bool, can_wake: bool) -> HostRow {
        HostRow {
            key: key.into(),
            name: key.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: if paired { "ab".into() } else { String::new() },
            paired,
            saved: true,
            online,
            mgmt_port: 47990,
            can_wake,
            last_used: None,
        }
    }

    fn ctx_settings() -> pf_client_core::trust::Settings {
        pf_client_core::trust::Settings::default()
    }

    #[test]
    fn cursor_follows_the_key_through_churn() {
        let mut s = HomeScreen::new();
        let a = host("a", true, true, false);
        let b = host("b", true, true, false);
        s.reconcile(&[a.clone(), b.clone()]);
        s.cursor = 1; // on "b"
        s.keys = vec!["a".into(), "b".into(), ADD_KEY.into()];
        // A new host lands in front — focus must stay on "b".
        let c = host("c", false, true, false);
        s.reconcile(&[c, a, b]);
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn confirm_routes_by_host_state() {
        let mut settings = ctx_settings();
        let hosts = [
            host("paired-online", true, true, false),
            host("unpaired", false, true, false),
            host("asleep", true, false, true),
        ];
        let pads: Vec<pf_client_core::gamepad::PadInfo> = Vec::new();

        // Paired+online → connect intent.
        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "test",
            t: 0.0,
        };
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.connect.is_some());

        // Unpaired → the pair screen.
        let mut fx = Outbox::default();
        s.cursor = 1;
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(_))));
        assert!(fx.connect.is_none());

        // Asleep + MAC on file → wake-then-connect.
        let mut fx = Outbox::default();
        s.cursor = 2;
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::Wake {
                then_connect: true,
                ..
            })
        ));
    }

    #[test]
    fn add_tile_is_always_last() {
        let mut settings = ctx_settings();
        let pads: Vec<pf_client_core::gamepad::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(b)) if matches!(*b, Screen::AddHost(_)))
        );
    }
}
