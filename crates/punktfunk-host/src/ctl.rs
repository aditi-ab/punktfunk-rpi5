//! `punktfunk-host ctl` — the operator surface as a **subcommand**, not a second binary.
//!
//! Everything the web console's daily 95 % does — approve a pending device, type a Moonlight PIN,
//! rename or unpair, change an access preset, stop a session, watch events — reachable from a
//! terminal, a shell script, or a Quickshell `Process`. Its first consumer is the Omarchy shell
//! plugin (design D10/D13), but nothing here is Omarchy-specific: the surface is a loopback client
//! of the mgmt API, and `watch`'s line-JSON is as usable from a waybar module or `while read`.
//!
//! **Why a subcommand.** A new binary touches every Linux artifact we ship (the Arch PKGBUILD, the
//! deb/rpm builders, the sysext, the Nix module, signing and manifests) to buy nothing: `main.rs`
//! already dispatches a dozen verbs, and in-crate means the client deserialises what the server
//! serialises with no second declaration of the types to drift. The one cost is startup — the host
//! binary links the world — and it is paid once per *action*, not per poll, because the
//! interactive consumer holds one long-lived `watch`. Measured threshold, recorded so the decision
//! is falsifiable: if `ctl status --json` p50 exceeds ~150 ms on the target box, lift `ctl/` behind
//! a thin bin; the module boundary makes that mechanical.
//!
//! **Security** is [`client`]'s module docs: pin before token (I2), no credential on argv or in
//! the environment (I1), no server-side change at all (I4 — this crate's `mgmt/auth.rs` is
//! untouched by the whole surface). ctl consumes the token the host persists; it never mints one.
//!
//! **Approval UX (I6)** is enforced here rather than left to each front-end: `approve`/`deny` take
//! an **id**, never "the newest", and every listing prints the claimed name next to the
//! fingerprint tail so an operator approving a device is looking at what the device claims *and*
//! at something it cannot forge.
//!
//! Exit codes: 0 success, 1 the host refused, 2 usage, 3 no host reachable, 4 certificate pin
//! mismatch. 4 is separate on purpose — it is the security signal, and a script that treats it as
//! "host down" would retry straight into a squatter.

pub mod client;
mod watch;

use client::{Client, Failure, Result, SCHEMA_VERSION};
use punktfunk_core::quic::{
    GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_FULL, GRANT_PRESET_VIEW_ONLY,
};
use serde_json::{json, Value};

pub fn main(args: &[String]) -> anyhow::Result<()> {
    // `--json` is positionless: it is a mode, not an argument to any one verb.
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    match run(&rest, json) {
        Ok(()) => Ok(()),
        Err(f) => {
            if json {
                // Machine consumers get the failure on stdout in the same envelope as a success,
                // so a QML `Process` parses one shape and reads `error` or `data`.
                println!(
                    "{}",
                    json!({"v": SCHEMA_VERSION, "error": {"code": f.code, "message": f.message}})
                );
            } else {
                eprintln!("punktfunk-host ctl: {}", f.message);
            }
            std::process::exit(f.code)
        }
    }
}

fn run(args: &[&str], json: bool) -> Result<()> {
    let Some(verb) = args.first().copied() else {
        print_usage();
        return Err(Failure::usage("no verb given"));
    };
    let rest = &args[1..];
    match verb {
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        "status" => {
            let v = Client::connect(None)?.get("/api/v1/status")?;
            out(json, &v, render_status);
            Ok(())
        }
        "sessions" => {
            let v = Client::connect(None)?.get("/api/v1/status")?;
            let slice = json!({
                "active_sessions": v.get("active_sessions").cloned().unwrap_or(Value::Null),
                "video_streaming": v.get("video_streaming").cloned().unwrap_or(Value::Null),
                "audio_streaming": v.get("audio_streaming").cloned().unwrap_or(Value::Null),
                "session": v.get("session").cloned().unwrap_or(Value::Null),
                "stream": v.get("stream").cloned().unwrap_or(Value::Null),
                "games": v.get("games").cloned().unwrap_or(Value::Null),
            });
            out(json, &slice, render_sessions);
            Ok(())
        }
        // `/status` deliberately exposes no device names, so it cannot answer "who is streaming".
        // `/local/summary` is the one endpoint that does — one label, the streaming client's — and
        // it folds in the host version and the conflicting-host warning a status surface wants.
        "summary" => {
            let v = Client::connect(None)?.get("/api/v1/local/summary")?;
            out(json, &v, render_summary);
            Ok(())
        }
        "stop-session" => {
            let v = Client::connect(None)?.delete("/api/v1/session")?;
            out(json, &v, |_| println!("session stopped"));
            Ok(())
        }
        "end-game" => {
            let v = Client::connect(None)?.post("/api/v1/game/end", &json!({}))?;
            out(json, &v, |_| println!("game ended"));
            Ok(())
        }
        "pair" => pair(rest, json),
        "pending" => {
            let v = Client::connect(None)?.get("/api/v1/native/pending")?;
            out(json, &v, render_pending);
            Ok(())
        }
        "approve" => approve(rest, json),
        "deny" => {
            let id = one_id(rest, "deny")?;
            let v = Client::connect(None)?
                .post(&format!("/api/v1/native/pending/{id}/deny"), &json!({}))?;
            out(json, &v, move |_| println!("denied device {id}"));
            Ok(())
        }
        "pin" => {
            let pin = rest
                .first()
                .copied()
                .ok_or_else(|| Failure::usage("pin: give the PIN the client is showing"))?;
            // Optional target: with several clients parked the PIN must be addressed to one
            // ceremony (security-review 2026-08-31 H-4) — `ctl status` shows the identities.
            let mut body = json!({ "pin": pin });
            if let Some(uid) = rest.get(1) {
                body["uniqueid"] = json!(uid);
            }
            if let Some(fp) = rest.get(2) {
                body["fingerprint"] = json!(fp);
            }
            let v = Client::connect(None)?.post("/api/v1/pair/pin", &body)?;
            out(json, &v, |_| println!("PIN submitted"));
            Ok(())
        }
        // Not an API call at all: a ticket the console can verify with the token it already holds.
        // See `console_url` — the point is that reading the 0600 token IS the proof.
        "console-url" => {
            let url = console_url()?;
            out(json, &json!({ "url": url }), move |_| println!("{url}"));
            Ok(())
        }
        "clients" => {
            let c = Client::connect(None)?;
            // Both planes, labelled — a device list that silently covered only one of them is how
            // "I unpaired it and it still connects" happens.
            let both = json!({
                "native": c.get("/api/v1/native/clients")?,
                "gamestream": c.get("/api/v1/clients")?,
            });
            out(json, &both, render_clients);
            Ok(())
        }
        "rename" => rename(rest, json),
        "unpair" => unpair(rest, json),
        "access" => access(rest, json),
        "display" => display(rest, json),
        "stats" => stats(rest, json),
        "watch" => {
            let kinds = flag_value(rest, "--kinds");
            let since = flag_value(rest, "--since")
                .map(|s| {
                    s.parse::<u64>().map_err(|_| {
                        Failure::usage("watch: --since takes an event sequence number")
                    })
                })
                .transpose()?;
            watch::run(kinds.as_deref(), since)
        }
        other => {
            print_usage();
            Err(Failure::usage(format!("unknown ctl verb '{other}'")))
        }
    }
}

