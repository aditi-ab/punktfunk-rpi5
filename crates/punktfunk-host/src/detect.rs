//! Conflicting game-streaming host detection.
//!
//! Punktfunk is one of a family of Moonlight-compatible desktop-streaming hosts. The others —
//! Sunshine and its many forks (Apollo, Vibeshine, Vibepollo, LuminalShine, …) — all impersonate
//! NVIDIA GameStream: they bind the **same** ports (47984/47989 nvhttp, 47998-48010 stream,
//! 47990 web UI — which is also our management API), advertise the **same** `_nvstream._tcp`
//! mDNS service, and frequently install a **conflicting virtual-display driver**. Running one of
//! them alongside Punktfunk is unsupported — the symptoms are `address already in use` bind
//! failures, pairing that silently fails, and capture/virtual-display glitches.
//!
//! This module proactively detects such a host (installed and/or running) so we can surface it as
//! early as possible: at host startup (a `warn!` into the log ring + tray/console summary) and via
//! the `detect-conflicts` subcommand the installers/support run.
//!
//! Detection is **fingerprint-first by name**: a small table of the known products (extend
//! [`KNOWN`] as new forks appear) matched against running processes, registered OS services/units,
//! and on-disk install markers. The platform back-ends (`detect/windows.rs`, `detect/linux.rs`)
//! provide the raw facts; the matching + rendering here is portable and unit-tested.
//!
//! **Not every fingerprint is a conflict.** Only a host that is running, or that will start on its
//! own, can take the ports or load a second virtual-display driver. A leftover `Program Files`
//! folder from an uninstall, a binary on `PATH`, or a service registered but *disabled* clashes
//! with nothing — Sunshine's and Apollo's uninstallers both leave their config/log directories
//! behind, so treating mere presence as a conflict cries wolf on a machine whose other host is long
//! gone. [`Evidence::is_active`] draws that line and [`Detection::is_active`] lifts it to the
//! product; the warning surfaces (startup log, `/local/summary` → the web console's conflicts card,
//! the `detect-conflicts` exit code) report **only** active detections, while the full report still
//! lists the dormant ones as context for support. This matches the installer's own probe
//! (`punktfunk-host.iss`'s `StreamHostEnabled`: service start type <= 2), which was narrowed to
//! exactly this rule after a dormant Sunshine aborted a `winget install` in the field, and the tray,
//! which dropped its always-on warning over a merely-installed Sunshine in `3e782852`.

use std::sync::OnceLock;

/// Lowercased executable basenames (no `.exe`) of every running process — the same snapshot the
/// conflicting-host scan uses, exposed for the few callers that need to ask "is X running?"
/// without duplicating a Toolhelp walk. Best-effort: an empty vec means "could not tell", never
/// "nothing is running", so callers must not read absence as proof.
pub(crate) fn running_process_names() -> Vec<String> {
    platform::running_processes()
}

#[cfg(target_os = "windows")]
#[path = "detect/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "detect/linux.rs"]
mod platform;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    //! The host only runs on Windows/Linux; the crate still compiles on macOS (dev) — nothing to
    //! scan there.
    pub fn running_processes() -> Vec<String> {
        Vec::new()
    }
    pub fn static_evidence(_known: &super::Known) -> Vec<super::Evidence> {
        Vec::new()
    }
}

/// A known competing GameStream/Moonlight host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Sunshine,
    Apollo,
    Vibeshine,
    Vibepollo,
    Luminalshine,
}

impl Product {
    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Product::Sunshine => "Sunshine",
            Product::Apollo => "Apollo",
            Product::Vibeshine => "Vibeshine",
            Product::Vibepollo => "Vibepollo",
            Product::Luminalshine => "LuminalShine",
        }
    }
}

/// How a conflicting host was observed on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// A matching process is running **right now** (process/executable basename).
    Running { process: String },
    /// An OS service / systemd unit for the product is registered. `autostart` is the load-bearing
    /// bit: a service that comes up on its own (Windows start type boot/system/automatic; an enabled
    /// systemd unit) *will* clash, whereas a disabled/manual one is inert until someone starts it by
    /// hand — at which point the `Running` evidence catches it on the next scan.
    Service { name: String, autostart: bool },
    /// Installed on disk — a Program Files directory, a flatpak app id, or a binary on `PATH`.
    /// Always dormant: files that nothing launches bind no ports.
    Installed { at: String },
}

