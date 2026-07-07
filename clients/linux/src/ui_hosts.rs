//! The hosts page as a relm4 component: adaptive card grids for saved (trusted/paired)
//! and mDNS-discovered hosts — avatar + name + `addr:port` + status pills, online pips,
//! dashed discovered cards, an overflow menu, an add-host dialog, and a connect-failure
//! banner. Cards are a [`FactoryVecDeque`]; both grids re-populate from one state
//! snapshot (known hosts on disk + the live advert map) on every change, so dedup and
//! the online pips stay consistent. Actions leave as typed [`HostsOutput`]s — the
//! callback bag and `Rc<RefCell<HostsUi>>` pokes of the pre-relm4 shell are gone.

use crate::discovery::{self, DiscoveredHost, DiscoveryEvent};
use crate::trust::{KnownHost, KnownHosts, Settings};
use adw::prelude::*;
use gtk::{gio, glib};
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// What the user asked to connect to. `fp_hex` comes from the mDNS TXT record when the
/// host was discovered (drives the trust decision *before* connecting); manual entries
/// have none. `pair_optional` is true ONLY when a discovered host advertised
/// `pair=optional` — the sole case in which the reduced-security TOFU path may be
/// offered; every other case mandates PIN pairing.
#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub fp_hex: Option<String>,
    pub pair_optional: bool,
    /// A library title to launch on connect (`(library id, display name)`).
    pub launch: Option<(String, String)>,
    /// Wake-on-LAN MAC(s) for this host. Empty when none is known.
    pub mac: Vec<String>,
}

impl ConnectRequest {
    /// The key the page tracks an in-flight connect under (the card that swaps its
    /// avatar for a spinner): the fingerprint when known, else the address.
    pub fn card_key(&self) -> String {
        self.fp_hex
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.addr, self.port))
    }
}

// --- The card factory ---------------------------------------------------------------------

/// One card's full render input — rebuilt (clear + repopulate) on every state change,
/// exactly like the pre-relm4 full-grid rebuild (a handful of widgets; simpler than row
/// surgery and keeps every derived view consistent).
#[derive(Debug)]
pub struct HostCard {
    kind: CardKind,
    connecting: bool,
}

#[derive(Debug)]
enum CardKind {
    Saved {
        host: KnownHost,
        online: bool,
        recent: bool,
        library_enabled: bool,
    },
    Discovered(DiscoveredHost),
}

#[derive(Debug)]
pub enum CardOutput {
    Connect(ConnectRequest),
    WakeConnect(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    Library(ConnectRequest),
    Rename { fp_hex: String, name: String },
    Forget { fp_hex: String, name: String },
    Wake { mac: Vec<String>, addr: String },
}

impl HostCard {
    fn request(&self) -> ConnectRequest {
        match &self.kind {
            CardKind::Saved { host: k, .. } => ConnectRequest {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                // Saved host: its fp is already pinned → silent pinned connect.
                pair_optional: false,
                launch: None,
                mac: k.mac.clone(),
            },
            CardKind::Discovered(a) => ConnectRequest {
                name: a.name.clone(),
                addr: a.addr.clone(),
                port: a.port,
                fp_hex: (!a.fp_hex.is_empty()).then(|| a.fp_hex.clone()),
                // TOFU only when the host explicitly opts in with pair=optional.
                pair_optional: a.pair == "optional",
                launch: None,
                mac: a.mac.clone(),
            },
        }
    }
}

impl relm4::factory::FactoryComponent for HostCard {
    type Init = HostCard;
    type Input = ();
    type Output = CardOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::FlowBox;
    type Root = gtk::Overlay;
    type Widgets = ();
    type Index = relm4::factory::DynamicIndex;

    fn init_model(
        init: Self::Init,
        _index: &Self::Index,
        _sender: relm4::FactorySender<Self>,
    ) -> Self {
        init
    }

    fn init_root(&self) -> Self::Root {
        gtk::Overlay::new()
    }