// ── verbs with enough argument shape to deserve their own function ─────────────────────────────

fn pair(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("arm") => {
            let mut body = json!({});
            if let Some(ttl) = num_flag(args, "--ttl")? {
                body["ttl_secs"] = json!(ttl);
            }
            if let Some(exp) = num_flag(args, "--expires-in")? {
                body["expires_in_secs"] = json!(exp);
            }
            if let Some(g) = preset_flag(args)? {
                body["grants"] = json!(g);
            }
            // Binding the window to one fingerprint is the difference between "a device may pair"
            // and "any LAN peer may burn my pairing window" (security review #9) — so it is a
            // first-class flag here, not console-only.
            if let Some(fp) = flag_value(args, "--fingerprint") {
                body["fingerprint"] = json!(fp);
            }
            let v = c.post("/api/v1/native/pair/arm", &body)?;
            out(json, &v, render_pair);
            Ok(())
        }
        Some("disarm") => {
            let v = c.delete("/api/v1/native/pair")?;
            out(json, &v, |_| println!("pairing window closed"));
            Ok(())
        }
        Some("status") | None => {
            let v = c.get("/api/v1/native/pair")?;
            // The GameStream PIN flow lives on a different route; fold it in so one command
            // answers "is anything waiting for me?" for both planes.
            let mut v = v;
            if let Ok(gs) = c.get("/api/v1/pair") {
                v["pin_pending"] = gs.get("pin_pending").cloned().unwrap_or(json!(false));
                v["pending"] = gs.get("pending").cloned().unwrap_or(json!([]));
            }
            out(json, &v, render_pair);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "pair: expected arm | disarm | status, got '{other}'"
        ))),
    }
}

fn approve(args: &[&str], json: bool) -> Result<()> {
    let id = one_id(args, "approve")?;
    let mut body = json!({});
    if let Some(name) = flag_value(args, "--name") {
        body["name"] = json!(name);
    }
    if let Some(g) = preset_flag(args)? {
        body["grants"] = json!(g);
    }
    if let Some(exp) = num_flag(args, "--expires-in")? {
        body["expires_in_secs"] = json!(exp);
    }
    let v = Client::connect(None)?.post(&format!("/api/v1/native/pending/{id}/approve"), &body)?;
    out(json, &v, move |_| println!("approved device {id}"));
    Ok(())
}

fn rename(args: &[&str], json: bool) -> Result<()> {
    let (fp, name) = match (args.first(), args.get(1)) {
        (Some(fp), Some(name)) => (*fp, *name),
        _ => return Err(Failure::usage("rename: <fingerprint> <name>")),
    };
    let c = Client::connect(None)?;
    // The two planes keep separate stores and separate routes, and a fingerprint belongs to
    // exactly one of them. Try native first (the default plane), fall back to GameStream, so the
    // operator does not have to know which store a device they can see in `ctl clients` lives in.
    let native = c.patch(
        &format!("/api/v1/native/clients/{fp}"),
        &json!({ "name": name }),
    );
    let v = match native {
        Ok(v) => v,
        Err(e) if e.code == client::EXIT_API => {
            c.patch(&format!("/api/v1/clients/{fp}"), &json!({ "label": name }))?
        }
        Err(e) => return Err(e),
    };
    out(json, &v, move |_| println!("renamed {fp} to {name}"));
    Ok(())
}

