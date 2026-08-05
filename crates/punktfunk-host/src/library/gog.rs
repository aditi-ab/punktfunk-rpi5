//! GOG Galaxy store provider: installed games from the Galaxy DB + play-task launch resolution. Split out of the `library` facade (plan §W5).

use super::art::cached_art;
use super::*;

/// Reads the GOG.com install registry + per-game `.info` files. Windows-only. Best-effort: empty
/// when GOG isn't installed.
#[cfg(windows)]
pub struct GogProvider;

#[cfg(windows)]
impl LibraryProvider for GogProvider {
    fn store(&self) -> &'static str {
        "gog"
    }

    fn list(&self) -> Vec<GameEntry> {
        gog_games()
    }
}

#[cfg(windows)]
fn gog_games() -> Vec<GameEntry> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    // 32-bit GOG writes under WOW6432Node; a 64-bit process reads the explicit path directly.
    let Ok(games_key) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SOFTWARE\\WOW6432Node\\GOG.com\\Games")
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sub in games_key.enum_keys().flatten() {
        // The subkey name IS the GOG product id.
        let Ok(k) = games_key.open_subkey(&sub) else {
            continue;
        };
        let Ok(path) = k.get_value::<String, _>("PATH") else {
            continue;
        };
        if !Path::new(&path).is_dir() {
            continue;
        }
        let title = k
            .get_value::<String, _>("GAMENAME")
            .unwrap_or_else(|_| sub.clone());
        // Resolve the primary play task (exe + args + workdir) from goggame-<id>.info; skip if absent.
        let Some((exe, args, workdir)) = gog_play_task(&path, &sub) else {
            continue;
        };
        let id = format!("gog:{sub}");
        // Art (public api.gog.com) is resolved off the hot path by the background warmer; read
        // whatever it has cached (title-only until warmed).
        let art = cached_art(&id).unwrap_or_default();
        // GOG launches the game's exe directly (no Galaxy), so the host owns the process and the
        // spec is only the fallback for a stub launcher that hands off; both signals are exact here.
        let detect = DetectSpec::exe(&exe).with_dir(&path);
        out.push(GameEntry {
            provider: None,
            role: GameRole::Game,
            meta: GameMeta::pc(),
            id,
            store: "gog".into(),
            title,
            art,
            launch: Some(LaunchSpec {
                kind: "gog".into(),
                value: format!("{exe}\t{args}\t{workdir}"),
            }),
            detect,
        });
    }
    out
}

/// Join a manifest-supplied relative path onto `install`, rejecting anything that could escape (or
/// replace) it: a drive prefix (`C:`), a root (`\`), or a `..` component — any of which `Path::join`
/// lets REPLACE or climb out of `install` on Windows. Keeps a crafted `goggame-*.info` from pointing
/// the play task's exe or working dir at an arbitrary program (security-review 2026-07-17). `None`
/// ⇒ the path is out of bounds and the caller refuses it.
#[cfg(windows)]
fn confined_join(install: &str, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rp = Path::new(rel);
    if rp.components().any(|c| {
        matches!(
            c,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return None;
    }
    Some(Path::new(install).join(rp))
}

/// The primary play task from `<install>\goggame-<id>.info`: `(absolute exe, args, working dir)`.
/// Prefers `isPrimary` + `FileTask`, else the first `FileTask`. Paths are resolved against `install`
/// and confined to it ([`confined_join`]).
#[cfg(windows)]
fn gog_play_task(install: &str, id: &str) -> Option<(String, String, String)> {
    let text =
        std::fs::read_to_string(Path::new(install).join(format!("goggame-{id}.info"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tasks = v.get("playTasks")?.as_array()?;
    let is_file =
        |t: &serde_json::Value| t.get("type").and_then(|s| s.as_str()) == Some("FileTask");
    let pick = tasks
        .iter()
        .find(|t| {
            t.get("isPrimary")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
                && is_file(t)
        })
        .or_else(|| tasks.iter().find(|t| is_file(t)))?;
    let rel = pick.get("path").and_then(|s| s.as_str())?;
    // Refuse the launch outright if the manifest's exe path escapes the install dir.
    let exe = confined_join(install, rel)?;
    let args = pick
        .get("arguments")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let workdir = pick
        .get("workingDir")
        .and_then(|s| s.as_str())
        // A working dir that escapes falls back to the install root (safe) rather than failing.
        .and_then(|w| confined_join(install, w))
        .unwrap_or_else(|| Path::new(install).to_path_buf());
    Some((
        exe.to_string_lossy().into_owned(),
        args,
        workdir.to_string_lossy().into_owned(),
    ))
}

// The `gog` launch mapping (`gog_spawn`) lives in `launch.rs` (WP1.1) — this module enumerates and
// resolves the spawn triple off disk, but turning that triple into a command line is launch-side.

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn gog_play_task_picks_primary_filetask() {
        let dir = std::env::temp_dir().join(format!("pf-gog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "1207658924";
        std::fs::write(
        dir.join(format!("goggame-{id}.info")),
        r#"{"playTasks":[
            {"isPrimary":false,"type":"FileTask","path":"other.exe"},
            {"isPrimary":true,"type":"FileTask","path":"bin\\game.exe","arguments":"-w","workingDir":"bin"}
        ]}"#,
    )
    .unwrap();
        let (exe, args, wd) = gog_play_task(&dir.to_string_lossy(), id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(exe.ends_with("bin\\game.exe"), "exe={exe}");
        assert_eq!(args, "-w");
        assert!(wd.ends_with("bin"), "wd={wd}");
    }
}
