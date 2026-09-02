//! Console home: a center-snapping carousel of host tiles plus trailing Add Host
//! and Rescan actions.
//!
//! The cursor is the index; the sprung position chases it. Focus scale,
//! brightness, and fade read off the live sprung distance so the look matches
//! the strip mid-motion. A connects, wakes, or pairs; Y opens a paired library;
//! X or Down open Settings; B pops the root (quit).
//!
//! Discovery churns the list; focus follows the tile key, not the index. A
//! press on a side tile only retargets the cursor — Confirm starts a session.
//! Pin with the tests in this module: key-follow, confirm routing, padless
//! Settings/Options, pinned-card profile, trailing Add Host.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    step_cursor, StepResult, BUMP_C, BUMP_K, BUMP_PX, ENTER_RISE, ENTER_SCALE, SPRING_C, SPRING_K,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, fg, fill, stroke, Fonts, PanelStroke, ONLINE_GREEN, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, MaskFilter, PathBuilder, Point, RRect, Rect};

const TILE_W: f64 = 340.0;
const TILE_H: f64 = 224.0;
const TILE_GAP: f64 = 30.0;
const TILE_CORNER: f64 = 26.0;

/// Sentinel. Host keys are fingerprints or `addr:port`; neither starts with `\0`.
const ADD_KEY: &str = "\0add";
/// Sentinel for the trailing Rescan tile; same `\0` prefix as [`ADD_KEY`].
const SCAN_KEY: &str = "\0scan";

/// Do not use `hosts.get(i)`: `None` is both trailing actions.
enum Slot<'h> {
    Host(&'h HostRow),
    AddHost,
    /// Re-run discovery. A pad has no pull-to-refresh.
    Rescan,
}

fn slot_at(i: usize, hosts: &[HostRow]) -> Slot<'_> {
    match hosts.get(i) {
        Some(h) => Slot::Host(h),
        None if i == hosts.len() => Slot::AddHost,
        None => Slot::Rescan,
    }
}

pub(crate) struct HomeScreen {
    cursor: i32,
    anim: Spring,
    bump: Spring,
    /// Last-seen tile keys. Discovery churns the list; focus follows the key.
    keys: Vec<String>,
    /// Last-drawn tile rects, device px; empty for culled tiles. Hit-testing uses
    /// the drawn (0.88) side-tile size so an edge press does not pick a neighbour.
    geom: Vec<Rect>,
    /// Mount entrance. `None` until the first frame (no clock in the constructor)
    /// and again once finished. [`Self::entrance_armed`] stops it re-arming.
    entrance: Option<Entrance>,
    entrance_armed: bool,
}

impl HomeScreen {
    pub(crate) fn new() -> HomeScreen {
        HomeScreen {
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            keys: Vec::new(),
            geom: Vec::new(),
            entrance: None,
            entrance_armed: false,
        }
    }

    /// Focus follows the tile key, not the index.
    fn reconcile(&mut self, hosts: &[HostRow]) {
        let keys: Vec<String> = hosts
            .iter()
            .map(|h| h.key.clone())
            .chain([ADD_KEY.to_string(), SCAN_KEY.to_string()])
            .collect();
        if keys != self.keys {
            let followed = self
                .keys
                .get(self.cursor as usize)
                .and_then(|old| keys.iter().position(|k| k == old));
            self.cursor = followed.unwrap_or(self.cursor as usize).min(keys.len() - 1) as i32;
            // Leave the spring; render chases the new cursor so the strip animates.
            self.keys = keys;
        }
    }