fn unpair(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    if args.contains(&"--all") {
        // Unpairing everything is the one destructive verb here — every device on the box has to
        // be re-paired by hand afterwards. Human mode asks; machine mode demands `--yes`, because
        // a plugin cannot answer a prompt and must not be able to do this by accident.
        if !args.contains(&"--yes") {
            if json {
                return Err(Failure::usage(
                    "unpair --all needs --yes in --json mode (it cannot prompt)",
                ));
            }
            if !confirm("Unpair EVERY device on both planes? [y/N] ") {
                return Err(Failure::usage("cancelled"));
            }
        }
        let both = json!({
            "native": c.delete("/api/v1/native/clients")?,
            "gamestream": c.delete("/api/v1/clients")?,
        });
        out(json, &both, |_| println!("all devices unpaired"));
        return Ok(());
    }
    let fp = args
        .first()
        .copied()
        .filter(|a| !a.starts_with("--"))
        .ok_or_else(|| Failure::usage("unpair: <fingerprint>, or --all"))?;
    let native = c.delete(&format!("/api/v1/native/clients/{fp}"));
    let v = match native {
        Ok(v) => v,
        Err(e) if e.code == client::EXIT_API => c.delete(&format!("/api/v1/clients/{fp}"))?,
        Err(e) => return Err(e),
    };
    out(json, &v, move |_| println!("unpaired {fp}"));
    Ok(())
}

fn access(args: &[&str], json: bool) -> Result<()> {
    let (fp, preset) = match (args.first(), args.get(1)) {
        (Some(fp), Some(p)) => (*fp, *p),
        _ => {
            return Err(Failure::usage(
                "access: <fingerprint> <full|controller|view>",
            ))
        }
    };
    // Presets only. The full grant matrix is the console's job (per-client-access design): a
    // bitmask on a command line is exactly the kind of thing that gets a digit wrong and silently
    // hands a device the keyboard.
    let grants = grants_for(preset)?;
    let v = Client::connect(None)?.patch(
        &format!("/api/v1/native/clients/{fp}"),
        &json!({ "grants": grants }),
    )?;
    out(json, &v, move |_| println!("{fp}: access set to {preset}"));
    Ok(())
}

// ── displays ───────────────────────────────────────────────────────────────────────────────────

/// The virtual-display policy and what is live right now, in one answer.
///
/// Two GETs rather than one because the API keeps them apart on purpose — `/display/settings` is
/// the stored policy and its preset catalogue, `/display/state` is the displays that exist this
/// second — and a caller that had to make two calls to answer one question would show them at two
/// different instants.
///
/// ⚠ `displays` is EMPTY on wlroots, and not because nothing is streaming: a wlroots capture
/// arrives over a sandboxed xdp portal fd the host cannot re-open per attach, so
/// `vdisplay::registry` passes those displays through rather than owning them (its own module docs
/// say so) and never lists them. Measured on an Omarchy box: a live 2414x1188@240 head,
/// `displays: []`. Read an empty list as "this host does not track them", never as "there are
/// none" — the Omarchy panel shows no live-display section for exactly this reason.
fn display(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("preset") => {
            let id = args
                .get(1)
                .copied()
                .ok_or_else(|| Failure::usage("display preset: <ID> (see `ctl display`)"))?;
            let v = display_apply_preset(&c, id)?;
            out(json, &v, move |_| println!("display preset set to {id}"));
            Ok(())
        }
        Some("release") => {
            // The slot is optional and releases exactly one head; omitting it releases every KEPT
            // display. Never an active one — that is `stop-session`, and conflating the two would
            // make "give me my screen back" able to kill somebody's stream.
            let mut body = json!({});
            if let Some(slot) = args.get(1) {
                let n: u32 = slot
                    .parse()
                    .map_err(|_| Failure::usage("display release: SLOT must be a number"))?;
                body["slot"] = json!(n);
            }
            let v = c.post("/api/v1/display/release", &body)?;
            out(json, &v, |v| {
                println!(
                    "released {} kept display(s)",
                    v["released"].as_i64().unwrap_or(0)
                )
            });
            Ok(())
        }
        None | Some("status") => {
            let mut v = c.get("/api/v1/display/settings")?;
            // `/display/state` is unavailable on a build with no vdisplay backend; the policy half
            // is still worth answering, so an empty list beats failing the whole verb.
            let live = c
                .get("/api/v1/display/state")
                .map(|s| s["displays"].clone())
                .unwrap_or(Value::Null);
            v["displays"] = if live.is_array() { live } else { json!([]) };
            out(json, &v, render_display);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "display: expected status | preset | release, got '{other}'"
        ))),
    }
}

/// Switch the stored policy to `id`, preserving every axis a preset does not own.
///
/// `PUT /display/settings` replaces the whole object, so this reads the stored policy first and
/// edits it — the console does exactly this (`web/src/sections/Displays/DisplayCard.tsx`), and for
/// the same reason: `capture_monitor` and the experimental Windows axes are NOT preset behavior,
/// and a PUT that dropped them would swap the streamed screen out from under the operator because
/// they picked a different lifecycle.
///
/// A saved custom preset has no apply route of its own: applying it means writing a `Custom` policy
/// carrying its saved fields. That asymmetry is the API's, mirrored here so both kinds of id work.
fn display_apply_preset(c: &Client, id: &str) -> Result<Value> {
    let state = c.get("/api/v1/display/settings")?;
    let policy = policy_with_preset(&state, id)?;
    c.put("/api/v1/display/settings", &policy)
}

