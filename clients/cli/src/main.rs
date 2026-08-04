//! `punktfunk` — the headless client CLI (design/client-architecture-split.md §4).
//!
//! One console-subsystem binary over the same brain the GUI shells use, so a script gets the
//! same behaviour a click does — including wake-then-connect, which the Linux shell's old
//! exec-style `--connect` never had. It is a FRONT-END, not the brain: policy lives in
//! `pf_client_core`, and the shells call the same functions in-process rather than shelling
//! out to this. That is the whole point of the split — if the GUI shelled out for connects,
//! trust prompts and wake progress would have to squeeze through an IPC contract.
//!
//! Existing surfaces are a frozen compatibility contract and are NOT replaced by this: the
//! Linux shell keeps its headless flags (Decky invokes them), and `punktfunk-probe` stays the
//! diagnostics tool. This is the door new consumers should use — the Playnite importer shells
//! to `punktfunk library <host> --json`.
//!
//! Exit codes extend the session binary's: 0 ok, 2 connect failed, 3 trust rejected,
//! 4 renderer failed, 5 could not resolve what was asked for, 6 refused because it needs a
//! human (pairing, an unknown host). A machine consumer can branch on those without parsing
//! prose.

#![forbid(unsafe_code)]

#[cfg(any(target_os = "linux", windows))]
mod cli {
    use pf_client_core::deeplink::{self, DeepLink, HostResolution};
    use pf_client_core::orchestrate::{
        self, ConnectPlan, PlanOutcome, SessionEvent, WakeOutcome, WakeWait,
    };
    use pf_client_core::profiles::ProfilesFile;
    use pf_client_core::trust::{self, KnownHost, KnownHosts};
    use pf_client_core::{library, wol};
    use std::time::Duration;

    pub const OK: u8 = 0;
    pub const CONNECT_FAILED: u8 = 2;
    pub const TRUST_REJECTED: u8 = 3;
    pub const RENDERER_FAILED: u8 = 4;
    /// Nothing here matches what you named (host, profile, game).
    pub const UNRESOLVED: u8 = 5;
    /// Refused because it needs a person: pairing, or trusting an unknown host.
    pub const NEEDS_INTERACTION: u8 = 6;

    const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

    /// The handshake budget `--request-access` runs on. Matches the host's `PENDING_APPROVAL_WAIT`
    /// — the connect is PARKED for that long while an operator decides, so anything shorter would
    /// give up while the approval prompt is still on their screen.
    const REQUEST_ACCESS_TIMEOUT_SECS: u64 = 185;

    const USAGE: &str = "\
punktfunk — the Punktfunk client, headless

  punktfunk discover [--json] [--timeout SECS]
  punktfunk pair <host[:port]> [--pin N] [--name LABEL]
  punktfunk hosts list [--probe] [--json]
  punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]
  punktfunk hosts forget <host-ref>
  punktfunk wake <host-ref> [--wait]
  punktfunk library <host-ref> [--json]
  punktfunk launch <host-ref> [--game ID] [--profile REF] [--request-access]
                              [--exec] [--fullscreen]
  punktfunk open <punktfunk://…>
  punktfunk reachable <host-ref>
  punktfunk speed-test <host-ref>
  punktfunk profiles list [--json]
  punktfunk reset

A <host-ref> is a saved host's id, its name, or an address — the same reference a
punktfunk:// link takes. Exit codes: 0 ok, 2 connect, 3 trust, 4 renderer, 5 not found,
6 needs a person.

