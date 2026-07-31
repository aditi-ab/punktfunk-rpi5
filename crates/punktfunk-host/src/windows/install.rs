//! `punktfunk-host driver install|uninstall` / `web setup` - the install-time work the Windows
//! installer's Inno `[Run]`/`[UninstallRun]` sections delegate to the host EXE instead of
//! locale-parsed PowerShell *files*.
//!
//! Why: Windows PowerShell 5.1 reads a BOM-less `.ps1` *file* in the machine's ANSI codepage, so on a
//! non-English locale a stray non-ASCII byte mis-decodes and the script aborts "unterminated string" -
//! exactly how the pf-vdisplay driver install silently failed on a German box. A compiled subcommand has
//! no such surface: the external tools it drives (`certutil`/`pnputil`/`nefconc`/`schtasks`/`netsh`/
//! `icacls`) are fixed string literals, not a file parsed in some codepage. (The installer's *inline*
//! `-Command` PowerShell in the `.iss` is unaffected - that's a command-line string, not a file read -
//! so it stays.) Sits next to `service install` (`service.rs`), the established Rust-owns-install pattern.
//!
//! Everything here is BEST-EFFORT: a hiccup warns but returns `Ok` - a non-zero exit would abort the
//! whole installer, and a missing driver only degrades the host to a physical display.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ── arg + command helpers ──────────────────────────────────────────────────────────────────────
fn flag_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
/// Run a command, discard output, return whether it succeeded.
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// Run a command, capture stdout (lossy UTF-8); empty on failure.
fn run_capture(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ── `driver install [--gamepad] --dir <stage>` / `driver uninstall [--gamepad]` ────────────────
pub fn driver_main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => driver_install(&args[1..]),
        Some("uninstall") => driver_uninstall(&args[1..]),
        _ => bail!(
            "usage: punktfunk-host driver install --dir <stage> [--gamepad]\n\
             \x20      punktfunk-host driver uninstall [--gamepad]"
        ),
    }
}

fn driver_install(args: &[String]) -> Result<()> {
    let dir =
        PathBuf::from(flag_val(args, "--dir").context("driver install: --dir <stage> required")?);
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", install_gamepad(&dir))
    } else {
        ("pf-vdisplay", install_pf_vdisplay(&dir))
    };
    if let Err(e) = res {
        // Never abort the installer on a driver failure (matches the old best-effort PS scripts).
        eprintln!("warning: {what} driver install: {e:#} (the host degrades without it)");
    }
    Ok(())
}

/// The subject CN both driver-signing certs carry (`build-pf-vdisplay.ps1` /
/// `build-gamepad-drivers.ps1`). certutil matches a CertId against the subject, so this is how we
/// find our own certs again without parsing any localized output — see `purge_driver_certs`.
const DRIVER_CERT_CN: &str = "punktfunk-driver";

/// Remove every `CN=punktfunk-driver` cert this product ever added, from machine `Root` and
/// `TrustedPublisher`.
///
/// Two reasons this has to exist. Uninstall used to leave the certs behind forever, so removing
/// punktfunk left a trusted root CA on the machine — trust we asked for and then never gave back.
/// And before the signing cert was stabilised, every BUILD minted a fresh throwaway cert, so each
/// upgrade added two more roots under the same name; a box that has been upgraded a dozen times is
/// carrying two dozen of them. Purging by subject rather than by thumbprint is what lets one
/// install clean up the whole historical pile.
///
/// Deleting the root does NOT unload an already-installed driver: PnP validates the signature when
/// the package is staged into the driver store, not on every load. So a purge is safe to run before
/// re-adding the current cert.
///
/// Best-effort and silent, like everything else here. `certutil -delstore` deletes one match per
/// call and fails once nothing matches, so loop until it stops succeeding — bounded, because a
/// pathological store must not turn an uninstall into an infinite loop.
fn purge_driver_certs() {
    for store in ["Root", "TrustedPublisher"] {
        let mut removed = 0;
        while removed < 64 && run_quiet("certutil", &["-delstore", store, DRIVER_CERT_CN]) {
            removed += 1;
        }
        if removed > 0 {
            println!("removed {removed} stale '{DRIVER_CERT_CN}' cert(s) from {store}");
        }
    }
}

