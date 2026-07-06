//! The hosts page: saved (trusted/paired) hosts and live mDNS discovery as tap-to-connect
//! tiles in a responsive grid, with a per-host "…" menu (connect / speed test / rename /
//! forget) and a manual connect entry — the same card layout as the Linux and Apple clients.

use super::connect::{initiate, wake_and_connect};
use super::speed::SpeedState;
use super::style::*;
use super::{Screen, Svc, Target};
use crate::discovery::DiscoveredHost;
use crate::trust::KnownHosts;
use windows_reactor::*;

/// Overflow-menu item labels — `on_item_clicked` reports the clicked item by its text.
const MENU_CONNECT: &str = "Connect";
const MENU_SPEED: &str = "Test network speed\u{2026}";
const MENU_WAKE: &str = "Wake host";
const MENU_RENAME: &str = "Rename\u{2026}";
const MENU_FORGET: &str = "Forget\u{2026}";

/// Tile-grid metrics: minimum tile width before dropping a column, and the gap between tiles.
const TILE_MIN_WIDTH: f64 = 320.0;
const TILE_GAP: f64 = 12.0;

/// Props for the hosts page: the services plus the changing discovery/status data that must
/// drive its re-render (compared by value, so a new host list or error refreshes the page).
///
/// `forget` and `rename` are the per-host action state, and they live in ROOT (not this page's
/// own `use_state`) on purpose: the "…" overflow is a WinUI `MenuFlyout`, whose item clicks are
/// wired directly in the reactor backend (`add_Click`) and so bypass the normal event-dispatch
/// flush — a *sync* child `SetState` from that handler marks state dirty but never pumps the
/// reconciler, so nothing re-renders. Root `AsyncSetState` re-renders the whole tree; because
/// these values are props, the changed value propagates back into this page (a child's own async
/// state would be memoised away when its props are unchanged). `(fp_hex, _)` in each identifies
/// the target saved host; `rename`'s second field is the in-progress draft name.
#[derive(Clone)]
pub(crate) struct HostsProps {
    pub(crate) svc: Svc,
    pub(crate) hosts: Vec<DiscoveredHost>,
    pub(crate) status: String,
    pub(crate) forget: Option<(String, String)>,
    pub(crate) rename: Option<(String, String)>,
    /// Whether the "Add host" modal is open. Root state (like `forget`/`rename`), not the page's
    /// own `use_state`: a child component's sync `SetState` marks its slot dirty but does not
    /// re-render when its props are otherwise unchanged, so the toggle wouldn't take.
    pub(crate) show_add: bool,
    /// The modal's entrance-tween progress (0 → 1, root-driven): opacity + slide-up offset.
    pub(crate) add_anim: f64,
    /// The hovered tile's stable id (saved: fp_hex, discovered: `addr:port`) — root state because
    /// the pointer enter/exit handlers bypass the reconciler flush, like the flyout clicks above.
    pub(crate) hover: Option<String>,
    pub(crate) set_forget: AsyncSetState<Option<(String, String)>>,
    pub(crate) set_rename: AsyncSetState<Option<(String, String)>>,
    pub(crate) set_show_add: AsyncSetState<bool>,
    pub(crate) set_hover: AsyncSetState<Option<String>>,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        // Setters are identity-stable; only the value fields drive re-render.
        self.svc == other.svc
            && self.hosts == other.hosts
            && self.status == other.status
            && self.forget == other.forget
            && self.rename == other.rename
            && self.show_add == other.show_add
            && self.add_anim == other.add_anim
            && self.hover == other.hover
    }
}

