//! The trust gate and dialogs in front of every connect: TOFU, the SPAKE2 PIN ceremony,
//! and delegated (request-access) approval.

use crate::app::App;
use crate::launch::{start_session, start_session_with, StartOpts};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

/// The trust gate in front of every connect. The host is the policy authority (it
/// advertises `pair=optional` only when it accepts unpaired clients); the client renders
/// its trust UI from that:
///   1. PINNED RECONNECT — a host already pinned to this exact fingerprint connects silently.
///   2. FINGERPRINT CHANGED — a host we know at this address but whose fingerprint no longer
///      matches is the impostor signal: force re-pairing via the PIN ceremony, regardless of
///      the advertised policy.
///   3. NEW host — TOFU is offered only when the host advertised `pair=optional` (rule 3a);
///      otherwise (pair=required, unknown/empty policy, or a manual entry) PIN pairing is
///      mandatory (rule 3b).
///
/// A new host is never auto-connected without a stored pin or an explicit trust decision.
pub fn initiate_connect(app: Rc<App>, req: ConnectRequest) {
    if app.busy.get() {
        return;
    }
    let known = trust::KnownHosts::load();
    match &req.fp_hex {
        Some(fp_hex) => {
            if known.find_by_fp(fp_hex).is_some() {
                // Rule 1: pinned fingerprint matches — silent connect.
                start_session(app, req.clone(), trust::parse_hex32(fp_hex));
            } else if known.find_by_addr(&req.addr, req.port).is_some() {
                // Rule 2: we trust a host at this address but the fingerprint changed —
                // the impostor signal. Re-pair via the PIN ceremony (no TOFU shortcut).
                app.toast("Host fingerprint changed — re-pair with a PIN to continue");
                pin_dialog(app, req);
            } else if req.pair_optional {
                // Rule 3a: the host opted into reduced-security TOFU; offer it alongside PIN.
                tofu_dialog(app, req);
            } else {
                // Rule 3b: pair=required or unknown policy — offer no-PIN delegated approval
                // (request access → approve in the console) or the PIN ceremony.
                approval_dialog(app, req);
            }
        }
        None => {
            // Manual entry (no advertised fingerprint). A known address connects silently
            // on its stored pin (rule 1); an unknown one must pair — request access (approve in
            // the console) or use a PIN; never silent TOFU.
            match known
                .find_by_addr(&req.addr, req.port)
                .and_then(|k| trust::parse_hex32(&k.fp_hex))
            {
                Some(pin) => start_session(app, req, Some(pin)),
                None => approval_dialog(app, req), // rule 3b
            }
        }
    }
}

/// The certificate fingerprint as grouped monospaced hex — 4-char groups over 2 lines
/// (the Apple TrustCardView format), far easier to compare against the host's log than
/// one 64-char run.
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

/// First contact with a discovered host: show the advertised fingerprint and let the user
/// trust it (TOFU), run the PIN ceremony instead, or walk away.
pub fn tofu_dialog(app: Rc<App>, req: ConnectRequest) {
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
    let parent = app.window.clone();
    dialog.connect_response(None, move |_, response| match response {
        "trust" => {
            trust::persist_host(&req.name, &req.addr, req.port, &fp, false);
            start_session(app.clone(), req.clone(), trust::parse_hex32(&fp));
        }
        "pair" => pin_dialog(app.clone(), req.clone()),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The SPAKE2 ceremony: the host is armed and displays a 4-digit PIN; proving knowledge
/// of it pins the host's certificate (and registers ours) with no offline-guessable
/// transcript.
pub fn pin_dialog(app: Rc<App>, req: ConnectRequest) {
    let entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Digits)
        .placeholder_text("4-digit PIN shown by the host")
        .activates_default(true)
        .build();
    // The label the HOST stores this client under (its paired-devices list) — prefilled
    // with the machine hostname, editable (the Apple pair sheet does the same).
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
    let parent = app.window.clone();
    dialog.connect_response(Some("pair"), move |_, _| {
        let pin = entry.text().to_string();
        let app = app.clone();
        let req = req.clone();
        let identity = app.identity.clone();
        let (tx, rx) = async_channel::bounded::<Result<[u8; 32], String>>(1);
        let device = name_entry.text().trim().to_string();
        let name = if device.is_empty() {
            glib::host_name().to_string()
        } else {
            device
        };
        let (host, port) = (req.addr.clone(), req.port);
        std::thread::spawn(move || {
            let result = trust::pair_with_host(&host, port, &identity, &pin, &name)
                .map_err(|e| format!("Pairing failed: {e:?} (wrong PIN, or pairing not armed?)"));
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(fp)) => {
                    trust::persist_host(&req.name, &req.addr, req.port, &trust::hex(&fp), true);
                    app.toast("Paired — connecting…");
                    start_session(app.clone(), req, Some(fp));
                }
                Ok(Err(msg)) => app.toast(&msg),
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(&parent));
}

/// A fresh host that requires pairing: offer the two ways in. "Request access" is the no-PIN
/// path — connect and wait for the operator to click Approve in the host's console/web UI
/// (delegated approval); "Use a PIN instead…" runs the SPAKE2 ceremony.
fn approval_dialog(app: Rc<App>, req: ConnectRequest) {
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
    let parent = app.window.clone();
    dialog.connect_response(None, move |_, response| match response {
        "request" => request_access(app.clone(), req.clone()),
        "pin" => pin_dialog(app.clone(), req.clone()),
        _ => {}
    });
    dialog.present(Some(&parent));
}

/// The no-PIN "request access" flow: open an identified connect that the host PARKS until the
/// operator approves it in the console, showing a cancelable "waiting" dialog meanwhile. On
/// approval the same connection is admitted (no reconnect) and the host is saved as paired.
fn request_access(app: Rc<App>, req: ConnectRequest) {
    // Pin the advertised certificate for a discovered host (defence against a host impostor while
    // we wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
    let pin = req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let cancel = Rc::new(std::cell::Cell::new(false));

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
        let app = app.clone();
        let cancel = cancel.clone();
        waiting.connect_response(Some("cancel"), move |_, _| {
            // Return the UI immediately; the in-flight connect is left to time out and is torn
            // down silently by the event loop (see StartOpts::cancel).
            cancel.set(true);
            app.busy.set(false);
            app.toast("Cancelled — the request may still be pending on the host.");
        });
    }
    waiting.present(Some(&app.window));

    start_session_with(
        app,
        req,
        pin,
        StartOpts {
            // Must exceed the host's approval window (PENDING_APPROVAL_WAIT) so a slow operator
            // approval still lands on this connection rather than timing the client out first.
            connect_timeout: std::time::Duration::from_secs(185),
            persist_paired: true,
            waiting: Some(waiting),
            cancel: Some(cancel),
        },
    );
}
