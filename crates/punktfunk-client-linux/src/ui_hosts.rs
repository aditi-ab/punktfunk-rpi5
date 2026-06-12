//! The hosts page: saved (trusted) hosts, live mDNS discovery, manual connect entry.

use crate::discovery::{self, DiscoveredHost};
use crate::trust::KnownHosts;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// What the user asked to connect to. `fp_hex` comes from the mDNS TXT record when the
/// host was discovered (drives the TOFU prompt *before* connecting); manual entries have
/// none and trust on first use.
#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub fp_hex: Option<String>,
    pub pair_required: bool,
}

pub fn new(
    on_connect: Rc<dyn Fn(ConnectRequest)>,
    on_settings: Rc<dyn Fn()>,
    on_speed_test: Rc<dyn Fn(ConnectRequest)>,
) -> adw::NavigationPage {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let placeholder = gtk::Label::new(Some("Searching the LAN for hosts…"));
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(24);
    placeholder.set_margin_bottom(24);
    list.set_placeholder(Some(&placeholder));

    // key → (row, latest advert); the activation closure looks the advert up by key so
    // re-adverts (new address, pairing flipped) take effect without rebuilding rows.
    type Rows = Rc<RefCell<HashMap<String, (adw::ActionRow, DiscoveredHost)>>>;
    let rows: Rows = Rc::new(RefCell::new(HashMap::new()));

    {
        let rx = discovery::browse();
        let rows = rows.clone();
        let list = list.downgrade();
        let on_connect = on_connect.clone();
        glib::spawn_future_local(async move {
            while let Ok(host) = rx.recv().await {
                let Some(list) = list.upgrade() else { break };
                let mut map = rows.borrow_mut();
                let subtitle = format!(
                    "{}:{} · pairing {}",
                    host.addr,
                    host.port,
                    if host.pair.is_empty() {
                        "optional"
                    } else {
                        &host.pair
                    }
                );
                if let Some((row, stored)) = map.get_mut(&host.key) {
                    row.set_title(&host.name);
                    row.set_subtitle(&subtitle);
                    *stored = host;
                } else {
                    let row = adw::ActionRow::builder()
                        .title(&host.name)
                        .subtitle(&subtitle)
                        .activatable(true)
                        .build();
                    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                    {
                        let rows = rows.clone();
                        let key = host.key.clone();
                        let on_connect = on_connect.clone();
                        row.connect_activated(move |_| {
                            if let Some((_, h)) = rows.borrow().get(&key) {
                                on_connect(ConnectRequest {
                                    name: h.name.clone(),
                                    addr: h.addr.clone(),
                                    port: h.port,
                                    fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
                                    pair_required: h.pair == "required",
                                });
                            }
                        });
                    }
                    list.append(&row);
                    map.insert(host.key.clone(), (row, host));
                }
            }
        });
    }

    // Manual connect: host:port (punktfunk/1 default port 9777).
    let manual = adw::EntryRow::builder().title("host:port").build();
    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.set_valign(gtk::Align::Center);
    connect_btn.add_css_class("suggested-action");
    manual.add_suffix(&connect_btn);
    let submit = {
        let manual = manual.clone();
        let on_connect = on_connect.clone();
        move || {
            let text = manual.text().to_string();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let (addr, port) = match text.rsplit_once(':') {
                Some((a, p)) => match p.parse::<u16>() {
                    Ok(port) => (a.to_string(), port),
                    Err(_) => return,
                },
                None => (text.to_string(), 9777),
            };
            on_connect(ConnectRequest {
                name: addr.clone(),
                addr,
                port,
                fp_hex: None,
                pair_required: false,
            });
        }
    };
    {
        let submit = submit.clone();
        connect_btn.connect_clicked(move |_| submit());
    }
    manual.connect_entry_activated(move |_| submit());

    let manual_list = gtk::ListBox::new();
    manual_list.add_css_class("boxed-list");
    manual_list.set_selection_mode(gtk::SelectionMode::None);
    manual_list.append(&manual);

    // Saved (trusted/paired) hosts — reachable even when mDNS isn't. Rebuilt every time
    // the page is shown, so fresh TOFU/pairing entries appear on return.
    let saved_label = gtk::Label::new(Some("Saved hosts"));
    saved_label.add_css_class("heading");
    saved_label.set_halign(gtk::Align::Start);
    let saved_list = gtk::ListBox::new();
    saved_list.add_css_class("boxed-list");
    saved_list.set_selection_mode(gtk::SelectionMode::None);
    let rebuild_saved = {
        let saved_list = saved_list.clone();
        let saved_label = saved_label.clone();
        let on_connect = on_connect.clone();
        let on_speed_test = on_speed_test.clone();
        move || {
            saved_list.remove_all();
            let known = KnownHosts::load();
            saved_label.set_visible(!known.hosts.is_empty());
            saved_list.set_visible(!known.hosts.is_empty());
            for k in &known.hosts {
                let row = adw::ActionRow::builder()
                    .title(&k.name)
                    .subtitle(format!(
                        "{}:{}{}",
                        k.addr,
                        k.port,
                        if k.paired {
                            " · paired"
                        } else {
                            " · trusted"
                        }
                    ))
                    .activatable(true)
                    .build();
                let req = ConnectRequest {
                    name: k.name.clone(),
                    addr: k.addr.clone(),
                    port: k.port,
                    fp_hex: Some(k.fp_hex.clone()),
                    pair_required: false,
                };
                let speed_btn = gtk::Button::from_icon_name("network-transmit-receive-symbolic");
                speed_btn.set_tooltip_text(Some("Test network speed"));
                speed_btn.set_valign(gtk::Align::Center);
                speed_btn.add_css_class("flat");
                {
                    let on_speed_test = on_speed_test.clone();
                    let req = req.clone();
                    speed_btn.connect_clicked(move |_| on_speed_test(req.clone()));
                }
                row.add_suffix(&speed_btn);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let on_connect = on_connect.clone();
                row.connect_activated(move |_| on_connect(req.clone()));
                saved_list.append(&row);
            }
        }
    };
    rebuild_saved();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&saved_label);
    content.append(&saved_list);
    let discovered_label = gtk::Label::new(Some("Hosts on this network"));
    discovered_label.add_css_class("heading");
    discovered_label.set_halign(gtk::Align::Start);
    content.append(&discovered_label);
    content.append(&list);
    let manual_label = gtk::Label::new(Some("Manual connection"));
    manual_label.add_css_class("heading");
    manual_label.set_halign(gtk::Align::Start);
    content.append(&manual_label);
    content.append(&manual_list);

    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .child(&content)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    let header = adw::HeaderBar::new();
    let settings_btn = gtk::Button::from_icon_name("preferences-system-symbolic");
    settings_btn.set_tooltip_text(Some("Preferences"));
    settings_btn.connect_clicked(move |_| on_settings());
    header.pack_end(&settings_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));

    let page = adw::NavigationPage::builder()
        .title("Punktfunk")
        .tag("hosts")
        .child(&toolbar)
        .build();
    page.connect_shown(move |_| rebuild_saved());
    page
}
