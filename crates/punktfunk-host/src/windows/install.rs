//! Install-time `driver install|uninstall` and `web setup` that Inno `[Run]`/`[UninstallRun]`
//! delegates to this EXE instead of BOM-less `.ps1` files.
//!
//! PowerShell 5.1 reads a `.ps1` *file* in the machine ANSI codepage; a non-ASCII byte on a
//! non-English locale aborts as "unterminated string". A compiled subcommand has no such
//! surface: `certutil`/`pnputil`/`nefconc`/`schtasks`/`netsh`/`icacls` are string literals.
//! Inline `-Command` PowerShell in the `.iss` is a command-line string, not a file, so it stays.
//! Same pattern as `service install` in `service.rs`.
//!
//! Best-effort: a hiccup warns but returns `Ok`. A non-zero exit aborts the installer; a
//! missing driver only degrades the host to a physical display.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn flag_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
fn run_capture(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

pub fn driver_main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => driver_install(&args[1..]),
        Some("uninstall") => driver_uninstall(&args[1..]),
        _ => bail!(
            "usage: punktfunk-host driver install --dir <stage> [--gamepad]\n\
             \x20      punktfunk-host driver uninstall [--gamepad|--audio]"
        ),
    }
}

fn driver_install(args: &[String]) -> Result<()> {
    let dir =
        PathBuf::from(flag_val(args, "--dir").context("driver install: --dir <stage> required")?);
    // `--dir` is code and trust (Root cert, nefconc, INF). A stage a non-admin can write is LPE.
    ensure_admin_only_source(&dir).with_context(|| {
        format!(
            "refusing to install drivers from {} — the staging directory must be writable only by \
             SYSTEM/Administrators",
            dir.display()
        )
    })?;
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", install_gamepad(&dir))
    } else {
        ("pf-vdisplay", install_pf_vdisplay(&dir))
    };
    if let Err(e) = res {
        eprintln!("warning: {what} driver install: {e:#} (the host degrades without it)");
    }
    Ok(())
}

/// Refuse a staging directory anyone but SYSTEM/Administrators/TrustedInstaller can write.
///
/// Both: the owner is in that set (an owner always retains `WRITE_DAC` and can restore their
/// own ACE), and no allow ACE grants a write-shaped right to anyone else. `CREATOR OWNER` is
/// outside — a non-admin who created the dir under `%ProgramData%` keeps control through it.
///
/// Reads the security descriptor; `icacls` output uses localized account names.
#[cfg(windows)]
fn ensure_admin_only_source(dir: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, GetAce, IsValidSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    /// Rights that replace what we are about to trust and execute. GENERIC_ALL/WRITE are checked
    /// unmapped in case an ACE stores them without mapping.
    const WRITE_MASK: u32 = 0x0000_0002 // FILE_WRITE_DATA / FILE_ADD_FILE
        | 0x0000_0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
        | 0x0000_0010 // FILE_WRITE_EA
        | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
        | 0x0000_0040 // FILE_DELETE_CHILD
        | 0x0001_0000 // DELETE
        | 0x0004_0000 // WRITE_DAC
        | 0x0008_0000 // WRITE_OWNER
        | 0x1000_0000 // GENERIC_ALL
        | 0x4000_0000; // GENERIC_WRITE

    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are live locals; the
    // returned descriptor is the single allocation, LocalFree'd below (owner/dacl point into it).
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };

    let verdict = (|| -> Result<()> {
        rc.ok().context("GetNamedSecurityInfoW(owner + DACL)")?;
        let privileged = privileged_sids()?;
        let is_privileged = |sid: PSID| -> bool {
            // SAFETY: every `sid` handed in points into the descriptor returned above (or at an
            // ACE inside it) and is valid for this scope; IsValidSid is itself the probe.
            if sid.is_invalid() || !unsafe { IsValidSid(sid) }.as_bool() {
                return false;
            }
            privileged
                .iter()
                // SAFETY: `sid` passed IsValidSid above; `p` is an owned, length-exact SID copy.
                .any(|p| unsafe { EqualSid(sid, PSID(p.as_ptr().cast_mut().cast())) }.is_ok())
        };

        if !is_privileged(owner) {
            bail!(
                "the directory is owned by a non-administrative account, which retains WRITE_DAC \
                 and can restore its own access at any time"
            );
        }
        // A NULL DACL grants everyone everything; an absent one is not "no access".
        if dacl.is_null() {
            bail!("the directory has a NULL DACL (everyone has full control)");
        }
        // SAFETY: `dacl` is a valid ACL inside the descriptor; AceCount bounds the GetAce index.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..count as u32 {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: i < AceCount, and `ace` is a live out-param.
            unsafe { GetAce(dacl, i, &mut ace) }.context("GetAce")?;
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { *(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue; // deny ACEs only ever subtract; audit ACEs grant nothing
            }
            // SAFETY: an allow ACE is an ACCESS_ALLOWED_ACE, whose SidStart begins the trustee SID.
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            if allowed.Mask & WRITE_MASK == 0 {
                continue; // no write-shaped right
            }
            let sid = PSID(std::ptr::addr_of!(allowed.SidStart) as *mut core::ffi::c_void);
            if !is_privileged(sid) {
                bail!(
                    "a non-administrative trustee has write access (ACE {i}, mask {:#010x}) — \
                     anything staged here can be replaced before it is trusted or executed",
                    allowed.Mask
                );
            }
        }
        Ok(())
    })();

    // SAFETY: `sd` is the single LocalAlloc'd descriptor GetNamedSecurityInfoW returned.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    verdict
}

