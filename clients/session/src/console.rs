//! `--browse [host[:port]]` — the console shell. Bare `--browse` opens the host list
//! (discovery, pairing, settings, wake — the whole couch flow); with a target it opens
//! straight into that host's library (the Decky per-host launch), B backing out to the
//! list. A launches in the SAME window (no gamescope window handoff — the whole point
//! of one process), the session's end returns to the console, B at the root quits to
//! Gaming Mode.
//!
//! This file is the console's SERVICE side: the shell (pf-console-ui) renders and
//! raises [`ConsoleCmd`]s; worker threads here run everything that blocks — mDNS
//! discovery, reachability probes, the SPAKE2 pairing ceremony, wake-on-LAN loops,
//! library fetches, known-hosts persistence — and write results into the shared
//! models. `PUNKTFUNK_FAKE_LIBRARY=<file.json>` feeds canned entries with no host
//! (portrait paths starting with `/` load from disk), the GPU-only dev path.

use crate::session_main::{
    arg_flag, arg_value, fullscreen_mode, parse_host_port, session_params, window_pos,
};
use pf_client_core::gamepad::is_steam_deck;
use pf_client_core::{discovery, library, trust, wol};
use pf_console_ui::{
    ConsoleCmd, ConsoleEntry, ConsoleHandles, ConsoleOptions, ConsoleShared, HostRow, LibraryGame,
    LibraryPhase, LibraryShared, PairPhase, SkiaOverlay, WakeStatus,
};
use pf_presenter::overlay::OverlayAction;
use pf_presenter::ActionOutcome;
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A request-access connect awaiting the operator's approval on the host: stamped by the
/// launch handler and consumed by `on_connected`, which persists the host as paired.
struct PendingApproval {
    name: String,
    addr: String,
    port: u16,
    fp_hex: String,
}

