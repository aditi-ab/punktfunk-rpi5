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
        out.push(GameEntry {
            provider: None,
            id,
            store: "gog".into(),
            title,
            art,
            launch: Some(LaunchSpec {
                kind: "gog".into(),
                value: format!("{exe}\t{args}\t{workdir}"),
            }),
        });
    }
    out
}

/// The primary play task from `<install>\goggame-<id>.info`: `(absolute exe, args, working dir)`.
/// Prefers `isPrimary` + `FileTask`, else the first `FileTask`. Paths are resolved against `install`.
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
    let exe = Path::new(install).join(rel);
    let args = pick
        .get("arguments")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let workdir = pick
        .get("workingDir")
        .and_then(|s| s.as_str())
        .map(|w| Path::new(install).join(w))
        .unwrap_or_else(|| Path::new(install).to_path_buf());
    Some((
        exe.to_string_lossy().into_owned(),
        args,
        workdir.to_string_lossy().into_owned(),
    ))
}

/// Build the spawn `(command line, working dir)` for a `gog` launch value (`exe \t args \t workdir`,
/// all host-resolved from the operator's own disk). Direct exe — no shell, no Galaxy.
#[cfg(windows)]
pub(crate) fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    let mut parts = value.split('\t');
    let exe = parts.next().filter(|s| !s.is_empty())?;
    let args = parts.next().unwrap_or("");
    let workdir = parts.next().filter(|s| !s.is_empty()).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn gog_spawn_parses_and_guards() {
        let (cmd, wd) = gog_spawn("C:\\Games\\W3\\witcher3.exe\t--skip\tC:\\Games\\W3").unwrap();
        assert_eq!(cmd, "\"C:\\Games\\W3\\witcher3.exe\" --skip");
        assert_eq!(wd, Some(std::path::PathBuf::from("C:\\Games\\W3")));
        let (cmd2, wd2) = gog_spawn("C:\\g.exe").unwrap();
        assert_eq!(cmd2, "\"C:\\g.exe\"");
        assert!(wd2.is_none());
        assert!(gog_spawn("").is_none());
    }

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
