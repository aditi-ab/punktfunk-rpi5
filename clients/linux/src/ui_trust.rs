//! The trust dialogs in front of a connect: TOFU, the SPAKE2 PIN ceremony, and
//! delegated (request-access) approval. The trust GATE itself (rules 1–3) lives in
//! `AppModel::update` (`AppMsg::Connect`); these are the interaction surfaces it opens,
//! each resolving into typed [`AppMsg`]s.

use crate::app::{AppModel, AppMsg};
use crate::spawn::{CancelHandle, SpawnOpts};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::glib;
use relm4::prelude::*;

/// Wake-and-wait: the FALLBACK after a failed dial to a non-advertising saved host with a
/// known MAC (`AppMsg::WakeConnect` dials first — mDNS absence ≠ unreachable). The host is
/// sent a magic packet, then we poll mDNS until it comes back online — re-sending every few
/// seconds up to a timeout — and route back into the trust gate, **re-keying the saved
/// record if the host woke on a new DHCP IP** (matched by fingerprint). A "Waking…" dialog
/// lets the user cancel. Mirrors the Apple/Android `HostWaker` (90 s budget, resend every 6 s).
pub fn wake_and_connect(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
) {
    let cancel = std::rc::Rc::new(std::cell::Cell::new(false));
    let waiting = adw::AlertDialog::new(
        Some("Waking Host"),
        Some(&format!(
            "Sent a wake signal to “{}”. Waiting for it to come online…",
            req.name
        )),
    );
    waiting.add_responses(&[("cancel", "Cancel")]);
    waiting.set_close_response("cancel");
    {
        let cancel = cancel.clone();
        waiting.connect_response(Some("cancel"), move |_, _| cancel.set(true));
    }
    waiting.present(Some(window));

    let sender = sender.clone();
    glib::spawn_future_local(async move {
        use std::time::{Duration, Instant};
        let events = crate::discovery::browse();
        let started = Instant::now();
        let budget = Duration::from_secs(90);
        let resend = Duration::from_secs(6);
        crate::wol::wake(&req.mac, req.addr.parse().ok());
        let mut last_wake = Instant::now();
        loop {
            if cancel.get() {
                waiting.close();
                return;
            }
            if last_wake.elapsed() >= resend {
                crate::wol::wake(&req.mac, req.addr.parse().ok());
                last_wake = Instant::now();
            }
            // Drain resolved adverts; a match (fingerprint, else addr:port) = it's up.
            while let Ok(ev) = events.try_recv() {
                let crate::discovery::DiscoveryEvent::Resolved(h) = ev else {
                    continue;
                };
                let matched = match &req.fp_hex {
                    Some(fp) => !h.fp_hex.is_empty() && &h.fp_hex == fp,
                    None => h.addr == req.addr && h.port == req.port,
                };
                if matched {
                    waiting.close();
                    let mut req = req.clone();
                    // Re-key on a new DHCP lease so this + future connects dial the
                    // live address.
                    if h.addr != req.addr || h.port != req.port {
                        if let Some(fp) = &req.fp_hex {
                            trust::rekey_addr(fp, &h.addr, h.port);
                        }
                        req.addr = h.addr;
                        req.port = h.port;
                    }
                    sender.input(AppMsg::Connect(req));
                    return;
                }
            }
            if started.elapsed() >= budget {
                waiting.close();
                sender.input(AppMsg::Toast(format!(
                    "Couldn't reach “{}” — is it powered and on the network?",
                    req.name
                )));
                return;
            }
            glib::timeout_future(Duration::from_millis(500)).await;
        }
    });
}