pub fn run(target: Option<&str>) -> u8 {
    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return crate::session_main::EXIT_CONNECT_FAILED;
        }
    };

    // Resolve the entry point: a paired target opens straight into its library; an
    // unpaired/unknown one lands on Home with the target seeded into the list (one A
    // from pairing). The fake-library hook fabricates a paired host with no network.
    let fake = std::env::var_os("PUNKTFUNK_FAKE_LIBRARY").is_some();
    let known = trust::KnownHosts::load();
    let mut seed: Option<HostRow> = None;
    let (entry, window_label) = match target {
        Some(target) => {
            let (addr, port) = parse_host_port(target);
            let k = known
                .hosts
                .iter()
                .find(|h| h.addr == addr && h.port == port);
            let row = HostRow {
                key: k
                    .filter(|h| !h.fp_hex.is_empty())
                    .map_or_else(|| format!("{addr}:{port}"), |h| h.fp_hex.clone()),
                name: k
                    .map(|h| host_display_name(&h.name, &h.addr))
                    .unwrap_or_else(|| addr.clone()),
                addr: addr.clone(),
                port,
                fp_hex: k.map(|h| h.fp_hex.clone()).unwrap_or_default(),
                paired: k.is_some_and(|h| h.paired) || fake,
                saved: k.is_some(),
                online: false,
                mgmt_port: arg_value("--mgmt")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(library::DEFAULT_MGMT_PORT),
                can_wake: false,
                last_used: k.and_then(|h| h.last_used),
            };
            let label = row.name.clone();
            if k.is_none() {
                seed = Some(row.clone());
            }
            if row.paired {
                (ConsoleEntry::Library(row), Some(label))
            } else {
                (ConsoleEntry::Home, Some(label))
            }
        }
        None if fake => {
            let row = fake_host_row();
            (ConsoleEntry::Library(row), None)
        }
        None => (ConsoleEntry::Home, None),
    };
    let initial_fetch = match &entry {
        ConsoleEntry::Library(h) => Some(ConsoleCmd::FetchLibrary {
            addr: h.addr.clone(),
            mgmt: h.mgmt_port,
            fp_hex: h.fp_hex.clone(),
        }),
        ConsoleEntry::Home => None,
    };

    let opts = ConsoleOptions {
        device_name: device_name(),
        deck: is_steam_deck(),
    };
    let (overlay, handles) = match SkiaOverlay::console(opts, entry) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("console UI: {e:#}");
            return crate::session_main::EXIT_PRESENTER_FAILED;
        }
    };
    let ConsoleHandles {
        console,
        library: library_model,
        bus,
    } = handles;

    // The service loop: discovery, probes, wake, pairing, persistence, fetches.
    let service = Service::start(
        console.clone(),
        library_model.clone(),
        bus.clone(),
        identity.clone(),
        seed,
    );
    if let Some(cmd) = initial_fetch {
        bus.send(cmd);
    }

    // `--json-status`: a shell parent is reading stdout (the WinUI shell hides itself on
    // `{"ready":true}` and restores on exit) — plain CLI/gamescope runs stay silent.
    let json_status = arg_flag("--json-status");
    let settings_at_start = trust::Settings::load();

    // Request-access hand-off: the launch handler stamps this when it starts a delegated-approval
    // connect; `on_connected` reads it once the host lets us in and persists the host as PAIRED,
    // so the next connect is an ordinary one. `None` for every normal launch, so `on_connected`
    // then only touches last-used.
    let pending_approval: Arc<Mutex<Option<PendingApproval>>> = Arc::new(Mutex::new(None));
    let pending_cb = pending_approval.clone();

    let opts = pf_presenter::SessionOpts {
        window_title: window_label.map_or_else(
            || "Punktfunk".to_string(),
            |label| format!("Punktfunk · {label}"),
        ),
        fullscreen: fullscreen_mode(),
        window_pos: window_pos(),
        // `--stats` forces the overlay visible without demoting a richer chosen tier.
        stats_verbosity: match settings_at_start.stats_verbosity() {
            trust::StatsVerbosity::Off if arg_flag("--stats") => trust::StatsVerbosity::Normal,
            v => v,
        },
        touch_mode: settings_at_start.touch_mode(),
        json_status,
        on_connected: Some(Box::new(move |fingerprint: [u8; 32]| {
            let fp_hex = trust::hex(&fingerprint);
            trust::touch_last_used(&fp_hex);
            // A request-access connect just succeeded → the operator approved us. Save the
            // host as paired (it was unsaved/discovered), keyed to the fingerprint we pinned.
            if let Some(p) = pending_cb.lock().unwrap().take() {
                if p.fp_hex == fp_hex {
                    trust::persist_host(&p.name, &p.addr, p.port, &fp_hex, true);
                }
            }
        })),
        overlay: Some(Box::new(overlay)),
        window_size: crate::session_main::window_size(&settings_at_start),
        // Latched at console start (like the stats tier above): toggling Match window in
        // the console's settings screen applies from the next console launch.
        match_window: crate::session_main::match_window(&settings_at_start),
    };

    let result =
        pf_presenter::run_browse(opts, |action, gamepad, native, force_software, vulkan| {
            match action {
                OverlayAction::Launch {
                    addr,
                    port,
                    fp_hex,
                    launch,
                    title,
                    request_access,
                } => {
                    let Some(pin) = trust::parse_hex32(&fp_hex) else {
                        // Connect (and request-access) pin the host's advertised fingerprint;
                        // a pinless launch is a logic slip, never a silent TOFU.
                        tracing::warn!(%addr, "launch without a stored pin — refusing");
                        return ActionOutcome::Handled;
                    };
                    tracing::info!(%addr, %title, request_access,
                        launch = launch.as_deref().unwrap_or("desktop"),
                        "launching from the console");
                    // Settings re-load per launch: the console's own settings screen
                    // may have changed them since the last stream.
                    let settings = trust::Settings::load();
                    let mut params = session_params(
                        &settings,
                        addr.clone(),
                        port,
                        pin,
                        identity.clone(),
                        launch,
                        gamepad,
                        native,
                        force_software,
                        vulkan,
                    );
                    if request_access {
                        // The host PARKS the connect until the operator approves — outlast its
                        // approval window (host `PENDING_APPROVAL_WAIT`), matching the desktop
                        // shells' 185 s. On success `on_connected` persists the host as paired.
                        params.connect_timeout = Duration::from_secs(185);
                        *pending_approval.lock().unwrap() = Some(PendingApproval {
                            name: title.clone(),
                            addr,
                            port,
                            fp_hex: fp_hex.clone(),
                        });
                    }
                    ActionOutcome::Start(Box::new(params))
                }
                OverlayAction::CancelConnect => ActionOutcome::Handled, // run-loop-side
                OverlayAction::Quit => ActionOutcome::Quit,
            }
        });

    service.stop();

    match result {
        Ok(()) => 0,
        Err(e) => {
            // The shell contract's terminal line (a clean quit needs none — stdout EOF
            // already routes the shell back to its host list silently).
            if json_status {
                crate::session_main::json_line("error", &format!("{e:#}"), Some(false));
            }
            eprintln!("console: {e:#}");
            crate::session_main::EXIT_PRESENTER_FAILED
        }
    }
}