/// The edit itself, split from the two HTTP calls so it can be checked against a settings body
/// without a running host. Everything that can go wrong here — dropping an orthogonal axis,
/// accepting an id no preset has — is in this function, not in the transport around it.
fn policy_with_preset(state: &Value, id: &str) -> Result<Value> {
    let mut policy = state["settings"].clone();
    if !policy.is_object() {
        return Err(Failure::api("display: the host returned no stored policy"));
    }

    let custom = state["custom_presets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"].as_str() == Some(id))
        .cloned();

    if let Some(p) = custom {
        policy["preset"] = json!("custom");
        for (k, val) in p["fields"].as_object().into_iter().flatten() {
            policy[k.as_str()] = val.clone();
        }
        // `game_session` is the one orthogonal axis a saved preset does carry.
        if let Some(gs) = p.get("game_session") {
            policy["game_session"] = gs.clone();
        }
    } else {
        let known = state["presets"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["id"].as_str() == Some(id));
        if !known {
            // Listing what exists beats "invalid preset": the built-ins and the operator's saved
            // ones share one id space, and only the host knows the saved half.
            return Err(Failure::usage(format!(
                "display preset: no preset '{id}' — run `ctl display` for the list"
            )));
        }
        policy["preset"] = json!(id);
    }
    Ok(policy)
}