/// The certificate fingerprint as grouped monospaced hex — 4-char groups over 2 lines,
/// far easier to compare against the host's log than one 64-char run.
fn grouped_fingerprint(fp: &str) -> String {
    let groups: Vec<&str> = fp
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    groups
        .chunks(8)
        .map(|line| line.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// First contact with a discovered host that opted into TOFU: show the advertised
/// fingerprint and let the user trust it, run the PIN ceremony instead, or walk away.
/// Trusting starts a session pinned to the advertised fp; it persists once the child
/// proves the host holds that identity (`AppMsg::SessionReady` with `tofu`).
pub fn tofu_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
) {
    let fp = req.fp_hex.clone().unwrap_or_default();
    let dialog = adw::AlertDialog::new(
        Some("New Host"),
        Some(&format!(
            "{} at {}:{}\n\nPairing with a PIN verifies the certificate fingerprint below; \
             trusting accepts it as-is.",
            req.name, req.addr, req.port
        )),
    );
    let fp_label = gtk::Label::new(Some(&grouped_fingerprint(&fp)));
    fp_label.add_css_class("monospace");
    fp_label.set_selectable(true);
    fp_label.set_justify(gtk::Justification::Center);
    fp_label.set_halign(gtk::Align::Center);
    dialog.set_extra_child(Some(&fp_label));
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("pair", "Pair with PIN…"),
        ("trust", "Trust & Connect"),
    ]);
    dialog.set_response_appearance("trust", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("trust"));
    dialog.set_close_response("cancel");
    let sender = sender.clone();
    dialog.connect_response(None, move |_, response| match response {
        "trust" => sender.input(AppMsg::StartSession {
            req: req.clone(),
            fp_hex: fp.clone(),
            tofu: true,
            opts: SpawnOpts::default(),
        }),
        "pair" => sender.input(AppMsg::Pair(req.clone())),
        _ => {}
    });
    dialog.present(Some(window));
}