/// Trust the bundled self-signed driver cert: machine `Root` (so the chain validates) + `TrustedPublisher`
/// (so PnP installs without a prompt).
fn trust_cert(dir: &Path) {
    match first_with_ext(dir, "cer") {
        Some(cer) => {
            let cer = cer.to_string_lossy().into_owned();
            for store in ["Root", "TrustedPublisher"] {
                if !run_quiet("certutil", &["-addstore", "-f", store, &cer]) {
                    eprintln!("warning: certutil -addstore {store} failed for {cer}");
                }
            }
            println!("trusted driver cert {cer} (Root + TrustedPublisher)");
        }
        None => eprintln!(
            "warning: no .cer in {} - driver may not install silently",
            dir.display()
        ),
    }
}

fn install_pf_vdisplay(dir: &Path) -> Result<()> {
    let inf = dir.join("pf_vdisplay.inf");
    if !inf.exists() {
        bail!("no pf_vdisplay.inf in {}", dir.display());
    }
    // Sweep the old certs before adding the current one. Deliberately only on THIS path and not in
    // `install_gamepad`: the installer runs pf-vdisplay first and gamepad second, so one purge here
    // clears the pile and both trust_cert calls then add on top. Purging in both would have the
    // gamepad leg delete the cert the vdisplay leg just installed whenever the two bundles carry
    // different certs — which is exactly what a canary build's per-build fallback certs are.
    purge_driver_certs();
    trust_cert(dir);
    // Create the ROOT device node only if absent (a blind re-create spawns a phantom duplicate, and the
    // host binds interface index 0). ALWAYS nefconc (a clean ROOT\DISPLAY node), NEVER devgen (which makes
    // persistent SWD\DEVGEN software devices that survive reboot + registry deletion).
    if pf_vdisplay_present() {
        println!("pf-vdisplay device node already present - leaving it.");
    } else if let Some(nef) = first_named(dir, "nefconc.exe") {
        let (class, guid) = inf_class(&inf);
        let ok = run_quiet(
            &nef.to_string_lossy(),
            &[
                "--create-device-node",
                "--hardware-id",
                "root\\pf_vdisplay",
                "--class-name",
                &class,
                "--class-guid",
                &guid,
            ],
        );
        if ok {
            println!("created root\\pf_vdisplay device node (nefconc)");
        } else {
            eprintln!("warning: nefconc --create-device-node failed");
        }
    } else {
        eprintln!(
            "warning: nefconc.exe not found in {} - cannot create the device node",
            dir.display()
        );
    }
    // Stage + bind the driver (idempotent; re-staging the same .inf is harmless).
    if run_quiet(
        "pnputil",
        &["/add-driver", &inf.to_string_lossy(), "/install"],
    ) {
        println!("pnputil /add-driver pf_vdisplay.inf /install ok");
    } else {
        eprintln!("warning: pnputil /add-driver /install failed (driver may not have installed)");
    }
    Ok(())
}

fn install_gamepad(dir: &Path) -> Result<()> {
    let infs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")))
        .collect();
    if infs.is_empty() {
        bail!("no driver .inf in {}", dir.display());
    }
    trust_cert(dir);
    // Retire the PRE-RENAME package first. `pf-dualsense` became `pf-gamepad` (one driver always
    // served four identities, so the old name read as if the other three lived elsewhere) and the
    // HARDWARE IDS deliberately did not move — they are the binding contract with every devnode the
    // host creates and with every installed system. That means an upgraded box would otherwise hold
    // TWO store packages claiming `pf_dualsense`/`pf_dualshock4`/…, and PnP would be free to bind
    // the stale one. Matched on `pf_dualsense.dll`, a string only the OLD package's INF contains —
    // the new INF still mentions the hardware ids, so matching on those would delete what we are
    // about to install.
    delete_store_drivers(&["pf_dualsense.dll"]);
    // Add each package to the store - no /install, no device node: the host SwDeviceCreate's the
    // per-session devnode when a client forwards a pad, so PnP binds the store driver on demand.
    for inf in &infs {
        if run_quiet("pnputil", &["/add-driver", &inf.to_string_lossy()]) {
            println!("pnputil /add-driver {} ok", file_name(inf));
        } else {
            eprintln!("warning: pnputil /add-driver {} failed", inf.display());
        }
    }
    // Sweep pad devnodes, INCLUDING phantoms a host crash / service stop left behind: a re-created
    // SwDevice with a known instance id REVIVES the existing devnode with its previously-bound
    // driver — it never re-ranks against the store — so after an upgrade the old driver keeps
    // serving (or, across the v1→v2 sealed-channel fence, fails closed and the pad plays dead).
    // Proven in the field on the RTX box: a v1 phantom pinned the old package through a v2
    // install. The devnodes are per-session objects the host recreates on demand, so removing
    // them at driver-install time is always safe; the next pad binds the fresh package.
    remove_pad_devnodes();
    Ok(())
}