    fn init_widgets(
        &mut self,
        _index: &Self::Index,
        overlay: Self::Root,
        returned: &gtk::FlowBoxChild,
        sender: relm4::FactorySender<Self>,
    ) -> Self::Widgets {
        let req = self.request();

        // The shared scaffold: avatar (spinner while connecting) / name / addr / status.
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        if self.connecting {
            let spinner = gtk::Spinner::new();
            spinner.set_size_request(48, 48);
            spinner.start();
            spinner.set_halign(gtk::Align::Center);
            content.append(&spinner);
        } else {
            let avatar = adw::Avatar::new(48, Some(&req.name), true);
            avatar.set_halign(gtk::Align::Center);
            content.append(&avatar);
        }
        let name_label = gtk::Label::new(Some(&req.name));
        name_label.add_css_class("heading");
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        content.append(&name_label);
        let addr_label = gtk::Label::new(Some(&format!("{}:{}", req.addr, req.port)));
        addr_label.add_css_class("caption");
        addr_label.add_css_class("dim-label");
        addr_label.add_css_class("numeric");
        addr_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        content.append(&addr_label);

        let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        status.set_halign(gtk::Align::Center);
        status.set_margin_top(4);
        let pill = |text: &str, class: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("pf-pill");
            l.add_css_class(class);
            l
        };
        match &self.kind {
            CardKind::Saved {
                host: k, online, ..
            } => {
                // Presence pip + spelled-out state, then the trust pill.
                let pip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                pip.add_css_class("pf-pip");
                if *online {
                    pip.add_css_class("pf-online");
                }
                pip.set_valign(gtk::Align::Center);
                status.append(&pip);
                let presence = gtk::Label::new(Some(if *online { "Online" } else { "Offline" }));
                presence.add_css_class("caption");
                presence.add_css_class("dim-label");
                status.append(&presence);
                status.append(&if k.paired {
                    pill("Paired", "pf-green")
                } else {
                    pill("Trusted", "pf-accent")
                });
            }
            CardKind::Discovered(_) => {
                status.append(&if req.pair_optional {
                    pill("Open", "pf-neutral")
                } else {
                    pill("PIN", "pf-accent")
                });
            }
        }
        content.append(&status);

        overlay.set_child(Some(&content));
        overlay.add_css_class("card");
        overlay.add_css_class("pf-host-card");
        if self.connecting {
            returned.set_sensitive(false);
        }

        match &self.kind {
            CardKind::Saved {
                host: k,
                online,
                recent,
                library_enabled,
            } => {
                if *recent {
                    overlay.add_css_class("pf-recent");
                }
                // Overflow menu (top-right; also on right-click).
                let actions = gio::SimpleActionGroup::new();
                let add = |name: &str, out: Box<dyn Fn() -> CardOutput>| {
                    let a = gio::SimpleAction::new(name, None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let _ = sender.output(out());
                    });
                    actions.add_action(&a);
                };
                {
                    let req = req.clone();
                    add("pair", Box::new(move || CardOutput::Pair(req.clone())));
                }
                {
                    let req = req.clone();
                    add(
                        "speed",
                        Box::new(move || CardOutput::SpeedTest(req.clone())),
                    );
                }
                {
                    let req = req.clone();
                    add(
                        "library",
                        Box::new(move || CardOutput::Library(req.clone())),
                    );
                }
                {
                    let (fp, name) = (k.fp_hex.clone(), k.name.clone());
                    add(
                        "rename",
                        Box::new(move || CardOutput::Rename {
                            fp_hex: fp.clone(),
                            name: name.clone(),
                        }),
                    );
                }
                {
                    let (fp, name) = (k.fp_hex.clone(), k.name.clone());
                    add(
                        "forget",
                        Box::new(move || CardOutput::Forget {
                            fp_hex: fp.clone(),
                            name: name.clone(),
                        }),
                    );
                }
                {
                    let (mac, addr) = (k.mac.clone(), k.addr.clone());
                    add(
                        "wake",
                        Box::new(move || CardOutput::Wake {
                            mac: mac.clone(),
                            addr: addr.clone(),
                        }),
                    );
                }
                overlay.insert_action_group("card", Some(&actions));

                let menu = gio::Menu::new();
                menu.append(Some("Pair with PIN…"), Some("card.pair"));
                menu.append(Some("Test network speed…"), Some("card.speed"));
                // An explicit wake only when offline and a MAC is known.
                if !online && !k.mac.is_empty() {
                    menu.append(Some("Wake host"), Some("card.wake"));
                }
                // Experimental (Preferences gate): browse the host's game library.
                if *library_enabled {
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

                // Auto-wake: offline + a known MAC routes to wake-and-wait.
                let wake_first = !online && !req.mac.is_empty();
                let sender = sender.clone();
                returned.connect_activate(move |_| {
                    let _ = sender.output(if wake_first {
                        CardOutput::WakeConnect(req.clone())
                    } else {
                        CardOutput::Connect(req.clone())
                    });
                });
            }
            CardKind::Discovered(_) => {
                overlay.add_css_class("pf-discovered");
                // Tap-to-connect only (parity with Android's discovered cards).
                let sender = sender.clone();
                returned.connect_activate(move |_| {
                    let _ = sender.output(CardOutput::Connect(req.clone()));
                });
            }
        }
    }
}