fn render_display(v: &Value) {
    let cur = v["settings"]["preset"].as_str().unwrap_or("custom");
    println!("preset    {cur}");
    let eff = &v["effective"];
    println!(
        "policy    topology {} · identity {} · conflict {} · max {}",
        eff["topology"].as_str().unwrap_or("—"),
        eff["identity"].as_str().unwrap_or("—"),
        eff["mode_conflict"].as_str().unwrap_or("—"),
        eff["max_displays"].as_i64().unwrap_or(0)
    );
    for p in v["presets"].as_array().into_iter().flatten() {
        let id = p["id"].as_str().unwrap_or("");
        println!(
            "  {}{:<16} {}",
            if id == cur { "*" } else { " " },
            id,
            trunc(p["summary"].as_str().unwrap_or(""), 60)
        );
    }
    for p in v["custom_presets"].as_array().into_iter().flatten() {
        println!(
            "  {:<17} {} (saved)",
            p["id"].as_str().unwrap_or(""),
            trunc(p["name"].as_str().unwrap_or(""), 50)
        );
    }
    let live = v["displays"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if live.is_empty() {
        println!("live      none");
        return;
    }
    for d in live {
        println!(
            "live      slot {} · {} · {} · {} session(s){}",
            d["slot"].as_i64().unwrap_or(0),
            d["mode"].as_str().unwrap_or("—"),
            d["state"].as_str().unwrap_or("—"),
            d["sessions"].as_i64().unwrap_or(0),
            d["client"]
                .as_str()
                .map(|c| format!(" · {}", trunc(c, 24)))
                .unwrap_or_default()
        );
    }
}

// ── stats ──────────────────────────────────────────────────────────────────────────────────────

/// What the stream is doing right now.
///
/// Two layers, because they cost different things. The **free** layer is `/status`: the negotiated
/// mode and codec plus `bitrate_kbps`, which is the live encoder target and moves with every
/// adaptive-bitrate change — so it is a real read-out, not a copy of the handshake. The **detail**
/// layer (per-frame drops and stage percentiles) only exists while a performance capture is armed,
/// because that is when the streaming loops emit samples at all.
///
/// This verb never arms a capture as a side effect of being read. Arming has a consequence a reader
/// did not ask for — `stats record stop` writes a recording to disk, and the capture is a single
/// host-wide slot the web console also drives — so it stays an explicit verb.
fn stats(args: &[&str], json: bool) -> Result<()> {
    let c = Client::connect(None)?;
    match args.first().copied() {
        Some("record") => match args.get(1).copied() {
            Some("start") => {
                let v = c.post("/api/v1/stats/capture/start", &json!({}))?;
                out(json, &v, |_| println!("capture armed"));
                Ok(())
            }
            Some("stop") => {
                let v = c.post("/api/v1/stats/capture/stop", &json!({}))?;
                out(json, &v, |v| {
                    // 204 when nothing was recording — `finish` gives that back as a null body.
                    match v["id"].as_str() {
                        Some(id) => println!("capture saved as {id}"),
                        None => println!("nothing was recording"),
                    }
                });
                Ok(())
            }
            _ => Err(Failure::usage("stats record: expected start | stop")),
        },
        None | Some("status") => {
            let status = c.get("/api/v1/status")?;
            let capture = c
                .get("/api/v1/stats/capture/status")
                .unwrap_or_else(|_| json!({ "armed": false }));
            // The live capture holds its WHOLE time-series (up to 5400 samples). A read-out wants
            // the newest one, so the tail is taken here rather than shipping the series to every
            // caller that only ever renders one row of numbers.
            let mut sample = Value::Null;
            let mut meta = Value::Null;
            if capture["armed"].as_bool().unwrap_or(false) {
                if let Ok(live) = c.get("/api/v1/stats/capture/live") {
                    meta = live["meta"].clone();
                    sample = live["samples"]
                        .as_array()
                        .and_then(|s| s.last())
                        .cloned()
                        .unwrap_or(Value::Null);
                }
            }
            let v = json!({
                "video_streaming": status["video_streaming"],
                "active_sessions": status["active_sessions"],
                "session": status["session"],
                "stream": status["stream"],
                "games": status["games"],
                "capture": capture,
                "sample": sample,
                "meta": meta,
            });
            out(json, &v, render_stats);
            Ok(())
        }
        Some(other) => Err(Failure::usage(format!(
            "stats: expected status | record, got '{other}'"
        ))),
    }
}

/// A stage duration in the unit that shows it. Encode lands around 2 300 µs and `send` around
/// 15 µs, so one fixed unit loses an end: printing everything in ms flattens `send` to `0.0`, and
/// printing everything in µs makes encode a five-digit number to squint at.
fn dur_us(us: f64) -> String {
    if us >= 1000.0 {
        format!("{:.1} ms", us / 1000.0)
    } else {
        format!("{} µs", us.round() as i64)
    }
}

fn render_stats(v: &Value) {
    let s = &v["stream"];
    if s.is_null() {
        println!("stream    nothing is streaming");
    } else {
        println!(
            "stream    {}x{}@{} {}",
            s["width"].as_i64().unwrap_or(0),
            s["height"].as_i64().unwrap_or(0),
            s["fps"].as_i64().unwrap_or(0),
            s["codec"].as_str().unwrap_or("—")
        );
        // The ENCODER TARGET, which is where adaptive bitrate has settled — not the throughput.
        // Labelled as a target because the two differ by an order of magnitude on a still screen,
        // and a single "300 Mbps" next to 11 Mbps of actual traffic is the kind of number that
        // sends someone hunting a bandwidth problem that does not exist.
        println!(
            "target    {:.1} Mbps",
            s["bitrate_kbps"].as_f64().unwrap_or(0.0) / 1000.0
        );
        if let Some(ms) = s["time_to_first_frame_ms"].as_i64() {
            println!("bring-up  {ms} ms to first frame");
        }
    }
    if let Some(backend) = v["meta"]["encoder_backend"].as_str() {
        println!(
            "encoder   {backend}{}",
            v["meta"]["gpu"]
                .as_str()
                .map(|g| format!(" · {g}"))
                .unwrap_or_default()
        );
    }
    if !v["capture"]["armed"].as_bool().unwrap_or(false) {
        println!("capture   idle — run `ctl stats record start` for frame timings");
        return;
    }
    println!(
        "capture   recording · {} samples",
        v["capture"]["sample_count"].as_i64().unwrap_or(0)
    );
    let p = &v["sample"];
    if p.is_null() {
        return;
    }
    println!("sent      {:.1} Mbps", p["mbps"].as_f64().unwrap_or(0.0));
    // `fps` counts NEW frames and `repeat_fps` the re-sent last one. Capture is damage-driven, so a
    // still desktop legitimately reads 0 new fps while the stream is perfectly healthy — printing
    // only `fps` there says "the stream is dead" about a stream that is not.
    let new_fps = p["fps"].as_f64().unwrap_or(0.0);
    let repeat_fps = p["repeat_fps"].as_f64().unwrap_or(0.0);
    println!("frames    {new_fps:.1} fps new · {repeat_fps:.1} fps repeated");
    if new_fps < 1.0 && repeat_fps > 1.0 {
        println!("          (nothing on screen is changing — the last frame is being repeated)");
    }
    println!(
        "loss      {} frames dropped · {} packets lost · {} recovered by FEC",
        p["frames_dropped"].as_i64().unwrap_or(0),
        p["packets_dropped"].as_i64().unwrap_or(0),
        p["fec_recovered"].as_i64().unwrap_or(0)
    );
    for st in p["stages"].as_array().into_iter().flatten() {
        println!(
            "  {:<12} p50 {} · p99 {}",
            st["name"].as_str().unwrap_or("—"),
            dur_us(st["p50_us"].as_f64().unwrap_or(0.0)),
            dur_us(st["p99_us"].as_f64().unwrap_or(0.0))
        );
    }
}

/// A one-shot console URL carrying a ticket that logs the operator straight in.
///
/// **What is being trusted, and what is not.** The console binds all interfaces so it can be
/// reached from a phone on the LAN, and its admin surface is pairing, unpair and session control —
/// so the network is not evidence of anything and the password stays. What IS evidence is the
/// **mgmt token**: a 0600 file inside the 0700 config dir, readable only by the uid the host runs
/// as. Whoever can read it can already drive the whole admin API directly (it is the credential
/// the console's own proxy presents), so letting them skip a password they could simply read
/// widens nothing. A visitor without a ticket still meets the login page.
///
/// The ticket is `<unix-seconds>.<nonce>.<HMAC-SHA256>` over `pf-console-handoff:v1:ts:nonce`,
/// keyed by the token. The console recomputes it with its own copy — no new host route, no shared
/// state, nothing to expire on this side. TTL and single-use are enforced by the console
/// (`web/server/routes/_auth/handoff.get.ts`); the nonce is what keeps two launches in the same
/// second from colliding in its replay set.
fn console_url() -> Result<String> {
    use hmac::{Hmac, KeyInit, Mac};
    let dir = pf_paths::config_dir();
    let token = crate::mgmt_token::read_persisted(&dir).ok_or_else(|| {
        Failure::unreachable(format!(
            "no management token in {} — the console shares this file, so without it there is \
             nothing for a handoff to prove. Start the host once and retry.",
            dir.join("mgmt-token").display()
        ))
    })?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut raw = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut raw);
    let nonce = hex::encode(raw);
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes())
        .map_err(|e| Failure::api(format!("could not key the handoff HMAC: {e}")))?;
    mac.update(format!("pf-console-handoff:v1:{ts}:{nonce}").as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    // The console's own port, not the mgmt one. It is not published anywhere the way
    // `mgmt-endpoint` is, so the documented default stands until somebody moves it.
    Ok(format!(
        "https://localhost:47992/_auth/handoff?t={ts}.{nonce}.{sig}"
    ))
}