/// `pnputil /remove-device` every punktfunk virtual-pad devnode (live or phantom).
fn remove_pad_devnodes() {
    for id in pad_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed stale pad devnode {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
}

// ── `driver uninstall [--gamepad]` ──────────────────────────────────────────────────────────────
// The uninstaller's cleanup counterpart (Inno [UninstallRun]) — the field report was that our
// virtual-device drivers survived an uninstall. Removes the pf-vdisplay device node(s) + driver
// package, or (--gamepad) the pf-gamepad/pf-xusb driver packages (their devnodes are per-session
// SwDeviceCreate'd and are already gone once the service stopped). Locale-safe by construction: we
// never parse pnputil's localized LABELS — devices are matched on the un-localized VALUE side
// (instance IDs / device IDs), and driver packages are found by scanning %WINDIR%\INF\oem*.inf
// CONTENT for our driver names, then passed to pnputil by file name.

fn driver_uninstall(args: &[String]) -> Result<()> {
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", uninstall_gamepad())
    } else {
        ("pf-vdisplay", uninstall_pf_vdisplay())
    };
    if let Err(e) = res {
        // Same best-effort contract as install: never abort the (un)installer over a driver.
        eprintln!("warning: {what} driver uninstall: {e:#}");
    }
    // Give back the trust we asked for. Here in the dispatcher rather than in the two uninstall
    // bodies so it runs exactly once per invocation, and idempotently when the installer calls both
    // legs back to back. Uninstalling punktfunk must not leave a trusted root CA behind — and this
    // also collects the historical pile from the era when every build signed with a new cert.
    purge_driver_certs();
    Ok(())
}