\"punktfunk help <command>\" (or any command with --help) explains that command.";

    /// The long help for one verb — `punktfunk help <verb>`, or `--help` after the verb.
    /// Each entry documents its flags and the behaviour a script would need to know
    /// (what goes to stdout vs stderr, and which exit codes mean what).
    fn verb_help(verb: &str) -> Option<&'static str> {
        Some(match verb {
            "discover" => {
                "\
punktfunk discover [--json] [--timeout SECS] — browse the LAN for hosts

Listens for Punktfunk hosts advertising over mDNS and prints what answered:
name TAB addr:port TAB saved|new TAB paired|unpaired. `saved` means this
device already has a record for it, matched by fingerprint first and address
second — the same rule every other surface joins the two lists by.

  --timeout SECS  how long to browse (default 3, capped at 30) — a bounded
                  call, so a panel can wait for it
  --json          {\"hosts\":[{\"name\",\"addr\",\"port\",\"fp\",\"pair\",\"id\",\"mgmt\",
                  \"os\",\"saved\",\"paired\"}]}

Nothing answering is an answer, not a failure: an empty list exits 0. A host
mDNS never sees (Tailscale, another subnet) will not appear here — save it by
address with `punktfunk hosts add` and it shows in `hosts list --probe`."
            }
            "pair" => {
                "\
punktfunk pair <host[:port]> — enrol this device with a host (PIN ceremony)

  --pin N       the PIN the host is showing; without it the command asks, and
                refuses (exit 6) when there is no terminal to ask on
  --name LABEL  the label the host files this device under
                (default: this machine's name)

Pairing verifies the host end-to-end and pins its fingerprint in the saved-hosts
store, so every later connect — here, in the desktop client or the console — is
silent. The port defaults to 9777. Prints `paired <addr>:<port> fp=<hex>` on
success; exit 3 if the host refuses or the PIN is wrong."
            }
            "hosts" => {
                "\
punktfunk hosts — the saved-hosts store (shared with the desktop client)

  punktfunk hosts list [--probe] [--json]
      Every saved host, name TAB addr:port TAB paired/trusted TAB state.
      --probe asks each host directly (no mDNS, so routed/VPN hosts answer
      too); --json emits one object with per-host detail, profiles included.

  punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]
      Save a host by address — the door for a box mDNS never sees (Tailscale,
      another subnet). Without --fp it is a placeholder to pair later; with a
      64-hex fingerprint it is pinned immediately (still unpaired).

      Idempotent, and keyed on the FINGERPRINT once there is one: re-running it
      for a host already saved is a no-op, and giving a known fingerprint a new
      address MOVES that host's record there rather than filing a second one
      (which is how a host that changed DHCP lease stays reachable by its id).
      A different fingerprint for an address already saved is refused, exit 3 —
      a changed identity is a decision for a person.

  punktfunk hosts forget <host-ref>
      Remove a saved host, its pinned fingerprint included. A later connect
      must pair or trust it again."
            }
            "wake" => {
                "\
punktfunk wake <host-ref> [--wait] — Wake-on-LAN

Sends a magic packet to a saved host's MAC (learned from its advert while it
was awake; exit 5 if none is known yet). With --wait, keeps sending every 6 s
and polls presence every second for up to 90 s, exiting 0 the moment the host
answers — the same cadence every graphical shell uses."
            }
            "library" => {
                "\
punktfunk library <host-ref> [--json] — the host's game library

TSV on stdout by default (id TAB store TAB title), one game per line; --json
emits {\"games\":[…]} for tools — the Playnite importer shells to exactly
this. Needs a paired host (exit 6 otherwise)."
            }
            "launch" => {
                "\
punktfunk launch <host-ref> [--game ID] [--profile REF] [--request-access]
                            [--exec] [--fullscreen]

Start a stream — waking the host first if it is asleep and its MAC is known.
The stream runs in the punktfunk-session renderer; this command supervises it
and relays its lifecycle to stderr.

  --game ID      ask the host to launch this library title into the stream
  --profile REF  use a settings profile (id or name) for this connect only;
                 without it the host's own binding applies
  --fullscreen   start the stream window fullscreen
  --exec         become the session process instead of supervising it — the
                 gamescope-wrapper mode, where the launched process must BE
                 the streaming one for focus and lifecycle to work
  --request-access
                 ask the host's operator to let this device in instead of
                 typing a PIN. The host PARKS the connect until somebody
                 approves it in its console or web UI (up to ~185 s), then
                 admits it and the stream starts by itself; the host is
                 recorded as paired once that happens, so later streams are
                 silent. Needs the host's fingerprint pinned already
                 (`punktfunk hosts add <addr> --fp <hex>`), and cannot be
                 combined with --exec — under --exec there is no process
                 left to record the approval.

Exit 0 when the stream ends cleanly, 2 connect failed, 3 the host no longer
trusts this device (re-pair), 4 the renderer could not start."
            }
            "open" => {
                "\
punktfunk open <punktfunk://…> — follow a punktfunk:// link, headless

Same parser and same refusal rules as clicking the link in a shell: a
contradicted fingerprint refuses and says so, an ambiguous name refuses
rather than guessing, and an unknown host is never trusted from a URL —
that is a decision for a person, at a surface that can show the fingerprint
(exit 6 points at `punktfunk pair`). --exec as in launch."
            }
            "reachable" => {
                "\
punktfunk reachable <host-ref> — one bounded reachability probe

Asks the host directly (no mDNS), so routed/VPN hosts answer too. The
reference may be an unsaved address — this verb answers \"can I reach it\",
not \"do I know it\". Exit 0 reachable, 2 not; one line either way."
            }
            "speed-test" => {
                "\
punktfunk speed-test <host-ref> [--json] — measure the real data plane

Runs the host's bandwidth probe over an actual session connect and prints the
measured throughput, loss, and the bitrate it recommends. Deliberately does
NOT apply the result: which layer a bitrate belongs in (a bound profile, the
global default) is a decision the GUI makes with the user, and a CLI silently
rewriting settings would be exactly the surprise that rule exists to prevent."
            }
            "profiles" => {
                "\
punktfunk profiles list [--json] — the settings profiles on this device

One line per profile: id TAB name TAB how many settings it overrides.
Profiles are created and edited in the desktop client; a connect uses one via
`punktfunk launch --profile` or a punktfunk:// link that names it."
            }
            "reset" => {
                "\
punktfunk reset — forget every saved host and reset stream settings

Asks for confirmation, and refuses (exit 6) when there is no terminal to ask
on. This device's identity keypair is deliberately kept, so hosts that knew
this machine still recognise it after re-pairing; delete the identity files
from the config directory for a true factory reset."
            }
            _ => return None,
        })
    }

    /// The value after `--flag`, if any.
    fn value(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .filter(|v| !v.starts_with("--"))
            .cloned()
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    /// The first argument that isn't a flag or a flag's value — the verb's subject.
    fn positional(args: &[String], skip: usize) -> Option<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i].starts_with("--") {
                // Skip the flag and, when it takes one, its value.
                if args
                    .get(i + 1)
                    .is_some_and(|v| !v.starts_with("--") && flag_takes_value(&args[i]))
                {
                    i += 1;
                }
            } else {
                out.push(args[i].clone());
            }
            i += 1;
        }
        out.get(skip).cloned()
    }

    fn flag_takes_value(flag: &str) -> bool {
        matches!(
            flag,
            "--pin" | "--name" | "--fp" | "--game" | "--profile" | "--port" | "--timeout"
        )
    }

    /// Resolve a host reference the way every other surface does: stable id, then a unique
    /// name, then `addr[:port]` (design/client-deep-links.md §2). Sharing `resolve_host` is
    /// what keeps `punktfunk launch desk` and `punktfunk://connect/desk` from disagreeing.
    fn resolve(reference: &str) -> Result<(KnownHosts, usize), u8> {
        let known = KnownHosts::load();
        let link = DeepLink {
            host_ref: reference.to_string(),
            ..Default::default()
        };
        match deeplink::resolve_host(&link, &known) {
            HostResolution::Known(i) => Ok((known, i)),
            HostResolution::Ambiguous => {
                eprintln!(
                    "more than one saved host is called \"{reference}\" — use its address or id"
                );
                Err(UNRESOLVED)
            }
            HostResolution::Unknown { addr, port, .. } => {
                eprintln!(
                    "{addr}:{port} isn't a saved host — pair it first (punktfunk pair {addr}:{port})"
                );
                Err(NEEDS_INTERACTION)
            }
            HostResolution::Unresolvable => {
                eprintln!("no saved host matches \"{reference}\"");
                Err(UNRESOLVED)
            }
        }
    }

    pub fn run(args: Vec<String>) -> u8 {
        let Some(verb) = args.first().cloned() else {
            println!("{USAGE}");
            return OK;
        };
        let rest: Vec<String> = args[1..].to_vec();
        // `--help`/`-h` after any verb prints that verb's help — before dispatch, so a verb
        // never mistakes the flag for its subject (`punktfunk pair -h` must not dial "-h").
        if rest.iter().any(|a| a == "--help" || a == "-h") {
            println!("{}", verb_help(&verb).unwrap_or(USAGE));
            return OK;
        }
        match verb.as_str() {
            "discover" => discover(&rest),
            "pair" => pair(&rest),
            "hosts" => hosts(&rest),
            "wake" => wake(&rest),
            "library" => library_cmd(&rest),
            "launch" => launch(&rest),
            "open" => open(&rest),
            "reachable" => reachable(&rest),
            "speed-test" => speed_test(&rest),
            "profiles" => profiles(&rest),
            "reset" => reset(),
            "-h" | "--help" | "help" => match positional(&rest, 0) {
                None => {
                    println!("{USAGE}");
                    OK
                }
                Some(topic) => match verb_help(&topic) {
                    Some(h) => {
                        println!("{h}");
                        OK
                    }
                    None => {
                        eprintln!("no command called \"{topic}\"\n\n{USAGE}");
                        UNRESOLVED
                    }
                },
            },
            "--version" | "version" => {
                println!("punktfunk {}", env!("CARGO_PKG_VERSION"));
                OK
            }
            other => {
                eprintln!("unknown command \"{other}\"\n\n{USAGE}");
                UNRESOLVED
            }
        }
    }

    /// How long `discover` browses when nobody says, and the ceiling on what they can ask for.
    /// The cap is not politeness: this verb is called from a Quick Access panel, and a typo'd
    /// `--timeout 3000` would hang that panel with no way to cancel it.
    const DISCOVER_DEFAULT_SECS: f64 = 3.0;
    const DISCOVER_MAX_SECS: f64 = 30.0;

    /// `discover [--json] [--timeout SECS]` — browse the LAN over mDNS and print what answered,
    /// annotated against the saved-hosts store.
    ///
    /// The annotation is the point: a caller wants "can I stream this", which is a question
    /// about BOTH lists, and joining them itself is how two surfaces end up disagreeing about
    /// the same host. So the match rule lives here, once, and is the same one every other
    /// surface uses — fingerprint first (survives a DHCP move), address second.
    fn discover(args: &[String]) -> u8 {
        let secs = value(args, "--timeout")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(DISCOVER_DEFAULT_SECS)
            .min(DISCOVER_MAX_SECS);
        let found = pf_client_core::discovery::discover_for(Duration::from_secs_f64(secs));
        // `read`, not `load`: this verb only LOOKS at the records to annotate what it found, and
        // never hands their ids back. `load` would mint ids for a pre-mint store and save them —
        // a write from a read-only verb, and one that races the `hosts list` a caller is very
        // likely running at the same moment (the Decky panel issues both together).
        let known = KnownHosts::read();
        let rows: Vec<(
            &pf_client_core::discovery::DiscoveredHost,
            Option<&KnownHost>,
        )> = found.iter().map(|d| (d, match_saved(&known, d))).collect();
        if has(args, "--json") {
            let hosts: Vec<serde_json::Value> = rows
                .iter()
                .map(|(d, saved)| {
                    serde_json::json!({
                        "name": d.name,
                        "addr": d.addr,
                        "port": d.port,
                        "fp": d.fp_hex,
                        "pair": d.pair,
                        "id": d.advertised_id(),
                        // 0 = not advertised, which is what a consumer's own "no mgmt port"
                        // already means — an older host simply omits the TXT.
                        "mgmt": d.mgmt_port.unwrap_or(0),
                        "os": d.os,
                        "saved": saved.is_some(),
                        "paired": saved.is_some_and(|h| h.paired),
                    })
                })
                .collect();
            println!("{}", serde_json::json!({ "hosts": hosts }));
        } else {
            for (d, saved) in &rows {
                println!(
                    "{}\t{}:{}\t{}\t{}",
                    d.name,
                    d.addr,
                    d.port,
                    if saved.is_some() { "saved" } else { "new" },
                    if saved.is_some_and(|h| h.paired) {
                        "paired"
                    } else {
                        "unpaired"
                    },
                );
            }
        }
        // An empty LAN is an answer, not a failure — a caller branching on the exit code is
        // asking "did the browse run", and it did.
        OK
    }

    /// The saved record an advert belongs to, if any: fingerprint first, address second.
    ///
    /// Fingerprint FIRST is deliberate and load-bearing — a host that moved to a new DHCP lease
    /// still matches its record, and a *different* host that inherited the old address does not
    /// inherit its pairing. This is the rule the plugin's `mergeHosts` and the shells' hosts
    /// pages already use; keeping one copy is what stops two surfaces disagreeing about whether
    /// the box in front of you is paired.
    fn match_saved<'a>(
        known: &'a KnownHosts,
        advert: &pf_client_core::discovery::DiscoveredHost,
    ) -> Option<&'a KnownHost> {
        known
            .hosts
            .iter()
            .find(|h| {
                !h.fp_hex.is_empty()
                    && !advert.fp_hex.is_empty()
                    && h.fp_hex.eq_ignore_ascii_case(&advert.fp_hex)
            })
            .or_else(|| {
                known
                    .hosts
                    .iter()
                    .find(|h| h.addr == advert.addr && h.port == advert.port)
            })
    }

    /// `pair <host[:port]> [--pin N]` — the SPAKE2 ceremony. Without `--pin` it prompts, which
    /// is the interactive shape; with one it is scriptable. Refuses rather than prompting when
    /// stdin isn't a terminal and no PIN was given: a pairing that silently blocks a CI job
    /// forever is worse than an exit code.
    fn pair(args: &[String]) -> u8 {
        let Some(target) = positional(args, 0) else {
            eprintln!("usage: punktfunk pair <host[:port]> [--pin N]");
            return UNRESOLVED;
        };
        let (addr, port) = split_host_port(&target);
        let pin = match value(args, "--pin") {
            Some(p) => p,
            None => {
                if !is_tty() {
                    eprintln!("no --pin and no terminal to ask on");
                    return NEEDS_INTERACTION;
                }
                eprint!("PIN shown on {addr}: ");
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    return NEEDS_INTERACTION;
                }
                line.trim().to_string()
            }
        };
        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let name = value(args, "--name").unwrap_or_else(trust::device_name);
        match trust::pair_with_host(&addr, port, &identity, &pin, &name) {
            Ok(fp) => {
                let fp_hex = trust::hex(&fp);
                trust::persist_host(&addr, &addr, port, &fp_hex, true);
                trust::forget_placeholder(&addr, port);
                println!("paired {addr}:{port} fp={fp_hex}");
                OK
            }
            Err(e) => {
                eprintln!("{}", trust::pair_error_message(&e));
                TRUST_REJECTED
            }
        }
    }

    /// `hosts list|add|forget` over the shared store — the same file the shells and the session
    /// read, so a change here shows up there.
    fn hosts(args: &[String]) -> u8 {
        match positional(args, 0).as_deref() {
            Some("list") | None => {
                let known = KnownHosts::load();
                let online: Option<Vec<bool>> = has(args, "--probe").then(|| {
                    trust::probe_reachable_many(
                        known
                            .hosts
                            .iter()
                            .map(|h| (h.addr.clone(), h.port))
                            .collect(),
                        PROBE_TIMEOUT,
                    )
                });
                if has(args, "--json") {
                    let catalog = ProfilesFile::load();
                    let rows: Vec<serde_json::Value> = known
                        .hosts
                        .iter()
                        .enumerate()
                        .map(|(i, h)| {
                            serde_json::json!({
                                "id": h.id,
                                "name": h.name,
                                "addr": h.addr,
                                "port": h.port,
                                "fp_hex": h.fp_hex,
                                "paired": h.paired,
                                "mac": h.mac,
                                "os": h.os,
                                "last_used": h.last_used,
                                "clipboard_sync": h.clipboard_sync,
                                "profile": h.profile_id.as_ref()
                                    .and_then(|id| catalog.find_by_id(id))
                                    .map(|p| serde_json::json!({"id": p.id, "name": p.name})),
                                "pinned_profiles": h.resolved_pins(&catalog)
                                    .iter()
                                    .map(|p| serde_json::json!({"id": p.id, "name": p.name}))
                                    .collect::<Vec<_>>(),
                                "online": online.as_ref().map(|v| v[i]),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::json!({ "hosts": rows }));
                } else {
                    for (i, h) in known.hosts.iter().enumerate() {
                        let state = match online.as_ref().map(|v| v[i]) {
                            Some(true) => "online",
                            Some(false) => "offline",
                            None => "-",
                        };
                        println!(
                            "{}\t{}:{}\t{}\t{state}",
                            h.name,
                            h.addr,
                            h.port,
                            if h.paired { "paired" } else { "trusted" }
                        );
                    }
                }
                OK
            }
            Some("add") => {
                let Some(target) = positional(args, 1) else {
                    eprintln!("usage: punktfunk hosts add <host[:port]> [--name LABEL] [--fp HEX]");
                    return UNRESOLVED;
                };
                let (addr, port) = split_host_port(&target);
                let fp = value(args, "--fp").unwrap_or_default();
                let name = value(args, "--name");
                let mut known = KnownHosts::load();
                if let Some(i) = known
                    .hosts
                    .iter()
                    .position(|h| h.addr == addr && h.port == port)
                {
                    return match merge_saved_host(&mut known, i, &fp, name.as_deref()) {
                        AddOutcome::Unchanged => {
                            eprintln!("{addr}:{port} is already saved");
                            OK
                        }
                        AddOutcome::Conflict => {
                            eprintln!(
                                "{addr}:{port} is already saved with a different fingerprint — \
                                 forget it first if you really mean to replace it \
                                 (punktfunk hosts forget {addr}:{port})"
                            );
                            TRUST_REJECTED
                        }
                        AddOutcome::Pinned => match known.save() {
                            Ok(()) => {
                                println!("updated {addr}:{port}");
                                OK
                            }
                            Err(e) => {
                                eprintln!("saving: {e:#}");
                                CONNECT_FAILED
                            }
                        },
                    };
                }
                // No record at this address — but a record carrying this exact FINGERPRINT is
                // this same host at a new one. Re-point it rather than filing a second record:
                // the fingerprint is the identity, and a host that changed DHCP lease is the
                // whole reason `hosts add --fp` is idempotent in the first place. Without this a
                // moved host accumulates one record per address it has ever held, and the one a
                // stable id resolves to keeps the address it can no longer be reached at.
                if let Some(i) = known
                    .hosts
                    .iter()
                    .position(|h| !fp.is_empty() && h.fp_hex.eq_ignore_ascii_case(&fp))
                {
                    let was = format!("{}:{}", known.hosts[i].addr, known.hosts[i].port);
                    known.hosts[i].addr = addr.clone();
                    known.hosts[i].port = port;
                    return match known.save() {
                        Ok(()) => {
                            println!("moved {was} to {addr}:{port}");
                            OK
                        }
                        Err(e) => {
                            eprintln!("saving: {e:#}");
                            CONNECT_FAILED
                        }
                    };
                }
                known.hosts.push(KnownHost {
                    name: name.unwrap_or_else(|| addr.clone()),
                    addr: addr.clone(),
                    port,
                    fp_hex: fp,
                    ..Default::default()
                });
                match known.save() {
                    Ok(()) => {
                        println!("added {addr}:{port}");
                        OK
                    }
                    Err(e) => {
                        eprintln!("saving: {e:#}");
                        CONNECT_FAILED
                    }
                }
            }
            Some("forget") => {
                let Some(reference) = positional(args, 1) else {
                    eprintln!("usage: punktfunk hosts forget <host-ref>");
                    return UNRESOLVED;
                };
                let (mut known, i) = match resolve(&reference) {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                let gone = known.hosts.remove(i);
                match known.save() {
                    Ok(()) => {
                        println!("forgot {}", gone.name);
                        OK
                    }
                    Err(e) => {
                        eprintln!("saving: {e:#}");
                        CONNECT_FAILED
                    }
                }
            }
            Some(other) => {
                eprintln!("unknown hosts command \"{other}\" — list, add or forget");
                UNRESOLVED
            }
        }
    }

    /// What `hosts add` did to a record that was ALREADY saved for this address.
    #[derive(Debug, PartialEq, Eq)]
    enum AddOutcome {
        /// Nothing to do — no fingerprint was offered, or the record already carries this one.
        /// Exits 0 on purpose: a panel retrying step 1 of request access must not have to
        /// invent an error to show for a state that is already correct.
        Unchanged,
        /// The record had no fingerprint and now has this one.
        Pinned,
        /// The record carries a DIFFERENT fingerprint. Refused, never overwritten.
        Conflict,
    }

    /// `hosts add --fp` against an address that is already saved. The difference between these
    /// three is a trust decision, not bookkeeping.
    ///
    /// Filling in an empty fingerprint is step 1 of request access (design §5): a host found by
    /// advert is saved by address first and pinned second. Without it the `--fp` is dropped on
    /// the floor and the launch that follows refuses for want of a pin — which is what this did
    /// before, silently and with exit 0.
    ///
    /// A *different* fingerprint is refused because a changed identity is a decision for a
    /// person, at a surface that can show them both. That is what `upsert_trusted` exists to
    /// enforce; quietly overwriting it here would be a back door through the pinning the rest
    /// of the client is built on.
    fn merge_saved_host(
        known: &mut KnownHosts,
        i: usize,
        fp: &str,
        name: Option<&str>,
    ) -> AddOutcome {
        let existing = known.hosts[i].fp_hex.clone();
        if fp.is_empty() || existing.eq_ignore_ascii_case(fp) {
            return AddOutcome::Unchanged;
        }
        if !existing.is_empty() {
            return AddOutcome::Conflict;
        }
        known.hosts[i].fp_hex = fp.to_string();
        // Only a record still named after its own address is renamed: a label the user chose is
        // theirs, and an advert's name must not quietly overwrite it.
        if let Some(label) = name {
            if known.hosts[i].name == known.hosts[i].addr {
                known.hosts[i].name = label.to_string();
            }
        }
        AddOutcome::Pinned
    }

    /// `wake <host-ref> [--wait]` — a magic packet, and with `--wait` the same bounded
    /// wake-and-wait the shells run (`WakeWait`: a packet every 6 s, presence polled every
    /// second, 90 s budget).
    fn wake(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk wake <host-ref> [--wait]");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        if host.mac.is_empty() {
            eprintln!("no Wake-on-LAN address known for {} — connect to it once while it's awake so the client can learn it", host.name);
            return UNRESOLVED;
        }
        if !has(args, "--wait") {
            wol::wake(&host.mac, host.addr.parse().ok());
            println!("sent a wake packet to {}", host.name);
            return OK;
        }
        let mut wait = WakeWait::new();
        loop {
            let online = trust::probe_reachable_many(
                vec![(host.addr.clone(), host.port)],
                Duration::from_millis(900),
            )
            .first()
            .copied()
            .unwrap_or(false);
            let tick = wait.tick(online);
            if tick.send_packet {
                wol::wake(&host.mac, host.addr.parse().ok());
            }
            match tick.outcome {
                Some(WakeOutcome::Online) => {
                    println!("{} is up after {}s", host.name, tick.seconds);
                    return OK;
                }
                Some(WakeOutcome::TimedOut) => {
                    eprintln!("{} didn't come online within {}s", host.name, tick.seconds);
                    return CONNECT_FAILED;
                }
                None => std::thread::sleep(Duration::from_secs(1)),
            }
        }
    }

    /// `library <host-ref> [--json]` — the host's games. TSV by default because that is what
    /// Decky's existing consumer parses; `--json` is the door for tools (the Playnite importer
    /// shells to exactly this).
    fn library_cmd(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk library <host-ref> [--json]");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let pin = trust::parse_hex32(&host.fp_hex);
        if pin.is_none() {
            eprintln!(
                "{} isn't paired yet — punktfunk pair {}",
                host.name, host.addr
            );
            return NEEDS_INTERACTION;
        }
        match library::fetch_games(&host.addr, library::DEFAULT_MGMT_PORT, &identity, pin) {
            Ok(games) => {
                if has(args, "--json") {
                    let rows: Vec<serde_json::Value> = games
                        .iter()
                        .map(
                            |g| serde_json::json!({"id": g.id, "store": g.store, "title": g.title}),
                        )
                        .collect();
                    println!("{}", serde_json::json!({ "games": rows }));
                } else {
                    for g in &games {
                        println!("{}\t{}\t{}", g.id, g.store, g.title);
                    }
                    println!("{} game(s)", games.len());
                }
                OK
            }
            Err(e) => {
                eprintln!("library: {e}");
                CONNECT_FAILED
            }
        }
    }

    /// `launch <host-ref> [--game ID] [--profile REF] [--exec]` — start a stream, wake included.
    /// `--exec` becomes the session process instead of supervising it: under a gamescope wrapper
    /// the launched process must BE the streaming one for focus and lifecycle to work.
    fn launch(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk launch <host-ref> [--game ID] [--profile REF] [--exec]");
            return UNRESOLVED;
        };
        let exec = has(args, "--exec");
        let request_access = has(args, "--request-access");
        // Refused rather than silently downgraded: under `--exec` this process BECOMES the
        // session, so nothing survives to see `Ready` and record the approval. A launch that
        // quietly dropped the persistence would leave hosts reading "trusted" forever with
        // nobody able to say why.
        if request_access && exec {
            eprintln!(
                "--request-access can't be combined with --exec: under --exec there is no \
                 process left to record the host's approval"
            );
            return UNRESOLVED;
        }
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let mut plan = ConnectPlan::for_host(
            &known.hosts[i],
            value(args, "--game").as_deref(),
            value(args, "--profile").as_deref(),
        );
        if has(args, "--fullscreen") {
            plan.settings.fullscreen_on_stream = true;
        }
        if request_access {
            plan.connect_timeout_secs = Some(REQUEST_ACCESS_TIMEOUT_SECS);
        }
        run_plan(plan, exec, request_access)
    }

    /// `open <url>` — the `punktfunk://` grammar, headless. Same parser, same refusal rules and
    /// same connect path as a card click; what changes is only where the notices go.
    fn open(args: &[String]) -> u8 {
        let Some(url) = positional(args, 0) else {
            eprintln!("usage: punktfunk open <punktfunk://…>");
            return UNRESOLVED;
        };
        let link = match deeplink::parse(&url) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}", e.message());
                return UNRESOLVED;
            }
        };
        let known = KnownHosts::load();
        let outcome = orchestrate::plan_from_link(
            &link,
            &known,
            &ProfilesFile::load(),
            &trust::Settings::load(),
        );
        match outcome {
            Ok(PlanOutcome::Connect(plan)) => run_plan(*plan, has(args, "--exec"), false),
            // A URL may never pair or trust on its own — that is a decision for a person, at a
            // surface that can show them the fingerprint.
            Ok(PlanOutcome::ConfirmUnknown(u)) => {
                eprintln!(
                    "{} isn't paired with this device — punktfunk pair {}:{}",
                    u.name.unwrap_or_else(|| u.addr.clone()),
                    u.addr,
                    u.port
                );
                NEEDS_INTERACTION
            }
            Ok(PlanOutcome::Unsupported(route)) => {
                eprintln!("punktfunk can't open \"{}\" links yet", route.as_str());
                UNRESOLVED
            }
            Err(e) => {
                eprintln!("{}", e.message());
                UNRESOLVED
            }
        }
    }

    /// Wake if needed, then run the session — supervising it, or becoming it under `--exec`.
    ///
    /// `persist_paired` records the host as *paired* when the child reports ready. Only
    /// `launch --request-access` passes true: there, the host parked the connect until an
    /// operator approved this device, so `Ready` IS the approval arriving — the same thing
    /// `SpawnOpts::persist_paired` means in the GTK shell. Every other launch records nothing,
    /// which is correct: a plain connect proves reachability, not a new trust decision.
    fn run_plan(plan: ConnectPlan, exec: bool, persist_paired: bool) -> u8 {
        if plan.host.fp_hex.is_none() {
            eprintln!(
                "{} has no pinned fingerprint — punktfunk pair {}",
                plan.host.name, plan.host.addr
            );
            return NEEDS_INTERACTION;
        }
        // Wake first when the host is asleep and we know how to reach it. This is the thing the
        // old exec-style CLI never did: it fired a packet at best and dialled into the void.
        if plan.wake
            && !trust::probe_reachable_many(
                vec![(plan.host.addr.clone(), plan.host.port)],
                Duration::from_millis(900),
            )
            .first()
            .copied()
            .unwrap_or(false)
        {
            eprintln!("waking {}…", plan.host.name);
            let mut wait = WakeWait::new();
            loop {
                let online = trust::probe_reachable_many(
                    vec![(plan.host.addr.clone(), plan.host.port)],
                    Duration::from_millis(900),
                )
                .first()
                .copied()
                .unwrap_or(false);
                let tick = wait.tick(online);
                if tick.send_packet {
                    wol::wake(&plan.host.mac, plan.host.addr.parse().ok());
                }
                match tick.outcome {
                    Some(WakeOutcome::Online) => break,
                    Some(WakeOutcome::TimedOut) => {
                        eprintln!("{} didn't come online", plan.host.name);
                        return CONNECT_FAILED;
                    }
                    None => std::thread::sleep(Duration::from_secs(1)),
                }
            }
        }
        if let Some(p) = &plan.profile {
            eprintln!("streaming with \"{}\"", p.name);
        }
        if exec {
            let e = orchestrate::exec_session(&plan);
            eprintln!("couldn't exec the session binary: {e}");
            return RENDERER_FAILED;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = orchestrate::spawn_session(&plan, None, move |ev| {
            let _ = tx.send(ev);
        });
        if let Err(e) = spawned {
            eprintln!("{e}");
            return RENDERER_FAILED;
        }
        let mut failure: Option<(String, bool)> = None;
        while let Ok(ev) = rx.recv() {
            match ev {
                SessionEvent::Ready => {
                    eprintln!("streaming");
                    // The pin we connected WITH, not one re-derived from the store: the record
                    // is what we are about to rewrite, and the session proved the host holds
                    // exactly this identity by completing a pinned handshake against it.
                    if persist_paired {
                        if let Some(fp_hex) = &plan.host.fp_hex {
                            trust::persist_host(
                                &plan.host.name,
                                &plan.host.addr,
                                plan.host.port,
                                fp_hex,
                                true,
                            );
                            trust::forget_placeholder(&plan.host.addr, plan.host.port);
                        }
                    }
                }
                SessionEvent::Error {
                    msg,
                    trust_rejected,
                } => failure = Some((msg, trust_rejected)),
                SessionEvent::Ended(reason) => eprintln!("{reason}"),
                // Persisted by the brain on the way past; nothing to report here.
                SessionEvent::Window { .. } => {}
                SessionEvent::Exited(code) => {
                    return match failure {
                        Some((msg, true)) => {
                            eprintln!("{msg}");
                            TRUST_REJECTED
                        }
                        Some((msg, false)) => {
                            eprintln!("{msg}");
                            CONNECT_FAILED
                        }
                        None if code == 0 => OK,
                        None => RENDERER_FAILED,
                    };
                }
            }
        }
        OK
    }

    /// `reachable <host-ref>` — one bounded, mDNS-independent probe. Exit 0 = reachable.
    fn reachable(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk reachable <host-ref>");
            return UNRESOLVED;
        };
        // An address that isn't saved is still a legitimate thing to probe — this verb answers
        // "can I reach this?", not "do I know this?". Resolved QUIETLY for the same reason:
        // `resolve`'s "pair it first" advice is for verbs that need a saved host, and printing
        // it here would scold the exact usage this verb documents.
        let known = KnownHosts::load();
        let link = DeepLink {
            host_ref: reference.clone(),
            ..Default::default()
        };
        let (addr, port) = match deeplink::resolve_host(&link, &known) {
            HostResolution::Known(i) => (known.hosts[i].addr.clone(), known.hosts[i].port),
            _ => split_host_port(&reference),
        };
        if punktfunk_core::client::NativeClient::probe(&addr, port, PROBE_TIMEOUT) {
            println!("reachable {addr}:{port}");
            OK
        } else {
            eprintln!("unreachable {addr}:{port}");
            CONNECT_FAILED
        }
    }

    /// `speed-test <host-ref>` — measure the real data plane and print what it recommends.
    /// Deliberately does NOT apply the result: which layer a bitrate belongs in is a decision
    /// the GUI makes with the user (bound profile vs global, design §5.3), and a CLI silently
    /// rewriting a profile would be the surprise that rule exists to prevent.
    fn speed_test(args: &[String]) -> u8 {
        let Some(reference) = positional(args, 0) else {
            eprintln!("usage: punktfunk speed-test <host-ref>");
            return UNRESOLVED;
        };
        let (known, i) = match resolve(&reference) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let host = &known.hosts[i];
        let Some(pin) = trust::parse_hex32(&host.fp_hex) else {
            eprintln!("{} isn't paired yet", host.name);
            return NEEDS_INTERACTION;
        };
        let identity = match trust::load_or_create_identity() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("client identity: {e:#}");
                return CONNECT_FAILED;
            }
        };
        let client = match punktfunk_core::client::NativeClient::connect(
            &host.addr,
            host.port,
            punktfunk_core::config::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            punktfunk_core::config::CompositorPref::Auto,
            punktfunk_core::config::GamepadPref::Auto,
            0,     // bitrate_kbps: the host's default; this connect never presents
            0,     // video_caps: nothing decodes here
            2,     // audio_channels
            0,     // video_codecs: the probe carries no video
            0,     // preferred_codec
            None,  // display_hdr
            0,     // client_caps: nothing renders a cursor
            false, // frame_parts: probe/whole-AU consumer
            None,  // launch
            Some(punktfunk_core::client::device_name()),
            Some(pin),
            Some(identity),
            Duration::from_secs(15),
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect: {e:?}");
                return CONNECT_FAILED;
            }
        };
        if let Err(e) = client.request_probe(3_000_000, 2_000) {
            eprintln!("probe: {e:?}");
            return CONNECT_FAILED;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let r = client.probe_result();
            if r.done {
                std::thread::sleep(Duration::from_millis(400));
                let r = client.probe_result();
                let recommended = r.throughput_kbps / 10 * 7;
                if has(args, "--json") {
                    println!(
                        "{}",
                        serde_json::json!({
                            "mbps": f64::from(r.throughput_kbps) / 1000.0,
                            "loss_pct": r.loss_pct,
                            "recommended_kbps": recommended,
                        })
                    );
                } else {
                    println!(
                        "{:.0} Mbit/s measured · {:.1}% loss · recommended {:.0} Mbit/s",
                        f64::from(r.throughput_kbps) / 1000.0,
                        r.loss_pct,
                        f64::from(recommended) / 1000.0
                    );
                }
                return OK;
            }
            if std::time::Instant::now() > deadline {
                eprintln!("probe timed out");
                return CONNECT_FAILED;
            }
        }
    }

    /// `profiles list` — the settings profiles this device has, and what each overrides.
    fn profiles(args: &[String]) -> u8 {
        match positional(args, 0).as_deref() {
            Some("list") | None => {
                let catalog = ProfilesFile::load();
                if has(args, "--json") {
                    println!(
                        "{}",
                        serde_json::to_string(&catalog).unwrap_or_else(|_| "{}".into())
                    );
                } else {
                    for p in &catalog.profiles {
                        let n = serde_json::to_value(&p.overrides)
                            .ok()
                            .and_then(|v| v.as_object().map(|o| o.len()))
                            .unwrap_or(0);
                        println!("{}\t{}\t{n} override(s)", p.id, p.name);
                    }
                }
                OK
            }
            Some(other) => {
                eprintln!("unknown profiles command \"{other}\" — list");
                UNRESOLVED
            }
        }
    }

    /// `reset` — forget this device's saved hosts and stream settings. The identity (and so the
    /// hosts' record of this device) is deliberately NOT touched: re-pairing is the user's call.
    fn reset() -> u8 {
        if !is_tty() {
            eprintln!("refusing to reset without a terminal to confirm on");
            return NEEDS_INTERACTION;
        }
        eprint!("Forget every saved host and reset settings? [y/N] ");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || !line.trim().eq_ignore_ascii_case("y")
        {
            eprintln!("cancelled");
            return OK;
        }
        let mut known = KnownHosts::load();
        known.hosts.clear();
        let _ = known.save();
        trust::Settings::default().save();
        println!("client state reset");
        OK
    }

    fn split_host_port(target: &str) -> (String, u16) {
        match target.rsplit_once(':') {
            Some((a, p)) => match p.parse() {
                Ok(port) => (a.to_string(), port),
                Err(_) => (target.to_string(), 9777),
            },
            None => (target.to_string(), 9777),
        }
    }

    /// Is stdin a terminal? Decides whether a verb may ask a question or must refuse with
    /// [`NEEDS_INTERACTION`] — a CLI that blocks a CI job on a prompt is a hang, not a UX.
    fn is_tty() -> bool {
        std::io::IsTerminal::is_terminal(&std::io::stdin())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn argv(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }

        /// Flags and their values never masquerade as the verb's subject — the bug that makes
        /// `launch --profile Work desk` reach for a host called "Work".
        #[test]
        fn positional_skips_flags_and_their_values() {
            assert_eq!(
                positional(&argv(&["desk", "--game", "steam:570"]), 0),
                Some("desk".into())
            );
            assert_eq!(
                positional(&argv(&["--profile", "Work", "desk"]), 0),
                Some("desk".into())
            );
            assert_eq!(
                positional(&argv(&["--exec", "desk"]), 0),
                Some("desk".into()),
                "a valueless flag must not swallow the subject"
            );
            assert_eq!(
                positional(&argv(&["add", "10.0.0.1"]), 1),
                Some("10.0.0.1".into())
            );
            assert_eq!(positional(&argv(&["--json"]), 0), None);
        }

        #[test]
        fn host_port_splitting() {
            assert_eq!(split_host_port("desk"), ("desk".into(), 9777));
            assert_eq!(split_host_port("desk:1234"), ("desk".into(), 1234));
            // Not a port: keep the whole thing as the address rather than inventing one.
            assert_eq!(split_host_port("desk:nope"), ("desk:nope".into(), 9777));
        }

        /// Every advertised verb documents itself — a USAGE line without a help entry is a
        /// promise `help <verb>` breaks. The overview and each entry must also name the verb.
        #[test]
        fn every_usage_verb_has_help() {
            for verb in [
                "discover",
                "pair",
                "hosts",
                "wake",
                "library",
                "launch",
                "open",
                "reachable",
                "speed-test",
                "profiles",
                "reset",
            ] {
                let h = verb_help(verb).unwrap_or_else(|| panic!("no help for {verb}"));
                assert!(
                    h.starts_with(&format!("punktfunk {verb}")),
                    "help for {verb} must lead with its own invocation"
                );
                assert!(USAGE.contains(verb), "USAGE must advertise {verb}");
            }
            assert!(verb_help("bogus").is_none());
        }

        fn saved(name: &str, addr: &str, fp: &str) -> KnownHost {
            KnownHost {
                name: name.into(),
                addr: addr.into(),
                port: 9777,
                fp_hex: fp.into(),
                ..Default::default()
            }
        }

        /// Step 1 of request access: a host saved by address gains the fingerprint its advert
        /// carried. Before this, `hosts add --fp` on an existing record exited 0 having done
        /// NOTHING — the launch that followed then refused for want of a pin, and the panel had
        /// no way to tell why.
        #[test]
        fn adding_a_fingerprint_to_a_placeholder_fills_it_in() {
            let mut known = KnownHosts {
                hosts: vec![saved("192.168.1.9", "192.168.1.9", "")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "abc123", Some("living-room")),
                AddOutcome::Pinned
            );
            assert_eq!(known.hosts[0].fp_hex, "abc123");
            assert_eq!(
                known.hosts[0].name, "living-room",
                "a record still named after its address takes the offered label"
            );
        }

        /// A label the user chose is theirs — an advert's name must not overwrite it.
        #[test]
        fn filling_in_a_fingerprint_keeps_a_user_chosen_name() {
            let mut known = KnownHosts {
                hosts: vec![saved("Basement rig", "192.168.1.9", "")],
            };
            merge_saved_host(&mut known, 0, "abc123", Some("living-room"));
            assert_eq!(known.hosts[0].name, "Basement rig");
        }

        /// Idempotent: the panel may retry step 1, and re-offering the fingerprint a record
        /// already carries is a state that is already correct, not an error to render.
        #[test]
        fn re_adding_the_same_fingerprint_changes_nothing() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "ABC123")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "abc123", None),
                AddOutcome::Unchanged,
                "fingerprints compare case-insensitively"
            );
            // And a bare `hosts add` with no --fp at all leaves the pin alone.
            assert_eq!(
                merge_saved_host(&mut known, 0, "", None),
                AddOutcome::Unchanged
            );
            assert_eq!(known.hosts[0].fp_hex, "ABC123");
        }

        /// A changed identity is a decision for a person. Never a silent overwrite — this is the
        /// same rule `upsert_trusted` enforces, and a back door here would defeat it everywhere.
        #[test]
        fn a_different_fingerprint_is_refused_not_overwritten() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "abc123")],
            };
            assert_eq!(
                merge_saved_host(&mut known, 0, "deadbeef", None),
                AddOutcome::Conflict
            );
            assert_eq!(
                known.hosts[0].fp_hex, "abc123",
                "the pin must survive intact"
            );
        }

        /// A host that changed DHCP lease is re-pointed, not filed a second time. Without this
        /// the record a stable id resolves to keeps an address the host has left, so a launch
        /// dials into the void while the panel shows the live one.
        #[test]
        fn a_known_fingerprint_at_a_new_address_moves_the_record() {
            let mut known = KnownHosts {
                hosts: vec![saved("desk", "192.168.1.9", "abc123")],
            };
            // Simulates `hosts add 192.168.1.50 --fp abc123` finding no record at that address.
            let by_addr = known
                .hosts
                .iter()
                .position(|h| h.addr == "192.168.1.50" && h.port == 9777);
            assert!(
                by_addr.is_none(),
                "the new address is not yet on any record"
            );
            let by_fp = known
                .hosts
                .iter()
                .position(|h| h.fp_hex.eq_ignore_ascii_case("abc123"));
            assert_eq!(by_fp, Some(0), "the fingerprint still identifies the host");
            known.hosts[0].addr = "192.168.1.50".into();
            assert_eq!(known.hosts.len(), 1, "one host, one record");
        }

        #[test]
        fn value_reads_the_argument_after_its_flag() {
            let a = argv(&["--game", "steam:570", "--exec"]);
            assert_eq!(value(&a, "--game"), Some("steam:570".into()));
            assert_eq!(value(&a, "--profile"), None);
            // A flag followed by another flag has no value.
            assert_eq!(value(&argv(&["--profile", "--exec"]), "--profile"), None);
            assert!(has(&a, "--exec"));
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> std::process::ExitCode {
    // Logs to stderr; stdout is the machine interface (TSV/JSON), exactly like the session
    // binary's contract.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(cli::run(args))
}

/// Keeps `cargo build --workspace` green on macOS, where the client is clients/apple.
#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!("punktfunk runs on Linux and Windows — the macOS client lives in clients/apple");
    std::process::exit(2);
}