/// The machine's name — what the host lists this client as after pairing.
fn device_name() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "This device".into())
}

fn host_display_name(name: &str, addr: &str) -> String {
    if name.trim().is_empty() {
        addr.to_string()
    } else {
        name.to_string()
    }
}

fn fake_host_row() -> HostRow {
    HostRow {
        key: "fake".into(),
        name: "Demo Host".into(),
        addr: "127.0.0.1".into(),
        port: 9777,
        fp_hex: String::new(),
        paired: true,
        saved: true,
        online: true,
        mgmt_port: library::DEFAULT_MGMT_PORT,
        can_wake: false,
        last_used: None,
    }
}

/// The background service: owns discovery, probing, waking, pairing and persistence.
struct Service {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    fn start(
        console: ConsoleShared,
        library_model: LibraryShared,
        bus: pf_console_ui::ConsoleBus,
        identity: (String, String),
        seed: Option<HostRow>,
    ) -> Service {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-console".into())
            .spawn(move || {
                ServiceState {
                    console,
                    library: library_model,
                    bus,
                    identity,
                    seed,
                    discovered: HashMap::new(),
                    probed: Arc::new(Mutex::new(HashMap::new())),
                    probe_inflight: Arc::new(AtomicBool::new(false)),
                    last_probe: Instant::now() - Duration::from_secs(60),
                    wake_cancel: None,
                }
                .run(stop_w)
            })
            .ok();
        Service { stop, thread }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct ServiceState {
    console: ConsoleShared,
    library: LibraryShared,
    bus: pf_console_ui::ConsoleBus,
    identity: (String, String),
    /// A `--browse` target that isn't in the store yet — kept on the list until the
    /// store or discovery covers it.
    seed: Option<HostRow>,
    discovered: HashMap<String, discovery::DiscoveredHost>,
    /// Probe results by row key, written by sweep threads.
    probed: Arc<Mutex<HashMap<String, bool>>>,
    probe_inflight: Arc<AtomicBool>,
    last_probe: Instant,
    /// Cancels the active wake thread (it owns the model's wake status).
    wake_cancel: Option<Arc<AtomicBool>>,
}

impl ServiceState {
    fn run(mut self, stop: Arc<AtomicBool>) {
        let discovery_rx = discovery::browse();
        while !stop.load(Ordering::SeqCst) {
            // mDNS churn.
            while let Ok(ev) = discovery_rx.try_recv() {
                match ev {
                    discovery::DiscoveryEvent::Resolved(host) => {
                        self.discovered.insert(host.fullname.clone(), host);
                    }
                    discovery::DiscoveryEvent::Removed { fullname } => {
                        self.discovered.remove(&fullname);
                    }
                }
            }

            // Shell commands (plus the binary's own seeded initial fetch).
            for cmd in self.bus.drain() {
                self.handle(cmd);
            }

            // The 10 s reachability sweep — saved hosts that don't advertise (routed /
            // multicast-filtered networks) still get honest presence pips.
            if self.last_probe.elapsed() >= Duration::from_secs(10) {
                self.last_probe = Instant::now();
                self.sweep();
            }

            self.console.set_hosts(self.rows());
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(c) = &self.wake_cancel {
            c.store(true, Ordering::SeqCst);
        }
    }

    fn handle(&mut self, cmd: ConsoleCmd) {
        match cmd {
            ConsoleCmd::FetchLibrary { addr, mgmt, fp_hex } => {
                spawn_fetch(
                    self.library.clone(),
                    addr,
                    mgmt,
                    self.identity.clone(),
                    trust::parse_hex32(&fp_hex),
                );
            }
            ConsoleCmd::Pair {
                addr,
                port,
                pin,
                device_name,
            } => {
                // Prefer what the list already calls this host (advert or store).
                let name = self
                    .rows()
                    .into_iter()
                    .find(|r| r.addr == addr && r.port == port)
                    .map_or_else(|| addr.clone(), |r| r.name);
                self.console.set_pair(PairPhase::Busy);
                let console = self.console.clone();
                let identity = self.identity.clone();
                std::thread::Builder::new()
                    .name("punktfunk-pair".into())
                    .spawn(move || {
                        match trust::pair_with_host(&addr, port, &identity, &pin, &device_name) {
                            Ok(fp) => {
                                let fp_hex = trust::hex(&fp);
                                trust::persist_host(&name, &addr, port, &fp_hex, true);
                                console.set_pair(PairPhase::Paired { key: fp_hex });
                            }
                            Err(e) => {
                                let msg = match e {
                                    punktfunk_core::PunktfunkError::Crypto => {
                                        "Wrong PIN — check the host's Pairing page and try again."
                                            .to_string()
                                    }
                                    punktfunk_core::PunktfunkError::Timeout => {
                                        "The host didn't answer. Is it running and reachable?"
                                            .to_string()
                                    }
                                    other => format!("Pairing failed: {other:?}"),
                                };
                                console.set_pair(PairPhase::Failed(msg));
                            }
                        }
                    })
                    .ok();
            }
            ConsoleCmd::SaveHost { name, addr, port } => {
                let mut known = trust::KnownHosts::load();
                // Manual entries have no fingerprint yet, so `upsert` (fp-keyed) would
                // collide two of them — key manual saves by address instead.
                if let Some(h) = known
                    .hosts
                    .iter_mut()
                    .find(|h| h.addr == addr && h.port == port)
                {
                    if !name.is_empty() {
                        h.name = name;
                    }
                } else {
                    known.hosts.push(trust::KnownHost {
                        name: if name.is_empty() { addr.clone() } else { name },
                        addr,
                        port,
                        fp_hex: String::new(),
                        paired: false,
                        last_used: None,
                        mac: Vec::new(),
                    });
                }
                if let Err(e) = known.save() {
                    tracing::warn!(error = %format!("{e:#}"), "saving known hosts");
                }
                self.last_probe = Instant::now() - Duration::from_secs(60); // probe it now
            }
            ConsoleCmd::Wake { key, then_connect } => {
                if let Some(c) = self.wake_cancel.take() {
                    c.store(true, Ordering::SeqCst);
                }
                let Some(row) = self.rows().into_iter().find(|r| r.key == key) else {
                    return;
                };
                let known = trust::KnownHosts::load();
                let macs = known
                    .hosts
                    .iter()
                    .find(|h| {
                        h.fp_hex == row.fp_hex && !row.fp_hex.is_empty()
                            || (h.addr == row.addr && h.port == row.port)
                    })
                    .map(|h| h.mac.clone())
                    .unwrap_or_default();
                if macs.is_empty() {
                    self.console.set_pair(PairPhase::Idle); // no-op; keep state sane
                    return;
                }
                let cancel = Arc::new(AtomicBool::new(false));
                self.wake_cancel = Some(cancel.clone());
                spawn_wake(self.console.clone(), row, macs, then_connect, cancel);
            }
            ConsoleCmd::CancelWake => {
                if let Some(c) = self.wake_cancel.take() {
                    c.store(true, Ordering::SeqCst);
                }
                self.console.set_wake(None);
            }
            ConsoleCmd::Probe => {
                self.last_probe = Instant::now() - Duration::from_secs(60);
            }
        }
    }

    /// One parallel reachability pass over every non-advertising row (advertising ones
    /// are online by definition). Runs on its own thread; at most one in flight.
    fn sweep(&self) {
        if self.probe_inflight.swap(true, Ordering::SeqCst) {
            return;
        }
        let targets: Vec<(String, (String, u16))> = self
            .rows()
            .into_iter()
            .filter(|r| !self.advertised(r))
            .map(|r| (r.key.clone(), (r.addr.clone(), r.port)))
            .collect();
        let probed = self.probed.clone();
        let inflight = self.probe_inflight.clone();
        std::thread::Builder::new()
            .name("punktfunk-probe".into())
            .spawn(move || {
                let (keys, addrs): (Vec<_>, Vec<_>) = targets.into_iter().unzip();
                let results = trust::probe_reachable_many(addrs, Duration::from_millis(900));
                let mut map = probed.lock().unwrap();
                for (key, ok) in keys.into_iter().zip(results) {
                    map.insert(key, ok);
                }
                inflight.store(false, Ordering::SeqCst);
            })
            .ok();
    }

    fn advertised(&self, row: &HostRow) -> bool {
        self.discovered.values().any(|d| {
            (!row.fp_hex.is_empty() && d.fp_hex == row.fp_hex)
                || (d.addr == row.addr && d.port == row.port)
        })
    }

    /// The console home's rows: saved hosts (most recent first), then
    /// discovered-but-unsaved ones, then a still-uncovered `--browse` seed.
    fn rows(&self) -> Vec<HostRow> {
        let known = trust::KnownHosts::load();
        let probed = self.probed.lock().unwrap();
        let mut rows: Vec<HostRow> = known
            .hosts
            .iter()
            .map(|h| {
                let key = if h.fp_hex.is_empty() {
                    format!("{}:{}", h.addr, h.port)
                } else {
                    h.fp_hex.clone()
                };
                let advert = self.discovered.values().find(|d| {
                    (!h.fp_hex.is_empty() && d.fp_hex == h.fp_hex)
                        || (d.addr == h.addr && d.port == h.port)
                });
                let online = advert.is_some() || probed.get(&key).copied().unwrap_or(false);
                HostRow {
                    key,
                    name: host_display_name(&h.name, &h.addr),
                    addr: h.addr.clone(),
                    port: h.port,
                    fp_hex: h.fp_hex.clone(),
                    paired: h.paired,
                    saved: true,
                    online,
                    mgmt_port: advert
                        .and_then(|d| d.mgmt_port)
                        .unwrap_or(library::DEFAULT_MGMT_PORT),
                    can_wake: !online && !h.mac.is_empty(),
                    last_used: h.last_used,
                }
            })
            .collect();
        rows.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.name.cmp(&b.name)));

        let mut extra: Vec<HostRow> = self
            .discovered
            .values()
            .filter(|d| {
                !known.hosts.iter().any(|h| {
                    (!h.fp_hex.is_empty() && h.fp_hex == d.fp_hex)
                        || (h.addr == d.addr && h.port == d.port)
                })
            })
            .map(|d| HostRow {
                key: if d.fp_hex.is_empty() {
                    format!("{}:{}", d.addr, d.port)
                } else {
                    d.fp_hex.clone()
                },
                name: host_display_name(&d.name, &d.addr),
                addr: d.addr.clone(),
                port: d.port,
                fp_hex: d.fp_hex.clone(),
                paired: false,
                saved: false,
                online: true,
                mgmt_port: d.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
                can_wake: false,
                last_used: None,
            })
            .collect();
        extra.sort_by(|a, b| a.name.cmp(&b.name));
        rows.extend(extra);

        if let Some(seed) = &self.seed {
            if !rows
                .iter()
                .any(|r| r.addr == seed.addr && r.port == seed.port)
            {
                let mut seed = seed.clone();
                seed.online = probed.get(&seed.key).copied().unwrap_or(false);
                rows.push(seed);
            }
        }
        rows
    }
}

/// The wake-and-wait loop (one per wake): re-send the magic packet every 6 s, probe the
/// host once a second, 90 s timeout — the Apple `HostWaker`'s cadence. The thread owns
/// the model's wake status; the shell reads `online`/`timed_out` and acts.
fn spawn_wake(
    console: ConsoleShared,
    row: HostRow,
    macs: Vec<String>,
    then_connect: bool,
    cancel: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("punktfunk-wake".into())
        .spawn(move || {
            let last_ip = row.addr.parse::<Ipv4Addr>().ok();
            let started = Instant::now();
            let mut last_packet: Option<Instant> = None;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    console.set_wake(None);
                    return;
                }
                let elapsed = started.elapsed();
                let timed_out = elapsed >= Duration::from_secs(90);
                if !timed_out && last_packet.is_none_or(|t| t.elapsed() >= Duration::from_secs(6)) {
                    wol::wake(&macs, last_ip);
                    last_packet = Some(Instant::now());
                }
                let online = trust::probe_reachable_many(
                    vec![(row.addr.clone(), row.port)],
                    Duration::from_millis(900),
                )
                .first()
                .copied()
                .unwrap_or(false);
                console.set_wake(Some(WakeStatus {
                    key: row.key.clone(),
                    name: row.name.clone(),
                    seconds: elapsed.as_secs() as u32,
                    timed_out,
                    online,
                    then_connect,
                }));
                if online || timed_out {
                    // Awake → the shell connects and cancels; timed out → the card
                    // waits for Try Again / Cancel. Either way this thread is done —
                    // a retry spawns a fresh one.
                    return;
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        })
        .ok();
}

/// Fetch the library off the service thread, then stream poster art into the shared
/// model as results land (the renderer drains `push_art` per frame).
fn spawn_fetch(
    shared: LibraryShared,
    addr: String,
    mgmt: u16,
    identity: (String, String),
    pin: Option<[u8; 32]>,
) {
    shared.set_phase(LibraryPhase::Loading);
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            if let Ok(path) = std::env::var("PUNKTFUNK_FAKE_LIBRARY") {
                load_fake(&shared, &path);
                return;
            }
            match library::fetch_games(&addr, mgmt, &identity, pin) {
                Ok(games) => {
                    let base = library::base_url(&addr, mgmt);
                    let jobs: VecDeque<(String, Vec<String>)> = games
                        .iter()
                        .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
                        .filter(|(_, candidates)| !candidates.is_empty())
                        .collect();
                    shared.set_games(
                        games
                            .iter()
                            .map(|g| LibraryGame {
                                id: g.id.clone(),
                                title: g.title.clone(),
                                store: g.store.clone(),
                            })
                            .collect(),
                    );
                    if !jobs.is_empty() {
                        let rx = library::spawn_art_fetch(base, identity, pin, jobs);
                        while let Ok((id, bytes)) = rx.recv_blocking() {
                            shared.push_art(id, bytes);
                        }
                    }
                }
                Err(e) => shared.set_phase(LibraryPhase::Error {
                    title: "Couldn't load the library".into(),
                    body: e.to_string(),
                    can_retry: true,
                }),
            }
        })
        .ok();
}

/// Dev hook: entries from a JSON file; portrait paths starting with `/` load from disk.
fn load_fake(shared: &LibraryShared, path: &str) {
    let games: Vec<library::GameEntry> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for g in &games {
        if let Some(p) = g.art.portrait.as_deref().filter(|p| p.starts_with('/')) {
            if let Ok(bytes) = std::fs::read(p) {
                shared.push_art(g.id.clone(), bytes);
            }
        }
    }
    shared.set_games(
        games
            .iter()
            .map(|g| LibraryGame {
                id: g.id.clone(),
                title: g.title.clone(),
                store: g.store.clone(),
            })
            .collect(),
    );
}
