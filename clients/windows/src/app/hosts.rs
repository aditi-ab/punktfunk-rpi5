//! The hosts page: saved (trusted/paired) hosts with per-host actions (speed test, forget),
//! live mDNS discovery, and a manual connect entry.

use super::connect::initiate;
use super::speed::SpeedState;
use super::style::*;
use super::{Screen, Svc, Target};
use crate::discovery::DiscoveredHost;
use crate::trust::KnownHosts;
use windows_reactor::*;

/// Props for the hosts page: the services plus the changing discovery/status data that must
/// drive its re-render (compared by value, so a new host list or error refreshes the page).
#[derive(Clone)]
pub(crate) struct HostsProps {
    pub(crate) svc: Svc,
    pub(crate) hosts: Vec<DiscoveredHost>,
    pub(crate) status: String,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.hosts == other.hosts && self.status == other.status
    }
}

/// A clickable host row: monogram + name/address + optional action buttons + status pill +
/// chevron. `actions` land between the text and the pill (saved hosts: speed test / forget).
fn host_card(
    name: &str,
    sub: &str,
    badge: &str,
    actions: Vec<Element>,
    on_tap: impl Fn() + 'static,
) -> Element {
    let kind = match badge {
        "Paired" => Pill::Good,
        "Open" => Pill::Neutral,
        _ => Pill::Accent, // Trusted / PIN
    };
    card(
        grid((
            avatar(name)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack((
                text_block(name).font_size(15.0).semibold(),
                text_block(sub)
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(2.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .margin(edges(12.0, 0.0, 0.0, 0.0)),
            hstack(actions)
                .spacing(4.0)
                .grid_column(2)
                .vertical_alignment(VerticalAlignment::Center)
                .margin(edges(0.0, 0.0, 10.0, 0.0)),
            pill(badge, kind)
                .grid_column(3)
                .vertical_alignment(VerticalAlignment::Center)
                .margin(edges(0.0, 0.0, 10.0, 0.0)),
            text_block("\u{203A}")
                .font_size(18.0)
                .foreground(ThemeRef::SecondaryText)
                .grid_column(4)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([
            GridLength::Auto,
            GridLength::Star(1.0),
            GridLength::Auto,
            GridLength::Auto,
            GridLength::Auto,
        ]),
    )
    .on_tapped(on_tap)
    .into()
}

pub(crate) fn hosts_page(props: &HostsProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let hosts = props.hosts.as_slice();
    let status = props.status.as_str();
    let set_screen = &props.svc.set_screen;
    let set_status = &props.svc.set_status;
    let (manual, set_manual) = cx.use_state(String::new());
    // Pending "forget host" confirmation: `(fp_hex, name)` of the saved host to drop. Drives the
    // ContentDialog below; sync state, so setting it re-renders this page.
    let (forget, set_forget) = cx.use_state(Option::<(String, String)>::None);
    let known = KnownHosts::load();

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
            button("Settings")
                .icon(SymbolGlyph::Setting)
                .on_click({
                    let ss = set_screen.clone();
                    move || ss.call(Screen::Settings)
                })
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .margin(edges(0.0, 0.0, 0.0, 6.0))
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

    // Saved (trusted/paired) hosts — reachable even when mDNS isn't.
    if !known.hosts.is_empty() {
        body.push(section("SAVED HOSTS"));
        for k in &known.hosts {
            let target = Target {
                name: k.name.clone(),
                addr: k.addr.clone(),
                port: k.port,
                fp_hex: Some(k.fp_hex.clone()),
                pair_optional: false,
            };
            // Per-host actions: measure the path (probe burst → recommended bitrate) and forget
            // (drops the pinned fingerprint — a later connect re-pairs).
            let speed_btn = {
                let (svc, target) = (props.svc.clone(), target.clone());
                button("Test")
                    .icon(SymbolGlyph::Sync)
                    .subtle()
                    .on_click(move || {
                        *svc.ctx.shared.target.lock().unwrap() = target.clone();
                        // New run: invalidate any still-in-flight probe and reset the screen.
                        svc.ctx
                            .shared
                            .speed_gen
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        svc.set_speed.call(SpeedState::Running);
                        svc.set_screen.call(Screen::SpeedTest);
                    })
            };
            let forget_btn = {
                let (sf, fp, name) = (set_forget.clone(), k.fp_hex.clone(), k.name.clone());
                button("Forget")
                    .icon(SymbolGlyph::Delete)
                    .subtle()
                    .on_click(move || sf.call(Some((fp.clone(), name.clone()))))
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            body.push(host_card(
                &k.name,
                &format!("{}:{}", k.addr, k.port),
                if k.paired { "Paired" } else { "Trusted" },
                vec![speed_btn.into(), forget_btn.into()],
                move || initiate(&ctx2, target.clone(), &ss, &st),
            ));
        }
    }

    // Discovered hosts.
    body.push(section("ON YOUR NETWORK"));
    if hosts.is_empty() {
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
        for h in hosts {
            let target = Target {
                name: h.name.clone(),
                addr: h.addr.clone(),
                port: h.port,
                fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
                pair_optional: h.pair == "optional",
            };
            let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
            let badge = if h.pair == "required" { "PIN" } else { "Open" };
            body.push(host_card(
                &h.name,
                &format!("{}:{}", h.addr, h.port),
                badge,
                Vec::new(),
                move || initiate(&ctx2, target.clone(), &ss, &st),
            ));
        }
    }

    // Manual connection.
    body.push(section("CONNECT MANUALLY"));
    let connect_manual = {
        let (ctx2, ss, st, text) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            manual.clone(),
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
            initiate(
                &ctx2,
                Target {
                    name: addr.clone(),
                    addr,
                    port,
                    fp_hex: None,
                    pair_optional: false,
                },
                &ss,
                &st,
            );
        }
    };
    body.push(
        card(
            grid((
                text_box(manual)
                    .placeholder("host or host:port")
                    .on_changed(move |s| set_manual.call(s))
                    .grid_column(0)
                    .vertical_alignment(VerticalAlignment::Center),
                button("Connect")
                    .accent()
                    .icon(SymbolGlyph::Forward)
                    .on_click(connect_manual)
                    .grid_column(1)
                    .margin(edges(8.0, 0.0, 0.0, 0.0)),
            ))
            .columns([GridLength::Star(1.0), GridLength::Auto]),
        )
        .into(),
    );

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

    page(body)
}