// --- The page component ---------------------------------------------------------------------

pub struct HostsPage {
    adverts: HashMap<String, DiscoveredHost>,
    connecting: Option<String>,
    settings: Rc<RefCell<Settings>>,
    saved: FactoryVecDeque<HostCard>,
    discovered: FactoryVecDeque<HostCard>,
    widgets: PageWidgets,
}

struct PageWidgets {
    stack: gtk::Stack,
    banner: adw::Banner,
    saved_heading: gtk::Label,
    disc_heading: gtk::Label,
    searching: gtk::Box,
}

#[derive(Debug)]
pub enum HostsMsg {
    /// A resolved mDNS advert (also the CI scenes' injection path).
    Advert(DiscoveredHost),
    AdvertRemoved {
        fullname: String,
    },
    /// Reload the disk store and re-render (fresh pairings, renames, the library gate).
    Refresh,
    /// Mark the card matching `ConnectRequest::card_key` as connecting; `None` restores.
    SetConnecting(Option<String>),
    ShowError(String),
    ClearError,
    ShowAddHost,
    /// Forwarded card actions (factory outputs).
    Card(CardOutput),
}

#[derive(Debug)]
pub enum HostsOutput {
    Connect(ConnectRequest),
    WakeConnect(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    /// With the advertised mgmt port when a live advert carries one.
    Library(ConnectRequest, Option<u16>),
}

impl SimpleComponent for HostsPage {
    type Init = Rc<RefCell<Settings>>;
    type Input = HostsMsg;
    type Output = HostsOutput;
    type Root = adw::NavigationPage;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::NavigationPage::builder()
            .title("Punktfunk")
            .tag("hosts")
            .build()
    }

    fn init(
        settings: Self::Init,
        page: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let make_flow = || {
            let f = gtk::FlowBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .activate_on_single_click(true)
                .homogeneous(true)
                .min_children_per_line(1)
                .max_children_per_line(4)
                .column_spacing(12)
                .row_spacing(12)
                .build();
            // Scopes the concentric hover-highlight radius (see app.rs CSS).
            f.add_css_class("pf-host-grid");
            f
        };
        let heading = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("heading");
            l.set_halign(gtk::Align::Start);
            l
        };
        let saved_heading = heading("Saved hosts");
        let disc_heading = heading("On this network");

        let saved = FactoryVecDeque::<HostCard>::builder()
            .launch(make_flow())
            .forward(sender.input_sender(), HostsMsg::Card);
        let discovered = FactoryVecDeque::<HostCard>::builder()
            .launch(make_flow())
            .forward(sender.input_sender(), HostsMsg::Card);

