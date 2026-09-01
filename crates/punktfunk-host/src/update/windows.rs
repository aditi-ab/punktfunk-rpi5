//! Windows update apply: download the immutable installer, verify it, persist intent and spawn it
//! outside the service's kill-on-close job. Boot reconciliation records the outcome.
//!
//! The Ed25519-signed manifest binds the file SHA-256. Stable releases additionally require the
//! per-artifact signing-leaf pin, a trusted Authenticode chain and the expected publisher subject;
//! old schema-1 clients already enforce the leaf pin. Canary/local builds may use a self-signed
//! certificate, but still require a valid signature and any pins the manifest supplies.
//!
//! Signer information comes from the same `WinVerifyTrust` state used for verification. No second
//! file parse can inspect different bytes.

#![cfg(target_os = "windows")]

use super::jobs::{self, IntentRecord};
use super::manifest::WindowsHostAsset;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Free-space preflight: require this multiple of the download size (installer + Inno's
/// unpack scratch + headroom).
const DISK_MARGIN: u64 = 3;

/// Keep the target + this many previous installers cached for the manual-rollback path.
const KEEP_INSTALLERS: usize = 2;

const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The winget-blessed silent flags (`packaging/winget/unom.PunktfunkHost.installer.yaml`).
const SILENT_ARGS: [&str; 4] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"];

fn staging_dir() -> PathBuf {
    pf_paths::config_dir().join("updates")
}

fn log_path(version: &str) -> PathBuf {
    pf_paths::config_dir()
        .join("logs")
        .join(format!("update-{version}.log"))
}

/// The whole pipeline, run on a blocking thread. Reports progress/stage through the callbacks
/// so this file stays free of the runtime-state lock.
pub(super) fn run_apply(
    asset: &WindowsHostAsset,
    target_version: &str,
    serial: u64,
    progress: &dyn Fn(u64, Option<u64>),
    stage: &dyn Fn(&'static str),
) -> Result<(), (&'static str, String)> {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| ("downloading", format!("create staging dir: {e}")))?;

    let final_path = dir.join(format!("punktfunk-host-setup-{target_version}.exe"));
    let part_path = dir.join(format!("punktfunk-host-setup-{target_version}.exe.part"));

    download(&asset.url, &part_path, progress).map_err(|e| ("downloading", e))?;

    stage("verifying");
    verify_sha256(&part_path, &asset.sha256).map_err(|e| {
        quarantine(&part_path);
        ("verifying", e)
    })?;
    verify_authenticode(
        &part_path,
        &asset.authenticode_sha256,
        Some(&asset.authenticode_subject)
            .filter(|s| !s.is_empty())
            .map(String::as_str),
    )
    .map_err(|e| {
        quarantine(&part_path);
        ("verifying", e)
    })?;
    std::fs::rename(&part_path, &final_path)
        .map_err(|e| ("verifying", format!("stage rename: {e}")))?;
    prune_installers(&dir, &final_path);

    stage("applying");
    let log = log_path(target_version);
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The point of no return: after this record exists, boot reconciliation owns the outcome.
    //
    // Nothing about the status tray is recorded here: the installer force-kills every tray to
    // unlock punktfunk-tray.exe and, under /VERYSILENT, never runs its own relaunch entry, but
    // `tray::supervise` in the new host puts one back without needing to be told.
    jobs::write_json_atomic(
        &jobs::intent_path(),
        &IntentRecord {
            from: env!("PUNKTFUNK_VERSION").into(),
            to: target_version.into(),
            serial,
            started_unix: super::now_unix(),
            installer_sha256: asset.sha256.to_ascii_lowercase(),
            log_path: log.display().to_string(),
            source_build: false,
        },
    )
    .map_err(|e| ("applying", format!("write intent record: {e}")))?;

    // Let the 202 + the console's next status poll leave the box before the installer starts
    // stopping the service under us (plan R4).
    std::thread::sleep(std::time::Duration::from_secs(2));

    let spawned = {
        use std::os::windows::process::CommandExt as _;
        std::process::Command::new(&final_path)
            .args(SILENT_ARGS)
            .arg(format!("/LOG={}", log.display()))
            .current_dir(&dir)
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW)
            .spawn()
    };
    match spawned {
        Ok(child) => {
            // Detached on purpose: the child must outlive us. Dropping a Child does not kill it.
            drop(child);
            stage("restarting");
            Ok(())
        }
        Err(e) => {
            // Most likely: the job object stopped allowing breakaway (R3). Clear the intent —
            // nothing irreversible happened — and surface the real error.
            let _ = std::fs::remove_file(jobs::intent_path());
            Err((
                "applying",
                format!(
                    "spawn installer (CREATE_BREAKAWAY_FROM_JOB — if this is ACCESS_DENIED, \
                     the service job object no longer permits breakaway): {e}"
                ),
            ))
        }
    }
}

fn quarantine(part: &Path) {
    let bad = part.with_extension("bad");
    let _ = std::fs::remove_file(&bad);
    let _ = std::fs::rename(part, &bad);
}

/// Download `url` to `part`, resuming an existing partial file when the server honors Range.
fn download(url: &str, part: &Path, progress: &dyn Fn(u64, Option<u64>)) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("installer url must be https".into());
    }
    // Connect timeout only, deliberately no global one: this streams an installer that is tens of
    // MB, and a whole-request deadline would abort a slow-but-healthy download.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .max_redirects(3)
        .user_agent(format!(
            "punktfunk-host/{} (update-apply)",
            env!("PUNKTFUNK_VERSION")
        ))
        .build()
        .into();

    let existing = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut req = agent.get(url);
    if existing > 0 {
        req = req.header("Range", &format!("bytes={existing}-"));
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => format!("download returned HTTP {code}"),
        other => format!("download failed: {other}"),
    })?;

    let resumed = resp.status() == 206;
    let content_len: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let total = content_len.map(|l| if resumed { existing + l } else { l });
    if let Some(t) = total {
        preflight_disk(part, t.saturating_mul(DISK_MARGIN))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Never truncate at open: on a 206 we append to the existing partial, and the
        // fresh-download path truncates explicitly via `set_len(0)` below.
        .truncate(false)
        .open(part)
        .map_err(|e| format!("open staging file: {e}"))?;
    let mut received = if resumed {
        file.seek(std::io::SeekFrom::End(0))
            .map_err(|e| format!("seek: {e}"))?
    } else {
        file.set_len(0).map_err(|e| format!("truncate: {e}"))?;
        0
    };
    progress(received, total);

    // Unlimited reader, not `read_to_vec` — the installer is streamed to disk in 64 KiB chunks so
    // it never lands in memory, and ureq 3's body-read caps do not apply to this path.
    let mut reader = resp.into_body().into_reader();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write: {e}"))?;
        received += n as u64;
        progress(received, total);
    }
    file.sync_all().map_err(|e| format!("fsync: {e}"))?;
    if let Some(t) = total {
        if received != t {
            return Err(format!("download truncated: {received} of {t} bytes"));
        }
    }
    Ok(())
}