fn grants_for(preset: &str) -> Result<u32> {
    match preset {
        "full" => Ok(GRANT_PRESET_FULL),
        "controller" => Ok(GRANT_PRESET_CONTROLLER_ONLY),
        "view" => Ok(GRANT_PRESET_VIEW_ONLY),
        other => Err(Failure::usage(format!(
            "unknown access preset '{other}' (want full | controller | view)"
        ))),
    }
}

// ── argument helpers ───────────────────────────────────────────────────────────────────────────

fn flag_value(args: &[&str], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == flag)?;
    args.get(i + 1).map(|s| (*s).to_string())
}

fn num_flag(args: &[&str], flag: &str) -> Result<Option<u64>> {
    match flag_value(args, flag) {
        None => Ok(None),
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| Failure::usage(format!("{flag} takes a number of seconds, got '{v}'"))),
    }
}

fn preset_flag(args: &[&str]) -> Result<Option<u32>> {
    flag_value(args, "--preset")
        .map(|p| grants_for(&p))
        .transpose()
}

/// The pending-device id, which is always the **first** argument. Deliberately not "the first
/// thing that parses as a number anywhere in the line": that would let `--expires-in 3600` be read
/// as the device to approve, which is the one mistake I6 exists to make impossible.
fn one_id(args: &[&str], verb: &str) -> Result<u32> {
    let raw = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .ok_or_else(|| Failure::usage(format!("{verb}: <id> first, from `ctl pending`")))?;
    raw.parse()
        .map_err(|_| Failure::usage(format!("{verb}: '{raw}' is not a pending-device id")))
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

// ── output ─────────────────────────────────────────────────────────────────────────────────────

/// One shape for every verb: the versioned envelope in `--json` mode, the terse table otherwise.
/// Nothing we ship parses the human half (I8), which is what lets it stay readable.
fn out(json: bool, v: &Value, human: impl FnOnce(&Value)) {
    if json {
        println!("{}", json!({ "v": SCHEMA_VERSION, "data": v }));
    } else {
        human(v);
    }
}

fn render_status(v: &Value) {
    let streaming = v["video_streaming"].as_bool().unwrap_or(false);
    println!("host      {}", if streaming { "streaming" } else { "idle" });
    println!("sessions  {}", v["active_sessions"].as_i64().unwrap_or(0));
    println!(
        "paired    {} native, {} gamestream",
        v["native_paired_clients"].as_i64().unwrap_or(0),
        v["paired_clients"].as_i64().unwrap_or(0)
    );
    if v["pin_pending"].as_bool().unwrap_or(false) {
        println!("pin       PENDING — run `ctl pin <PIN>` with the code the client shows");
    }
    render_games(v);
}

fn render_sessions(v: &Value) {
    println!("sessions  {}", v["active_sessions"].as_i64().unwrap_or(0));
    if let Some(s) = v["session"].as_object() {
        println!(
            "mode      {}x{} @ {}",
            s.get("width").and_then(Value::as_i64).unwrap_or(0),
            s.get("height").and_then(Value::as_i64).unwrap_or(0),
            s.get("fps").and_then(Value::as_i64).unwrap_or(0)
        );
    }
    render_games(v);
}

fn render_summary(v: &Value) {
    println!("version   {}", v["version"].as_str().unwrap_or("—"));
    println!(
        "state     {}",
        if v["video_streaming"].as_bool().unwrap_or(false) {
            match v["audio_streaming"].as_bool().unwrap_or(false) {
                true => "streaming video + audio",
                false => "streaming video",
            }
        } else {
            "idle"
        }
    );
    if let Some(name) = v["client_name"].as_str() {
        println!("client    {name}");
    }
    if let Some(s) = v["session"].as_object() {
        println!(
            "mode      {}x{} @ {}",
            s.get("width").and_then(Value::as_i64).unwrap_or(0),
            s.get("height").and_then(Value::as_i64).unwrap_or(0),
            s.get("fps").and_then(Value::as_i64).unwrap_or(0)
        );
    }
    println!(
        "paired    {} native, {} gamestream",
        v["native_paired_clients"].as_i64().unwrap_or(0),
        v["paired_clients"].as_i64().unwrap_or(0)
    );
    let waiting = v["pending_approvals"].as_i64().unwrap_or(0);
    if waiting > 0 {
        println!("pending   {waiting} awaiting approval");
    }
    // Another Moonlight-compatible host on this box binds the same ports, and the symptom is a
    // client that pairs with the wrong one. Worth a line whenever it is true.
    for c in v["conflicts"].as_array().into_iter().flatten() {
        println!("conflict  {}", c.as_str().unwrap_or(""));
    }
}

fn render_games(v: &Value) {
    let Some(games) = v["games"].as_array().filter(|g| !g.is_empty()) else {
        return;
    };
    println!("\nGAME                           CLIENT              PLANE       STATE");
    for g in games {
        println!(
            "{:<30} {:<19} {:<11} {}",
            trunc(g["title"].as_str().unwrap_or("(desktop)"), 30),
            trunc(g["client"].as_str().unwrap_or("—"), 19),
            g["plane"].as_str().unwrap_or("—"),
            g["state"].as_str().unwrap_or("—"),
        );
    }
}

fn render_pending(v: &Value) {
    let Some(rows) = v.as_array().filter(|r| !r.is_empty()) else {
        println!("no devices waiting for approval");
        return;
    };
    // I6: the claimed name AND the fingerprint tail, always — the name is what the device says it
    // is, the tail is what it can't lie about.
    println!("ID    NAME                      FINGERPRINT  AGE      ACCESS");
    for r in rows {
        println!(
            "{:<5} {:<25} {:<12} {:<8} {}",
            r["id"].as_i64().unwrap_or(-1),
            trunc(r["name"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            format!("{}s", r["age_secs"].as_i64().unwrap_or(0)),
            r["access_level"].as_str().unwrap_or("—"),
        );
    }
    println!("\napprove with `ctl approve <ID>`, refuse with `ctl deny <ID>`");
}

fn render_clients(v: &Value) {
    println!("PLANE       NAME                      FINGERPRINT  ACCESS      EXPIRES");
    for r in v["native"].as_array().into_iter().flatten() {
        println!(
            "{:<11} {:<25} {:<12} {:<11} {}",
            "native",
            trunc(r["name"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            r["access_level"].as_str().unwrap_or("—"),
            match r["expires_unix"].as_i64() {
                None => "permanent".to_string(),
                Some(t) => format!("unix {t}"),
            }
        );
    }
    for r in v["gamestream"].as_array().into_iter().flatten() {
        // The GameStream store has no grants and no expiry — its devices are pinned certificates,
        // full stop. Two dashes rather than borrowed native semantics.
        println!(
            "{:<11} {:<25} {:<12} {:<11} —",
            "gamestream",
            trunc(r["label"].as_str().unwrap_or("(unnamed)"), 25),
            tail(r["fingerprint"].as_str().unwrap_or("")),
            "—",
        );
    }
}

fn render_pair(v: &Value) {
    println!(
        "native    {}",
        match (
            v["enabled"].as_bool().unwrap_or(false),
            v["armed"].as_bool().unwrap_or(false)
        ) {
            (false, _) => "the native plane is not running".to_string(),
            (true, false) => "disarmed".to_string(),
            (true, true) => match v["expires_in_secs"].as_i64() {
                Some(s) => format!("ARMED, {s}s left"),
                None => "ARMED".to_string(),
            },
        }
    );
    if let Some(pin) = v["pin"].as_str() {
        println!("pin       {pin}  — enter this on the device");
    }
    if v["pin_pending"].as_bool().unwrap_or(false) {
        println!("moonlight a client is waiting on its PIN — `ctl pin <PIN> [UNIQUEID] [FP]`");
        // Name each parked ceremony so the operator answers the device they can SEE, not a
        // blind prompt a racer may have joined (security-review 2026-08-31 H-4).
        for c in v["pending"].as_array().into_iter().flatten() {
            println!(
                "          waiting: uniqueid {}  cert …{}",
                c["uniqueid"].as_str().unwrap_or("?"),
                tail(c["fingerprint"].as_str().unwrap_or(""))
            );
        }
    }
    println!("paired    {}", v["paired_clients"].as_i64().unwrap_or(0));
}

/// Last 10 hex characters, the shape an operator compares against what the device shows. Never the
/// whole 64 — a wall of hex is exactly what makes people stop reading it.
fn tail(fp: &str) -> String {
    match fp.len() {
        0 => "—".to_string(),
        n if n <= 10 => fp.to_string(),
        n => format!("…{}", &fp[n - 10..]),
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn print_usage() {
    // A plain `&str`, printed through `{USAGE}` rather than as a format string: the `watch`
    // example below contains JSON braces, which a literal format string would try to interpolate.
    const USAGE: &str = r#"punktfunk-host ctl — operator control over the local management API

USAGE:
    punktfunk-host ctl <VERB> [ARGS] [--json]

STATE
    status                       host state, session count, paired counts
    sessions                     the active session(s) and any launched game
    summary                      one call for a status surface: host version, what is streaming,
                                 the streaming client's name, paired counts, and any conflicting
                                 host on this box. The only verb that names a connected device.
    watch [--kinds K,..] [--since N]
                                 the host event stream as line-JSON on stdout, one object per
                                 line; reconnects by itself and emits {"kind":"ctl.resync"}
                                 when it fell off the catch-up ring

PAIRING
    pair status                  is a pairing window open, and is a PIN waiting
    pair arm [--ttl S] [--expires-in S] [--preset P] [--fingerprint FP]
                                 open a native pairing window (--fingerprint binds it to ONE device)
    pair disarm                  close it
    pending                      devices knocking, awaiting approval
    approve <ID> [--name N] [--preset P] [--expires-in S]
    deny <ID>
    pin <PIN> [UNIQUEID] [FP]    submit the PIN a Moonlight/GameStream client is showing;
                                 name the target when several clients wait (see `status`)

CONSOLE
    console-url                  print a one-shot URL that opens the web console already logged in.
                                 The ticket is signed with the management token, so being able to
                                 read that 0600 file IS the proof — a visitor without one still
                                 meets the login page.

DEVICES
    clients                      paired devices on both planes
    rename <FP> <NAME>
    access <FP> <full|controller|view>
    unpair <FP> | unpair --all [--yes]

DISPLAYS
    display                      the virtual-display policy, every preset, and the live displays.
                                 `displays` is always empty on wlroots — the registry passes those
                                 through rather than owning them, so read it as "not tracked here".
    display preset <ID>          switch the policy to a preset — a built-in id or a saved one.
                                 Reads the stored policy and edits it, so the axes a preset does
                                 not own (the streamed screen, the experimental Windows ones)
                                 survive the switch.
    display release [SLOT]       tear down KEPT displays now, so a physical-screen user gets their
                                 screen back without waiting out the linger. Omit SLOT for all.
                                 Never touches a display that is actively streaming.

STATS
    stats                        the live stream: mode, codec and the adaptive bitrate, plus frame
                                 timings and drops while a capture is recording
    stats record start|stop      arm or disarm the performance capture. `stop` writes the recording
                                 to disk; the capture is one host-wide slot the web console shares.

OPTIONS
    --json                       versioned JSON on stdout (the contract; the tables are for humans)

EXIT CODES
    0 ok   1 the host refused   2 usage   3 no host reachable   4 certificate pin mismatch

The token and the certificate pin are read from the host's config directory; neither is ever
accepted on the command line or from the environment."#;
    eprintln!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_map_to_the_hosts_own_masks() {
        assert_eq!(grants_for("full").unwrap(), GRANT_PRESET_FULL);
        assert_eq!(
            grants_for("controller").unwrap(),
            GRANT_PRESET_CONTROLLER_ONLY
        );
        assert_eq!(grants_for("view").unwrap(), GRANT_PRESET_VIEW_ONLY);
        // A typo must be usage (2), never a silently-wrong mask.
        assert_eq!(grants_for("fulll").unwrap_err().code, client::EXIT_USAGE);
    }

    #[test]
    fn fingerprint_tails_stay_short_and_never_panic_on_multibyte() {
        assert_eq!(tail(""), "—");
        assert_eq!(tail("abc"), "abc");
        assert_eq!(tail(&"a".repeat(64)), format!("…{}", "a".repeat(10)));
        assert_eq!(trunc("ünïcödé title", 5), "ünïc…");
    }

    #[test]
    fn flags_are_read_positionlessly() {
        let args = ["arm", "--ttl", "120", "--preset", "view"];
        assert_eq!(num_flag(&args, "--ttl").unwrap(), Some(120));
        assert_eq!(preset_flag(&args).unwrap(), Some(GRANT_PRESET_VIEW_ONLY));
        assert_eq!(num_flag(&args, "--expires-in").unwrap(), None);
        // A non-numeric TTL is usage, not a silently-dropped flag.
        assert_eq!(
            num_flag(&["--ttl", "soon"], "--ttl").unwrap_err().code,
            client::EXIT_USAGE
        );
    }

    #[test]
    fn approve_takes_an_id_and_never_guesses() {
        assert_eq!(one_id(&["7", "--name", "tv"], "approve").unwrap(), 7);
        // A flag's VALUE must never be read as the id — `approve --expires-in 3600` naming
        // device 3600 is precisely the accident I6 forbids.
        assert!(one_id(&["--expires-in", "3600"], "approve").is_err());
        assert!(one_id(&["newest"], "approve").is_err());
        assert!(one_id(&[], "approve").is_err());
    }

    /// A settings body shaped like `GET /display/settings`: a stored policy carrying axes no preset
    /// owns, one built-in preset and one saved one.
    fn display_state() -> Value {
        json!({
            "settings": {
                "version": 1,
                "preset": "default",
                "topology": "auto",
                "max_displays": 4,
                "game_session": "auto",
                "capture_monitor": "DP-2",
                "ddc_power_off": true,
            },
            "presets": [{ "id": "gaming-rig", "summary": "…" }],
            "custom_presets": [{
                "id": "p-couch",
                "name": "Couch",
                "game_session": "dedicated",
                "fields": { "topology": "exclusive", "max_displays": 2 },
            }],
        })
    }

    #[test]
    fn a_preset_switch_keeps_the_axes_no_preset_owns() {
        let p = policy_with_preset(&display_state(), "gaming-rig").unwrap();
        assert_eq!(p["preset"], "gaming-rig");
        // The streamed screen is not display BEHAVIOR. A whole-object PUT that rebuilt the policy
        // from the verb's arguments would drop this and silently move the stream to a virtual
        // display — the on-glass regression the console carries the same guard against.
        assert_eq!(p["capture_monitor"], "DP-2");
        assert_eq!(p["ddc_power_off"], true);
    }

    #[test]
    fn a_saved_preset_is_applied_as_a_custom_policy_carrying_its_fields() {
        // Saved presets have no apply route of their own — the API expects `custom` plus the
        // fields. Getting this wrong stores the id in a field that only accepts the built-in names.
        let p = policy_with_preset(&display_state(), "p-couch").unwrap();
        assert_eq!(p["preset"], "custom");
        assert_eq!(p["topology"], "exclusive");
        assert_eq!(p["max_displays"], 2);
        assert_eq!(p["game_session"], "dedicated");
        assert_eq!(p["capture_monitor"], "DP-2");
    }

    #[test]
    fn stage_durations_keep_the_small_stages_visible() {
        // Real numbers off an Omarchy box: `send` p50 15 µs against `encode` p50 2321 µs. One
        // fixed unit loses an end of that range — ms prints send as "0.0 ms".
        assert_eq!(dur_us(15.0), "15 µs");
        assert_eq!(dur_us(0.0), "0 µs");
        assert_eq!(dur_us(999.0), "999 µs");
        assert_eq!(dur_us(2321.0), "2.3 ms");
    }

    #[test]
    fn an_unknown_preset_is_refused_rather_than_stored() {
        assert!(policy_with_preset(&display_state(), "hologram").is_err());
        // No stored policy at all is the host's answer, not a preset the caller can fix.
        assert!(policy_with_preset(&json!({}), "gaming-rig").is_err());
    }
}
