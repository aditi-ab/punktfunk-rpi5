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
    /// A ONE-OFF settings profile for this connect ("Connect with ▸ X"): `Some(id)` overrides
    /// the host's binding for this launch, `Some("")` forces the global defaults on a bound
    /// host, `None` honors the binding. It never rebinds anything — the host's default changes
    /// only through an explicit "Default profile" pick (design/client-settings-profiles.md §5.2).
    pub profile: Option<String>,
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

/// One catalog entry as the cards need it: what to call the profile, and the colour its chips
/// carry (`StreamProfile.accent`) — the field the schema reserved for exactly this.
#[derive(Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub accent: Option<String>,
}

#[derive(Debug)]
enum CardKind {
    Saved {
        host: KnownHost,
        online: bool,
        recent: bool,
        library_enabled: bool,
        /// The profile catalog as `(id, name)`, for this card's menus and chip. Shared per
        /// refresh rather than re-read per card.
        profiles: Rc<Vec<Profile>>,
        /// `Some((id, name))` when this card is a PINNED host+profile pair rather than the
        /// host's primary card (design §5.2a): a one-click shortcut for a profile the user
        /// reaches for often. It is presentation state on the host record — never a second
        /// host entry, which would fork pairing, WoL and renames.
        pinned: Option<(String, String)>,
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
    /// Open the host edit sheet (name, profile binding, pinned cards, clipboard).
    Edit {
        fp_hex: String,
        name: String,
    },
    Forget {
        fp_hex: String,
        name: String,
    },
    Wake {
        mac: Vec<String>,
        addr: String,
    },
    /// Put this card's `punktfunk://` URL on the clipboard.
    CopyLink(String),
    /// Write a desktop entry that launches this card's URL.
    CreateShortcut {
        label: String,
        url: String,
    },
    /// Add or remove a pinned host+profile card (design §5.2a). Presentation only — it never
    /// changes the host's default profile, and unpinning never touches the profile itself.
    TogglePin {
        fp_hex: String,
        addr: String,
        port: u16,
        profile_id: String,
        pin: bool,
    },
}

impl HostCard {
    fn request(&self) -> ConnectRequest {
        match &self.kind {
            CardKind::Saved {
                host: k, pinned, ..
            } => ConnectRequest {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                // Saved host: its fp is already pinned → silent pinned connect.
                pair_optional: false,
                launch: None,
                mac: k.mac.clone(),
                // A pinned card IS its profile: clicking it connects with that one, without
                // touching the host's default.
                profile: pinned.as_ref().map(|(id, _)| id.clone()),
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
                profile: None,
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
        // The host's OS mark leads the row; nothing at all for an older host that doesn't
        // advertise one, so those cards render exactly as they always did.
        let os_chain = match &self.kind {
            CardKind::Saved { host: k, .. } => k.os.as_str(),
            CardKind::Discovered(a) => a.os.as_str(),
        };
        if let Some(img) = os_icon_image(os_chain) {
            status.append(&img);
        }
        let pill = |text: &str, class: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("pf-pill");
            l.add_css_class(class);
            l
        };
        match &self.kind {
            CardKind::Saved {
                host: k,
                online,
                profiles,
                pinned,
                ..
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
                // The chip says what a plain click on THIS card will do: its own profile on
                // a pinned card, the host's binding on the primary one. Both resolve through
                // the catalog so the chip can carry the profile's colour; a binding whose
                // profile was deleted shows nothing and resolves as the defaults, which is
                // exactly what will happen on connect (design §6).
                let chip = pinned
                    .as_ref()
                    .map(|(id, _)| id.as_str())
                    .or(k.profile_id.as_deref())
                    .and_then(|id| profiles.iter().find(|p| p.id == id));
                if let Some(p) = chip {
                    status.append(&profile_pill(p));
                }
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
                profiles,
                pinned,
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
                    add(
                        "connect",
                        Box::new(move || CardOutput::Connect(req.clone())),
                    );
                }
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
                        Box::new(move || CardOutput::Edit {
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
                // "Copy link" / "Create shortcut…": the self-emitted URL for this card, which
                // is what an external tool (a Playnite entry, a Stream Deck macro) is
                // configured with. It carries the stable id AND host+fp, so it still resolves
                // after a re-address or a reinstall (design/client-deep-links.md §2/§5).
                {
                    let (host, profile) = (k.clone(), pinned.clone());
                    let a = gio::SimpleAction::new("copy-link", None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let url = pf_client_core::deeplink::DeepLink::for_host(
                            &host,
                            None,
                            profile.as_ref().map(|(id, _)| id.as_str()),
                        )
                        .to_url();
                        let _ = sender.output(CardOutput::CopyLink(url));
                    });
                    actions.add_action(&a);
                }
                {
                    let (host, profile) = (k.clone(), pinned.clone());
                    let a = gio::SimpleAction::new("shortcut", None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let url = pf_client_core::deeplink::DeepLink::for_host(
                            &host,
                            None,
                            profile.as_ref().map(|(id, _)| id.as_str()),
                        )
                        .to_url();
                        let label = match &profile {
                            Some((_, name)) => format!("{} \u{00b7} {name}", host.name),
                            None => host.name.clone(),
                        };
                        let _ = sender.output(CardOutput::CreateShortcut { label, url });
                    });
                    actions.add_action(&a);
                }
                // A one-off connect ("Connect with") never rebinds the host — the whole
                // predictability rule is that it can't change what the card does next time.
                // Rebinding, and pinning, live in the edit sheet (design §5.2).
                {
                    let profile_action =
                        |name: &str, out: Box<dyn Fn(Option<String>) -> CardOutput>| {
                            let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
                            let sender = sender.clone();
                            a.connect_activate(move |_, param| {
                                // The empty string is "Default settings" — a real choice, not
                                // an absent one, so it has to survive as a value.
                                let id = param.and_then(|p| p.str()).unwrap_or("").to_string();
                                let _ = sender.output(out(Some(id).filter(|s| !s.is_empty())));
                            });
                            actions.add_action(&a);
                        };
                    let req_for_connect = req.clone();
                    profile_action(
                        "connect-with",
                        Box::new(move |id| {
                            let mut req = req_for_connect.clone();
                            // `Some("")` — not `None` — so a bound host really does connect
                            // with the defaults when the user asks for them.
                            req.profile = Some(id.unwrap_or_default());
                            CardOutput::Connect(req)
                        }),
                    );
                    // The same action pins from a primary card and unpins from a pinned one —
                    // which of the two this card is decides the direction.
                    let (fp, addr, port) = (k.fp_hex.clone(), k.addr.clone(), k.port);
                    let pinning = pinned.is_none();
                    profile_action(
                        "toggle-pin",
                        Box::new(move |id| CardOutput::TogglePin {
                            fp_hex: fp.clone(),
                            addr: addr.clone(),
                            port,
                            profile_id: id.unwrap_or_default(),
                            pin: pinning,
                        }),
                    );
                }
                overlay.insert_action_group("card", Some(&actions));

                // The card menu, kept short on purpose. It had grown to eleven entries and
                // three submenus, at which point the useful ones (connect, library, speed) were
                // buried in list management. Two rules thin it:
                //
                //   * anything that CONFIGURES the host lives in the edit sheet, not here —
                //     the default profile and the pinned cards are properties of the record,
                //     and the sheet is where you already go to change its name or address;
                //   * what remains is grouped into sections, so a glance lands on the right
                //     third of the menu instead of scanning eleven similar lines.
                //
                // What's left is: start something, look at something, take a link, manage the
                // host itself.
                let menu = gio::Menu::new();
                if let Some((pin_id, pin_name)) = pinned {
                    // A pinned card is a shortcut, not a second host: it starts a stream, hands
                    // out its link, and removes itself. Pair/edit/forget belong to the host and
                    // offering them here would blur what the card is.
                    let launch = gio::Menu::new();
                    launch.append(Some("Connect"), Some("card.connect"));
                    menu.append_section(None, &launch);

                    let links = gio::Menu::new();
                    links.append(Some("Copy link"), Some("card.copy-link"));
                    links.append(Some("Create shortcut\u{2026}"), Some("card.shortcut"));
                    menu.append_section(None, &links);

                    let manage = gio::Menu::new();
                    let unpin = gio::MenuItem::new(
                        Some(&format!("Unpin \u{201c}{pin_name}\u{201d}")),
                        None,
                    );
                    unpin.set_action_and_target_value(
                        Some("card.toggle-pin"),
                        Some(&pin_id.as_str().to_variant()),
                    );
                    manage.append_item(&unpin);
                    menu.append_section(None, &manage);
                } else {
                    // Starting a stream: a plain click already connects with the host's own
                    // profile, so the menu only needs the one-offs — and only when there are
                    // profiles to pick between.
                    if !profiles.is_empty() {
                        let with = gio::Menu::new();
                        for (label, id) in std::iter::once(("Default settings", ""))
                            .chain(profiles.iter().map(|p| (p.name.as_str(), p.id.as_str())))
                        {
                            let item = gio::MenuItem::new(Some(label), None);
                            item.set_action_and_target_value(
                                Some("card.connect-with"),
                                Some(&id.to_variant()),
                            );
                            with.append_item(&item);
                        }
                        let launch = gio::Menu::new();
                        launch.append_submenu(Some("Connect with"), &with);
                        menu.append_section(None, &launch);
                    }

                    let look = gio::Menu::new();
                    // Experimental (Preferences gate): browse the host's game library.
                    if *library_enabled {
                        look.append(Some("Browse library\u{2026}"), Some("card.library"));
                    }
                    look.append(Some("Test network speed\u{2026}"), Some("card.speed"));
                    // An explicit wake only when offline and a MAC is known.
                    if !online && !k.mac.is_empty() {
                        look.append(Some("Wake host"), Some("card.wake"));
                    }
                    menu.append_section(None, &look);

                    let links = gio::Menu::new();
                    links.append(Some("Copy link"), Some("card.copy-link"));
                    links.append(Some("Create shortcut\u{2026}"), Some("card.shortcut"));
                    menu.append_section(None, &links);

                    let manage = gio::Menu::new();
                    manage.append(Some("Edit\u{2026}"), Some("card.rename"));
                    manage.append(Some("Pair with PIN\u{2026}"), Some("card.pair"));
                    manage.append(Some("Forget"), Some("card.forget"));
                    menu.append_section(None, &manage);
                }
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

                // Auto-wake: not advertising + a known MAC routes to WakeConnect, which
                // dials first (a routed/Tailscale host is mDNS-blind, not asleep) and only
                // falls into the wake-and-wait when the dial fails.
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

/// How long each saved-host reachability probe waits, and how often the sweep runs. The pip
/// reads `advertising OR probed-reachable`, so a host reached only over a routed network
/// (Tailscale/VPN) — which never appears on mDNS — still shows Online.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12);

/// The key a saved host is tracked under in the probe-results map: its fingerprint (stable
/// across IP changes) when it has one, else `addr:port` (a not-yet-paired manual entry).
/// A profile chip in that profile's colour. `accent` is the field the catalog schema reserved
/// for this — without it every profile is the same grey, and telling them apart across a grid
/// at a glance is the whole reason the chip exists. No colour set keeps the neutral pill, so
/// the palette stays opt-in.
/// The OS-icon tokens this shell ships symbolic art for (`data/icons/.../pf-os-<t>-symbolic.svg`,
/// embedded via gresource): the families a chain can land on, plus the gaming distros that earn
/// their own mark because "a Bazzite box" and "a Fedora box" are different machines to the person
/// reading the card. Chains walk most-specific-first, so a distro without a mark of its own still
/// lands on its family's and finally on plain Tux.
const OS_ICON_TOKENS: &[&str] = &[
    "windows", "apple", "linux", "steam", "ubuntu", "fedora", "arch", "debian", "nixos",
    "opensuse", "bazzite", "cachyos", "nobara",
];

/// The card's OS glyph for an advertised chain, or `None` (no widget) when the host doesn't
/// advertise one / nothing in the chain is recognized-and-drawable. Symbolic, so it recolors
/// with the Adwaita theme like every other status glyph; the raw chain is the tooltip.
fn os_icon_image(chain: &str) -> Option<gtk::Image> {
    let token = crate::os::os_icon_tokens(chain)
        .into_iter()
        .find(|t| OS_ICON_TOKENS.contains(&t.as_str()))?;
    let img = gtk::Image::from_icon_name(&format!("pf-os-{token}-symbolic"));
    img.set_pixel_size(14);
    img.add_css_class("dim-label");
    img.set_valign(gtk::Align::Center);
    img.set_tooltip_text(Some(chain));
    Some(img)
}

fn profile_pill(p: &Profile) -> gtk::Widget {
    let label = gtk::Label::new(Some(&p.name));
    label.add_css_class("pf-pill");
    let Some(hex) = p.accent.as_deref().filter(|h| is_hex_colour(h)) else {
        label.add_css_class("pf-neutral");
        return label.upcast();
    };
    label.add_css_class(&tint_class(hex));
    label.upcast()
}

/// The CSS class that tints a pill with `hex`, registering its rule on the display the first
/// time that colour is seen.
///
/// Per-widget providers are gone since GTK 4.10, and the colour is user data rather than one of
/// a fixed set, so the rule is generated once per distinct colour and added display-wide. A
/// handful of profiles means a handful of tiny rules, and re-rendering the grid (which happens
/// on every state change) reuses them instead of allocating more.
fn tint_class(hex: &str) -> String {
    thread_local! {
        static REGISTERED: RefCell<HashMap<String, ()>> = RefCell::new(HashMap::new());
    }
    let class = format!("pf-tint-{}", &hex[1..]);
    REGISTERED.with_borrow_mut(|seen| {
        if seen.contains_key(&class) {
            return;
        }
        if let Some(display) = gtk::gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&format!(
                ".pf-pill.{class} {{ color: {hex}; background: alpha({hex}, 0.18); }}"
            ));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            seen.insert(class.clone(), ());
        }
    });
    class
}

/// `#RRGGBB` only — the value is interpolated into CSS, so anything else is refused rather
/// than injected.
fn is_hex_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn saved_key(h: &KnownHost) -> String {
    if h.fp_hex.is_empty() {
        format!("{}:{}", h.addr, h.port)
    } else {
        h.fp_hex.clone()
    }
}

pub struct HostsPage {
    adverts: HashMap<String, DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe (mDNS-independent), keyed by
    /// [`saved_key`]. OR'd with live-advert presence to drive the Online pip.
    probed: HashMap<String, bool>,
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
    /// A completed reachability sweep: saved-host key → reachable. Merged into the online pips.
    Probed(HashMap<String, bool>),
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
    /// A one-line confirmation for the window's toast overlay.
    Toast(String),
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

        // Periodic reachability sweep: a saved host reached only over a routed network
        // (Tailscale/VPN) never advertises on mDNS, so presence can't come from the advert map
        // alone. Each cycle probes every saved host off the main thread (bounded, trust-agnostic
        // QUIC handshake — the display-side companion to dial-first) and feeds results back as
        // `Probed`; the first sweep runs immediately, then every `PROBE_INTERVAL`.
        {
            let sender = sender.clone();
            glib::spawn_future_local(async move {
                loop {
                    let entries: Vec<(String, String, u16)> = KnownHosts::load()
                        .hosts
                        .iter()
                        .filter(|h| !h.addr.is_empty())
                        .map(|h| (saved_key(h), h.addr.clone(), h.port))
                        .collect();
                    if !entries.is_empty() {
                        let (tx, rx) = async_channel::bounded(1);
                        std::thread::Builder::new()
                            .name("punktfunk-probe".into())
                            .spawn(move || {
                                let targets =
                                    entries.iter().map(|(_, a, p)| (a.clone(), *p)).collect();
                                let results =
                                    crate::trust::probe_reachable_many(targets, PROBE_TIMEOUT);
                                let map: HashMap<String, bool> = entries
                                    .into_iter()
                                    .map(|(k, _, _)| k)
                                    .zip(results)
                                    .collect();
                                let _ = tx.send_blocking(map);
                            })
                            .expect("spawn probe thread");
                        if let Ok(map) = rx.recv().await {
                            sender.input(HostsMsg::Probed(map));
                        }
                    }
                    glib::timeout_future(PROBE_INTERVAL).await;
                }
            });
        }

        let mut model = HostsPage {
            adverts: HashMap::new(),
            probed: HashMap::new(),
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
            HostsMsg::Probed(map) => {
                self.probed = map;
                self.rebuild();
            }
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
                CardOutput::Edit { fp_hex, name } => self.edit_host_dialog(&sender, &fp_hex, &name),
                CardOutput::Forget { fp_hex, name } => self.forget_dialog(&sender, &fp_hex, &name),
                CardOutput::Wake { mac, addr } => crate::wol::wake(&mac, addr.parse().ok()),
                CardOutput::CopyLink(url) => {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                    let _ = sender.output(HostsOutput::Toast("Link copied".into()));
                }
                CardOutput::CreateShortcut { label, url } => {
                    self.shortcut_result(&sender, &label, &url);
                }
                CardOutput::TogglePin {
                    fp_hex,
                    addr,
                    port,
                    profile_id,
                    pin,
                } => {
                    let mut known = KnownHosts::load();
                    if let Some(h) = known.hosts.iter_mut().find(|h| {
                        (!fp_hex.is_empty() && h.fp_hex == fp_hex)
                            || (h.addr == addr && h.port == port)
                    }) {
                        h.pinned_profiles.retain(|p| p != &profile_id);
                        if pin {
                            h.pinned_profiles.push(profile_id);
                        }
                        if let Err(e) = known.save() {
                            tracing::warn!(error = %format!("{e:#}"), "saving the pinned cards");
                        }
                    }
                    self.rebuild();
                }
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
        // One catalog read per refresh, shared by every card's menus and chip.
        let profiles: Rc<Vec<Profile>> = Rc::new(
            pf_client_core::profiles::ProfilesFile::load()
                .profiles
                .into_iter()
                .map(|p| Profile {
                    id: p.id,
                    name: p.name,
                    accent: p.accent,
                })
                .collect(),
        );

        {
            let mut saved = self.saved.guard();
            saved.clear();
            for k in &known.hosts {
                // Online = advertising on mDNS OR proven reachable by the last probe sweep.
                let online = self.adverts.values().any(|a| matches(k, a))
                    || self.probed.get(&saved_key(k)).copied().unwrap_or(false);
                // Learn this host's wake MAC(s) from its live advert while it's online.
                if let Some(a) = self
                    .adverts
                    .values()
                    .find(|a| matches(k, a) && !a.mac.is_empty())
                {
                    crate::trust::learn_mac(&k.fp_hex, &k.addr, k.port, &a.mac);
                }
                // Same for its OS chain — the icon then survives the host going offline.
                if let Some(a) = self
                    .adverts
                    .values()
                    .find(|a| matches(k, a) && !a.os.is_empty())
                {
                    crate::trust::learn_os(&k.fp_hex, &k.addr, k.port, &a.os);
                }
                saved.push_back(HostCard {
                    connecting: self.connecting.as_deref() == Some(k.fp_hex.as_str()),
                    kind: CardKind::Saved {
                        host: k.clone(),
                        online,
                        profiles: profiles.clone(),
                        recent: most_recent.as_deref() == Some(k.fp_hex.as_str()),
                        library_enabled,
                        pinned: None,
                    },
                });
                // …then its pinned host+profile cards, in the order the user pinned them.
                // They share the host's live status because they read the same record, and a
                // pin whose profile is gone simply doesn't render (design §5.2a).
                for id in &k.pinned_profiles {
                    let Some(p) = profiles.iter().find(|p| &p.id == id) else {
                        continue;
                    };
                    let (id, name) = (p.id.clone(), p.name.clone());
                    saved.push_back(HostCard {
                        // The spinner belongs to whichever card was clicked; a pin has its own
                        // key so two cards for one host don't both spin.
                        connecting: false,
                        kind: CardKind::Saved {
                            host: k.clone(),
                            online,
                            profiles: profiles.clone(),
                            recent: false,
                            library_enabled,
                            pinned: Some((id, name)),
                        },
                    });
                }
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
    /// Write the shortcut, or — inside the flatpak sandbox, which cannot reach
    /// `~/.local/share/applications` — hand the user the URL to place themselves. The
    /// DynamicLauncher portal is the intended upgrade for that case (design §5); until then
    /// the fallback is the one the design already sanctions, not a dead end.
    fn shortcut_result(&self, sender: &ComponentSender<Self>, label: &str, url: &str) {
        if crate::shortcuts::sandboxed() {
            let dialog = adw::AlertDialog::new(
                Some("Create Shortcut"),
                Some(
                    "Punktfunk is sandboxed here, so it can't add the shortcut itself. Copy \
                     this link and make a launcher for it \u{2014} it opens the same stream.",
                ),
            );
            let entry = gtk::Entry::builder().text(url).editable(false).build();
            dialog.set_extra_child(Some(&entry));
            dialog.add_responses(&[("close", "Close"), ("copy", "Copy link")]);
            dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
            dialog.set_close_response("close");
            {
                let url = url.to_string();
                dialog.connect_response(Some("copy"), move |_, _| {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                });
            }
            dialog.present(Some(&self.widgets.stack));
            return;
        }
        let msg = match crate::shortcuts::write_desktop_entry(label, url) {
            Ok(_) => format!("Shortcut for \u{201c}{label}\u{201d} added to your applications"),
            Err(e) => {
                tracing::warn!(error = %e, "writing the shortcut");
                format!("Couldn't create the shortcut \u{2014} {e}")
            }
        };
        let _ = sender.output(HostsOutput::Toast(msg));
    }

    /// The host edit sheet — the per-host settings that are properties of the HOST, not of
    /// the stream: its name, whether this machine shares its clipboard with it, and which
    /// settings profile it defaults to.
    ///
    /// Linux had only "Rename" until now; the clipboard toggle in particular existed in the
    /// store and on the Apple and Windows clients but had no Linux surface at all, so a Linux
    /// user could not turn on a feature they were already paying the storage for.
    fn edit_host_dialog(&self, sender: &ComponentSender<Self>, fp_hex: &str, current: &str) {
        let stored = KnownHosts::load()
            .hosts
            .iter()
            .find(|h| h.fp_hex == fp_hex)
            .cloned();
        let name_row = adw::EntryRow::builder().title("Name").build();
        name_row.set_text(current);
        let clipboard_row = adw::SwitchRow::builder()
            .title("Share clipboard")
            .subtitle(
                "Copy and paste between this machine and that host. Per host \u{2014} handing a \
                 host your clipboard is a decision about that host.",
            )
            .build();
        clipboard_row.set_active(stored.as_ref().is_some_and(|h| h.clipboard_sync));

        // Profile picker: "Default settings" plus the catalog, seeded to the current binding.
        let catalog = pf_client_core::profiles::ProfilesFile::load();
        let mut labels = vec!["Default settings".to_string()];
        let mut ids: Vec<String> = vec![String::new()];
        for p in &catalog.profiles {
            labels.push(p.name.clone());
            ids.push(p.id.clone());
        }
        let bound = stored.as_ref().and_then(|h| h.profile_id.clone());
        // A binding whose profile is gone reads as Default settings and is cleaned up on save
        // — the same "dangling resolves as none" rule the connect path follows.
        let selected = bound
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let profile_row = adw::ComboRow::builder()
            .title("Profile")
            .subtitle("The settings a plain click uses for this host")
            .model(&gtk::StringList::new(
                &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .build();
        profile_row.set_selected(selected as u32);

        // Pinned cards: which profiles get their own one-click card for this host. They used
        // to be a third submenu on the card, which is what tipped that menu over — and this is
        // where they belong anyway, next to the default they sit beside (design §5.2a).
        let pin_rows: Vec<(String, adw::SwitchRow)> = catalog
            .profiles
            .iter()
            .map(|p| {
                let row = adw::SwitchRow::builder()
                    .title(&p.name)
                    .subtitle("Show as its own card")
                    .build();
                row.set_active(
                    stored
                        .as_ref()
                        .is_some_and(|h| h.pinned_profiles.iter().any(|id| id == &p.id)),
                );
                (p.id.clone(), row)
            })
            .collect();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&name_row);
        list.append(&profile_row);
        list.append(&clipboard_row);
        for (_, row) in &pin_rows {
            list.append(row);
        }

        let dialog = adw::AlertDialog::new(Some("Edit Host"), None);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            let fp = fp_hex.to_string();
            dialog.connect_response(Some("save"), move |_, _| {
                let name = name_row.text().trim().to_string();
                let mut known = KnownHosts::load();
                if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp) {
                    if !name.is_empty() {
                        h.name = name;
                    }
                    h.clipboard_sync = clipboard_row.is_active();
                    h.profile_id = ids
                        .get(profile_row.selected() as usize)
                        .filter(|id| !id.is_empty())
                        .cloned();
                    // Rebuilt from the switches rather than toggled, so the card order follows
                    // the catalog and a profile deleted meanwhile simply drops out.
                    h.pinned_profiles = pin_rows
                        .iter()
                        .filter(|(_, row)| row.is_active())
                        .map(|(id, _)| id.clone())
                        .collect();
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
                    profile: None,
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
