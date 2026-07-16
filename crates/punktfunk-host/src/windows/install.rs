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
// package, or (--gamepad) the pf-dualsense/pf-xusb driver packages (their devnodes are per-session
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
    delete_store_drivers(&["pf_dualsense", "pf_dualshock4", "pf_xusb"]);
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
    let pw_path = data_dir.join("web-password");
    let token_path = data_dir.join("mgmt-token");

    // 1. login password
    set_web_password(&pw_path, pw_file.as_deref());
    // 2. (upgrade-safe) stop any running console so the new task binds :47992 + the files unlock
    stop_web_console();
    // 3. register the PunktfunkWeb scheduled task
    let cmd = app_dir.join("web").join("web-run.cmd");
    if !cmd.exists() {
        bail!("web launcher missing: {}", cmd.display());
    }
    register_web_task(&cmd)?;
    // 4. firewall: inbound TCP 47992. The console serves HTTPS (HTTP/1.1 over TLS) with the host's
    //    identity cert. (No UDP/HTTP-3: browsers won't use QUIC against a self-signed/no-SAN cert.)
    //    Scoped to the same profiles as the streaming ports — Domain + Private by default, Public
    //    only with `--allow-public-network`. Delete any prior rule first so an upgrade re-scopes it
    //    instead of stacking a second (possibly all-profiles) rule behind the new one.
    let fw_profile =
        crate::service::firewall_profile_arg(crate::service::allow_public_network(args));
    run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=punktfunk web console (TCP 47992)",
        ],
    );
    if !run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=punktfunk web console (TCP 47992)",
            "dir=in",
            "action=allow",
            "protocol=TCP",
            "localport=47992",
            fw_profile,
        ],
    ) {
        eprintln!("warning: could not add the firewall rule for TCP 47992");
    }
    // 5. wait briefly for the host's mgmt token, then start (restart-on-failure picks it up otherwise)
    for _ in 0..30 {
        if token_path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    run_quiet("schtasks", &["/run", "/tn", WEB_TASK]);
    println!("web console set up + started (https://<host-ip>:47992)");
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

/// Stop + reap a running console before re-registering (upgrade-safe): end the task AND kill the :47992
/// listener owner (runtime-agnostic - a prior install may have run node vs the current bun). The listener
/// is identified by the wildcard foreign address (`0.0.0.0:0`/`[::]:0`), so the localized state word
/// ("LISTENING"/"ABHOEREN"/...) is never parsed.
fn stop_web_console() {
    run_quiet("schtasks", &["/end", "/tn", WEB_TASK]);
    for line in run_capture("netstat", &["-ano", "-p", "tcp"]).lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() >= 5
            && toks[0].eq_ignore_ascii_case("tcp")
            && toks[1].ends_with(":47992")
            && (toks[2] == "0.0.0.0:0" || toks[2] == "[::]:0")
        {
            let pid = toks[toks.len() - 1];
            if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) {
                run_quiet("taskkill", &["/PID", pid, "/F"]);
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
}

/// Register the boot/SYSTEM/restart-on-failure task via a generated Task Scheduler XML (`schtasks /xml`,
/// no COM). The XML declares UTF-16, so it's written UTF-16LE+BOM.
fn register_web_task(cmd: &Path) -> Result<()> {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo><Description>punktfunk web management console (Nitro SSR on bun, :47992)</Description></RegistrationInfo>\n\
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>\n\
  <Principals><Principal id=\"Author\"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>\n\
  <Settings>\n\
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
    <StartWhenAvailable>true</StartWhenAvailable>\n\
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
    <RestartOnFailure><Interval>PT1M</Interval><Count>10</Count></RestartOnFailure>\n\
  </Settings>\n\
  <Actions Context=\"Author\"><Exec><Command>{}</Command></Exec></Actions>\n\
</Task>",
        xml_escape(&cmd.to_string_lossy())
    );
    let xml_path = std::env::temp_dir().join("punktfunk-web-task.xml");
    write_utf16le_bom(&xml_path, &xml)?;
    let ok = run_quiet(
        "schtasks",
        &[
            "/create",
            "/tn",
            WEB_TASK,
            "/xml",
            &xml_path.to_string_lossy(),
            "/f",
        ],
    );
    let _ = std::fs::remove_file(&xml_path);
    if ok {
        println!("registered scheduled task {WEB_TASK} -> {}", cmd.display());
        Ok(())
    } else {
        bail!("schtasks /create {WEB_TASK} failed")
    }
}

fn write_utf16le_bom(path: &Path, s: &str) -> Result<()> {
    let mut bytes = vec![0xFFu8, 0xFE]; // UTF-16LE BOM
    for u in s.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