/// The SPAKE2 ceremony: the host is armed and displays a 4-digit PIN; proving knowledge
/// of it pins the host's certificate (and registers ours) with no offline-guessable
/// transcript. Success persists the host as paired and connects.
pub fn pin_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    identity: (String, String),
    req: ConnectRequest,
) {
    let entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .placeholder_text("4-digit PIN shown by the host")
        .activates_default(true)
        .build();
    // The label the HOST stores this client under — prefilled with the hostname.
    let name_entry = gtk::Entry::builder()
        .text(glib::host_name().as_str())
        .activates_default(true)
        .build();
    let name_caption = gtk::Label::new(Some("This device"));
    name_caption.add_css_class("caption");
    name_caption.add_css_class("dim-label");
    name_caption.set_halign(gtk::Align::Start);
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
    fields.append(&name_caption);
    fields.append(&name_entry);
    let pin_caption = gtk::Label::new(Some("PIN"));
    pin_caption.add_css_class("caption");
    pin_caption.add_css_class("dim-label");
    pin_caption.set_halign(gtk::Align::Start);
    fields.append(&pin_caption);
    fields.append(&entry);
    let dialog = adw::AlertDialog::new(
        Some("Pair with PIN"),
        Some(&format!(
            "Arm pairing on {} (console or web UI), then enter the PIN it displays.",
            req.name
        )),
    );
    dialog.set_extra_child(Some(&fields));
    dialog.add_responses(&[("cancel", "Cancel"), ("pair", "Pair")]);
    dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("pair"));
    dialog.set_close_response("cancel");
    let sender = sender.clone();
    dialog.connect_response(Some("pair"), move |_, _| {
        let pin = entry.text().to_string();
        let req = req.clone();
        let identity = identity.clone();
        let sender = sender.clone();
        let (tx, rx) = async_channel::bounded::<Result<[u8; 32], String>>(1);
        let device = name_entry.text().trim().to_string();
        let name = if device.is_empty() {
            glib::host_name().to_string()
        } else {
            device
        };
        let (host, port) = (req.addr.clone(), req.port);
        std::thread::spawn(move || {
            // Cause-specific wording (wrong PIN vs not-armed vs unreachable vs a typed host
            // rejection) — never blame the PIN for a dead network path.
            let result = trust::pair_with_host(&host, port, &identity, &pin, &name)
                .map_err(|e| trust::pair_error_message(&e));
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(fp)) => {
                    let fp_hex = trust::hex(&fp);
                    trust::persist_host(&req.name, &req.addr, req.port, &fp_hex, true);
                    sender.input(AppMsg::Toast("Paired — connecting…".into()));
                    sender.input(AppMsg::StartSession {
                        req,
                        fp_hex,
                        tofu: false,
                        opts: SpawnOpts::default(),
                    });
                }
                Ok(Err(msg)) => sender.input(AppMsg::Toast(msg)),
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(window));
}

/// A fresh host that requires pairing: "Request access" (connect and wait for the
/// operator to click Approve in the host's console — delegated approval) or the PIN
/// ceremony.
pub fn approval_dialog(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    waiting_slot: std::rc::Rc<std::cell::RefCell<Option<adw::AlertDialog>>>,
    req: ConnectRequest,
) {
    let dialog = adw::AlertDialog::new(
        Some("Pairing Required"),
        Some(&format!(
            "{} requires pairing.\n\nRequest access and approve this device in the host's console \
             (or web UI) — no PIN needed. Or pair with the 4-digit PIN it can display.",
            req.name
        )),
    );
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("pin", "Use a PIN instead…"),
        ("request", "Request Access"),
    ]);
    dialog.set_response_appearance("request", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("request"));
    dialog.set_close_response("cancel");
    let parent = window.clone();
    let window = window.clone();
    let sender = sender.clone();
    dialog.connect_response(None, move |_, response| match response {
        "request" => request_access(&window, &sender, waiting_slot.clone(), req.clone()),
        "pin" => sender.input(AppMsg::Pair(req.clone())),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The no-PIN "request access" flow: the session child opens an identified connect the
/// host PARKS until the operator approves it in the console; a cancelable "waiting"
/// dialog covers the wait. On approval the same connection is admitted and the host is
/// saved as paired. Cancel kills the child (the only abort a parked connect has).
///
/// The pinned fingerprint is the advertised one for a discovered host (defence against
/// an impostor while we wait). A manually-typed host has no advertised fingerprint —
/// the session binary refuses pinless connects, so this path requires the advert; a
/// manual entry's Request Access rides the same flow only when a fingerprint exists.
fn request_access(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
    waiting_slot: std::rc::Rc<std::cell::RefCell<Option<adw::AlertDialog>>>,
    req: ConnectRequest,
) {
    let Some(fp_hex) = req.fp_hex.clone() else {
        // No fingerprint to pin (manual entry): the strict child can't do a
        // trust-on-approval connect — route to the PIN ceremony instead.
        sender.input(AppMsg::Toast(
            "No advertised identity for this host — pair with a PIN instead.".into(),
        ));
        sender.input(AppMsg::Pair(req));
        return;
    };
    let cancel = CancelHandle::default();
    let waiting = adw::AlertDialog::new(
        Some("Waiting for Approval"),
        Some(&format!(
            "Approve “{}” in {}’s console or web UI.\n\nThis device is waiting to be let in — it \
             connects automatically once you approve it.",
            glib::host_name(),
            req.name
        )),
    );
    waiting.add_responses(&[("cancel", "Cancel")]);
    waiting.set_close_response("cancel");
    {
        let sender = sender.clone();
        let cancel = cancel.clone();
        waiting.connect_response(Some("cancel"), move |_, _| {
            cancel.kill();
            sender.input(AppMsg::CancelPending);
        });
    }
    waiting.present(Some(window));
    *waiting_slot.borrow_mut() = Some(waiting);

    sender.input(AppMsg::StartSession {
        req,
        fp_hex,
        tofu: false,
        opts: SpawnOpts {
            // Must exceed the host's approval window (PENDING_APPROVAL_WAIT) so a slow
            // operator approval still lands on this connection.
            connect_timeout_secs: Some(185),
            persist_paired: true,
            cancel: Some(cancel),
        },
    });
}