fn uninstall_pf_vdisplay() -> Result<()> {
    // 1. Remove the ROOT device node(s) the installer created via nefconc (leaving them would keep
    //    a ghost "punktfunk virtual display" in Device Manager forever — the exact complaint).
    for id in pf_vdisplay_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed device node {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
    // 2. Delete the driver package from the driver store.
    delete_store_drivers(&["pf_vdisplay"]);
    Ok(())
}

fn uninstall_gamepad() -> Result<()> {
    // Devnodes first (incl. phantoms — the same ghost-device complaint the vdisplay uninstall
    // fixed), then the store packages.
    remove_pad_devnodes();
    delete_store_drivers(&[
        "pf_gamepad",
        "pf_dualsense",
        "pf_dualshock4",
        "pf_xusb",
        "pf_mouse",
    ]);
    Ok(())
}

/// Instance IDs of enumerated punktfunk virtual-display devices. Parses `pnputil /enum-devices`
/// per-device blocks (blank-line separated); a block is ours if it mentions the pf_vdisplay
/// hardware id / description, and its instance ID is the first line's VALUE (never the localized
/// label) — pnputil prints "Instance ID:" (or its translation) first in every block.
fn pf_vdisplay_instance_ids() -> Vec<String> {
    let out = run_capture("pnputil", &["/enum-devices", "/class", "Display"]);
    let mut ids = Vec::new();
    for block in out.split("\r\n\r\n").flat_map(|b| b.split("\n\n")) {
        let lo = block.to_ascii_lowercase();
        if !lo.contains("pf_vdisplay") && !lo.contains("punktfunk virtual display") {
            continue;
        }
        let Some(first) = block.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some((_, value)) = first.split_once(':') else {
            continue;
        };
        let id = value.trim();
        // Sanity: an instance ID is a backslashed path with no spaces (e.g. ROOT\DISPLAY\0000).
        if !id.is_empty() && id.contains('\\') && !id.contains(' ') {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Instance IDs of punktfunk virtual-pad devnodes (`SWD\PUNKTFUNK\…`), INCLUDING phantoms left by
/// a host crash / service stop (`pnputil /enum-devices` lists disconnected devnodes too). Same
/// un-localized VALUE-side parsing as [`pf_vdisplay_instance_ids`]; matched on the instance-id
/// prefix itself — the pads span two device classes (HIDClass + System), so no `/class` filter.
fn pad_instance_ids() -> Vec<String> {
    let out = run_capture("pnputil", &["/enum-devices"]);
    let mut ids = Vec::new();
    for block in out.split("\r\n\r\n").flat_map(|b| b.split("\n\n")) {
        let Some(first) = block.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some((_, value)) = first.split_once(':') else {
            continue;
        };
        let id = value.trim();
        if id.to_ascii_uppercase().starts_with("SWD\\PUNKTFUNK\\") && !id.contains(' ') {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Delete every driver-store package (`%WINDIR%\INF\oem*.inf`) whose INF text mentions one of
/// `needles` — our driver names are unique enough that a content match identifies the package
/// without parsing `pnputil /enum-drivers`' localized output. `/uninstall /force` also unbinds it
/// from any remaining devnodes.
fn delete_store_drivers(needles: &[&str]) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let inf_dir = Path::new(&windir).join("INF");
    let Ok(entries) = std::fs::read_dir(&inf_dir) else {
        eprintln!("warning: cannot read {}", inf_dir.display());
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        let name = file_name(&path).to_ascii_lowercase();
        if !name.starts_with("oem") || !name.ends_with(".inf") {
            continue;
        }
        let text = read_inf_text(&path).to_ascii_lowercase();
        if !needles.iter().any(|n| text.contains(n)) {
            continue;
        }
        if run_quiet(
            "pnputil",
            &["/delete-driver", &name, "/uninstall", "/force"],
        ) {
            println!("deleted driver package {name}");
        } else {
            eprintln!("warning: pnputil /delete-driver {name} /uninstall /force failed");
        }
    }
}

/// INF files in %WINDIR%\INF are ANSI or UTF-16LE(+BOM); decode either so content matching works.
fn read_inf_text(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Is a punktfunk virtual-display device already enumerated AND connected? `/connected` is
/// load-bearing: without it a PHANTOM (disconnected) devnode left by an earlier uninstall satisfies
/// this check, the install skips creating a live ROOT node, and every session then fails "driver not
/// installed" (the host enumerates present devices only). Matches the device ID / description, which
/// are NOT localized, so the substring check is locale-safe.
fn pf_vdisplay_present() -> bool {
    let lo = run_capture(
        "pnputil",
        &["/enum-devices", "/connected", "/class", "Display"],
    )
    .to_ascii_lowercase();
    lo.contains("pf_vdisplay") || lo.contains("punktfunk virtual display")
}

/// Read `Class` + `ClassGuid` from an INF so the node matches the shipped driver; falls back to Display.
fn inf_class(inf: &Path) -> (String, String) {
    let text = std::fs::read_to_string(inf).unwrap_or_default();
    let (mut class, mut guid) = (None, None);
    for line in text.lines() {
        let t = line.trim();
        if let Some(eq) = t.find('=') {
            let key = t[..eq].trim().to_ascii_lowercase();
            let val = t[eq + 1..]
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            match key.as_str() {
                "class" => class = Some(val),
                "classguid" => guid = Some(val),
                _ => {}
            }
        }
    }
    (
        class
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "Display".into()),
        guid.filter(|g| !g.is_empty())
            .unwrap_or_else(|| "{4d36e968-e325-11ce-bfc1-08002be10318}".into()),
    )
}

// ── `web setup --app-dir <app> [--password-file <file>]` ────────────────────────────────────────
//
// Provisioning ONLY. The console is a supervised child of the PunktfunkHost service (see
// `service.rs`'s "web console child" section; design: punktfunk-planning
// design/windows-web-console-lifecycle.md): the service spawns bun itself, gated on the files the
// console needs, so this subcommand no longer registers a scheduled task, waits for the host's
// cert, or starts anything. What remains is what genuinely belongs to install time: the login
// password (the wizard's --password-file only exists here), the firewall rule, and deleting the
// legacy `PunktfunkWeb` scheduled task a pre-supervision install left behind — a live legacy task
// would race the service's own console child for :47992.

/// The RETIRED scheduled task's name — referenced only to migrate old installs off it.
const WEB_TASK: &str = "PunktfunkWeb";

pub fn web_main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("setup") => web_setup(&args[1..]),
        _ => bail!("usage: punktfunk-host web setup --app-dir <app> [--password-file <file>]"),
    }
}

fn web_setup(args: &[String]) -> Result<()> {
    let app_dir =
        PathBuf::from(flag_val(args, "--app-dir").context("web setup: --app-dir <app> required")?);
    let pw_file = flag_val(args, "--password-file");
    let data_dir = pf_paths::config_dir();
    std::fs::create_dir_all(&data_dir).ok();

    // 1. login password
    set_web_password(&data_dir.join("web-password"), pw_file.as_deref());
    // 2. migration: end + delete the legacy scheduled task (idempotent; harmless when absent).
    //    On the migrating upgrade the installer's StopBunRuntimes DISABLED the task before the file
    //    copy, so it cannot respawn between the new service's start and this delete.
    run_quiet("schtasks", &["/end", "/tn", WEB_TASK]);
    run_quiet("schtasks", &["/delete", "/tn", WEB_TASK, "/f"]);
    // 3. payload sanity. Purely informational — the supervisor logs and keeps waiting on its own —
    //    but install time is when a human is watching, and a WithWeb installer whose payload is
    //    missing has shipped before (the 0.22.1/0.22.2 CI cache bug).
    let server = app_dir
        .join("web")
        .join(".output")
        .join("server")
        .join("index.mjs");
    if !server.exists() {
        eprintln!(
            "warning: web console payload missing at {} - the service will not serve a console",
            server.display()
        );
    }
    // 4. firewall: inbound TCP 47992. The console serves HTTPS (HTTP/1.1 over TLS) with the host's
    //    identity cert. (No UDP/HTTP-3: browsers won't use QUIC against a self-signed/no-SAN cert.)
    //    Scoped to the same profiles as the streaming ports — Domain + Private by default, Public
    //    only with `--allow-public-network`. Delete any prior rule first so an upgrade re-scopes it
    //    instead of stacking a second (possibly all-profiles) rule behind the new one.
    let fw_profile =
        crate::service::firewall_profile_arg(crate::service::allow_public_network(args)?);
    run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=Punktfunk web console (TCP 47992)",
        ],
    );
    if !run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=Punktfunk web console (TCP 47992)",
            "dir=in",
            "action=allow",
            "protocol=TCP",
            "localport=47992",
            fw_profile,
        ],
    ) {
        eprintln!("warning: could not add the firewall rule for TCP 47992");
    }
    // No start step: the PunktfunkHost service supervises the console and starts it the moment the
    // host has written the files it needs (mgmt token + identity cert/key) — there is nothing an
    // install-time one-shot start could add except a new way to fail.
    println!(
        "web console set up (https://<host-ip>:47992; supervised by the PunktfunkHost service)"
    );
    Ok(())
}