/// SYSTEM, `BUILTIN\Administrators`, and TrustedInstaller (owns `%ProgramFiles%`).
#[cfg(windows)]
fn privileged_sids() -> Result<Vec<Vec<u8>>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{GetLengthSid, PSID};

    [
        "S-1-5-18",
        "S-1-5-32-544",
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    ]
    .iter()
    .map(|s| {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid = PSID::default();
        // SAFETY: `wide` is NUL-terminated and outlives the call; psid is a live out-param.
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid) }
            .with_context(|| format!("ConvertStringSidToSidW({s})"))?;
        // SAFETY: psid is a valid SID; copy it out so the caller owns plain bytes.
        let len = unsafe { GetLengthSid(psid) } as usize;
        // SAFETY: GetLengthSid just measured exactly `len` readable bytes at `psid`.
        let bytes = unsafe { std::slice::from_raw_parts(psid.0 as *const u8, len) }.to_vec();
        // SAFETY: ConvertStringSidToSidW allocates with LocalAlloc.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(psid.0)));
        }
        Ok(bytes)
    })
    .collect()
}

/// `Some(true)` SYSTEM/Administrators/TrustedInstaller, `Some(false)` any other owner, `None`
/// if the owner cannot be read. Call before `create_private_dir` re-owns the file and erases
/// the signal. Distrusts a `host.env` / `web-password` a non-admin planted under `%ProgramData%`.
#[cfg(windows)]
pub(crate) fn is_admin_owned(path: &Path) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out-params are live locals; the
    // returned descriptor is the single allocation, LocalFree'd below (owner points into it).
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    let verdict = (|| -> Option<bool> {
        rc.ok().ok()?;
        let privileged = privileged_sids().ok()?;
        // SAFETY: `owner` points into the descriptor returned above; IsValidSid is the probe.
        if owner.is_invalid() || !unsafe { IsValidSid(owner) }.as_bool() {
            return None;
        }
        let admin = privileged.iter().any(|p| {
            // SAFETY: `owner` passed IsValidSid; `p` is an owned, length-exact SID copy.
            unsafe { EqualSid(owner, PSID(p.as_ptr().cast_mut().cast())) }.is_ok()
        });
        Some(admin)
    })();
    // SAFETY: `sd` is the single LocalAlloc'd descriptor GetNamedSecurityInfoW returned.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    verdict
}

/// Subject CN both signing certs carry. `certutil` matches CertId on subject, not a localized name.
const DRIVER_CERT_CN: &str = "punktfunk-driver";

/// Delete every `CN=punktfunk-driver` cert from machine `Root` and `TrustedPublisher`.
///
/// Match on subject, not thumbprint, so one run collects every historical cert under this CN.
/// Deleting the root does not unload an installed driver: PnP validates the signature when the
/// package is staged, not on every load. Safe to run before re-adding the current cert.
///
/// `certutil -delstore` deletes one match per call and fails when none remain. Bound the loop
/// so a pathological store cannot hang uninstall.
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

