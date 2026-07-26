//! The Linux matcher: `/proc`.
//!
//! Contract, rules, and the shared vocabulary live in [`super`]; this module is only the reading of
//! `/proc` — and the parsing that gets wrong more often than it looks (`stat`'s `comm` field can
//! contain spaces *and* parentheses, so fields must be counted from the last `)`).

use super::{ProcRef, START_SLACK_SECS};
use crate::library::DetectSpec;
use std::path::{Path, PathBuf};

/// Largest `cmdline`/`environ` blob we will read from a single process. Both are bounded by `ARG_MAX`
/// in practice; the cap exists so a pathological process can't make the host allocate without bound
/// during a once-a-second scan.
const MAX_PROC_BLOB: u64 = 512 * 1024;

/// A `/proc` reader. Parameterized on its root, uid filter, and clock rate so the matching logic can
/// be unit-tested against a fixture tree instead of the live system.
pub struct Scanner {
    root: PathBuf,
    /// Only consider processes owned by this uid. `None` (tests only) considers all.
    uid: Option<u32>,
    ticks_per_sec: f64,
}

impl Scanner {
    /// The live system: `/proc`, this host process's own uid, the kernel's clock rate.
    pub fn system() -> Self {
        // SAFETY: a parameterless POSIX call that always succeeds and touches no memory.
        let uid = unsafe { libc::getuid() };
        // SAFETY: `sysconf` reads a static system limit by name; no memory of ours is involved, and a
        // non-positive answer (which the caller below handles) is its documented failure signal.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            root: PathBuf::from("/proc"),
            uid: Some(uid),
            // A non-positive sysconf answer would poison every start-time comparison; fall back to
            // the universal Linux value rather than divide by nonsense.
            ticks_per_sec: if ticks > 0 { ticks as f64 } else { 100.0 },
        }
    }

    /// Seconds since boot — the Linux process-start timeline (see [`super::launch_stamp`]). `None`
    /// when `/proc/uptime` can't be read, which disables start-time filtering rather than rejecting
    /// every candidate.
    pub fn now_stamp(&self) -> Option<f64> {
        let text = std::fs::read_to_string(self.root.join("uptime")).ok()?;
        text.split_whitespace().next()?.parse().ok()
    }

    /// Every process matching any of `spec`'s signals, restricted to those that started at or after
    /// `min_start` (seconds since boot; `None` disables the filter).
    ///
    /// The signals are a union: a Steam appid, an env marker, an exact executable, or an install
    /// directory each independently qualify a process. That is deliberate — the store with the
    /// sharpest signal wins, but a store that only knows its install directory is still covered.
    pub fn find(&self, spec: &DetectSpec, min_start: Option<f64>) -> Vec<ProcRef> {
        if spec.is_empty() {
            return Vec::new();
        }
        // Resolve symlinks in the install dir once per scan: a game reached through a symlinked
        // library folder would otherwise never prefix-match its own canonical image path.
        let dir = spec
            .install_dir
            .as_deref()
            .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));
        let exe = spec
            .exe
            .as_deref()
            .map(|e| e.canonicalize().unwrap_or_else(|_| e.to_path_buf()));
        let steam_tok = spec.steam_appid.map(|id| format!("AppId={id}"));

        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue; // not a pid dir
            };
            let dir_path = e.path();
            if let Some(uid) = self.uid {
                let owned = std::fs::metadata(&dir_path)
                    .map(|m| {
                        use std::os::unix::fs::MetadataExt;
                        m.uid() == uid
                    })
                    .unwrap_or(false);
                if !owned {
                    continue;
                }
            }
            let Some(start_ticks) = self.start_ticks(&dir_path) else {
                continue; // vanished mid-scan, or an unparseable stat
            };
            if let Some(min) = min_start {
                let started = start_ticks as f64 / self.ticks_per_sec;
                if started + START_SLACK_SECS < min {
                    continue; // predates this launch — never ours (rule 1)
                }
            }
            if self.matches(
                &dir_path,
                spec,
                steam_tok.as_deref(),
                exe.as_deref(),
                dir.as_deref(),
            ) {
                out.push(ProcRef {
                    pid,
                    start: start_ticks,
                });
            }
        }
        out
    }

    /// Which of `procs` are still the same live processes — pid present **and** start time unchanged,
    /// so a recycled pid is never reported alive (rule 2).
    pub fn alive(&self, procs: &[ProcRef]) -> Vec<ProcRef> {
        procs
            .iter()
            .copied()
            .filter(|p| self.start_ticks(&self.root.join(p.pid.to_string())) == Some(p.start))
            .collect()
    }

    /// Does this process match any of the spec's signals?
    fn matches(
        &self,
        dir_path: &Path,
        spec: &DetectSpec,
        steam_tok: Option<&str>,
        exe: Option<&Path>,
        install_dir: Option<&Path>,
    ) -> bool {
        // The process's own image, resolved through /proc/<pid>/exe. Absent for a kernel thread or a
        // process whose binary was replaced.
        let image = std::fs::read_link(dir_path.join("exe")).ok();
        if let Some(want) = exe {
            if image.as_deref() == Some(want) {
                return true;
            }
        }
        if let Some(dir) = install_dir {
            if image.as_deref().is_some_and(|i| i.starts_with(dir)) {
                return true;
            }
        }
        // A bare executable name, the operator-supplied fallback. Case-insensitive because it is typed
        // by hand and the cost of a case mismatch (the game is never recognized) far outweighs the
        // cost of a case collision (two differently-cased binaries, both started since this launch).
        if let Some(want) = spec.process_name.as_deref() {
            let named = image
                .as_deref()
                .and_then(|i| i.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(want));
            if named {
                return true;
            }
        }

        // The command line covers what the image can't: a Proton/Wine title's image is the *runtime*
        // (`…/proton`, `wine64-preloader`), and the game only appears as an argument; Steam's launch
        // reaper is likewise identified by its argv, not its binary.
        let cmdline = read_capped(&dir_path.join("cmdline"));
        if let Some(cmdline) = cmdline.as_deref() {
            if let Some(tok) = steam_tok {
                // Both tokens together, exact-matched, so `AppId=57` never satisfies appid 570 and
                // Steam's own (non-reaper) helper steps aren't mistaken for the game.
                let mut launch = false;
                let mut appid = false;
                for arg in cmdline.split(|&b| b == 0) {
                    if arg == b"SteamLaunch" {
                        launch = true;
                    } else if arg == tok.as_bytes() {
                        appid = true;
                    }
                }
                if launch && appid {
                    return true;
                }
            }
            if let Some(dir) = install_dir {
                // Require a path separator after the directory, so an install dir of `/games/x` is
                // not satisfied by an unrelated `/games/xyz/…` argument. (The image-path check above
                // needs no such care — `Path::starts_with` already compares whole components.)
                let needle = dir.as_os_str().as_encoded_bytes();
                let under_dir = |arg: &[u8]| {
                    arg.strip_prefix(needle)
                        .is_some_and(|rest| rest.first() == Some(&b'/'))
                };
                if cmdline.split(|&b| b == 0).any(under_dir) {
                    return true;
                }
            }
            if let Some(want) = exe {
                let needle = want.as_os_str().as_encoded_bytes();
                if cmdline.split(|&b| b == 0).any(|arg| arg == needle) {
                    return true;
                }
            }
        }

        // Env markers last: reading another process's environment is the most invasive of these
        // reads, so it only happens for a spec that actually asked for one. The contents are matched
        // and discarded — never logged.
        if let Some(marker) = &spec.env_marker {
            if let Some(env) = read_capped(&dir_path.join("environ")) {
                let want: Vec<u8> = match &marker.value {
                    Some(v) => format!("{}={v}", marker.key).into_bytes(),
                    None => format!("{}=", marker.key).into_bytes(),
                };
                let hit = env.split(|&b| b == 0).any(|kv| match marker.value {
                    // An exact `KEY=VALUE` entry.
                    Some(_) => kv == want.as_slice(),
                    // Presence of the key with any value.
                    None => kv.starts_with(want.as_slice()),
                });
                if hit {
                    return true;
                }
            }
        }
        false
    }

    /// `/proc/<pid>/stat` field 22 (`starttime`). The `comm` field is parenthesized and may itself
    /// contain spaces and parentheses, so fields are counted from the **last** `)`: the token right
    /// after it is field 3 (`state`), which puts `starttime` at index 19 of the remainder.
    fn start_ticks(&self, dir_path: &Path) -> Option<u64> {
        let stat = std::fs::read_to_string(dir_path.join("stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 1..];
        tail.split_whitespace().nth(19)?.parse().ok()
    }
}

/// Read a `/proc` blob with a hard size cap (see [`MAX_PROC_BLOB`]). `None` when the process vanished
/// or the file is unreadable — both routine during a scan.
fn read_capped(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_PROC_BLOB).read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::DetectSpec;

    /// Build a fake `/proc` tree. Each process gets a `stat` (with a deliberately nasty `comm`), and
    /// optionally a `cmdline`, an `environ`, and an `exe` symlink.
    struct FakeProc {
        pid: u32,
        start: u64,
        exe: Option<PathBuf>,
        cmdline: Vec<&'static str>,
        environ: Vec<&'static str>,
    }

    impl FakeProc {
        fn new(pid: u32, start: u64) -> Self {
            Self {
                pid,
                start,
                exe: None,
                cmdline: Vec::new(),
                environ: Vec::new(),
            }
        }
        fn exe(mut self, p: impl Into<PathBuf>) -> Self {
            self.exe = Some(p.into());
            self
        }
        fn cmdline(mut self, args: &[&'static str]) -> Self {
            self.cmdline = args.to_vec();
            self
        }
        fn environ(mut self, kvs: &[&'static str]) -> Self {
            self.environ = kvs.to_vec();
            self
        }
    }

    fn fake_proc_root(uptime: f64, procs: &[FakeProc]) -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        std::fs::write(td.path().join("uptime"), format!("{uptime} 1000.0\n")).unwrap();
        // Non-pid entries must be skipped, not parsed.
        std::fs::create_dir_all(td.path().join("self")).unwrap();
        std::fs::write(td.path().join("cmdline"), b"").unwrap();
        for p in procs {
            let dir = td.path().join(p.pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            // `stat` is `pid (comm) state ppid … starttime …`, with starttime as field 22. Counting
            // from the last ')', index 0 is `state` (field 3), so starttime lands at index 19. The
            // comm is hostile on purpose — a space and a close-paren inside the parens, which is
            // exactly what naive field-splitting gets wrong.
            let mut tail = vec!["0".to_string(); 20];
            tail[0] = "S".to_string(); // field 3
            tail[19] = p.start.to_string(); // field 22
            std::fs::write(
                dir.join("stat"),
                format!("{} (evil ) name) {}\n", p.pid, tail.join(" ")),
            )
            .unwrap();
            if !p.cmdline.is_empty() {
                let mut blob = Vec::new();
                for a in &p.cmdline {
                    blob.extend_from_slice(a.as_bytes());
                    blob.push(0);
                }
                std::fs::write(dir.join("cmdline"), blob).unwrap();
            }
            if !p.environ.is_empty() {
                let mut blob = Vec::new();
                for a in &p.environ {
                    blob.extend_from_slice(a.as_bytes());
                    blob.push(0);
                }
                std::fs::write(dir.join("environ"), blob).unwrap();
            }
            if let Some(exe) = &p.exe {
                std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
            }
        }
        td
    }

    fn scanner(root: &Path) -> Scanner {
        Scanner {
            root: root.to_path_buf(),
            uid: None, // the fixture's owner is the test user; don't couple the test to it
            ticks_per_sec: 100.0,
        }
    }

    fn pids(mut v: Vec<ProcRef>) -> Vec<u32> {
        v.sort_by_key(|p| p.pid);
        v.into_iter().map(|p| p.pid).collect()
    }

    #[test]
    fn empty_spec_matches_nothing() {
        let td = fake_proc_root(100.0, &[FakeProc::new(1, 100).exe("/games/x/run")]);
        let s = scanner(td.path());
        assert!(s.find(&DetectSpec::default(), None).is_empty());
    }

    #[test]
    fn matches_exact_exe_and_install_dir() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(10, 50_000).exe("/games/hades/Hades"),
                FakeProc::new(11, 50_000).exe("/games/hades/tools/crash.bin"),
                FakeProc::new(12, 50_000).exe("/usr/bin/firefox"),
            ],
        );
        let s = scanner(td.path());
        // Exact exe → only that process.
        assert_eq!(
            pids(s.find(&DetectSpec::exe("/games/hades/Hades"), None)),
            vec![10]
        );
        // Install dir → every process running out of the game's tree, but nothing outside it.
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/hades"), None)),
            vec![10, 11]
        );
    }

    /// The operator-supplied fallback ([`DetectSpec::process_name`]): the image's own file name and
    /// nothing else — never a directory that happens to be called the same, and never a process whose
    /// path merely contains the name.
    #[test]
    fn matches_a_bare_process_name_against_the_image_name_only() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(20, 50_000).exe("/opt/retroarch/bin/RetroArch"),
                FakeProc::new(21, 50_000).exe("/usr/bin/retroarch-assets-helper"),
                FakeProc::new(22, 50_000).exe("/home/p/retroarch/launcher.sh"),
            ],
        );
        let s = scanner(td.path());
        let spec = DetectSpec {
            process_name: Some("retroarch".into()),
            ..Default::default()
        };
        // Case-insensitive (the name is typed by hand), but a whole-name match: neither the
        // longer-named helper nor the script living in a `retroarch/` directory qualifies.
        assert_eq!(pids(s.find(&spec, None)), vec![20]);
    }

    #[test]
    fn install_dir_does_not_match_a_sibling_with_the_same_prefix() {
        // `/games/x` must not adopt a process running out of `/games/xyz` — a real hazard when one
        // library folder's name is a prefix of another's.
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(90, 50_000).exe("/games/xyz/other"),
                FakeProc::new(91, 50_000).cmdline(&["wrapper", "/games/xyz/other"]),
            ],
        );
        let s = scanner(td.path());
        assert!(s.find(&DetectSpec::dir("/games/x"), None).is_empty());
        // The genuine directory still matches, by image path and by argument.
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/xyz"), None)),
            vec![90, 91]
        );
    }

    #[test]
    fn matches_proton_style_game_only_in_the_cmdline() {
        // The image is the Proton runtime; the game appears only as an argument.
        let td = fake_proc_root(
            1000.0,
            &[FakeProc::new(20, 50_000)
                .exe("/steam/runtime/proton")
                .cmdline(&["proton", "waitforexitandrun", "/games/elden/eldenring.exe"])],
        );
        let s = scanner(td.path());
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/elden"), None)),
            vec![20]
        );
    }

    #[test]
    fn matches_steam_reaper_exactly() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(30, 50_000).cmdline(&[
                    "reaper",
                    "SteamLaunch",
                    "AppId=570",
                    "--",
                    "dota",
                ]),
                // A different appid that shares a prefix must NOT match (AppId=57 vs 570).
                FakeProc::new(31, 50_000).cmdline(&[
                    "reaper",
                    "SteamLaunch",
                    "AppId=57",
                    "--",
                    "other",
                ]),
                // `SteamLaunch` without the appid, and the appid without `SteamLaunch`, are both no.
                FakeProc::new(32, 50_000).cmdline(&["reaper", "SteamLaunch", "--", "shader"]),
                FakeProc::new(33, 50_000).cmdline(&["something", "AppId=570"]),
            ],
        );
        let s = scanner(td.path());
        assert_eq!(pids(s.find(&DetectSpec::steam(570), None)), vec![30]);
        assert_eq!(pids(s.find(&DetectSpec::steam(57), None)), vec![31]);
    }

    #[test]
    fn matches_env_marker_by_exact_value_or_presence() {
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(40, 50_000).environ(&["HOME=/home/u", "HEROIC_APP_NAME=Quail"]),
                FakeProc::new(41, 50_000).environ(&["HEROIC_APP_NAME=OtherGame"]),
                FakeProc::new(42, 50_000).environ(&["HOME=/home/u"]),
            ],
        );
        let s = scanner(td.path());
        let exact = DetectSpec::default().with_env("HEROIC_APP_NAME", Some("Quail".to_string()));
        assert_eq!(pids(s.find(&exact, None)), vec![40]);
        // Presence-only matches any value, but still nothing that lacks the key.
        let any = DetectSpec::default().with_env("HEROIC_APP_NAME", None);
        assert_eq!(pids(s.find(&any, None)), vec![40, 41]);
    }

    #[test]
    fn never_adopts_a_process_that_predates_the_launch() {
        // Uptime 1000 s; the launch happened at t=900. pid 50 started at t=500 (a copy the player
        // already had open), pid 51 at t=950 (ours).
        let td = fake_proc_root(
            1000.0,
            &[
                FakeProc::new(50, 50_000).exe("/games/hades/Hades"),
                FakeProc::new(51, 95_000).exe("/games/hades/Hades"),
            ],
        );
        let s = scanner(td.path());
        let spec = DetectSpec::dir("/games/hades");
        assert_eq!(pids(s.find(&spec, Some(900.0))), vec![51]);
        // Without the filter both are visible — proving the filter is what excluded it.
        assert_eq!(pids(s.find(&spec, None)), vec![50, 51]);
    }

    #[test]
    fn start_slack_tolerates_tick_granularity() {
        // Started a hair (0.5 s) BEFORE the recorded launch instant: still ours.
        let td = fake_proc_root(1000.0, &[FakeProc::new(60, 89_950).exe("/games/x/run")]);
        let s = scanner(td.path());
        assert_eq!(
            pids(s.find(&DetectSpec::dir("/games/x"), Some(900.0))),
            vec![60]
        );
        // A full minute early is not.
        assert!(s.find(&DetectSpec::dir("/games/x"), Some(960.0)).is_empty());
    }

    #[test]
    fn alive_rejects_a_recycled_pid() {
        let td = fake_proc_root(1000.0, &[FakeProc::new(70, 50_000).exe("/games/x/run")]);
        let s = scanner(td.path());
        let same = ProcRef {
            pid: 70,
            start: 50_000,
        };
        let recycled = ProcRef {
            pid: 70,
            start: 99_999,
        };
        let gone = ProcRef {
            pid: 71,
            start: 50_000,
        };
        assert_eq!(s.alive(&[same]), vec![same]);
        assert!(s.alive(&[recycled]).is_empty());
        assert!(s.alive(&[gone]).is_empty());
    }

    /// Everything above runs against a fixture tree, which proves the parsing but not that the
    /// parsing describes this kernel. This one scans the **real** `/proc` for a process it just
    /// started — the only version of this test that would notice `/proc/<pid>/stat` field ordering,
    /// the uid check, or the uptime clock being wrong on a real box.
    #[test]
    fn finds_a_real_process_it_just_started() {
        // A real game's *binary* lives under its install dir, which is what makes the install-dir
        // recipe work. So the stand-in must too: copy a long-running binary into the directory and run
        // it from there. (A wrapper script that `exec`s something outside the directory would leave no
        // trace of the directory in either the image path or the command line — worth knowing, and the
        // reason a copied binary is the honest fixture here.)
        //
        // It has to keep the name `sleep`. Modern coreutils (uutils on Ubuntu 25.10+, busybox
        // elsewhere) is a MULTI-CALL binary that dispatches on `argv[0]`: copied to any other name it
        // prints "unknown program" and exits instantly, leaving nothing in `/proc` to find — which
        // looks exactly like the matcher being broken. That cost a CI red, green on a distro with a
        // standalone `sleep` and failing on one without.
        let td = tempfile::tempdir().expect("tempdir");
        let game = td.path().join("sleep");
        std::fs::copy("/bin/sleep", &game).expect("copy a stand-in game binary");
        let s = Scanner::system();
        let before = s.now_stamp().expect("real /proc/uptime is readable");

        let mut child = std::process::Command::new(&game)
            .arg("20")
            .spawn()
            .expect("spawn the fake game");
        // Give it a moment to be visible in /proc.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let found = s.find(&DetectSpec::dir(td.path()), Some(before));
        assert!(
            found.iter().any(|p| p.pid == child.id()),
            "scanning the real /proc did not find pid {} under {}; found {found:?}",
            child.id(),
            td.path().display()
        );
        // The same process is still alive when re-verified by (pid, start time).
        assert!(!s.alive(&found).is_empty());
        // The exact-exe recipe finds it too, on the same live process.
        assert!(s
            .find(&DetectSpec::exe(&game), Some(before))
            .iter()
            .any(|p| p.pid == child.id()));

        // A launch reference AFTER this process started must exclude it — the rule that keeps a
        // pre-existing copy of a game from being adopted, checked against a real start time.
        let after = s.now_stamp().unwrap() + 60.0;
        assert!(s.find(&DetectSpec::dir(td.path()), Some(after)).is_empty());

        let _ = child.kill();
        let _ = child.wait();
        // Once it is gone and reaped, neither the scan nor the liveness re-check sees it.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(s.find(&DetectSpec::dir(td.path()), Some(before)).is_empty());
        assert!(s.alive(&found).is_empty());
    }

    #[test]
    fn parses_uptime_and_hostile_comm() {
        let td = fake_proc_root(4321.5, &[FakeProc::new(80, 1234)]);
        let s = scanner(td.path());
        assert_eq!(s.now_stamp(), Some(4321.5));
        // The comm field contains a space and a ')' — start time must still parse.
        assert_eq!(s.start_ticks(&td.path().join("80")), Some(1234));
    }
}