fn verify_sha256(path: &Path, expected_hex: &str) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open for hashing: {e}"))?;
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read for hashing: {e}"))?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    let got = hex(ctx.finish().as_ref());
    if got != expected_hex.to_ascii_lowercase() {
        return Err(format!(
            "installer sha256 mismatch: got {got}, manifest says {expected_hex}"
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn preflight_disk(at: &Path, needed: u64) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let dir = at.parent().unwrap_or(at);
    let mut free: u64 = 0;
    // SAFETY: the HSTRING is a valid NUL-terminated path living across the call, and the out
    // param points at a live local u64; the API retains neither.
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(dir.as_os_str()), Some(&mut free), None, None) }
        .map_err(|e| format!("disk preflight: {e}"))?;
    if free < needed {
        return Err(format!(
            "not enough disk space for the update: {free} bytes free, {needed} needed"
        ));
    }
    Ok(())
}

/// Authenticode. With `subject` (the stable channel): the signature must chain to a TRUSTED
/// root (`S_OK`) and the signing certificate's simple display name must equal it — the
/// publisher property that survives Azure's per-request leaf rotation (security-review
/// 2026-08-31 H-3). Without: valid embedded signature (untrusted root tolerated — canary/local
/// builds are still self-signed), signing-leaf SHA-256 ∈ `pins` when pins are present. The leaf
/// comes out of the same `WinVerifyTrust` state via `WTHelperGetProvSignerFromChain`.
/// (`pub(crate)`: the service supervisor's boot-loop rollback re-checks the cached previous
/// installer with it.)
pub(crate) fn verify_authenticode(
    path: &Path,
    pins: &[String],
    subject: Option<&str>,
) -> Result<(), String> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{CERT_E_UNTRUSTEDROOT, S_OK};
    use windows::Win32::Security::WinTrust::{
        WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        hFile: Default::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &file_info as *const _ as *mut _,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    // SAFETY: `data`, the `file_info` it points to, and the path's wide buffer all outlive
    // the call; `action` is a live mutable GUID. WinVerifyTrust reads the structs and stores
    // its state into `data.hWVTStateData`, released by the CLOSE call below.
    let status = unsafe {
        WinVerifyTrust(
            Default::default(),
            &mut action,
            &mut data as *mut _ as *mut core::ffi::c_void,
        )
    };
    let verdict = (|| {
        // A manifest that names the publisher demands the REAL chain verdict: `S_OK` means
        // "signed by a certificate chaining to a root this machine trusts". The untrusted-root
        // tolerance exists only for the subject-less legacy lanes — canary and local builds,
        // still self-signed (security-review 2026-08-31 H-3).
        let ok = status == S_OK.0 || (subject.is_none() && status == CERT_E_UNTRUSTEDROOT.0);
        if !ok {
            return Err(format!(
                "installer Authenticode signature is invalid{} (WinVerifyTrust 0x{status:08x})",
                if subject.is_some() {
                    " or does not chain to a trusted root"
                } else {
                    ""
                }
            ));
        }
        if pins.is_empty() && subject.is_none() {
            tracing::warn!(
                "update manifest carries no Authenticode subject or leaf pins — accepting on \
                 the manifest sha256 + signature validity alone"
            );
            return Ok(());
        }
        // Same-state leaf extraction: no second parse of the file.
        // SAFETY: `hWVTStateData` is the live verification state the VERIFY call above
        // populated (status checked OK); the returned pointer borrows that state, which stays
        // alive until the CLOSE call below, and is null-checked before use.
        let prov = unsafe { WTHelperProvDataFromStateData(data.hWVTStateData) };
        if prov.is_null() {
            return Err("WinVerifyTrust returned no provider state".into());
        }
        // SAFETY: `prov` was null-checked and borrows the same live verification state;
        // index 0 addresses the primary (only) signer, no counter-signer requested.
        let signer = unsafe { WTHelperGetProvSignerFromChain(prov, 0, false, 0) };
        if signer.is_null() {
            return Err("no signer in the Authenticode chain".into());
        }
        // SAFETY: `signer` was null-checked and borrows the live verification state; the
        // chain array is length/null-checked before indexing, and the CERT_CONTEXT borrows
        // the same state — every read of it happens before the CLOSE call below.
        let leaf = unsafe {
            let s = &*signer;
            if s.csCertChain == 0 || s.pasCertChain.is_null() {
                return Err("empty Authenticode cert chain".into());
            }
            // pasCertChain[0] is the SIGNING cert (leaf → root order).
            &*(*s.pasCertChain).pCert
        };
        if let Some(expected) = subject {
            use windows::Win32::Security::Cryptography::{
                CertGetNameStringW, CERT_CONTEXT, CERT_NAME_SIMPLE_DISPLAY_TYPE,
            };
            let leaf_ptr: *const CERT_CONTEXT = leaf;
            // The signing certificate's simple display name (its subject CN) — the publisher
            // value the Ed25519-signed manifest binds. Two-call pattern: required length
            // (including the NUL), then the string itself.
            // SAFETY: `leaf` borrows the live verification state checked above; the API only
            // reads the context on the length call.
            let len = unsafe {
                CertGetNameStringW(leaf_ptr, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None)
            } as usize;
            let mut buf = vec![0u16; len.max(1)];
            // SAFETY: same live context; `buf` is a valid mutable u16 buffer whose length the
            // slice itself carries.
            let n = unsafe {
                CertGetNameStringW(
                    leaf_ptr,
                    CERT_NAME_SIMPLE_DISPLAY_TYPE,
                    0,
                    None,
                    Some(&mut buf),
                )
            } as usize;
            let name = String::from_utf16_lossy(&buf[..n.saturating_sub(1)]);
            if !name.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "installer signing subject {name:?} does not match the manifest's {expected:?}"
                ));
            }
        }
        if !pins.is_empty() {
            // SAFETY: `pbCertEncoded`/`cbCertEncoded` describe the DER buffer owned by the live
            // cert context above; the slice is consumed (hashed) before the state is closed.
            let der = unsafe {
                std::slice::from_raw_parts(leaf.pbCertEncoded, leaf.cbCertEncoded as usize)
            };
            let fp = hex(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der).as_ref());
            if !pins.iter().any(|p| p.eq_ignore_ascii_case(&fp)) {
                return Err(format!(
                    "installer signing-leaf fingerprint {fp} matches none of the manifest's \
                     {} pin(s)",
                    pins.len()
                ));
            }
        }
        Ok(())
    })();

    // Always release the verification state.
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: same live `data`/`file_info`/`action` as the VERIFY call; CLOSE releases
    // `hWVTStateData`, after which no borrow of the state remains (the leaf/DER reads all
    // happened inside `verdict` above).
    unsafe {
        WinVerifyTrust(
            Default::default(),
            &mut action,
            &mut data as *mut _ as *mut core::ffi::c_void,
        )
    };
    verdict
}

/// Keep the freshly-verified target plus the newest previous installers; sweep the rest (and
/// any stale `.part`/`.bad` from other versions).
fn prune_installers(dir: &Path, keep_newest: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut exes: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != keep_newest)
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("punktfunk-host-setup-"))
                .unwrap_or(false)
        })
        .map(|p| {
            let t = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (t, p)
        })
        .collect();
    exes.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (_, p) in exes.into_iter().skip(KEEP_INSTALLERS - 1) {
        let _ = std::fs::remove_file(p);
    }
}

use std::os::windows::ffi::OsStrExt as _;