impl Evidence {
    /// Does this observation mean a conflicting host will actually take the ports / load a second
    /// virtual-display driver? See the module docs — this is the whole false-alarm fix.
    pub fn is_active(&self) -> bool {
        match self {
            Evidence::Running { .. } => true,
            Evidence::Service { autostart, .. } => *autostart,
            Evidence::Installed { .. } => false,
        }
    }

    fn render(&self) -> String {
        match self {
            Evidence::Running { process } => format!("running now ({process})"),
            Evidence::Service {
                name,
                autostart: true,
            } => format!("service {name} (starts automatically)"),
            Evidence::Service {
                name,
                autostart: false,
            } => format!("service {name} (disabled/manual — dormant)"),
            Evidence::Installed { at } => format!("installed at {at}"),
        }
    }
}

/// A detected conflicting host, with every piece of corroborating evidence found.
#[derive(Clone, Debug)]
pub struct Detection {
    pub product: Product,
    pub evidence: Vec<Evidence>,
}

impl Detection {
    /// True when a matching process is live — the acute case (a guaranteed resource clash the
    /// moment Punktfunk tries to bind its ports).
    pub fn is_running(&self) -> bool {
        self.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Running { .. }))
    }

    /// True when this host is running **or** will start on its own — i.e. the detection is worth
    /// warning a user about. A product seen only as files on disk or a disabled service is dormant
    /// and reports `false`; see the module docs.
    pub fn is_active(&self) -> bool {
        self.evidence.iter().any(Evidence::is_active)
    }

    /// A compact one-line label for the console summary, e.g. `Sunshine (running)`. The qualifier
    /// names what was actually observed, so a card built from these labels can never claim a
    /// dormant install is running.
    pub fn label(&self) -> String {
        let name = self.product.label();
        if self.is_running() {
            format!("{name} (running)")
        } else if self.is_active() {
            format!("{name} (starts automatically)")
        } else {
            format!("{name} (installed, not running)")
        }
    }
}

/// One row of the known-conflicting-host table. Names are matched case-insensitively; process /
/// binary basenames are given **without** an extension (the platform code lowercases + strips
/// `.exe`). Extend this as new Sunshine forks appear — the runtime, the subcommand, and the
/// tray/console summary all key off this one list.
pub struct Known {
    pub product: Product,
    /// Process / executable basenames (lowercase, no extension) that identify this host.
    pub processes: &'static [&'static str],
    /// Windows service names (SCM keys under `HKLM\SYSTEM\CurrentControlSet\Services`).
    pub win_services: &'static [&'static str],
    /// Windows install-dir basenames under `%ProgramFiles%` / `%ProgramFiles(x86)%`.
    pub win_dirs: &'static [&'static str],
    /// Linux systemd unit basenames (without `.service`), checked in the standard unit dirs.
    pub linux_units: &'static [&'static str],
    /// Linux flatpak application ids.
    pub flatpaks: &'static [&'static str],
}

/// The known Moonlight-compatible hosts that clash with Punktfunk. All are Sunshine or Sunshine
/// forks; add new forks here (one row) and every surface picks them up.
pub const KNOWN: &[Known] = &[
    Known {
        product: Product::Sunshine,
        processes: &["sunshine"],
        win_services: &["SunshineService"],
        win_dirs: &["Sunshine"],
        linux_units: &["sunshine"],
        flatpaks: &["dev.lizardbyte.app.Sunshine"],
    },
    Known {
        product: Product::Apollo,
        processes: &["apollo"],
        win_services: &["ApolloService"],
        win_dirs: &["Apollo"],
        linux_units: &["apollo"],
        flatpaks: &["dev.lizardbyte.app.Apollo"],
    },
    Known {
        product: Product::Vibeshine,
        processes: &["vibeshine"],
        win_services: &["VibeshineService"],
        win_dirs: &["Vibeshine"],
        linux_units: &["vibeshine"],
        flatpaks: &[],
    },
    Known {
        product: Product::Vibepollo,
        processes: &["vibepollo"],
        win_services: &["VibepolloService"],
        win_dirs: &["Vibepollo"],
        linux_units: &["vibepollo"],
        flatpaks: &[],
    },
    Known {
        product: Product::Luminalshine,
        processes: &["luminalshine"],
        win_services: &["LuminalShineService"],
        win_dirs: &["LuminalShine"],
        linux_units: &["luminalshine"],
        flatpaks: &[],
    },
];

