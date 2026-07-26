//! The Windows matcher: a Toolhelp snapshot plus each process's full image path.
//!
//! Contract and rules live in [`super`]. Two differences from the Linux side shape everything here:
//!
//! * **The host is SYSTEM**, so it can open essentially any process. That makes rule 1 (never adopt a
//!   process that predates the launch) load-bearing rather than a nicety — without it a scan would
//!   happily adopt a copy of the game the player started an hour ago, and a session ending could kill
//!   it.
//! * **There is no launch reaper and no readable environment.** Steam's `SteamLaunch AppId=` argv
//!   trick is Linux-only, and reading another process's environment block on Windows needs
//!   `NtQueryInformationProcess` against an undocumented layout — not something to build a
//!   game-killing decision on. So the recipe is the image path: exactly the game's executable, or any
//!   executable under its install directory. Every Windows store the library scans reports one or
//!   both ([`crate::library::DetectSpec`]).

use super::{ProcRef, START_SLACK_SECS};
use crate::library::DetectSpec;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 100-nanosecond `FILETIME` ticks per second — the unit every Win32 time API here reports in.
const FILETIME_TICKS_PER_SEC: f64 = 10_000_000.0;

/// Image-path buffer, in UTF-16 units. Deliberately well past `MAX_PATH` (260): a game installed under
/// a deep library folder can exceed it, and a truncated path would silently fail to match. A path
/// longer than this makes `QueryFullProcessImageNameW` fail, which skips that process — the same
/// outcome as any other unreadable one.
const IMAGE_PATH_MAX: usize = 4096;

/// A process scanner over the live system. Unit (unlike the Linux one, which is parameterized for its
/// fixture tests): there is no way to hand Win32 a fake process table, so the Windows matching logic
/// is tested through its pure helpers ([`under_dir`], [`same_path`]) plus a live-process test.
pub struct Scanner;

impl Scanner {
    pub fn system() -> Self {
        Self
    }

    /// Seconds on the Windows process-start timeline — the `FILETIME` epoch, which is what
    /// `GetProcessTimes` reports a creation time in. `None` if the clock could not be read.
    ///
    /// Derived from `SystemTime` rather than a Win32 call because both are the same UTC epoch offset
    /// by a constant, and the only use is comparing against a creation time read moments later; a
    /// clock step between the two would need to exceed [`START_SLACK_SECS`] to matter.
    pub fn now_stamp(&self) -> Option<f64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        /// Seconds between the `FILETIME` epoch (1601-01-01) and the Unix epoch.
        const FILETIME_EPOCH_OFFSET_SECS: f64 = 11_644_473_600.0;
        Some(now.as_secs_f64() + FILETIME_EPOCH_OFFSET_SECS)
    }

    /// Every process matching any of `spec`'s signals, restricted to those that started at or after
    /// `min_start` (seconds on the [`Self::now_stamp`] timeline; `None` disables the filter).
    pub fn find(&self, spec: &DetectSpec, min_start: Option<f64>) -> Vec<ProcRef> {
        // Only the image-based signals exist on Windows: no reaper argv, no readable environment. A
        // spec carrying none must match *nothing* — falling through would scan on an empty predicate.
        if spec.exe.is_none() && spec.install_dir.is_none() && spec.process_name.is_none() {
            return Vec::new();
        }
        let exe = spec
            .exe
            .as_deref()
            .map(|e| e.canonicalize().unwrap_or_else(|_| e.to_path_buf()));
        let dir = spec
            .install_dir
            .as_deref()
            .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));

        let mut out = Vec::new();
        for pid in snapshot_pids() {
            // pid 0 (System Idle) and 4 (System) are neither openable nor ever a game.
            if pid <= 4 {
                continue;
            }
            let Some((start, image)) = process_start_and_image(pid) else {
                continue; // exited mid-scan, or a protected process we can't query — never a game
            };
            if let Some(min) = min_start {
                if start as f64 / FILETIME_TICKS_PER_SEC + START_SLACK_SECS < min {
                    continue; // predates this launch — never ours (rule 1)
                }
            }
            let hit = exe.as_deref().is_some_and(|w| same_path(&image, w))
                || dir.as_deref().is_some_and(|d| under_dir(&image, d))
                // The operator-supplied fallback: the image's file name alone. Windows paths are
                // case-insensitive anyway, so this matches the platform rather than relaxing anything.
                || spec
                    .process_name
                    .as_deref()
                    .is_some_and(|w| same_name(&image, w));
            if hit {
                out.push(ProcRef { pid, start });
            }
        }
        out
    }

    /// Which of `procs` are still the same live processes — pid present **and** creation time
    /// unchanged, so a recycled pid is never reported alive (rule 2). Windows reuses pids briskly, so
    /// this check is what makes signalling a remembered pid safe at all.
    pub fn alive(&self, procs: &[ProcRef]) -> Vec<ProcRef> {
        procs
            .iter()
            .copied()
            .filter(|p| process_start_and_image(p.pid).is_some_and(|(start, _)| start == p.start))
            .collect()
    }
}