        // A pointer click (and keyboard activate) emits `child-activated` on the
        // *FlowBox*, never the child's own `activate` signal — bridge it back to the
        // child, where each card wires its connect handler. The re-entrancy flag breaks
        // the child-activated ↔ activate ping-pong that otherwise recurses forever
        // (a real stack overflow on every card click; see the ignored display test).
        for flow in [saved.widget(), discovered.widget()] {
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
        content.append(saved.widget());
        content.append(&disc_heading);
        content.append(&searching);
        content.append(discovered.widget());

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
            .description(
                "Hosts on your network appear here automatically.\nAdd one by address with +.",
            )
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

        // Connect failures land here, not in toasts.
        let banner = adw::Banner::new("");
        banner.set_button_label(Some("Dismiss"));
        banner.connect_button_clicked(|b| b.set_revealed(false));

        let header = adw::HeaderBar::new();
        let add_host_btn = gtk::Button::from_icon_name("list-add-symbolic");
        add_host_btn.set_tooltip_text(Some("Add host"));
        add_host_btn.set_action_name(Some("win.add-host"));
        header.pack_start(&add_host_btn);
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
        page.set_child(Some(&toolbar));

        // Rebuilt every time the page is shown, so fresh TOFU/pairing entries appear on
        // return.
        {
            let sender = sender.clone();
            page.connect_shown(move |_| sender.input(HostsMsg::Refresh));
        }

        // Stream mDNS adverts into the model; every add/remove re-evaluates both grids.
        {
            let rx = discovery::browse();
            let sender = sender.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        DiscoveryEvent::Resolved(h) => sender.input(HostsMsg::Advert(h)),
                        DiscoveryEvent::Removed { fullname } => {
                            sender.input(HostsMsg::AdvertRemoved { fullname })
                        }
                    }
                }
            });
        }

        let mut model = HostsPage {
            adverts: HashMap::new(),
            connecting: None,
            settings,
            saved,
            discovered,
            widgets: PageWidgets {
                stack,
                banner,
                saved_heading,
                disc_heading,
                searching,
            },
        };
        model.rebuild();

        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: HostsMsg, sender: ComponentSender<Self>) {
        match msg {
            HostsMsg::Advert(h) => {
                self.adverts.insert(h.key.clone(), h);
                self.rebuild();
            }
            HostsMsg::AdvertRemoved { fullname } => {
                self.adverts.retain(|_, a| a.fullname != fullname);
                self.rebuild();
            }
            HostsMsg::Refresh => self.rebuild(),
            HostsMsg::SetConnecting(key) => {
                self.connecting = key;
                self.rebuild();
            }
            HostsMsg::ShowError(msg) => {
                self.widgets.banner.set_title(&msg);
                self.widgets.banner.set_revealed(true);
            }
            HostsMsg::ClearError => self.widgets.banner.set_revealed(false),
            HostsMsg::ShowAddHost => self.add_host_dialog(&sender),
            HostsMsg::Card(out) => match out {
                CardOutput::Connect(req) => {
                    let _ = sender.output(HostsOutput::Connect(req));
                }
                CardOutput::WakeConnect(req) => {
                    let _ = sender.output(HostsOutput::WakeConnect(req));
                }
                CardOutput::Pair(req) => {
                    let _ = sender.output(HostsOutput::Pair(req));
                }
                CardOutput::SpeedTest(req) => {
                    let _ = sender.output(HostsOutput::SpeedTest(req));
                }
                CardOutput::Library(req) => {
                    let mgmt = self.mgmt_port_for(&req);
                    let _ = sender.output(HostsOutput::Library(req, mgmt));
                }
                CardOutput::Rename { fp_hex, name } => self.rename_dialog(&sender, &fp_hex, &name),
                CardOutput::Forget { fp_hex, name } => self.forget_dialog(&sender, &fp_hex, &name),
                CardOutput::Wake { mac, addr } => crate::wol::wake(&mac, addr.parse().ok()),
            },
        }
    }
}