/// Machine `Root` (chain validates) and `TrustedPublisher` (PnP installs without a prompt).
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
    // Purge here only, not in `install_gamepad`. The installer runs vdisplay then gamepad; a
    // second purge would delete the cert the vdisplay leg just added when the two bundles differ.
    purge_driver_certs();
    trust_cert(dir);
    // Create the ROOT node only if absent: a re-create is a phantom duplicate, and the host binds
    // index 0. nefconc (ROOT\DISPLAY), never devgen (SWD\DEVGEN nodes survive reboot + registry delete).
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
    // Hardware IDs did not move when `pf-dualsense` became `pf-gamepad`. Match the old INF on
    // `pf_dualsense.dll` — matching those IDs would also delete the package we are about to add.
    delete_store_drivers(&["pf_dualsense.dll"]);
    // No `/install`, no device node: the host SwDeviceCreate's the per-session devnode when a
    // client forwards a pad, so PnP binds the store driver on demand.
    for inf in &infs {
        if run_quiet("pnputil", &["/add-driver", &inf.to_string_lossy()]) {
            println!("pnputil /add-driver {} ok", file_name(inf));
        } else {
            eprintln!("warning: pnputil /add-driver {} failed", inf.display());
        }
    }
    // Phantoms too: SwDeviceCreate with a known instance id revives the bound driver and never
    // re-ranks the store. Per-session objects; the next pad binds the fresh package.
    remove_pad_devnodes();
    Ok(())
}

fn remove_pad_devnodes() {
    for id in pad_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed stale pad devnode {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
}

fn driver_uninstall(args: &[String]) -> Result<()> {
    // Audio removes host-minted devnodes on Valve's drivers; return before the cert purge so a
    // `--audio` uninstall does not run that purge a third time.
    if flag_present(args, "--audio") {
        return uninstall_audio_devices();
    }
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", uninstall_gamepad())
    } else {
        ("pf-vdisplay", uninstall_pf_vdisplay())
    };
    if let Err(e) = res {
        eprintln!("warning: {what} driver uninstall: {e:#}");
    }
    // Once per invocation, here rather than in each uninstall body: the uninstaller calls both
    // legs back to back, and a leftover trusted root CA must not survive.
    purge_driver_certs();
    Ok(())
}

/// Host-minted "Punktfunk Speakers"/"Punktfunk Microphone" and per-pad DualSense speaker
/// endpoints. Run after `service uninstall`: a live host re-mints them on the next wiring pass.
///
/// Does not remove Steam's streaming-audio drivers. Marker-matched in `audio::devnode_cleanup`.
fn uninstall_audio_devices() -> Result<()> {
    match crate::audio::devnode_cleanup::purge() {
        Ok(r) if r.devnodes == 0 && r.devnode_failures == 0 => {
            println!("no punktfunk audio devices to remove")
        }
        Ok(r) => {
            println!(
                "removed {} punktfunk audio device(s), {} endpoint record(s)",
                r.devnodes, r.endpoint_records
            );
            if r.devnode_failures > 0 {
                eprintln!(
                    "warning: {} punktfunk audio device(s) could not be removed — they can be \
                     deleted from Device Manager (View ▸ Show hidden devices)",
                    r.devnode_failures
                );
            }
        }
        Err(e) => eprintln!("warning: audio device cleanup: {e:#}"),
    }
    Ok(())
}