/// An out-of-band opinion on whether the game is still running, used only to **veto** declaring it
/// gone (see [`super::running_hint`]).
///
/// For a Steam title: Steam records a per-app `Running` flag in the logged-in user's hive. That flag is
/// not trustworthy on its own — Steam sets it around updates and DLC installs too, and leaves it stale
/// if it crashes — but as a veto it is exactly right. If the matcher can't see the game (its executable
/// lives outside the install dir the manifest named, or a nested launcher owns it) while Steam still
/// says the app is running, ending the session would be a false positive the player feels immediately.
/// Erring towards "still running" only ever leaves a stream up.
///
/// Reads every **loaded** user hive under `HKEY_USERS` rather than resolving the interactive user's SID
/// through `WTSQueryUserToken`: only logged-in users' hives are loaded, which is the same set, and it
/// avoids a token dance for a best-effort hint.
pub fn steam_running_hint(appid: u32) -> Option<bool> {
    use winreg::enums::{HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    let users = RegKey::predef(HKEY_USERS);
    let mut saw_key = false;
    for sid in users.enum_keys().flatten() {
        // The `…_Classes` companion hives hold no Steam state.
        if sid.ends_with("_Classes") {
            continue;
        }
        let path = format!("{sid}\\Software\\Valve\\Steam\\Apps\\{appid}");
        let Ok(app) = users.open_subkey_with_flags(&path, KEY_READ) else {
            continue;
        };
        saw_key = true;
        if app.get_value::<u32, _>("Running").unwrap_or(0) != 0 {
            return Some(true);
        }
    }
    // A key that exists and says 0 is a real "not running"; no key at all means Steam has never run
    // this app on this box, which is no opinion rather than a negative one.
    saw_key.then_some(false)
}

/// Every pid in a Toolhelp snapshot. Mirrors the walk in [`crate::detect`]'s Windows facts, which
/// wants basenames rather than pids.
fn snapshot_pids() -> Vec<u32> {
    let mut out = Vec::new();
    // SAFETY: the canonical Toolhelp walk. `entry` is zeroed with `dwSize` set before the first read
    // (the 260-wide `szExeFile` array has no usable `Default`), and the snapshot handle is closed on
    // every exit path.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..std::mem::zeroed()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                out.push(entry.th32ProcessID);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// A process's creation time (`FILETIME` ticks) and full image path.
///
/// Opened with `PROCESS_QUERY_LIMITED_INFORMATION` — the least privilege that answers both questions,
/// and the one that works against elevated and protected-ish processes without asking for the right to
/// read their memory. `None` for anything that can't be opened or has already exited, which is the
/// routine case during a scan and never a reason to log.
fn process_start_and_image(pid: u32) -> Option<(u64, PathBuf)> {
    // SAFETY: `OpenProcess` yields an owned handle only on `Ok`; it is closed exactly once below on
    // every path. `GetProcessTimes` writes four `FILETIME`s we fully own; `QueryFullProcessImageNameW`
    // writes into `buf` and updates `len` in place, bounded by the `len` we pass in (buf.len()).
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = Default::default();
        let (mut exit, mut kernel, mut user) =
            (Default::default(), Default::default(), Default::default());
        let times =
            GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user).is_ok();

        let mut buf = [0u16; IMAGE_PATH_MAX];
        let mut len = buf.len() as u32;
        // `PROCESS_NAME_FORMAT(0)` is `PROCESS_NAME_WIN32` — a drive-letter path, which is what the
        // library's store-derived paths look like (the alternative, `PROCESS_NAME_NATIVE`, yields
        // `\Device\HarddiskVolume…` and would never compare equal to one).
        let named = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);

        if !times || !named {
            return None;
        }
        let start = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        let image = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
        Some((start, image))
    }
}

