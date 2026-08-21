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

// ── `driver install [--gamepad] --dir <stage>` / `driver uninstall [--gamepad|--audio]` ────────
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
    // Everything below this line runs with the caller's privileges — which, on the installer path,
    // are SYSTEM/Administrator — and it does three things with the CONTENTS of `dir`: trusts a
    // `.cer` into the machine `Root` store, runs `nefconc.exe` from it, and stages an `.inf` into
    // the driver store. So the directory is not merely an input, it is code and trust; a stage a
    // non-admin can write is a local privilege escalation, whoever passed the flag.
    //
    // This is the check the 2026-07-05 audit recorded as FIXED (F-8) and which was never actually
    // in the tree — re-found by the 2026-08-05 review as H-5, and the payload half of H-4's
    // plant-then-elevate chain (`PUNKTFUNK_HOST_CMD=driver install --dir C:\Users\attacker\stage`).
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
        // Never abort the installer on a driver failure (matches the old best-effort PS scripts).
        eprintln!("warning: {what} driver install: {e:#} (the host degrades without it)");
    }
    Ok(())
}

/// Refuse a driver staging directory that anyone but SYSTEM/Administrators can write.
///
/// Two conditions, both necessary:
/// - the directory is **owned** by SYSTEM, Administrators, or TrustedInstaller — an owner always
///   retains `WRITE_DAC`, so a non-admin owner can put their own access back no matter what the
///   DACL currently says;
/// - no **allow** ACE grants a write-shaped right to any trustee outside that same set. `CREATOR
///   OWNER` counts as outside: on a directory a non-admin pre-created under `C:\ProgramData`, it is
///   precisely what keeps handing them control of everything inside.
///
/// Reads the security descriptor directly rather than parsing `icacls` output, which prints
/// *localized account names* — the same class of locale trap this whole module exists to avoid.
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
    /// Rights that let a trustee change what we are about to trust and execute: write/append data,
    /// write attributes/EA, delete (incl. child delete), and the two that let them rewrite the
    /// security descriptor itself. `GENERIC_WRITE`/`GENERIC_ALL` map onto these once mapped, and
    /// both generic bits are checked explicitly in case an ACE stores them unmapped.
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
                continue; // read-only for this trustee — harmless
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

/// The SIDs allowed to own or write a driver staging directory: `SYSTEM`, `BUILTIN\Administrators`,
/// and `TrustedInstaller` (which owns much of `%ProgramFiles%`, a perfectly good stage).
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

/// Owner check for a SINGLE secret file: `Some(true)` if owned by SYSTEM / Administrators /
/// TrustedInstaller, `Some(false)` if owned by any other (non-privileged) account, `None` if the
/// owner could not be determined. Used to distrust a `host.env` / `web-password` a non-admin
/// pre-created under `%ProgramData%` before a privileged install ran — the file's bytes would
/// otherwise be adopted verbatim into the SYSTEM service's environment / the console password
/// (security-review 2026-08-15 findings 3c and 4). Reads the security descriptor directly, like
/// [`ensure_admin_only_source`], to stay locale-independent. Must be consulted BEFORE any
/// `create_private_dir` re-owns the file to Administrators and erases the signal.
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

// ── `driver uninstall [--gamepad|--audio]` ──────────────────────────────────────────────────────
// The uninstaller's cleanup counterpart (Inno [UninstallRun]) — the field report was that our
// virtual devices survived an uninstall. Removes the pf-vdisplay device node(s) + driver package,
// or (--gamepad) the pf-gamepad/pf-xusb driver packages (their devnodes are per-session
// SwDeviceCreate'd and are already gone once the service stopped), or (--audio) the audio devnodes
// the HOST mints at runtime — the same complaint one layer up, since those are created by the
// running host rather than by any driver payload the installer laid down. Locale-safe by
// construction: we never parse pnputil's localized LABELS — devices are matched on the
// un-localized VALUE side (instance IDs / device IDs / registry markers), and driver packages are
// found by scanning %WINDIR%\INF\oem*.inf CONTENT for our driver names, then passed to pnputil by
// file name.

fn driver_uninstall(args: &[String]) -> Result<()> {
    // The audio leg touches no driver package and no certificate — it removes devnodes the host
    // minted on Valve's drivers — so it returns before the cert purge below rather than making
    // that purge run a third time per uninstall.
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

/// Remove the "Punktfunk Speakers"/"Punktfunk Microphone" endpoints and the per-pad DualSense
/// speaker endpoints the running host minted — the audio half of the surviving-virtual-device
/// complaint. Must run AFTER `service uninstall`: a live host re-mints them on its next wiring
/// pass, which would make this sweep look like it did nothing.
///
/// Never removes Steam's streaming-audio DRIVERS. Ours are extra devnodes riding on drivers that
/// belong to Steam and that the user's own Remote Play still needs; the sweep is marker-matched
/// (see `audio::devnode_cleanup`) precisely so it can tell the two apart.
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
        // Best-effort like every other leg: an enumeration that fails must not fail the uninstall.
        Err(e) => eprintln!("warning: audio device cleanup: {e:#}"),
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
    // `create_private_dir`, not `create_dir_all`: this runs at install time, before anything else
    // touches the config dir, and the very next line writes the console login password into it. A
    // plain `create_dir_all` leaves the inherited `%ProgramData%` ACL, under which BUILTIN\Users may
    // create files — so the one call that most needs the hardened directory was the one creating it
    // unhardened (2026-08-05 review H-4).
    pf_paths::create_private_dir(&data_dir).ok();

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
    // 4. firewall: inbound TCP 47992 (console) and 47993 (plugin UIs). The console serves HTTPS
    //    (HTTP/1.1 over TLS) with the host's identity cert. (No UDP/HTTP-3: browsers won't use QUIC
    //    against a self-signed/no-SAN cert.) Scoped to the same profiles as the streaming ports —
    //    Domain + Private by default, Public only with `--allow-public-network`. Delete any prior
    //    rule first so an upgrade re-scopes it instead of stacking a second (possibly all-profiles)
    //    rule behind the new one.
    //
    //    47993 is a SEPARATE ORIGIN, not a second copy of the console: plugin UIs are served there
    //    precisely so a plugin cannot act as the logged-in operator on the console's origin
    //    (security-review 2026-08-05 H-3). Same host, same certificate, different port — which is
    //    what makes it a different origin to the browser while staying same-site for the session
    //    cookie. Without this rule, plugin interfaces simply do not load from another device.
    //    Both rules are scoped to the bundled bun binary that actually listens on them, not left
    //    open to any program: a port-only `dir=in action=allow` rule admits whatever binds the port
    //    first, needs no elevation to do so, and suppresses the Windows prompt that would otherwise
    //    be the only way in (see `service::fw_add_rule_args`). The console child is
    //    `<app>/bun/bun.exe` — the same path `service.rs`'s supervisor spawns — so the rule follows
    //    it. If that binary isn't there, fall back to the port-only rule rather than leaving the
    //    console unreachable, and say which happened.
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
    // A password file that exists but is owned by a NON-admin was planted by an unprivileged user
    // before this privileged install (`%ProgramData%` CREATOR OWNER). The installer's
    // `FreshWebInstall := not FileExists` check then mistakes it for an upgrade, skips the password
    // page, and adopts the attacker's console password. Distrust it: rename aside and rotate to a
    // fresh random below (`!planted` forces the random branch even if the rename failed). A password
    // file from a prior privileged install is Administrators-owned and is kept. security-review
    // 2026-08-15 finding 4.
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