impl HostsPage {
    /// Re-populate both factories from disk + the advert map. Cheap (a handful of
    /// widgets) and keeps every derived view — online pips, dedup, most-recent accent,
    /// spinner — in one straight-line pass.
    fn rebuild(&mut self) {
        let known = KnownHosts::load();
        // A saved host is ONLINE iff a live advert matches it (fingerprint, or address
        // when the advert carries no fp).
        let matches = |k: &KnownHost, a: &DiscoveredHost| {
            (!a.fp_hex.is_empty() && a.fp_hex == k.fp_hex) || (a.addr == k.addr && a.port == k.port)
        };
        let most_recent = known
            .hosts
            .iter()
            .filter_map(|h| h.last_used.map(|t| (h.fp_hex.clone(), t)))
            .max_by_key(|&(_, t)| t)
            .map(|(fp, _)| fp);
        let library_enabled = self.settings.borrow().library_enabled;

        {
            let mut saved = self.saved.guard();
            saved.clear();
            for k in &known.hosts {
                let online = self.adverts.values().any(|a| matches(k, a));
                // Learn this host's wake MAC(s) from its live advert while it's online.
                if let Some(a) = self
                    .adverts
                    .values()
                    .find(|a| matches(k, a) && !a.mac.is_empty())
                {
                    crate::trust::learn_mac(&k.fp_hex, &k.addr, k.port, &a.mac);
                }
                saved.push_back(HostCard {
                    connecting: self.connecting.as_deref() == Some(k.fp_hex.as_str()),
                    kind: CardKind::Saved {
                        host: k.clone(),
                        online,
                        recent: most_recent.as_deref() == Some(k.fp_hex.as_str()),
                        library_enabled,
                    },
                });
            }
        }

        // The discovered grid only surfaces genuinely-new hosts: anything matching a
        // saved entry renders as that saved card (with its pip now green) instead.
        let mut fresh: Vec<&DiscoveredHost> = self
            .adverts
            .values()
            .filter(|a| !known.hosts.iter().any(|k| matches(k, a)))
            .collect();
        fresh.sort_by(|a, b| a.name.cmp(&b.name).then(a.key.cmp(&b.key)));
        let have_disc = !fresh.is_empty();
        {
            let mut discovered = self.discovered.guard();
            discovered.clear();
            for a in fresh {
                let key = if a.fp_hex.is_empty() {
                    format!("{}:{}", a.addr, a.port)
                } else {
                    a.fp_hex.clone()
                };
                discovered.push_back(HostCard {
                    connecting: self.connecting.as_deref() == Some(key.as_str()),
                    kind: CardKind::Discovered(a.clone()),
                });
            }
        }

        let have_saved = !known.hosts.is_empty();
        let w = &self.widgets;
        w.saved_heading.set_visible(have_saved);
        self.saved.widget().set_visible(have_saved);
        w.disc_heading.set_visible(true);
        self.discovered.widget().set_visible(have_disc);
        w.searching.set_visible(!have_disc);
        w.stack.set_visible_child_name(if have_saved || have_disc {
            "grid"
        } else {
            "empty"
        });
    }

    /// The advertised mgmt port for the host `req` points at, when a matching live
    /// advert carries the `mgmt` TXT.
    fn mgmt_port_for(&self, req: &ConnectRequest) -> Option<u16> {
        self.adverts
            .values()
            .find(|a| {
                req.fp_hex
                    .as_deref()
                    .is_some_and(|fp| !a.fp_hex.is_empty() && a.fp_hex == fp)
                    || (a.addr == req.addr && a.port == req.port)
            })
            .and_then(|a| a.mgmt_port)
    }

    /// Rename a saved host — an entry in an alert, then upsert + refresh.
    fn rename_dialog(&self, sender: &ComponentSender<Self>, fp_hex: &str, current: &str) {
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
            let sender = sender.clone();
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
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }

    /// Forget this host (drops the pinned fingerprint — a later connect re-pairs).
    fn forget_dialog(&self, sender: &ComponentSender<Self>, fp_hex: &str, name: &str) {
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
            let sender = sender.clone();
            let fp = fp_hex.to_string();
            dialog.connect_response(Some("remove"), move |_, _| {
                let mut known = KnownHosts::load();
                known.remove_by_fp(&fp);
                let _ = known.save();
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }

    /// "+": name (optional) / address / port. Submit runs the normal trust gate.
    fn add_host_dialog(&self, sender: &ComponentSender<Self>) {
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
            let sender = sender.clone();
            let (name_row, addr_row, port_row) =
                (name_row.clone(), addr_row.clone(), port_row.clone());
            dialog.connect_response(Some("connect"), move |_, _| {
                let text = addr_row.text().trim().to_string();
                if text.is_empty() {
                    return;
                }
                // A pasted `host:port` wins over the port field; else the field.
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
                let _ = sender.output(HostsOutput::Connect(ConnectRequest {
                    name: if name.is_empty() { addr.clone() } else { name },
                    addr,
                    port,
                    fp_hex: None,
                    // Manual entry carries no advertised policy — never TOFU-eligible.
                    pair_optional: false,
                    launch: None,
                    mac: Vec::new(),
                }));
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }
}

#[cfg(test)]
mod tests {
    use adw::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // Reproduces the exact FlowBox/FlowBoxChild wiring from `init()`: `child-activated`
    // bridges to `child.activate()`, whose own default handler re-emits
    // `child-activated` — that ping-pong recursed forever (stack overflow on every
    // host-card click/Enter) until the re-entrancy guard was added.
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

        flow.emit_by_name::<()>("child-activated", &[&child]);

        assert_eq!(
            fired.get(),
            1,
            "the per-card handler should fire exactly once"
        );
    }
}