/// Why running side-by-side breaks — shared by every surface (log, subcommand, installers).
pub const UNSUPPORTED_BLURB: &str =
    "Running Punktfunk alongside another Moonlight-compatible host \
(Sunshine and its forks) is UNSUPPORTED: they bind the same GameStream ports (47984/47989, \
47998-48010), advertise the same _nvstream mDNS name, and often install a conflicting \
virtual-display driver. Expect \"address already in use\" errors, failed pairing, and capture \
glitches. Stop and uninstall the other host, or don't run them at the same time.";

/// Scan the machine for conflicting hosts. Portable; dispatches into the platform back-end. Does
/// real OS work (process enumeration, service/registry queries, filesystem stats) — cheap, but not
/// free, so prefer the cached [`init`]/[`snapshot`] for hot paths.
pub fn scan() -> Vec<Detection> {
    let procs = platform::running_processes();
    let mut out = Vec::new();
    for known in KNOWN {
        let mut evidence: Vec<Evidence> = Vec::new();
        for p in &procs {
            if known.processes.iter().any(|n| p == n) {
                evidence.push(Evidence::Running { process: p.clone() });
            }
        }
        evidence.extend(platform::static_evidence(known));
        if !evidence.is_empty() {
            out.push(Detection {
                product: known.product,
                evidence,
            });
        }
    }
    out
}

static SNAPSHOT: OnceLock<Vec<Detection>> = OnceLock::new();

/// Scan once and cache the result for the life of the process (the conflict set doesn't change at
/// streaming granularity — a snapshot taken at host bring-up is the right resolution and keeps the
/// per-poll `/local/summary` free). Returns the cached detections.
pub fn init() -> &'static [Detection] {
    SNAPSHOT.get_or_init(scan)
}

/// The cached snapshot, or empty if [`init`] hasn't run. Non-scanning: safe to call from hot paths
/// and from tests without touching the OS.
pub fn snapshot() -> &'static [Detection] {
    SNAPSHOT.get().map(Vec::as_slice).unwrap_or(&[])
}

/// True if any detection is active — the one gate the warning surfaces share (startup log, the
/// `detect-conflicts` exit code, the console card).
pub fn any_active(detections: &[Detection]) -> bool {
    detections.iter().any(Detection::is_active)
}

/// Compact labels for the web-console summary (e.g. `["Sunshine (running)"]`).
///
/// **Active detections only.** A dormant leftover (an uninstalled Sunshine's `Program Files` folder,
/// a disabled service) is deliberately absent: this feeds the console's conflicts card, which exists
/// to explain why clients cannot reach a working-looking host, and files that nothing launches never
/// cause that. The full [`render_report`] still lists them for support.
pub fn summary_labels(detections: &[Detection]) -> Vec<String> {
    detections
        .iter()
        .filter(|d| d.is_active())
        .map(Detection::label)
        .collect()
}

