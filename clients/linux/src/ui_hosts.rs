//! The hosts page: adaptive card grids for saved (trusted/paired) and mDNS-discovered
//! hosts, matching the other clients' look — avatar + name + `addr:port` + status pills,
//! online pips on saved cards, dashed discovered cards, an overflow menu, an add-host
//! dialog, and a connect-failure banner. Both grids re-render from one state snapshot
//! (known hosts on disk + the live advert map), so dedup and the online pips stay
//! consistent on every change.

use crate::discovery::{self, DiscoveredHost, DiscoveryEvent};
use crate::trust::{KnownHost, KnownHosts, Settings};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// What the user asked to connect to. `fp_hex` comes from the mDNS TXT record when the
/// host was discovered (drives the trust decision *before* connecting); manual entries have
/// none. `pair_optional` is true ONLY when a discovered host advertised `pair=optional`,
/// which is the sole case in which the reduced-security TOFU path may be offered — every
/// other case (pair=required, unknown/empty policy, manual entry) mandates PIN pairing.
#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub fp_hex: Option<String>,
    pub pair_optional: bool,
    /// A library title to launch on connect (`(library id, display name)`, e.g.
    /// `("steam:570", "Dota 2")`) — set by the library page's card activation; the id
    /// rides the Hello and the name titles the stream page. `None` = plain desktop session.
    pub launch: Option<(String, String)>,
    /// Wake-on-LAN MAC(s) for this host (from the saved store or the live advert). Used to send a
    /// magic packet before connecting to an offline host. Empty when none is known.
    pub mac: Vec<String>,
}

impl ConnectRequest {
    /// The key the hosts page tracks an in-flight connect under (the card that swaps its
    /// avatar for a spinner): the fingerprint when known, else the address.
    pub fn card_key(&self) -> String {
        self.fp_hex
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.addr, self.port))
    }
}

/// The actions the page hands off to the app shell (trust gate, speed test, PIN pairing,
/// the library browser).
pub struct HostsCallbacks {
    pub on_connect: Rc<dyn Fn(ConnectRequest)>,
    /// Connect to an OFFLINE saved host with a known MAC: wake it, poll until it's up (re-keying a
    /// new DHCP IP), then connect. Falls back to `on_connect` when there's nothing to wake.
    pub on_wake_connect: Rc<dyn Fn(ConnectRequest)>,
    pub on_speed_test: Rc<dyn Fn(ConnectRequest)>,
    pub on_pair: Rc<dyn Fn(ConnectRequest)>,
    pub on_library: Rc<dyn Fn(ConnectRequest)>,
}

/// The page plus the handle the launch path drives: connect-failure banner and the
/// per-card connecting spinner. Held by the `App` (`app.hosts_ui()`).
pub struct HostsUi {
    pub page: adw::NavigationPage,
    state: Rc<State>,
}

impl HostsUi {
    /// Surface a connect failure at the top of the page (dismissible; replaces raw-error toasts).
    pub fn show_error(&self, msg: &str) {
        self.state.banner.set_title(msg);
        self.state.banner.set_revealed(true);
    }

    pub fn clear_error(&self) {
        self.state.banner.set_revealed(false);
    }

    /// Mark the card matching `key` (see `ConnectRequest::card_key`) as connecting —
    /// spinner in place of the avatar, card insensitive. `None` restores all cards.
    pub fn set_connecting(&self, key: Option<String>) {
        *self.state.connecting.borrow_mut() = key;
        rebuild(&self.state);
    }

    /// Feed one advert through the same path the mDNS stream uses (CI screenshot scenes).
    pub fn inject_advert(&self, host: DiscoveredHost) {
        self.state
            .adverts
            .borrow_mut()
            .insert(host.key.clone(), host);
        rebuild(&self.state);
    }

    /// The "+" add-host dialog (name optional / address / port), also reachable from the
    /// empty state. Reuses the manual-connect plumbing: submit runs the trust gate.
    pub fn show_add_host(&self) {
        add_host_dialog(&self.state);
    }

    /// Re-render both grids (e.g. the library toggle changed in Preferences, which adds/
    /// removes the saved cards' "Browse library…" menu item).
    pub fn refresh(&self) {
        rebuild(&self.state);
    }