/// A host tile. The tap-to-connect summary (monogram, name, address, status row) and the
/// optional "…" menu button are SIBLINGS overlaid in one grid cell, never nested: WinUI bubbles
/// `Tapped` out of buttons (reactor doesn't mark it handled), so a button inside the tap target
/// would fire both its own click and the tile's connect (the old forget-also-connects bug).
///
/// Hover renders the WinUI card pointer-over look — the card background lifts to the control
/// hover fill while the pointer is inside the tile (tracked via `hover`, see `HostsProps`).
fn host_tile(
    id: &str,
    hover: &Hover,
    name: &str,
    sub: &str,
    status_row: Element,
    menu: Option<Button>,
    on_tap: Option<Box<dyn Fn()>>,
) -> Element {
    let mut summary = border(
        vstack((
            avatar(name)
                .width(44.0)
                .height(44.0)
                .horizontal_alignment(HorizontalAlignment::Left),
            text_block(name)
                .font_size(15.0)
                .semibold()
                .margin(edges(0.0, 12.0, 0.0, 0.0)),
            text_block(sub)
                .font_size(12.0)
                .font_family("Consolas")
                .foreground(ThemeRef::SecondaryText)
                .margin(edges(0.0, 2.0, 0.0, 0.0)),
            status_row,
        ))
        .spacing(0.0),
    )
    .background(hit_test_backstop())
    .padding(uniform(18.0));
    if let Some(f) = on_tap {
        summary = summary.on_tapped(f);
    }

    let mut children: Vec<Element> = vec![summary.into()];
    if let Some(m) = menu {
        children.push(
            m.horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(0.0, 8.0, 8.0, 0.0))
                .into(),
        );
    }
    let mut tile = card_flush(grid(children));
    if hover.current.as_deref() == Some(id) {
        tile = tile.background(ThemeRef::ControlFillSecondary);
    }
    let enter = {
        let (set, id) = (hover.set.clone(), id.to_string());
        move |_: PointerEventInfo| set.call(Some(id.clone()))
    };
    let exit = {
        let set = hover.set.clone();
        move || set.call(None)
    };
    tile.on_pointer_entered(enter)
        .on_pointer_exited(exit)
        .into()
}

/// The hover-tracking pair `host_tile` needs: the currently hovered tile id + its root setter.
pub(crate) struct Hover {
    pub(crate) current: Option<String>,
    pub(crate) set: AsyncSetState<Option<String>>,
}