/// A full human-readable report, split by whether the finding can actually clash. Empty string when
/// nothing was detected at all (callers gate on `is_empty()`).
///
/// The dormant section is why this stays verbose where [`summary_labels`] is quiet: when a user asks
/// "why does Punktfunk think I have Apollo?", the answer is the exact leftover path, and the report
/// says in the same breath that it needs no action.
pub fn render_report(detections: &[Detection]) -> String {
    if detections.is_empty() {
        return String::new();
    }
    let bullet = |d: &Detection| {
        let ev = d
            .evidence
            .iter()
            .map(Evidence::render)
            .collect::<Vec<_>>()
            .join("; ");
        format!("  \u{2022} {} \u{2014} {ev}\n", d.product.label())
    };
    let (active, dormant): (Vec<_>, Vec<_>) = detections.iter().partition(|d| d.is_active());
    let mut s = String::new();
    if !active.is_empty() {
        s.push_str("Detected another game-streaming host on this machine.\n");
        s.push_str(UNSUPPORTED_BLURB);
        s.push_str("\n\nDetected:\n");
        for d in &active {
            s.push_str(&bullet(d));
        }
    }
    if !dormant.is_empty() {
        if !active.is_empty() {
            s.push('\n');
        }
        s.push_str(
            "Also present but DORMANT — not running and not set to start on its own, so it clashes \
with nothing and needs no action (typically leftovers from an uninstall):\n",
        );
        for d in &dormant {
            s.push_str(&bullet(d));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(product: Product, evidence: Vec<Evidence>) -> Detection {
        Detection { product, evidence }
    }

    #[test]
    fn empty_report_and_labels() {
        assert!(render_report(&[]).is_empty());
        assert!(summary_labels(&[]).is_empty());
    }

    #[test]
    fn running_detection_is_flagged_and_labelled() {
        let d = det(
            Product::Sunshine,
            vec![
                Evidence::Running {
                    process: "sunshine".into(),
                },
                Evidence::Service {
                    name: "SunshineService".into(),
                    autostart: true,
                },
            ],
        );
        assert!(d.is_running());
        assert!(d.is_active());
        assert_eq!(d.label(), "Sunshine (running)");
    }

    /// The field case this split exists for: Apollo uninstalled, its `Program Files` folder left
    /// behind. Nothing launches it, so it is NOT a conflict and must never reach the console card.
    #[test]
    fn a_leftover_install_dir_is_dormant_and_never_surfaces() {
        let d = det(
            Product::Apollo,
            vec![Evidence::Installed {
                at: "C:\\Program Files\\Apollo".into(),
            }],
        );
        assert!(!d.is_running());
        assert!(!d.is_active(), "files on disk cannot bind a port");
        assert_eq!(d.label(), "Apollo (installed, not running)");
        assert!(summary_labels(std::slice::from_ref(&d)).is_empty());
        assert!(!any_active(&[d]));
    }

    /// A registered-but-DISABLED service is the other half of the same false alarm: `service_exists`
    /// used to count it, which disagreed with the installer's `Start <= 2` probe.
    #[test]
    fn a_disabled_service_is_dormant_but_an_autostart_one_is_not() {
        let disabled = det(
            Product::Sunshine,
            vec![Evidence::Service {
                name: "SunshineService".into(),
                autostart: false,
            }],
        );
        assert!(!disabled.is_active());
        assert!(summary_labels(&[disabled]).is_empty());

        let auto = det(
            Product::Sunshine,
            vec![Evidence::Service {
                name: "SunshineService".into(),
                autostart: true,
            }],
        );
        assert!(auto.is_active());
        assert!(!auto.is_running(), "registered to start != started");
        assert_eq!(auto.label(), "Sunshine (starts automatically)");
        assert_eq!(
            summary_labels(&[auto]),
            vec!["Sunshine (starts automatically)".to_string()]
        );
    }

    #[test]
    fn report_separates_active_from_dormant_and_keeps_the_blurb() {
        let active = det(
            Product::Sunshine,
            vec![Evidence::Running {
                process: "sunshine".into(),
            }],
        );
        let dormant = det(
            Product::Apollo,
            vec![Evidence::Installed {
                at: "/usr/bin/apollo".into(),
            }],
        );
        let report = render_report(&[active.clone(), dormant.clone()]);
        assert!(report.contains("UNSUPPORTED"));
        // The bullets name the PRODUCT and let the evidence speak — `Detection::label`'s qualifier
        // would only restate what follows the dash ("Sunshine (running) — running now (sunshine)").
        // The qualifier is for `summary_labels`, which has no evidence text beside it.
        assert!(report.contains("Sunshine \u{2014} running now (sunshine)"));
        assert!(report.contains("DORMANT"));
        assert!(report.contains("Apollo \u{2014} installed at /usr/bin/apollo"));
        // Only the live one is offered to the console card.
        assert_eq!(
            summary_labels(&[active, dormant.clone()]),
            vec!["Sunshine (running)".to_string()]
        );

        // A dormant-only machine gets the explanatory listing WITHOUT the "unsupported" alarm — the
        // whole point is that this needs no action.
        let dormant_only = render_report(&[dormant]);
        assert!(dormant_only.contains("DORMANT"));
        assert!(
            !dormant_only.contains("UNSUPPORTED"),
            "a leftover folder must not read as an unsupported dual-host setup:\n{dormant_only}"
        );
    }

    #[test]
    fn known_table_rows_are_well_formed() {
        // Every known product carries at least a process name and a Windows service so the runtime
        // scan and the installer's registry check stay in agreement.
        for k in KNOWN {
            assert!(
                !k.processes.is_empty(),
                "{:?} has no process name",
                k.product
            );
            assert!(
                !k.win_services.is_empty(),
                "{:?} has no Windows service name",
                k.product
            );
        }
    }
}
