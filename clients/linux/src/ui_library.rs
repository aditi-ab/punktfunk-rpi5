//! The game-library page (the Apple `LibraryView` ported): a poster grid of the host's
//! unified library fetched over the management API (`library.rs`), pushed onto the nav
//! stack from a saved card's "Browse library…" action. Poster art loads asynchronously
//! (worker threads → texture on the main loop) with a monogram placeholder, and tapping
//! a title starts a session that asks the host to launch it (the library id rides the
//! Hello via `ConnectRequest::launch`).

use crate::app::App;
use crate::library::{self, GameEntry};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Concurrent poster fetches — a handful is plenty for a LAN art proxy without turning a
/// big library into a connection burst.
const ART_WORKERS: usize = 3;

/// Everything the page re-renders from. Kept alive by the widget closures (reload/retry/
/// card activation); dropped when the page is popped, which also winds down any in-flight
/// art consumer (its weak upgrade fails).
struct State {
    app: Rc<App>,
    /// The host this library belongs to — cards clone it and add `launch`.
    req: ConnectRequest,
    stack: gtk::Stack,
    flow: gtk::FlowBox,
    error_page: adw::StatusPage,
    /// Per-page poster cache (entry id → texture) — a Retry re-renders without refetching.
    art: RefCell<HashMap<String, gdk::Texture>>,
    /// The Picture each entry currently renders into (rebuilt per render), so async art
    /// results land on the right card.
    pics: RefCell<HashMap<String, gtk::Picture>>,
    /// Screenshot mode: render injected entries only, never touch the network.
    mock: Cell<bool>,
}

/// Open the library page for a saved host and start the fetch.
pub fn open(app: Rc<App>, req: ConnectRequest) {
    let state = build(app.clone(), req);
    load(&state);
}

/// Screenshot-scene entry: render injected entries (plus pre-seeded textures, keyed by
/// entry id) with no host and no network — the CI `library` scene.
pub fn open_mock(
    app: Rc<App>,
    req: ConnectRequest,
    games: Vec<GameEntry>,
    art: Vec<(String, gdk::Texture)>,
) {
    let state = build(app.clone(), req);
    state.mock.set(true);
    state.art.borrow_mut().extend(art);
    if games.is_empty() {
        state.stack.set_visible_child_name("empty");
    } else {
        render(&state, &games);
        state.stack.set_visible_child_name("grid");
    }
}

/// Build the page (loading / error / empty / grid states in a stack) and push it.
fn build(app: Rc<App>, req: ConnectRequest) -> Rc<State> {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(6)
        .column_spacing(12)
        .row_spacing(18)
        .valign(gtk::Align::Start)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&flow);
    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .child(&content)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    let loading = gtk::Box::new(gtk::Orientation::Vertical, 12);
    loading.set_valign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.start();
    spinner.set_halign(gtk::Align::Center);
    loading.append(&spinner);
    let loading_label = gtk::Label::new(Some("Loading library…"));
    loading_label.add_css_class("dim-label");
    loading.append(&loading_label);

    let error_page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Couldn't load the library")
        .build();
    let retry = gtk::Button::with_label("Retry");
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_halign(gtk::Align::Center);
    error_page.set_child(Some(&retry));

    let empty = adw::StatusPage::builder()
        .icon_name("applications-games-symbolic")
        .title("No games found")
        .description(
            "No games found on this host. Install Steam titles or add custom \
                      entries in the host's web console.",
        )
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&error_page, Some("error"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scrolled, Some("grid"));

    let header = adw::HeaderBar::new();
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload"));
    header.pack_end(&reload);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let page = adw::NavigationPage::builder()
        .title(format!("{} — Library", req.name))
        .child(&toolbar)
        .build();

    let state = Rc::new(State {
        app: app.clone(),
        req,
        stack,
        flow,
        error_page,
        art: RefCell::new(HashMap::new()),
        pics: RefCell::new(HashMap::new()),
        mock: Cell::new(false),
    });
    {
        let state = state.clone();
        reload.connect_clicked(move |_| load(&state));
    }
    {
        let state = state.clone();
        retry.connect_clicked(move |_| load(&state));
    }
    app.nav.push(&page);
    state
}

/// The mgmt port for this host: the live mDNS `mgmt` TXT when the host is advertising,
/// else the well-known default (Apple's `effectiveMgmtPort`).
fn mgmt_port(state: &State) -> u16 {
    state
        .app
        .hosts_ui()
        .and_then(|h| h.mgmt_port_for(&state.req))
        .unwrap_or(library::DEFAULT_MGMT_PORT)
}

/// Fetch the library off the main thread and route the result into the grid or the
/// error/empty states.
fn load(state: &Rc<State>) {
    if state.mock.get() {
        return; // screenshot scene renders injected entries only
    }
    state.stack.set_visible_child_name("loading");
    let port = mgmt_port(state);
    let addr = state.req.addr.clone();
    let identity = state.app.identity.clone();
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            let _ = tx.send_blocking(library::fetch_games(&addr, port, &identity, pin));
        })
        .expect("spawn library thread");
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else { return };
        let Some(state) = weak.upgrade() else { return };
        match result {
            Ok(games) if games.is_empty() => state.stack.set_visible_child_name("empty"),
            Ok(games) => {
                render(&state, &games);
                state.stack.set_visible_child_name("grid");
                load_art(&state, &games);
            }
            Err(e) => {
                state.error_page.set_description(Some(&e.to_string()));
                state.stack.set_visible_child_name("error");
            }
        }
    });
}