    /// The advertised mgmt port for the host `req` points at, when a matching live advert
    /// carries the `mgmt` TXT — the library client's port override (default otherwise).
    pub fn mgmt_port_for(&self, req: &ConnectRequest) -> Option<u16> {
        let adverts = self.state.adverts.borrow();
        adverts
            .values()
            .find(|a| {
                req.fp_hex
                    .as_deref()
                    .is_some_and(|fp| !a.fp_hex.is_empty() && a.fp_hex == fp)
                    || (a.addr == req.addr && a.port == req.port)
            })
            .and_then(|a| a.mgmt_port)
    }
}

/// Everything the grids re-render from, plus the widgets they render into.
struct State {
    stack: gtk::Stack,
    banner: adw::Banner,
    saved_heading: gtk::Label,
    saved_flow: gtk::FlowBox,
    disc_flow: gtk::FlowBox,
    searching: gtk::Box,
    /// Live mDNS adverts, keyed by the advert key — the source for the discovered grid,
    /// the saved cards' online pips, and dedup.
    adverts: RefCell<HashMap<String, DiscoveredHost>>,
    /// `card_key` of the connect currently in flight, if any.
    connecting: RefCell<Option<String>>,
    /// App settings — read on every rebuild for the experimental library-item gate.
    settings: Rc<RefCell<Settings>>,
    cbs: HostsCallbacks,
}

pub fn new(settings: Rc<RefCell<Settings>>, cbs: HostsCallbacks) -> HostsUi {
    let make_flow = || {
        gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .homogeneous(true)
            .min_children_per_line(1)
            .max_children_per_line(4)
            .column_spacing(12)
            .row_spacing(12)
            .build()
    };
    let heading = |text: &str| {
        let l = gtk::Label::new(Some(text));
        l.add_css_class("heading");
        l.set_halign(gtk::Align::Start);
        l
    };
    let saved_heading = heading("Saved hosts");
    let saved_flow = make_flow();
    let disc_heading = heading("On this network");
    let disc_flow = make_flow();

    // A pointer click (and keyboard activate) emits `child-activated` on the *FlowBox*, never
    // the child's own `activate` signal — so bridge it back to the child, where each card wires
    // its connect handler (`saved_card`/`discovered_card`). Without this, clicking a card is dead.
    //
    // `child.activate()` in turn runs `GtkFlowBoxChild`'s own default handler, which re-emits
    // `child-activated` on the FlowBox — bouncing straight back into this closure. Unguarded,
    // that ping-pong recurses forever and overflows the stack on every single card click/Enter
    // (a real crash seen live, not hypothetical); the re-entrancy flag breaks the cycle after
    // the one real activation.
    for flow in [&saved_flow, &disc_flow] {
        let activating = std::cell::Cell::new(false);
        flow.connect_child_activated(move |_, child| {
            if activating.replace(true) {
                return;
            }
            child.activate();
            activating.set(false);
        });
    }

    // Shown under the discovered heading while no (unsaved) advert is live yet.
    let searching = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spinner = gtk::Spinner::new();
    spinner.start();
    searching.append(&spinner);
    let searching_label = gtk::Label::new(Some("Searching the LAN…"));
    searching_label.add_css_class("dim-label");
    searching.append(&searching_label);
    searching.set_margin_top(6);
    searching.set_margin_bottom(6);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&saved_heading);
    content.append(&saved_flow);
    content.append(&disc_heading);
    content.append(&searching);
    content.append(&disc_flow);

    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .child(&content)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    // No saved hosts AND nothing on the LAN → the whole page is the empty state.
    let empty = adw::StatusPage::builder()
        .icon_name("network-workgroup-symbolic")
        .title("No hosts yet")
        .description("Hosts on your network appear here automatically.\nAdd one by address with +.")
        .build();
    let add_btn = gtk::Button::with_label("Add host");
    add_btn.add_css_class("pill");
    add_btn.add_css_class("suggested-action");
    add_btn.set_halign(gtk::Align::Center);
    add_btn.set_action_name(Some("win.add-host"));
    empty.set_child(Some(&add_btn));

    let stack = gtk::Stack::new();
    stack.add_named(&scrolled, Some("grid"));
    stack.add_named(&empty, Some("empty"));

    // Connect failures land here (launch.rs routes on_failed/on_ended), not in toasts.
    let banner = adw::Banner::new("");
    banner.set_button_label(Some("Dismiss"));
    banner.connect_button_clicked(|b| b.set_revealed(false));

    let header = adw::HeaderBar::new();
    let add_host_btn = gtk::Button::from_icon_name("list-add-symbolic");
    add_host_btn.set_tooltip_text(Some("Add host"));
    add_host_btn.set_action_name(Some("win.add-host"));
    header.pack_start(&add_host_btn);
    // Primary menu — the actions live on the window (installed in app.rs).
    let menu = gio::Menu::new();
    menu.append(Some("Preferences"), Some("win.preferences"));
    menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    menu.append(Some("About Punktfunk"), Some("win.about"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    header.pack_end(&menu_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));

    let page = adw::NavigationPage::builder()
        .title("Punktfunk")
        .tag("hosts")
        .child(&toolbar)
        .build();

    let state = Rc::new(State {
        stack,
        banner,
        saved_heading,
        saved_flow,
        disc_flow,
        searching,
        adverts: RefCell::new(HashMap::new()),
        connecting: RefCell::new(None),
        settings,
        cbs,
    });
    rebuild(&state);

    // Rebuilt every time the page is shown, so fresh TOFU/pairing entries appear on return.
    {
        let state = state.clone();
        page.connect_shown(move |_| rebuild(&state));
    }

    // Stream mDNS adverts into the map; every add/remove re-evaluates both grids (online
    // pips + dedup included).
    {
        let rx = discovery::browse();
        let weak = Rc::downgrade(&state);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(state) = weak.upgrade() else { break };
                match event {
                    DiscoveryEvent::Resolved(h) => {
                        state.adverts.borrow_mut().insert(h.key.clone(), h);
                    }
                    DiscoveryEvent::Removed { fullname } => {
                        state
                            .adverts
                            .borrow_mut()
                            .retain(|_, a| a.fullname != fullname);
                    }
                }
                rebuild(&state);
            }
        });
    }

    HostsUi { page, state }
}