    fn focused<'h>(&self, hosts: &'h [HostRow]) -> Option<&'h HostRow> {
        hosts.get(self.cursor as usize)
    }

    fn slot<'h>(&self, hosts: &'h [HostRow]) -> Slot<'h> {
        slot_at(self.cursor.max(0) as usize, hosts)
    }

    fn len(hosts: &[HostRow]) -> usize {
        hosts.len() + 2
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.reconcile(ctx.hosts);
        let len = Self::len(ctx.hosts);
        match ev {
            MenuEvent::Move(MenuDir::Left) => self.step(-1, len, false),
            MenuEvent::Move(MenuDir::Right) => self.step(1, len, false),
            MenuEvent::JumpBack => self.step(-5, len, true),
            MenuEvent::JumpForward => self.step(5, len, true),
            MenuEvent::Confirm => {
                match self.slot(ctx.hosts) {
                    Slot::AddHost => {
                        fx.push(Screen::AddHost(super::add_host::AddHostScreen::new()))
                    }
                    Slot::Rescan => {
                        fx.cmds.push(ConsoleCmd::Probe);
                        fx.toast = Some("Scanning for hosts…".into());
                    }
                    Slot::Host(h) if !h.paired => fx.push(Screen::Pair(
                        super::pair::PairScreen::new(h, ctx.device_name),
                    )),
                    Slot::Host(h) if !h.online && h.can_wake => {
                        // Wake first; the overlay connects once the host answers.
                        fx.cmds.push(ConsoleCmd::Wake {
                            key: h.key.clone(),
                            then_connect: true,
                        });
                    }
                    Slot::Host(h) => {
                        // Dial even when the pips say offline: a routed or VPN host
                        // can miss mDNS and still answer.
                        fx.connect = Some(ConnectIntent {
                            addr: h.addr.clone(),
                            port: h.port,
                            fp_hex: h.fp_hex.clone(),
                            launch: None,
                            title: match &h.pin {
                                Some(p) => format!("{} · {}", h.name, p.name),
                                None => h.name.clone(),
                            },
                            request_access: false,
                            profile: h.pin.as_ref().map(|p| p.id.clone()),
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
                    // Sample the epoch before the fetch drains so the shelf can tell
                    // its titles from the model's.
                    fx.push(Screen::Library(super::library::LibraryScreen::new(
                        h,
                        ctx.library.fetch_epoch(),
                    )));
                    Some(MenuPulse::Confirm)
                }
                Some(_) => {
                    fx.toast = Some("Pair with this host to browse its library".into());
                    Some(MenuPulse::Boundary)
                }
                None => None,
            },
            // Sector is the ring; this carousel steps on `Move`.
            MenuEvent::Sector(_) => None,
            MenuEvent::Tertiary => {
                fx.push(Screen::Settings(super::settings::SettingsScreen::new(
                    ctx.store,
                )));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop(); // root pop is quit (shell rule)
                None
            }
            // The strip is horizontal, so up is the free direction (host menu).
            MenuEvent::Move(MenuDir::Up) => match self.focused(ctx.hosts) {
                Some(h) if super::host_options::HostOptionsScreen::available(h) => {
                    fx.push(Screen::HostOptions(
                        super::host_options::HostOptionsScreen::new(h),
                    ));
                    Some(MenuPulse::Confirm)
                }
                _ => Some(MenuPulse::Boundary),
            },
            // A D-pad remote never sends X; Down is the only route to Settings.
            MenuEvent::Move(MenuDir::Down) => {
                fx.push(Screen::Settings(super::settings::SettingsScreen::new(
                    ctx.store,
                )));
                Some(MenuPulse::Confirm)
            }
        }
    }

    /// Only the centre tile activates. A press that also connected would start a
    /// session for a host that was merely aimed at.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        self.reconcile(ctx.hosts);
        let len = Self::len(ctx.hosts);
        match p.kind {
            PointerKind::Scroll { up } => {
                self.step(if up { -1 } else { 1 }, len, false);
                true
            }
            // Geometry is a frame old: discovery can shorten the strip between draw
            // and press, and an index past `len` would land on Add Host.
            PointerKind::Press => match p.pick(&self.geom).filter(|i| *i < len) {
                Some(i) if i == self.cursor as usize => {
                    self.menu(MenuEvent::Confirm, ctx, fx);
                    true
                }
                Some(i) => {
                    self.cursor = i as i32;
                    true
                }
                None => false,
            },
            _ => false,
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
        match self.slot(ctx.hosts) {
            Slot::AddHost => hints.push(Hint::new(HintKey::Confirm, "Add Host")),
            Slot::Rescan => hints.push(Hint::new(HintKey::Confirm, "Scan Again")),
            Slot::Host(h) if !h.paired => hints.push(Hint::new(HintKey::Confirm, "Pair…")),
            Slot::Host(h) if !h.online && h.can_wake => {
                hints.push(Hint::new(HintKey::Confirm, "Wake & Connect"))
            }
            Slot::Host(_) => hints.push(Hint::new(HintKey::Confirm, "Connect")),
        }
        if self.focused(ctx.hosts).is_some_and(|h| h.paired && h.saved) {
            hints.push(Hint::new(HintKey::Secondary, "Library"));
        }
        if self
            .focused(ctx.hosts)
            .is_some_and(super::host_options::HostOptionsScreen::available)
        {
            hints.push(Hint::new(HintKey::Up, "Options"));
        }
        // Down opens Settings for everyone; only the legend changes. A TV remote has no X.
        hints.push(if ctx.pads.is_empty() {
            Hint::new(HintKey::Down, "Settings")
        } else {
            Hint::new(HintKey::Tertiary, "Settings")
        });
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
        // Reduced motion drops bump travel, not the chase. Freezing the cursor
        // spring would jump the strip; the refusal is already a Boundary haptic.
        if crate::theme::reduce_motion() {
            self.bump = Spring::rest(0.0);
        }
        // Origin is the cursor, not 0: a restored selection must assemble in place.
        if !self.entrance_armed {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(
                entrances::CARDS,
                self.cursor.max(0) as usize,
                ctx.t,
            ));
        }
        if self.entrance.is_some_and(|e| e.done(ctx.t)) {
            self.entrance = None;
        }

        let w = f64::from(rect.width());
        let tile_w = (TILE_W * k).min(w * 0.84);
        let tile_h = (TILE_H * k)
            .min(f64::from(rect.height()) - 48.0 * k)
            .max(118.0 * k);
        let pitch = tile_w + TILE_GAP * k;
        let cx0 = f64::from(rect.left) + w / 2.0 + self.bump.pos * k;
        let cy = f64::from(rect.top) + f64::from(rect.height()) / 2.0;

        let len = Self::len(ctx.hosts);
        self.geom.clear();
        self.geom.resize(len, Rect::new_empty());
        for i in 0..len {
            let d = i as f64 - self.anim.pos;
            if d.abs() > 2.6 {
                continue;
            }
            let f = 1.0 - d.abs().min(1.0); // 1 at focus → 0 one slot out
            let ent = self
                .entrance
                .map_or(EntranceAt::SETTLED, |e| e.at(i, ctx.t));
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let scale = (0.88 + 0.12 * f) * arrive;
            let alpha = (0.78 + 0.22 * f) * ent.fade;
            let cx = cx0 + d * pitch;
            let cy = cy + (1.0 - ent.travel) * ENTER_RISE * k;
            let tile = Rect::from_xywh(
                (cx - tile_w / 2.0) as f32,
                (cy - tile_h / 2.0) as f32,
                tile_w as f32,
                tile_h as f32,
            );
            // Hit boxes track the drawn tile, entrance included: it is still offset
            // by ENTER_RISE while arriving.
            self.geom[i] = Rect::from_xywh(
                (cx - tile_w * scale / 2.0) as f32,
                (cy - tile_h * scale / 2.0) as f32,
                (tile_w * scale) as f32,
                (tile_h * scale) as f32,
            );
            canvas.save();
            canvas.translate((cx as f32, cy as f32));
            canvas.scale((scale as f32, scale as f32));
            canvas.translate((-cx as f32, -cy as f32));
            // Bounded save_layer only when alpha or recede is not identity.
            // Unbounded allocates a surface-sized offscreen. 36k = halo (outset 4k,
            // sigma 10k) + 10k shadow drop; clip to the tile and both vanish.
            let recede = 1.0 - f;
            let layered = alpha < 0.999 || recede > 0.001;
            if layered {
                let mut lp = crate::theme::layer();
                lp.set_alpha_f(alpha as f32);
                if recede > 0.001 {
                    lp.set_color_filter(skia_safe::color_filters::matrix_row_major(
                        &crate::theme::recede_matrix(recede),
                        None,
                    ));
                }
                let bounds = tile.with_outset(((36.0 * k) as f32, (36.0 * k) as f32));
                canvas.save_layer(
                    &skia_safe::canvas::SaveLayerRec::default()
                        .bounds(&bounds)
                        .paint(&lp),
                );
            }
            // Focus glow under the shadow: a 12% scale step does not read at couch distance.
            crate::theme::focus_halo(canvas, tile, TILE_CORNER as f32, k as f32, f as f32);
            if f > 0.4 {
                crate::theme::drop_shadow(
                    canvas,
                    tile,
                    TILE_CORNER as f32,
                    k as f32,
                    0.45 * f as f32,
                );
            }
            match slot_at(i, ctx.hosts) {
                Slot::Host(h) => draw_host_tile(canvas, fonts, h, tile, k, ctx.t),
                Slot::AddHost => draw_action_tile(canvas, fonts, tile, k, ActionTile::AddHost),
                Slot::Rescan => draw_action_tile(canvas, fonts, tile, k, ActionTile::Rescan),
            }
            // Scrim veil, not black: a pale palette recedes the same way the colour
            // matrix does.
            if f < 1.0 {
                let veil = (1.0 - f) as f32 * 0.07;
                canvas.draw_rrect(
                    RRect::new_rect_xy(tile, (TILE_CORNER * k) as f32, (TILE_CORNER * k) as f32),
                    &fill(crate::theme::shade(veil)),
                );
            }
            if layered {
                canvas.restore();
            }
            canvas.restore();
        }

        if ctx.hosts.is_empty() {
            fonts.centered(
                canvas,
                "Hosts on this network appear automatically — add one by address for everything else.",
                W::Regular,
                13.0 * k,
                fg(0.55),
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
        h.saved.then(|| accent(0.20)),
        if h.saved {
            PanelStroke::Gradient
        } else {
            PanelStroke::GradientDashed
        },
        k as f32,
    );
    crate::theme::panel_highlight(canvas, rect, TILE_CORNER as f32, k as f32);
    let pad = 20.0 * k;
    let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
    draw_badge(canvas, fonts, &h.name, &h.os, h.saved, l, t, k);

    let mut sx = f64::from(rect.right) - pad;
    if h.online {
        let r = 4.5 * k;
        let center = Point::new((sx - r) as f32, (t + 9.0 * k) as f32);
        let mut glow = fill(Color4f::new(
            ONLINE_GREEN.r,
            ONLINE_GREEN.g,
            ONLINE_GREEN.b,
            0.7,
        ));
        glow.set_mask_filter(MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            (5.0 * k) as f32,
            None,
        ));
        canvas.draw_circle(center, r as f32, &glow);
        canvas.draw_circle(center, r as f32, &fill(ONLINE_GREEN));
        sx -= 2.0 * r + 9.0 * k;
    }
    if h.paired {
        draw_lock(canvas, sx - 9.0 * k, t + 4.0 * k, k);
    }

    let max_w = f64::from(rect.width()) - 2.0 * pad;
    let sub_base = f64::from(rect.bottom) - pad;
    match (&h.pin, &h.bound_profile) {
        (Some(p), _) => {
            fonts.draw_clipped(
                canvas,
                &p.name,
                l,
                sub_base,
                W::SemiBold,
                13.0 * k,
                accent_color(p.accent.as_deref()),
                max_w,
            );
        }
        (None, Some(b)) => {
            let addr = format!("{}:{}", h.addr, h.port);
            let addr_w = f64::from(fonts.measure(&addr, W::Regular, 13.0 * k));
            fonts.draw_clipped(
                canvas,
                &addr,
                l,
                sub_base,
                W::Regular,
                13.0 * k,
                fg(0.55),
                max_w,
            );
            let x = l + addr_w + 8.0 * k;
            if x < l + max_w {
                fonts.draw_clipped(
                    canvas,
                    &format!("· {}", b.name),
                    x,
                    sub_base,
                    W::SemiBold,
                    13.0 * k,
                    accent_color(b.accent.as_deref()),
                    l + max_w - x,
                );
            }
        }
        (None, None) => {
            fonts.draw_clipped(
                canvas,
                &format!("{}:{}", h.addr, h.port),
                l,
                sub_base,
                W::Regular,
                13.0 * k,
                fg(0.55),
                max_w,
            );
        }
    }
    fonts.draw_clipped(
        canvas,
        &h.name,
        l,
        sub_base - 22.0 * k,
        W::Bold,
        23.0 * k,
        fg(1.0),
        max_w,
    );
}

/// `#RRGGBB` accent, or the palette accent. A malformed value falls back.
fn accent_color(hex: Option<&str>) -> skia_safe::Color4f {
    let Some(hex) = hex
        .and_then(|a| a.strip_prefix('#'))
        .filter(|h| h.len() == 6)
    else {
        return accent(1.0);
    };
    let Ok(v) = u32::from_str_radix(hex, 16) else {
        return accent(1.0);
    };
    skia_safe::Color4f::new(
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
        1.0,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionTile {
    AddHost,
    Rescan,
}

fn draw_action_tile(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64, kind: ActionTile) {
    crate::theme::panel(
        canvas,
        rect,
        TILE_CORNER as f32,
        None,
        PanelStroke::GradientDashed,
        k as f32,
    );
    crate::theme::panel_highlight(canvas, rect, TILE_CORNER as f32, k as f32);
    let pad = 20.0 * k;
    let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
    let badge = Rect::from_xywh(l as f32, t as f32, (52.0 * k) as f32, (52.0 * k) as f32);
    canvas.draw_rrect(
        RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32),
        &fill(accent(0.16)),
    );
    canvas.draw_rrect(
        RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32),
        &stroke(accent(0.5), 1.0),
    );
    let (bcx, bcy) = (l + 26.0 * k, t + 26.0 * k);
    let mut p = stroke(accent(1.0), (3.0 * k) as f32);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    let r = 9.0 * k;
    match kind {
        ActionTile::AddHost => {
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
        }
        // Static refresh mark. A spinning tile would claim a sweep that is not running.
        ActionTile::Rescan => {
            let mut arc = PathBuilder::new();
            arc.add_arc(
                Rect::from_xywh(
                    (bcx - r) as f32,
                    (bcy - r) as f32,
                    (2.0 * r) as f32,
                    (2.0 * r) as f32,
                ),
                -45.0,
                280.0,
            );
            canvas.draw_path(&arc.detach(), &p);
            let head = 4.6 * k;
            let (hx, hy) = (bcx + r * 0.72, bcy - r * 0.72);
            let mut tip = PathBuilder::new();
            tip.move_to(((hx - head) as f32, (hy - head * 0.2) as f32));
            tip.line_to(((hx + head * 0.5) as f32, (hy - head * 1.1) as f32));
            tip.line_to(((hx + head * 0.2) as f32, (hy + head * 0.7) as f32));
            tip.close();
            canvas.draw_path(&tip.detach(), &fill(accent(1.0)));
        }
    }

    let (title, sub) = match kind {
        ActionTile::AddHost => ("Add Host", "Register a host by address"),
        ActionTile::Rescan => ("Rescan", "Look for hosts on this network again"),
    };
    let max_w = f64::from(rect.width()) - 2.0 * pad;
    let sub_base = f64::from(rect.bottom) - pad;
    fonts.draw_clipped(
        canvas,
        sub,
        l,
        sub_base,
        W::Regular,
        13.0 * k,
        fg(0.55),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        title,
        l,
        sub_base - 22.0 * k,
        W::Bold,
        23.0 * k,
        fg(1.0),
        max_w,
    );
}

/// OS mark when the advertised chain resolves; otherwise the host initial.
/// Substitution, not addition: unknown or empty `os` keeps the monogram. The
/// mark is decorative — the name beside it already states the host.
#[allow(clippy::too_many_arguments)]
fn draw_badge(
    canvas: &Canvas,
    fonts: &Fonts,
    name: &str,
    os: &str,
    filled: bool,
    x: f64,
    y: f64,
    k: f64,
) {
    let badge = Rect::from_xywh(x as f32, y as f32, (52.0 * k) as f32, (52.0 * k) as f32);
    let rr = RRect::new_rect_xy(badge, (15.0 * k) as f32, (15.0 * k) as f32);
    if filled {
        let mut p = crate::theme::shaded();
        let colors = [accent(1.0), accent(0.68)];
        p.set_shader(skia_safe::gradient::shaders::linear_gradient(
            (
                Point::new(badge.left, badge.top),
                Point::new(badge.left, badge.bottom),
            ),
            &skia_safe::gradient::Gradient::new(
                skia_safe::gradient::Colors::new_evenly_spaced(
                    &colors,
                    skia_safe::TileMode::Clamp,
                    None,
                ),
                skia_safe::gradient::Interpolation::default(),
            ),
            None,
        ));
        canvas.draw_rrect(rr, &p);
    } else {
        canvas.draw_rrect(rr, &fill(accent(0.16)));
        canvas.draw_rrect(rr, &stroke(accent(0.5), 1.0));
    }
    let ink = if filled { fg(1.0) } else { accent(1.0) };
    // ~54% of the badge so the mark sits on it, not cropped to it. `os_mark`
    // letterboxes a non-square master.
    let side = 28.0 * k;
    let inner = Rect::from_xywh(
        (x + 26.0 * k - side / 2.0) as f32,
        (y + 26.0 * k - side / 2.0) as f32,
        side as f32,
        side as f32,
    );
    if let Some(path) = crate::os_marks::os_mark(os, inner) {
        canvas.draw_path(&path, &fill(ink));
        return;
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
        ink,
    );
}

fn draw_lock(canvas: &Canvas, x: f64, y: f64, k: f64) {
    let ink = fg(0.5);
    let body_w = 11.0 * k;
    let body_h = 8.0 * k;
    let body_top = y + 5.0 * k;
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(x as f32, body_top as f32, body_w as f32, body_h as f32),
            (2.0 * k) as f32,
            (2.0 * k) as f32,
        ),
        &fill(ink),
    );
    let p = stroke(ink, (1.6 * k) as f32);
    let mut shackle = PathBuilder::new();
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
    canvas.draw_path(&shackle.detach(), &p);
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
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_profile: None,
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
        s.cursor = 1;
        s.keys = vec!["a".into(), "b".into(), ADD_KEY.into()];
        // A new host inserted in front; focus must stay on "b".
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
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();

        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        };
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.connect.is_some());

        let mut fx = Outbox::default();
        s.cursor = 1;
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(_))));
        assert!(fx.connect.is_none());

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

    /// D-pad, OK, and Back must reach Settings and the host menu. With no pad the
    /// legend names Down, not X.
    #[test]
    fn a_remote_reaches_settings_and_options_without_face_buttons() {
        let mut settings = ctx_settings();
        let hosts = [host("paired", true, true, false)];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Android,
            pads: &pads,
            deck: false,
            fallback_ui: true,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();

        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(ref sc)) if matches!(**sc, Screen::Settings(_))),
            "down must open Settings"
        );
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(ref sc)) if matches!(**sc, Screen::HostOptions(_))),
            "up must open the host options menu"
        );
        assert!(
            s.hints(&ctx).iter().any(|h| h.key == HintKey::Down),
            "a padless device is told about down"
        );
        let pads = vec![pf_client_core::menu_nav::PadInfo {
            name: "Pad".into(),
            key: "045e:028e:Pad".into(),
            pref: punktfunk_core::config::GamepadPref::Xbox360,
            steam_virtual: false,
            battery: None,
            detail: "045E:028E · gamepad".into(),
            forwarded: true,
            rumble: false,
        }];
        ctx.pads = &pads;
        assert!(
            s.hints(&ctx).iter().any(|h| h.key == HintKey::Tertiary),
            "a pad is told about X"
        );
    }

    /// A pin's Confirm connects with that profile (one-off); the overlay title
    /// names the host and the profile.
    #[test]
    fn pinned_card_connects_with_its_profile() {
        let mut settings = ctx_settings();
        let mut pinned = host("ab\0p1", true, true, false);
        pinned.name = "Tower".into();
        pinned.pin = Some(crate::model::ProfileChip {
            id: "p1".into(),
            name: "Work".into(),
            accent: None,
        });
        let hosts = [pinned];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        let intent = fx.connect.expect("a pinned card connects");
        assert_eq!(intent.profile.as_deref(), Some("p1"));
        assert_eq!(intent.title, "Tower · Work");
    }

    #[test]
    fn add_tile_is_always_last() {
        let mut settings = ctx_settings();
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
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