fn uninstall_pf_vdisplay() -> Result<()> {
    // ROOT nodes first; leaving them is a ghost "punktfunk virtual display" in Device Manager.
    for id in pf_vdisplay_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed device node {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
    delete_store_drivers(&["pf_vdisplay"]);
    Ok(())
}

fn uninstall_gamepad() -> Result<()> {
    // Devnodes (incl. phantoms) before store packages.
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

/// Instance IDs of enumerated pf-vdisplay devices. Blank-line blocks from `pnputil /enum-devices`;
/// ours if the block mentions the hardware id / description. The instance ID is the first line's
/// VALUE (never the localized "Instance ID:" label — pnputil prints that first in every block).
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
        // Instance IDs are backslashed paths with no spaces (`ROOT\DISPLAY\0000`).
        if !id.is_empty() && id.contains('\\') && !id.contains(' ') {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Pad instance IDs (`SWD\PUNKTFUNK\…`), including phantoms (`/enum-devices` lists disconnected
/// nodes). Same VALUE-side parse as [`pf_vdisplay_instance_ids`]. No `/class`: HIDClass + System.
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

/// Delete each `%WINDIR%\INF\oem*.inf` whose text mentions a needle — content match, not
/// `pnputil /enum-drivers` (localized). `/uninstall /force` also unbinds remaining devnodes.
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

/// `%WINDIR%\INF` is ANSI or UTF-16LE(+BOM); decode either so the needle match works.
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

/// Enumerated AND connected. Without `/connected` a phantom from an earlier uninstall satisfies
/// this, install skips the live ROOT node, and the host (present devices only) reports no driver.
/// Match device ID / description, not a localized label.
fn pf_vdisplay_present() -> bool {
    let lo = run_capture(
        "pnputil",
        &["/enum-devices", "/connected", "/class", "Display"],
    )
    .to_ascii_lowercase();
    lo.contains("pf_vdisplay") || lo.contains("punktfunk virtual display")
}

/// INF `Class` + `ClassGuid` so the node matches the shipped driver; fallback is Display.
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

// Install-time provisioning only: login password, firewall, and delete of the legacy
// `PunktfunkWeb` scheduled task (a live one races the service's console child for :47992).
// The service supervises bun; see `service.rs` and design/windows-web-console-lifecycle.md.

/// Retired scheduled-task name; referenced only to delete it from older installs.
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
    // `create_private_dir`, not `create_dir_all`: the next line writes the console password, and
    // `create_dir_all` would inherit `%ProgramData%` (BUILTIN\Users can create files).
    pf_paths::create_private_dir(&data_dir).ok();

    set_web_password(&data_dir.join("web-password"), pw_file.as_deref());
    // End + delete the legacy task (idempotent if absent). The installer disables it before the
    // file copy so it cannot respawn between service start and this delete.
    run_quiet("schtasks", &["/end", "/tn", WEB_TASK]);
    run_quiet("schtasks", &["/delete", "/tn", WEB_TASK, "/f"]);
    // Informational: the supervisor waits on its own; install time is when a human is watching.
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
    // TCP 47992 (console) and 47993 (plugin UIs — separate origin so a plugin cannot act as the
    // operator). Delete any prior rule first so an upgrade re-scopes instead of stacking. Bind
    // to `<app>/bun/bun.exe`; a port-only allow admits whoever binds first. No UDP: browsers
    // will not QUIC a self-signed/no-SAN cert. Missing bun.exe falls back to port-only.
    let fw_profile =
        crate::service::firewall_profile_arg(crate::service::allow_public_network(args)?);
    let bun = app_dir.join("bun").join("bun.exe");
    let program = bun.exists().then_some(bun.as_path());
    if program.is_none() {
        eprintln!(
            "warning: {} not found — the console firewall rules stay open to any program on those \
             ports instead of only the console",
            bun.display()
        );
    }
    for (name, port) in [
        ("Punktfunk web console (TCP 47992)", "47992"),
        ("Punktfunk plugin UIs (TCP 47993)", "47993"),
    ] {
        run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
        if !crate::service::run_netsh(&crate::service::fw_add_rule_args(
            name,
            "TCP",
            Some(port),
            program,
            fw_profile,
        )) {
            eprintln!("warning: could not add the firewall rule for TCP {port}");
        }
    }
    println!(
        "web console set up (https://<host-ip>:47992; supervised by the PunktfunkHost service)"
    );
    Ok(())
}

/// Non-empty `--password-file` (fresh) > keep existing (upgrade) > random. Writes
/// `PUNKTFUNK_UI_PASSWORD=<pw>\n` (LF, no BOM) and ACLs it to Administrators + SYSTEM only.
fn set_web_password(pw_path: &Path, pw_file: Option<&str>) {
    // Non-admin owner means planted under `%ProgramData%` CREATOR OWNER before this install.
    // `FileExists` would treat it as an upgrade and keep the attacker's password. Rename aside;
    // `!planted` still rotates if the rename failed. An Administrators-owned file is kept.
    let planted = pw_path.exists() && is_admin_owned(pw_path) == Some(false);
    if planted {
        let mut aside = pw_path.to_path_buf().into_os_string();
        aside.push(".untrusted");
        let aside = std::path::PathBuf::from(aside);
        let _ = std::fs::remove_file(&aside);
        let _ = std::fs::rename(pw_path, &aside);
        println!(
            "web console password file was owned by a non-admin (planted before install) — rotating to a fresh password"
        );
    }
    let password = pw_file
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if pw_path.exists() && !planted {
                println!("keeping existing web console password");
                None
            } else {
                Some(random_password())
            }
        });
    if let Some(pw) = password {
        // Empty file, lock DACL, then write: the secret must not sit on the inherited
        // `%ProgramData%` (Users-readable) ACL even for the window before icacls.
        if std::fs::write(pw_path, b"").is_err() {
            eprintln!("warning: could not create {}", pw_path.display());
            return;
        }
        // Drop inheritance; Administrators (S-1-5-32-544) + SYSTEM (S-1-5-18) only.
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
        // Truncate keeps the explicit DACL; write the secret into the already-locked file.
        if std::fs::write(pw_path, format!("PUNKTFUNK_UI_PASSWORD={pw}\n")).is_err() {
            eprintln!("warning: could not write {}", pw_path.display());
        }
    }
}

/// 20 chars, URL/shell-safe (no `/ + =`).
fn random_password() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
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