/// Re-render both grids from disk + the advert map. Cheap (a handful of widgets) and
/// keeps every derived view — online pips, dedup, most-recent accent, spinner — in one
/// straight-line pass instead of incremental row surgery.
fn rebuild(state: &Rc<State>) {
    let known = KnownHosts::load();
    let adverts = state.adverts.borrow();
    let connecting = state.connecting.borrow().clone();

    // A saved host is ONLINE iff a live advert matches it (fingerprint, or address when
    // the advert carries no fp) — same rule the Apple client uses.
    let matches = |k: &KnownHost, a: &DiscoveredHost| {
        (!a.fp_hex.is_empty() && a.fp_hex == k.fp_hex) || (a.addr == k.addr && a.port == k.port)
    };
    let most_recent = known
        .hosts
        .iter()
        .filter_map(|h| h.last_used.map(|t| (h.fp_hex.clone(), t)))
        .max_by_key(|&(_, t)| t)
        .map(|(fp, _)| fp);

    state.saved_flow.remove_all();
    for k in &known.hosts {
        let online = adverts.values().any(|a| matches(k, a));
        // Learn this host's wake MAC(s) from its live advert while it's online, so we can wake it
        // once it sleeps and stops advertising (no-op / no disk write when unchanged).
        if let Some(a) = adverts
            .values()
            .find(|a| matches(k, a) && !a.mac.is_empty())
        {
            crate::trust::learn_mac(&k.fp_hex, &k.addr, k.port, &a.mac);
        }
        let recent = most_recent.as_deref() == Some(k.fp_hex.as_str());
        state
            .saved_flow
            .append(&saved_card(state, k, online, recent, connecting.as_deref()));
    }

    // The discovered grid only surfaces genuinely-new hosts: anything matching a saved
    // entry renders as that saved card (with its pip now green) instead.
    let mut fresh: Vec<&DiscoveredHost> = adverts
        .values()
        .filter(|a| !known.hosts.iter().any(|k| matches(k, a)))
        .collect();
    fresh.sort_by(|a, b| a.name.cmp(&b.name).then(a.key.cmp(&b.key)));
    state.disc_flow.remove_all();
    for a in &fresh {
        state
            .disc_flow
            .append(&discovered_card(state, a, connecting.as_deref()));
    }

    let have_saved = !known.hosts.is_empty();
    let have_disc = !fresh.is_empty();
    state.saved_heading.set_visible(have_saved);
    state.saved_flow.set_visible(have_saved);
    state.disc_flow.set_visible(have_disc);
    state.searching.set_visible(!have_disc);
    state
        .stack
        .set_visible_child_name(if have_saved || have_disc {
            "grid"
        } else {
            "empty"
        });
}