/// Source: a non-empty `--password-file` (fresh install) > keep existing (upgrade) > random fallback.
/// Writes `PUNKTFUNK_UI_PASSWORD=<pw>\n` (LF, no BOM) + ACLs it to Administrators + SYSTEM only.
fn set_web_password(pw_path: &Path, pw_file: Option<&str>) {
    let password = pw_file
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if pw_path.exists() {
                println!("keeping existing web console password");
                None
            } else {
                Some(random_password())
            }
        });
    if let Some(pw) = password {
        // Create the file EMPTY first, lock its DACL, THEN write the secret — so the cleartext
        // password is never present at the inherited (Users-readable) %ProgramData% ACL, even for
        // the brief window before icacls runs (security-review 2026-06-28 #8).
        if std::fs::write(pw_path, b"").is_err() {
            eprintln!("warning: could not create {}", pw_path.display());
            return;
        }
        // Lock down: drop inheritance, grant only Administrators (S-1-5-32-544) + SYSTEM (S-1-5-18).
        let p = pw_path.to_string_lossy();
        run_quiet(
            "icacls",
            &[
                &p,
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-32-544:F",
                "*S-1-5-18:F",
            ],
        );
        // Now write the secret into the already-locked file (truncate keeps the explicit DACL).
        if std::fs::write(pw_path, format!("PUNKTFUNK_UI_PASSWORD={pw}\n")).is_err() {
            eprintln!("warning: could not write {}", pw_path.display());
        }
    }
}

/// 20-char URL/shell-safe password (no `/ + =`), like web-init.sh / the old web-setup.ps1.
fn random_password() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::STANDARD
        .encode(b)
        .chars()
        .filter(|c| !matches!(c, '/' | '+' | '='))
        .take(20)
        .collect()
}

fn first_with_ext(dir: &Path, ext: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)))
}
fn first_named(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(name);
    p.exists().then_some(p)
}
fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