/// Is `image` the same file as `want`? Windows paths are case-insensitive, and the scanner compares a
/// canonicalized store-derived path against a live image path, so the comparison must be too.
fn same_path(image: &Path, want: &Path) -> bool {
    eq_ignore_case(image, want)
}

/// Does `image` live under `dir`? Case-insensitive, and requires a separator after the directory so an
/// install dir of `…\Games\X` is not satisfied by `…\Games\XY\game.exe`.
fn under_dir(image: &Path, dir: &Path) -> bool {
    let (i, d) = (wide_lower(image), wide_lower(dir));
    let Some(rest) = i.strip_prefix(d.as_str()) else {
        return false;
    };
    rest.starts_with('\\') || rest.starts_with('/')
}

/// Is `image`'s file name `want`? The operator-supplied [`DetectSpec::process_name`] fallback: a bare
/// name, compared against the image's last component only, so `Hades.exe` never matches a path that
/// merely contains it.
fn same_name(image: &Path, want: &str) -> bool {
    image
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(want.trim()))
}

fn eq_ignore_case(a: &Path, b: &Path) -> bool {
    wide_lower(a) == wide_lower(b)
}

/// Lowercase a path for comparison, normalizing the `\\?\` prefix `canonicalize` prepends (a
/// store-derived path canonicalizes to the verbatim form while a live image path does not, and the two
/// must still compare equal).
fn wide_lower(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_dir_match_is_case_insensitive_and_separator_aware() {
        let dir = Path::new(r"C:\Games\Hades");
        assert!(under_dir(Path::new(r"c:\games\hades\Hades.exe"), dir));
        assert!(under_dir(Path::new(r"C:\Games\Hades\bin\crash.exe"), dir));
        // A sibling whose name merely starts with the same text is not under it.
        assert!(!under_dir(Path::new(r"C:\Games\HadesII\game.exe"), dir));
        // The directory itself is not "under" itself (no process image is a bare directory anyway).
        assert!(!under_dir(dir, dir));
        assert!(!under_dir(Path::new(r"D:\Games\Hades\Hades.exe"), dir));
    }

    #[test]
    fn exe_match_is_case_insensitive_and_ignores_the_verbatim_prefix() {
        // `canonicalize` yields the `\\?\` verbatim form; a live image path never has it.
        assert!(same_path(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            Path::new(r"\\?\c:\games\hades\hades.exe")
        ));
        assert!(!same_path(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            Path::new(r"C:\Games\Hades\Other.exe")
        ));
    }

    #[test]
    fn a_spec_with_no_path_signal_matches_nothing() {
        // No reaper and no environment reading on Windows: an appid-only or env-only spec has nothing
        // to match here, and must not fall through to matching everything.
        let s = Scanner::system();
        assert!(s.find(&DetectSpec::steam(570), None).is_empty());
        assert!(s
            .find(
                &DetectSpec::default().with_env("HEROIC_APP_NAME", Some("Quail".into())),
                None
            )
            .is_empty());
        assert!(s.find(&DetectSpec::default(), None).is_empty());
    }

    /// The live check: scan the real process table for this test process itself, which is the only way
    /// to notice a wrong `PROCESSENTRY32W` size, a failing `OpenProcess` access mask, or creation
    /// times read from the wrong `FILETIME` half.
    #[test]
    fn finds_this_process_by_its_own_image_path() {
        let me = std::env::current_exe().expect("current exe");
        let s = Scanner::system();
        let pid = std::process::id();
        let found = s.find(&DetectSpec::exe(&me), None);
        assert!(
            found.iter().any(|p| p.pid == pid),
            "scanning the real process table did not find this test process ({pid}) as {}",
            me.display()
        );
        // Its own directory finds it too.
        let dir = me.parent().expect("exe has a parent");
        assert!(s
            .find(&DetectSpec::dir(dir), None)
            .iter()
            .any(|p| p.pid == pid));
        // The same process re-verifies as alive by (pid, creation time)…
        let mine: Vec<ProcRef> = found.into_iter().filter(|p| p.pid == pid).collect();
        assert_eq!(s.alive(&mine), mine);
        // …and a wrong creation time for the same pid does not.
        let recycled = vec![ProcRef {
            pid,
            start: mine[0].start ^ 0xFFFF,
        }];
        assert!(s.alive(&recycled).is_empty());
        // A launch reference in the future excludes it — rule 1, against a real creation time.
        let future = s.now_stamp().unwrap() + 3_600.0;
        assert!(s.find(&DetectSpec::exe(&me), Some(future)).is_empty());
    }
}