/// The shared card scaffold: avatar (or a spinner while connecting) over name over
/// `addr:port` over a status row, in a `.card` overlay (the overlay hosts the saved
/// card's corner menu). Returned as the FlowBox child so callers wire activation on it.
fn card_scaffold(
    name: &str,
    addr_line: &str,
    status_row: &gtk::Box,
    connecting: bool,
) -> (gtk::FlowBoxChild, gtk::Overlay) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    if connecting {
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(48, 48);
        spinner.start();
        spinner.set_halign(gtk::Align::Center);
        content.append(&spinner);
    } else {
        let avatar = adw::Avatar::new(48, Some(name), true);
        avatar.set_halign(gtk::Align::Center);
        content.append(&avatar);
    }
    let name_label = gtk::Label::new(Some(name));
    name_label.add_css_class("heading");
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&name_label);
    let addr_label = gtk::Label::new(Some(addr_line));
    addr_label.add_css_class("caption");
    addr_label.add_css_class("dim-label");
    addr_label.add_css_class("numeric");
    addr_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&addr_label);
    status_row.set_halign(gtk::Align::Center);
    status_row.set_margin_top(4);
    content.append(status_row);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&content));
    overlay.add_css_class("card");
    overlay.add_css_class("pf-host-card");

    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&overlay));
    if connecting {
        child.set_sensitive(false);
    }
    (child, overlay)
}

/// A small rounded status chip (`.pf-pill` + a colour variant class).
fn pill(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("pf-pill");
    l.add_css_class(class);
    l
}