/// The status row at the bottom of a tile: presence dot + Online/Offline, plus the trust chip.
fn status_row(online: Option<bool>, badge: &str, kind: Pill) -> Element {
    let mut items: Vec<Element> = Vec::new();
    if let Some(online) = online {
        items.push(
            presence_dot(online)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        items.push(
            text_block(if online { "Online" } else { "Offline" })
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    items.push(
        pill(badge, kind)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    );
    hstack(items)
        .spacing(6.0)
        .margin(edges(0.0, 12.0, 0.0, 0.0))
        .into()
}

/// Lay tiles into a `cols`-wide grid of equal-width star columns (rows share the height of
/// their tallest tile, so a grid row always lines up).
fn tile_grid(tiles: Vec<Element>, cols: usize) -> Element {
    let rows = tiles.len().div_ceil(cols);
    let mut children = Vec::with_capacity(tiles.len());
    for (i, t) in tiles.into_iter().enumerate() {
        children.push(t.grid_row((i / cols) as i32).grid_column((i % cols) as i32));
    }
    grid(children)
        .columns(vec![GridLength::Star(1.0); cols])
        .rows(vec![GridLength::Auto; rows])
        .column_spacing(TILE_GAP)
        .row_spacing(TILE_GAP)
        .into()
}

/// The in-tile rename editor (ContentDialog can't hold a text field): name box + save/cancel.
/// No tap-to-connect while editing — a click into the box would bubble `Tapped` to the region.
fn rename_editor(
    draft: &str,
    fp: String,
    set_rename: AsyncSetState<Option<(String, String)>>,
) -> Element {
    let commit = {
        let (fp, draft, sr) = (fp.clone(), draft.to_string(), set_rename.clone());
        move || {
            let name = draft.trim();
            if !name.is_empty() {
                let mut known = KnownHosts::load();
                if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp) {
                    h.name = name.to_string();
                }
                let _ = known.save();
            }
            sr.call(None);
        }
    };
    let on_changed = {
        let sr = set_rename.clone();
        move |s: String| sr.call(Some((fp.clone(), s)))
    };
    card(
        vstack((
            text_box(draft)
                .placeholder_text("Host name")
                .on_text_changed(on_changed),
            hstack((
                button("Save")
                    .accent()
                    .icon(Symbol::Accept)
                    .on_click(commit),
                button("Cancel")
                    .subtle()
                    .on_click(move || set_rename.call(None)),
            ))
            .spacing(4.0),
        ))
        .spacing(10.0),
    )
    .into()
}

pub(crate) fn hosts_page(props: &HostsProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let hosts = props.hosts.as_slice();
    let status = props.status.as_str();
    let set_screen = &props.svc.set_screen;
    let set_status = &props.svc.set_status;
    let (manual, set_manual) = cx.use_state(String::new());
    // "Add host" modal open state lives in ROOT (see `HostsProps`).
    let show_add = props.show_add;
    let set_show_add = &props.set_show_add;
    // Forget confirmation and in-progress rename live in ROOT state (see `HostsProps`) — the
    // overflow menu's flyout clicks can't re-render off a sync setter. Both are `(fp_hex, _)`.
    let forget = props.forget.clone();
    let rename = props.rename.clone();
    let set_forget = &props.set_forget;
    let set_rename = &props.set_rename;
    let hover = Hover {
        current: props.hover.clone(),
        set: props.set_hover.clone(),
    };
    let known = KnownHosts::load();

    // Responsive column count from the live window width (re-renders on resize): as many
    // TILE_MIN_WIDTH columns as fit the page's content width, at least one.
    let window = cx.use_inner_size();
    let content_w = (window.width - 64.0).clamp(TILE_MIN_WIDTH, 1120.0);
    let cols = (((content_w + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP)).floor() as usize).max(1);

    let mut body: Vec<Element> = Vec::new();

    // Header: title block + Settings button.
    body.push(
        grid((
            vstack((
                text_block("Punktfunk").font_size(30.0).bold(),
                text_block("Stream from a host on your network.")
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
            hstack((
                button("Add host").accent().icon(Symbol::Add).on_click({
                    let sa = set_show_add.clone();
                    move || sa.call(true)
                }),
                button("Settings").icon(Symbol::Setting).on_click({
                    let ss = set_screen.clone();
                    move || ss.call(Screen::Settings)
                }),
            ))
            .spacing(8.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .margin(edges(0.0, 0.0, 0.0, 10.0))
        .into(),
    );

    if !status.is_empty() {
        body.push(
            InfoBar::new("Couldn't connect")
                .message(status.to_string())
                .error()
                .is_closable(false)
                .into(),
        );
    }

    // Saved (trusted/paired) hosts — reachable even when mDNS isn't. A saved host that's also
    // being advertised right now shows as Online (and is deduped out of the discovery section).
    if !known.hosts.is_empty() {
        body.push(section("SAVED HOSTS"));
        let mut tiles: Vec<Element> = Vec::new();
        for k in &known.hosts {
            // Rust 2021 (no let-chains): match the "this tile is being renamed" case explicitly.
            if matches!(&rename, Some((fp, _)) if fp == &k.fp_hex) {
                let (fp, draft) = rename.clone().unwrap();
                tiles.push(rename_editor(&draft, fp, set_rename.clone()));
                continue;
            }
            let target = Target {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                pair_optional: false,
                mac: k.mac.clone(),
            };
            let online = hosts
                .iter()
                .any(|h| h.fp_hex == k.fp_hex || (h.addr == k.addr && h.port == k.port));
            // Learn this host's wake MAC(s) from its live advert while it's online, so we can wake
            // it once it sleeps (no-op / no disk write when unchanged).
            if let Some(a) = hosts.iter().find(|h| {
                (h.fp_hex == k.fp_hex || (h.addr == k.addr && h.port == k.port))
                    && !h.mac.is_empty()
            }) {
                crate::trust::learn_mac(&k.fp_hex, &k.addr, k.port, &a.mac);
            }
            let can_wake = !online && !k.mac.is_empty();
            let menu = {
                let (svc, target) = (props.svc.clone(), target.clone());
                let (sf, sr) = (set_forget.clone(), set_rename.clone());
                let (fp, name) = (k.fp_hex.clone(), k.name.clone());
                button("")
                    .icon(Symbol::More)
                    .subtle()
                    .tooltip("More options")
                    .automation_name("More options")
                    .menu_flyout({
                        let mut items = vec![menu_item(MENU_CONNECT), menu_item(MENU_SPEED)];
                        // Offer an explicit wake only when the host is offline and we have a MAC.
                        if can_wake {
                            items.push(menu_item(MENU_WAKE));
                        }
                        items.push(menu_item(MENU_RENAME));
                        items.push(menu_separator());
                        items.push(menu_item(MENU_FORGET));
                        items
                    })
                    .on_item_clicked(move |item: String| match item.as_str() {
                        MENU_CONNECT => {
                            initiate(&svc.ctx, target.clone(), &svc.set_screen, &svc.set_status)
                        }
                        MENU_WAKE => crate::wol::wake(&target.mac, target.addr.parse().ok()),
                        MENU_SPEED => {
                            *svc.ctx.shared.target.lock().unwrap() = target.clone();
                            // New run: invalidate any still-in-flight probe, reset the screen.
                            svc.ctx
                                .shared
                                .speed_gen
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            svc.set_speed.call(SpeedState::Running);
                            svc.set_screen.call(Screen::SpeedTest);
                        }
                        MENU_RENAME => sr.call(Some((fp.clone(), name.clone()))),
                        MENU_FORGET => sf.call(Some((fp.clone(), name.clone()))),
                        _ => {}
                    })
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            tiles.push(host_tile(
                &k.fp_hex,
                &hover,
                &k.name,
                &format!("{}:{}", k.addr, k.port),
                status_row(
                    Some(online),
                    if k.paired { "Paired" } else { "Trusted" },
                    if k.paired { Pill::Good } else { Pill::Info },
                ),
                Some(menu),
                Some(Box::new(move || {
                    // Offline saved host with a known MAC: wake it and WAIT for it to reappear on
                    // the network (re-sending periodically) before dialing — a cold box boots far
                    // slower than a connect will sit. An online host dials straight away.
                    if can_wake {
                        wake_and_connect(&ctx2, target.clone(), &ss, &st);
                    } else {
                        initiate(&ctx2, target.clone(), &ss, &st);
                    }
                })),
            ));
        }
        body.push(tile_grid(tiles, cols));
    }

    // Discovered hosts not already saved above.
    body.push(section("ON THIS NETWORK"));
    let discovered: Vec<&DiscoveredHost> = hosts
        .iter()
        .filter(|h| {
            !known.hosts.iter().any(|k| {
                (!h.fp_hex.is_empty() && k.fp_hex == h.fp_hex)
                    || (k.addr == h.addr && k.port == h.port)
            })
        })
        .collect();
    if discovered.is_empty() {
        body.push(
            card(
                hstack((
                    ProgressRing::indeterminate().width(18.0).height(18.0),
                    text_block("Searching the LAN\u{2026}").foreground(ThemeRef::SecondaryText),
                ))
                .spacing(12.0),
            )
            .into(),
        );
    } else {
        let mut tiles: Vec<Element> = Vec::new();
        for h in discovered {
            let target = Target {
                name: h.name.clone(),
                addr: h.addr.clone(),
                port: h.port,
                fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
                pair_optional: h.pair == "optional",
                mac: h.mac.clone(),
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            let (badge, kind) = if h.pair == "required" {
                ("PIN", Pill::Info)
            } else {
                ("Open", Pill::Neutral)
            };
            tiles.push(host_tile(
                &format!("{}:{}", h.addr, h.port),
                &hover,
                &h.name,
                &format!("{}:{}", h.addr, h.port),
                status_row(None, badge, kind),
                None,
                Some(Box::new(move || initiate(&ctx2, target.clone(), &ss, &st))),
            ));
        }
        body.push(tile_grid(tiles, cols));
    }

    // Forget confirmation (modal; shown while `forget` holds a pending host). Confirmed first,
    // since it's destructive and re-establishing trust needs a fresh pairing.
    if let Some((fp, name)) = forget {
        let sf = set_forget.clone();
        body.push(
            ContentDialog::new("Remove saved host?")
                .content(format!(
                    "Forget \u{201C}{name}\u{201D}? You'll need to pair (or trust) it again to \
                     reconnect."
                ))
                .primary_button_text("Remove")
                .close_button_text("Cancel")
                .is_open(true)
                .on_closed(move |r: ContentDialogResult| {
                    if r == ContentDialogResult::Primary {
                        let mut known = KnownHosts::load();
                        known.remove_by_fp(&fp);
                        let _ = known.save();
                    }
                    sf.call(None); // re-renders the page; the row is gone on the next load
                })
                .into(),
        );
    }

    let page = page_wide(body);
    if !show_add {
        return page;
    }

    // "Add host" modal: a scrim + centered card. It's an in-tree overlay, not a WinUI
    // ContentDialog, because ContentDialog is text-only in windows-reactor (no room for a text
    // field). The scrim border fills the cell and is hit-testable, so it blocks the page behind;
    // it closes only via Cancel/Connect (a scrim tap would bubble `Tapped` up from the card too).
    let connect_manual = {
        let (ctx2, ss, st, text, sa) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            manual.clone(),
            set_show_add.clone(),
        );
        move || {
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let (addr, port) = match text.rsplit_once(':') {
                Some((a, p)) => (a.to_string(), p.parse().unwrap_or(9777)),
                None => (text.to_string(), 9777),
            };
            sa.call(false);
            initiate(
                &ctx2,
                Target {
                    name: addr.clone(),
                    addr,
                    port,
                    fp_hex: None,
                    pair_optional: false,
                    mac: Vec::new(),
                },
                &ss,
                &st,
            );
        }
    };
    let modal = dialog_surface(
        vstack((
            text_block("Add a host").font_size(20.0).bold(),
            text_block(
                "Enter the host's IP address or name. Append :port only for a non-standard port \
                 (the default is 9777).",
            )
            .font_size(13.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText),
            text_box(manual)
                .header("Address")
                .placeholder_text("192.168.1.20  or  my-pc.local")
                .on_text_changed(move |s| set_manual.call(s))
                .margin(edges(0.0, 6.0, 0.0, 0.0)),
            hstack((
                button("Connect")
                    .accent()
                    .icon(Symbol::Forward)
                    .on_click(connect_manual),
                button("Cancel").on_click({
                    let sa = set_show_add.clone();
                    move || sa.call(false)
                }),
            ))
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right)
            .margin(edges(0.0, 6.0, 0.0, 0.0)),
        ))
        .spacing(12.0),
    )
    .max_width(460.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    // Entrance: fade + slide up, driven by the root tween (`add_anim` 0 → 1). The card starts
    // a bit low and rises to centre — for a centred element, extra top margin shifts it down by
    // half the difference, so the offset is doubled.
    .opacity(props.add_anim)
    .margin(edges(
        24.0,
        24.0 + (1.0 - props.add_anim) * 56.0,
        24.0,
        24.0,
    ));

    // The scrim fades in with the same tween.
    let scrim = border(modal).background(Color {
        a: (140.0 * props.add_anim) as u8,
        r: 0,
        g: 0,
        b: 0,
    });
    grid(vec![page, scrim.into()]).into()
}