/// (Re)build the poster grid from one library snapshot. Cached textures apply
/// immediately; the rest keep their monogram placeholder until `load_art` delivers.
fn render(state: &Rc<State>, games: &[GameEntry]) {
    state.flow.remove_all();
    state.pics.borrow_mut().clear();
    for game in games {
        state.flow.append(&game_card(state, game));
    }
}

/// One poster tile: 2:3 art (~150×225 logical) over the title, with a store badge and a
/// monogram placeholder underneath the async art. Activation starts a session launching
/// this title (silent on a pinned host — the normal trust gate applies).
fn game_card(state: &Rc<State>, game: &GameEntry) -> gtk::FlowBoxChild {
    let monogram = gtk::Label::new(Some(&initials(&game.title)));
    monogram.add_css_class("pf-poster-monogram");
    monogram.set_halign(gtk::Align::Center);
    monogram.set_valign(gtk::Align::Center);
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    placeholder.append(&monogram);
    monogram.set_vexpand(true);

    let pic = gtk::Picture::new();
    pic.set_content_fit(gtk::ContentFit::Cover);
    if let Some(tex) = state.art.borrow().get(&game.id) {
        pic.set_paintable(Some(tex));
    }
    state.pics.borrow_mut().insert(game.id.clone(), pic.clone());

    let badge = gtk::Label::new(Some(store_label(&game.store)));
    badge.add_css_class("pf-pill");
    badge.add_css_class("pf-store-badge");
    badge.set_halign(gtk::Align::Start);
    badge.set_valign(gtk::Align::Start);
    badge.set_margin_start(6);
    badge.set_margin_top(6);

    let poster = gtk::Overlay::new();
    poster.set_child(Some(&placeholder));
    poster.add_overlay(&pic);
    poster.add_overlay(&badge);
    poster.add_css_class("pf-poster");
    poster.set_overflow(gtk::Overflow::Hidden);
    poster.set_size_request(150, 225);
    poster.set_halign(gtk::Align::Center);

    let title = gtk::Label::new(Some(&game.title));
    title.add_css_class("caption");
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_max_width_chars(16);
    title.set_tooltip_text(Some(&game.title));

    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.append(&poster);
    card.append(&title);

    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&card));
    let app = state.app.clone();
    let mut req = state.req.clone();
    req.launch = Some((game.id.clone(), game.title.clone()));
    child.connect_activate(move |_| crate::ui_trust::initiate_connect(app.clone(), req.clone()));
    child
}

/// Fetch poster art for every uncached entry on a small worker pool, walking each
/// entry's candidates in the Apple fallback order (portrait → header → hero) and
/// texturing the first that loads on the main loop.
fn load_art(state: &Rc<State>, games: &[GameEntry]) {
    let port = mgmt_port(state);
    let base = library::base_url(&state.req.addr, port);
    let jobs: VecDeque<(String, Vec<String>)> = {
        let cache = state.art.borrow();
        games
            .iter()
            .filter(|g| !cache.contains_key(&g.id))
            .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
            .filter(|(_, candidates)| !candidates.is_empty())
            .collect()
    };
    if jobs.is_empty() {
        return;
    }
    let identity = state.app.identity.clone();
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let queue = Arc::new(Mutex::new(jobs));
    let (tx, rx) = async_channel::unbounded::<(String, Vec<u8>)>();
    for _ in 0..ART_WORKERS {
        let queue = queue.clone();
        let tx = tx.clone();
        let base = base.clone();
        let identity = identity.clone();
        std::thread::Builder::new()
            .name("punktfunk-lib-art".into())
            .spawn(move || {
                let Ok(agent) = library::agent(&identity, pin) else {
                    return;
                };
                loop {
                    let job = queue.lock().unwrap().pop_front();
                    let Some((id, candidates)) = job else { break };
                    for url in &candidates {
                        match library::fetch_art(&agent, &base, url) {
                            Ok(bytes) => {
                                // Receiver gone (page popped) — stop fetching.
                                if tx.send_blocking((id, bytes)).is_err() {
                                    return;
                                }
                                break;
                            }
                            // 404 on a guessed CDN path is routine — try the next kind.
                            Err(e) => tracing::debug!(%id, url, error = %e, "poster miss"),
                        }
                    }
                }
            })
            .expect("spawn art thread");
    }
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        while let Ok((id, bytes)) = rx.recv().await {
            let Some(state) = weak.upgrade() else { break };
            // Texture decode happens here on the main loop — posters are small (tens of
            // KB), and `from_bytes` handles jpeg/png alike.
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                Ok(tex) => {
                    if let Some(pic) = state.pics.borrow().get(&id) {
                        pic.set_paintable(Some(&tex));
                    }
                    state.art.borrow_mut().insert(id, tex);
                }
                Err(e) => tracing::debug!(%id, error = %e, "undecodable poster"),
            }
        }
    });
}

/// The store badge text — `store` comes from the entry (today `steam`/`custom`; future
/// stores per the host's provider list), with the id prefix as a fallback spelling.
fn store_label(store: &str) -> &'static str {
    match store {
        "steam" => "Steam",
        "custom" => "Custom",
        "heroic" => "Heroic",
        "lutris" => "Lutris",
        "epic" => "Epic",
        "gog" => "GOG",
        "xbox" => "Xbox",
        _ => "Game",
    }
}

/// Monogram for the placeholder tile: the first letters of the first two words.
fn initials(title: &str) -> String {
    title
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_take_two_words() {
        assert_eq!(initials("Dota 2"), "D2");
        assert_eq!(initials("half-life"), "H");
        assert_eq!(initials("The Witness III"), "TW");
        assert_eq!(initials(""), "");
    }
}