fn saved_card(
    state: &Rc<State>,
    k: &KnownHost,
    online: bool,
    recent: bool,
    connecting: Option<&str>,
) -> gtk::FlowBoxChild {
    let req = ConnectRequest {
        name: k.name.clone(),
        addr: k.addr.clone(),
        port: k.port,
        fp_hex: Some(k.fp_hex.clone()),
        // Saved host: its fp is already pinned, so this routes to a silent pinned
        // connect; TOFU eligibility is irrelevant.
        pair_optional: false,
        launch: None,
        mac: k.mac.clone(),
    };

    // Presence pip + spelled-out state, then the trust pill.
    let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pip.add_css_class("pf-pip");
    if online {
        pip.add_css_class("pf-online");
    }
    pip.set_valign(gtk::Align::Center);
    status.append(&pip);
    let presence = gtk::Label::new(Some(if online { "Online" } else { "Offline" }));
    presence.add_css_class("caption");
    presence.add_css_class("dim-label");
    status.append(&presence);
    status.append(&if k.paired {
        pill("Paired", "pf-green")
    } else {
        pill("Trusted", "pf-accent")
    });

    let (child, overlay) = card_scaffold(
        &k.name,
        &format!("{}:{}", k.addr, k.port),
        &status,
        connecting == Some(k.fp_hex.as_str()),
    );
    if recent {
        overlay.add_css_class("pf-recent");
    }

    // Overflow menu (top-right; also on right-click): pair / speed test / rename / forget.
    let actions = gio::SimpleActionGroup::new();
    let add = |name: &str, f: Box<dyn Fn()>| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        actions.add_action(&a);
    };
    {
        let cb = state.cbs.on_pair.clone();
        let req = req.clone();
        add("pair", Box::new(move || cb(req.clone())));
    }
    {
        let cb = state.cbs.on_speed_test.clone();
        let req = req.clone();
        add("speed", Box::new(move || cb(req.clone())));
    }
    {
        let cb = state.cbs.on_library.clone();
        let req = req.clone();
        add("library", Box::new(move || cb(req.clone())));
    }
    {
        let state = state.clone();
        let fp = k.fp_hex.clone();
        let name = k.name.clone();
        add(
            "rename",
            Box::new(move || rename_dialog(&state, &fp, &name)),
        );
    }
    {
        let state = state.clone();
        let fp = k.fp_hex.clone();
        let name = k.name.clone();
        add(
            "forget",
            Box::new(move || forget_dialog(&state, &fp, &name)),
        );
    }
    {
        // Explicit "just wake it" (the tap-to-connect already auto-wakes an offline host).
        let mac = k.mac.clone();
        let addr = k.addr.clone();
        add(
            "wake",
            Box::new(move || crate::wol::wake(&mac, addr.parse().ok())),
        );
    }
    overlay.insert_action_group("card", Some(&actions));

    let menu = gio::Menu::new();
    menu.append(Some("Pair with PIN…"), Some("card.pair"));
    menu.append(Some("Test network speed…"), Some("card.speed"));
    // Offer an explicit wake only when the host is offline and we actually have a MAC to target.
    if !online && !k.mac.is_empty() {
        menu.append(Some("Wake host"), Some("card.wake"));
    }
    // Experimental (Preferences gate, Apple parity): browse the host's game library. The
    // item is offered on every saved card — an unpaired host answers with the friendly
    // "not paired" error state rather than the entry hiding itself.
    if state.settings.borrow().library_enabled {
        menu.append(Some("Browse library…"), Some("card.library"));
    }
    menu.append(Some("Rename…"), Some("card.rename"));
    menu.append(Some("Forget"), Some("card.forget"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .build();
    menu_btn.add_css_class("flat");
    overlay.add_overlay(&menu_btn);
    let right_click = gtk::GestureClick::builder().button(3).build();
    {
        let menu_btn = menu_btn.clone();
        right_click.connect_pressed(move |_, _, _, _| menu_btn.popup());
    }
    overlay.add_controller(right_click);

    let on_connect = state.cbs.on_connect.clone();
    let on_wake_connect = state.cbs.on_wake_connect.clone();
    // Auto-wake: if the host wasn't advertising when this card was built and we have a MAC, route to
    // the wake-and-wait flow (send a magic packet, poll mDNS until it's up — re-keying a new DHCP IP —
    // then connect). Otherwise a plain connect. A host that's genuinely off then times out as before.
    let wake_first = !online && !req.mac.is_empty();
    child.connect_activate(move |_| {
        if wake_first {
            on_wake_connect(req.clone());
        } else {
            on_connect(req.clone());
        }
    });
    child
}

fn discovered_card(
    state: &Rc<State>,
    a: &DiscoveredHost,
    connecting: Option<&str>,
) -> gtk::FlowBoxChild {
    let req = ConnectRequest {
        name: a.name.clone(),
        addr: a.addr.clone(),
        port: a.port,
        fp_hex: (!a.fp_hex.is_empty()).then(|| a.fp_hex.clone()),
        // TOFU is offered only when the host explicitly opts in with pair=optional;
        // required/empty means mandatory PIN.
        pair_optional: a.pair == "optional",
        launch: None,
        mac: a.mac.clone(),
    };

    let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status.append(&if req.pair_optional {
        pill("Open", "pf-neutral")
    } else {
        pill("PIN", "pf-accent")
    });

    let is_connecting = connecting == Some(req.card_key().as_str());
    let (child, overlay) = card_scaffold(
        &a.name,
        &format!("{}:{}", a.addr, a.port),
        &status,
        is_connecting,
    );
    overlay.add_css_class("pf-discovered");

    // Tap-to-connect only (parity with Android's discovered cards).
    let on_connect = state.cbs.on_connect.clone();
    child.connect_activate(move |_| on_connect(req.clone()));
    child
}

/// Rename a saved host — an entry in an alert, then upsert + refresh.
fn rename_dialog(state: &Rc<State>, fp_hex: &str, current: &str) {
    let entry = gtk::Entry::builder()
        .text(current)
        .activates_default(true)
        .build();
    let dialog = adw::AlertDialog::new(Some("Rename Host"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    {
        let state = state.clone();
        let fp = fp_hex.to_string();
        dialog.connect_response(Some("rename"), move |_, _| {
            let name = entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let mut known = KnownHosts::load();
            if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp) {
                h.name = name;
                let _ = known.save();
            }
            rebuild(&state);
        });
    }
    dialog.present(Some(&state.stack));
}

/// Forget this host (drops the pinned fingerprint — a later connect re-pairs).
/// Confirmed first, since it's destructive and a misclick on the Deck is easy.
fn forget_dialog(state: &Rc<State>, fp_hex: &str, name: &str) {
    let dialog = adw::AlertDialog::new(
        Some("Remove saved host?"),
        Some(&format!(
            "Forget “{name}”? You'll need to pair (or trust) it again to reconnect."
        )),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    {
        let state = state.clone();
        let fp = fp_hex.to_string();
        dialog.connect_response(Some("remove"), move |_, _| {
            let mut known = KnownHosts::load();
            known.remove_by_fp(&fp);
            let _ = known.save();
            rebuild(&state);
        });
    }
    dialog.present(Some(&state.stack));
}

/// "+": name (optional) / address / port — the Apple AddHostSheet / Android dialog
/// equivalent of the old inline entry. Submit runs the normal trust gate (`on_connect`).
fn add_host_dialog(state: &Rc<State>) {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let name_row = adw::EntryRow::builder().title("Name (optional)").build();
    let addr_row = adw::EntryRow::builder().title("Address").build();
    let port_row = adw::EntryRow::builder().title("Port").text("9777").build();
    list.append(&name_row);
    list.append(&addr_row);
    list.append(&port_row);
    list.set_size_request(320, -1);

    let dialog = adw::AlertDialog::new(Some("Add Host"), None);
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("connect"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("connect", false);
    {
        let dialog = dialog.clone();
        addr_row.connect_changed(move |row| {
            dialog.set_response_enabled("connect", !row.text().trim().is_empty());
        });
    }
    {
        let on_connect = state.cbs.on_connect.clone();
        let (name_row, addr_row, port_row) = (name_row.clone(), addr_row.clone(), port_row.clone());
        dialog.connect_response(Some("connect"), move |_, _| {
            let text = addr_row.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            // A pasted `host:port` wins over the port field; otherwise the field (default 9777).
            let (addr, port) = match text.rsplit_once(':') {
                Some((a, p)) if p.parse::<u16>().is_ok() => {
                    (a.to_string(), p.parse::<u16>().unwrap())
                }
                _ => (
                    text.clone(),
                    port_row.text().trim().parse::<u16>().unwrap_or(9777),
                ),
            };
            let name = name_row.text().trim().to_string();
            on_connect(ConnectRequest {
                name: if name.is_empty() { addr.clone() } else { name },
                addr,
                port,
                fp_hex: None,
                // Manual entry carries no advertised policy — never eligible for TOFU.
                pair_optional: false,
                launch: None,
                mac: Vec::new(),
            });
        });
    }
    dialog.present(Some(&state.stack));
}

#[cfg(test)]
mod tests {
    use adw::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // Reproduces the exact FlowBox/FlowBoxChild wiring from `new()`: `child-activated` bridges
    // to `child.activate()`, whose own default handler re-emits `child-activated` on the
    // FlowBox — that ping-pong recursed forever (stack overflow on every host-card click/Enter)
    // until the re-entrancy guard was added. This exercises the *real* GTK signal cycle, not a
    // simulation of it, so it fails the same way the shipped bug did if the guard regresses.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn flow_box_activation_bridge_does_not_recurse() {
        assert!(gtk::init().is_ok(), "no display");

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .activate_on_single_click(true)
            .build();
        let activating = Cell::new(false);
        flow.connect_child_activated(move |_, child| {
            if activating.replace(true) {
                return;
            }
            child.activate();
            activating.set(false);
        });

        let child = gtk::FlowBoxChild::new();
        flow.insert(&child, -1);
        let fired = Rc::new(Cell::new(0u32));
        {
            let fired = fired.clone();
            child.connect_activate(move |_| fired.set(fired.get() + 1));
        }

        // What a pointer click with `activate_on_single_click` does internally: emit
        // `child-activated` directly on the FlowBox. A regression here overflows the stack
        // instead of returning.
        flow.emit_by_name::<()>("child-activated", &[&child]);

        assert_eq!(fired.get(), 1, "the per-card handler should fire exactly once");
    }
}
