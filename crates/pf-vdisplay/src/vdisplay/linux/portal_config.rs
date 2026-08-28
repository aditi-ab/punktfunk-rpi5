//! Editing the user's xdg-desktop-portal config files WITHOUT eating the rest of them.
//!
//! Both wlr-family backends need one key set in one block of a config the USER also owns:
//! `chooser_cmd` in xdpw's `[screencast]`, `custom_picker_binary` in xdph's `screencopy { … }`.
//! Both used to do it with a flat `std::fs::write` of a complete file, so anything else the user
//! had in `~/.config/xdg-desktop-portal-wlr/config` or `~/.config/hypr/xdph.conf` was destroyed on
//! first connect, silently and permanently.
//!
//! So: set the one key in place, keep every other line byte-for-byte, and take a one-time backup
//! the first time we touch a file we did not write. A merge is only worth doing if it cannot
//! corrupt what it merges into — hence the tests at the bottom, which are the point of this module.

use anyhow::{Context, Result};
use std::path::Path;

/// The two config grammars this crate writes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Block<'a> {
    /// INI: a `[name]` header, running to the next `[`-header or EOF. `key=value`.
    Ini(&'a str),
    /// hyprlang: `name {` … `}`. `key = value`, conventionally indented.
    Hyprlang(&'a str),
}

/// The comment we leave beside a key we took over, recording what the user had there:
/// `# punktfunk: previous <key> = <value>` (or `= (none)` when the key did not exist).
///
/// Why in the file rather than in our own state directory: this has to survive a SIGKILLed host,
/// a reboot (which empties `$XDG_RUNTIME_DIR`) and an uninstall that leaves the config behind, and
/// it has to be written atomically together with the change it describes. A sidecar state file
/// satisfies none of those; a comment in the same atomic write satisfies all three. It is also
/// legible: an operator reading their own `xdph.conf` can see exactly what we replaced and put it
/// back by hand.
const PRIOR: &str = "# punktfunk: previous";
/// What the marker records when the key was absent before we set it.
const PRIOR_NONE: &str = "(none)";

/// Is `line` the header that opens `block`?
fn opens(line: &str, block: Block<'_>) -> bool {
    let t = line.trim();
    match block {
        Block::Ini(name) => t == format!("[{name}]"),
        // `name {` — tolerate `name{` and extra spacing.
        Block::Hyprlang(name) => {
            t.strip_suffix('{').map(str::trim_end) == Some(name) || t == format!("{name} {{")
        }
    }
}

/// Does `line` assign `key`? Matches on the key alone so a changed VALUE is still recognised as
/// ours to replace (that is the whole point — the shim path moves with `$XDG_RUNTIME_DIR`).
fn assigns(line: &str, key: &str) -> bool {
    line.split('=').next().is_some_and(|lhs| lhs.trim() == key)
}

/// The span of `block` in `lines`: `(index of its header, index one past its last line)`.
/// `None` when the block is absent.
fn block_span(lines: &[&str], block: Block<'_>) -> Option<(usize, usize)> {
    let open_at = lines.iter().position(|l| opens(l, block))?;
    let end_at = lines
        .iter()
        .enumerate()
        .skip(open_at + 1)
        .find(|(_, l)| match block {
            Block::Ini(_) => l.trim_start().starts_with('['),
            Block::Hyprlang(_) => l.trim() == "}",
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    Some((open_at, end_at))
}

/// The value `key` currently holds in `block`, if it holds one.
pub(crate) fn current_value(existing: &str, block: Block<'_>, key: &str) -> Option<String> {
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    (open_at + 1..end_at)
        .find(|&i| assigns(lines[i], key))
        .map(|i| {
            lines[i]
                .split_once('=')
                .map_or("", |(_, v)| v)
                .trim()
                .to_string()
        })
}

/// What the user had at `key` before we took it over, read back from our marker comment:
/// `Some(Some(v))` = they had `v`, `Some(None)` = the key was absent, `None` = we never took it
/// over (so there is nothing of ours to undo, and the value there is genuinely theirs).
pub(crate) fn prior_value(existing: &str, block: Block<'_>, key: &str) -> Option<Option<String>> {
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    let want = format!("{PRIOR} {key} =");
    let raw = (open_at + 1..end_at)
        .map(|i| lines[i].trim())
        .find_map(|l| l.strip_prefix(&want))?
        .trim()
        .to_string();
    Some((raw != PRIOR_NONE).then_some(raw))
}

/// Undo our takeover of `key`: put the recorded prior value back (or delete the key when there was
/// none) and drop the marker. `None` when no marker is present — the file is not ours to touch.
///
/// Deliberately NOT "delete our line": D6 asks for the *prior value*, because on Omarchy that value
/// is `hyprland-preview-share-picker`, i.e. every browser share on the box. Deleting the key would
/// fall back to whatever xdph defaults to, which is not the same thing as what the user had.
pub(crate) fn restore(existing: &str, block: Block<'_>, key: &str) -> Option<String> {
    let prior = prior_value(existing, block, key)?;
    let lines: Vec<&str> = existing.lines().collect();
    let (open_at, end_at) = block_span(&lines, block)?;
    let marker = format!("{PRIOR} {key} =");
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let inside = i > open_at && i < end_at;
        if inside && line.trim().starts_with(&marker) {
            continue; // the marker itself goes away with the takeover it records
        }
        if inside && assigns(line, key) {
            // `Some` → put their line back with their indentation and the grammar's separator,
            // exactly as `upsert` wrote ours. `None` → there was no such key before us, so there
            // is none after us either: drop the line rather than blank it.
            if let Some(v) = &prior {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                let sep = match block {
                    Block::Ini(_) => "=",
                    Block::Hyprlang(_) => " = ",
                };
                out.push(format!("{indent}{key}{sep}{v}"));
            }
            continue;
        }
        out.push((*line).to_string());
    }
    let mut joined = out.join("\n");
    if existing.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    Some(joined)
}

/// Set `key` to `value` inside `block`, preserving every other line.
///
/// Three cases, all of which the tests pin: the block is absent (append it), the block has the key
/// (replace that one line, keeping its indentation), and the block lacks the key (insert before the
/// block ends).
///
/// The first time we replace a key we also leave a [`PRIOR`] marker recording what was there, so
/// [`restore`] can put it back after a crash, a reboot or an uninstall. Written once: a later edit
/// (the shim path moves with `$XDG_RUNTIME_DIR`) must not record OUR previous value as the user's.
pub(crate) fn upsert(existing: &str, block: Block<'_>, key: &str, value: &str) -> String {
    let sep = match block {
        Block::Ini(_) => "=",
        Block::Hyprlang(_) => " = ",
    };
    let assignment = |indent: &str| format!("{indent}{key}{sep}{value}");

    let lines: Vec<&str> = existing.lines().collect();
    let Some((open_at, end_at)) = block_span(&lines, block) else {
        // Absent: append the whole block, keeping the user's file intact above it. There was no
        // key here, so the marker records that — an uninstall must remove our line, not leave a
        // key the user never had.
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let marker = |indent: &str| format!("{indent}{PRIOR} {key} = {PRIOR_NONE}");
        out.push_str(&match block {
            Block::Ini(name) => format!("[{name}]\n{}\n{}\n", marker(""), assignment("")),
            Block::Hyprlang(name) => format!(
                "{name} {{\n{}\n{}\n}}\n",
                marker("    "),
                assignment("    ")
            ),
        });
        return out;
    };

    // Already marked? Then we have taken this key over before and the marker holds the USER's
    // value; re-recording here would overwrite it with our own previous shim path.
    let marked = prior_value(existing, block, key).is_some();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    if let Some(i) = (open_at + 1..end_at).find(|&i| assigns(lines[i], key)) {
        let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
        let had = lines[i]
            .split_once('=')
            .map_or("", |(_, v)| v)
            .trim()
            .to_string();
        out[i] = assignment(&indent);
        if !marked {
            out.insert(i, format!("{indent}{PRIOR} {key} = {had}"));
        }
    } else {
        let indent = match block {
            Block::Ini(_) => "",
            Block::Hyprlang(_) => "    ",
        };
        if !marked {
            out.insert(end_at, format!("{indent}{PRIOR} {key} = {PRIOR_NONE}"));
            out.insert(end_at + 1, assignment(indent));
        } else {
            out.insert(end_at, assignment(indent));
        }
    }
    let mut joined = out.join("\n");
    if existing.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Read `path`, set `key` in `block`, write it back — and back the original up ONCE, the first time
/// we touch a file we did not write. Returns `true` when the file changed (the caller restarts the
/// portal only then).
///
/// The read is matched EXPLICITLY, and only [`ErrorKind::NotFound`](std::io::ErrorKind::NotFound)
/// may mean "empty". This used to be `read_to_string(path).unwrap_or_default()`, which folded every
/// read failure into an empty string — and an empty string is the one input for which this function
/// destroys data: `upsert("")` yields a file holding ONLY our block, the backup below is skipped
/// because there is nothing to back up, and the write replaces the user's config. One non-UTF-8 byte
/// in a comment (a Latin-1 character, an 8-bit paste) or a transient EIO on an NFS/overlay config
/// dir was enough, and the result was exactly the silent, permanent loss this module exists to
/// prevent. A config we cannot read is a config we refuse to rewrite.
pub(crate) fn ensure_key(path: &Path, block: Block<'_>, key: &str, value: &str) -> Result<bool> {
    // Read BYTES: whether a backup is owed is a question about what is on disk, not about what
    // decoded — and the decode failure below is itself one of the cases that must not be silent.
    let raw = match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "read {} (refusing to rewrite a portal config we could not read)",
                    path.display()
                )
            })
        }
    };
    let existing = match &raw {
        Some(bytes) => std::str::from_utf8(bytes)
            .with_context(|| {
                format!(
                    "{} is not UTF-8 — refusing to rewrite it (the one key we own is not worth \
                     losing the rest of the file for; fix or move the file and reconnect)",
                    path.display()
                )
            })?
            .to_string(),
        None => String::new(),
    };
    let updated = upsert(&existing, block, key, value);
    if updated == existing {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    // One-time backup, of the bytes we actually read. `create_new` makes this genuinely once: a
    // later edit must not overwrite the user's ORIGINAL with our own previous output.
    if let Some(bytes) = raw.as_deref().filter(|b| !b.is_empty()) {
        let backup = path.with_extension("punktfunk-backup");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = f.write_all(bytes);
                tracing::info!(
                    backup = %backup.display(),
                    "backed up the existing portal config before editing it"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => tracing::warn!(
                backup = %backup.display(),
                error = %e,
                "could not back up the existing portal config; editing it anyway"
            ),
        }
    }
    write_atomic(path, updated.as_bytes())?;
    Ok(true)
}

/// Read `path` and hand back its text, or `None` when it does not exist. Any other read failure —
/// including non-UTF-8 — is an error for the same reason [`ensure_key`] spells out: a config we
/// cannot read is a config we refuse to rewrite.
fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            std::str::from_utf8(&bytes)
                .with_context(|| {
                    format!("{} is not UTF-8 — refusing to rewrite it", path.display())
                })?
                .to_string(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The value `key` holds in `path` today, and what the user had there before we took it over.
/// Both `None` on a file we have never touched (or that does not exist).
pub(crate) fn peek(
    path: &Path,
    block: Block<'_>,
    key: &str,
) -> (Option<String>, Option<Option<String>>) {
    let Ok(Some(text)) = read_config(path) else {
        return (None, None);
    };
    (
        current_value(&text, block, key),
        prior_value(&text, block, key),
    )
}

/// Undo our takeover of `key` in `path` — see [`restore`]. Returns `true` when the file changed.
///
/// A file with no marker of ours is left byte-for-byte alone and reports `false`: this must be safe
/// to call unconditionally (at teardown, from an uninstall script, after a crash) on a box where we
/// never touched the config at all.
pub(crate) fn restore_key(path: &Path, block: Block<'_>, key: &str) -> Result<bool> {
    let Some(existing) = read_config(path)? else {
        return Ok(false);
    };
    let Some(updated) = restore(&existing, block, key) else {
        return Ok(false);
    };
    if updated == existing {
        return Ok(false);
    }
    write_atomic(path, updated.as_bytes())?;
    Ok(true)
}

/// Replace `path`'s contents with `bytes` **atomically**: fill a temp file beside it, then rename
/// over it. `fs::write` truncates first and fills afterwards, so a crash, a full disk or a killed
/// host between the two leaves the user's config truncated — the same loss this module exists to
/// prevent, arrived at from the other side. The temp file goes in the SAME directory because a
/// rename is only atomic within one filesystem, and it inherits the original's permission bits so
/// an operator's 0600 config does not come back at the umask default.
///
/// A **symlinked** config is followed first, and that is not a nicety: `fs::write` opens the path
/// and therefore writes through the link, while `rename(2)` replaces the link itself. Individual
/// files under `~/.config` are symlinks into a dotfiles repo on every stow / chezmoi / home-manager
/// setup, so renaming over `~/.config/hypr/xdph.conf` would detach the user's repo — their next
/// `stow` reports a conflict or quietly reverts our key, and the connect after that writes it
/// again, forever. Following the link keeps this write byte-for-byte equivalent to the `fs::write`
/// it replaced, atomicity aside; it also makes the permission copy below sample the file the
/// rename actually lands on rather than one it was about to orphan.
///
/// The case this deliberately does NOT paper over: a link into a read-only target (home-manager
/// pointing at `/nix/store`). Following it fails the write, and the caller fails the connect with
/// the store path in the error — exactly as the pre-atomic `fs::write` did. Renaming over the link
/// instead would "work" by quietly detaching a declaratively managed file, which the user's next
/// `home-manager switch` refuses or reverts; a nix-managed config has to gain our key in the
/// user's flake, and a legible error is the only thing that tells them so.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let resolved = follow_link(path);
    let path = resolved.as_path();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    // Per-process name: two hosts editing the same config must not fill one another's temp file.
    let tmp = dir.join(format!(".{stem}.punktfunk-{}.tmp", std::process::id()));
    let write = || -> Result<()> {
        {
            let mut f =
                std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            // The rename must not publish a name whose contents are still in the page cache only.
            f.sync_all()
                .with_context(|| format!("sync {}", tmp.display()))?;
        } // closed before the rename — Windows is far happier renaming a file nobody holds open.
        if let Ok(md) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, md.permissions());
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    };
    let r = write();
    if r.is_err() {
        // Never leave a half-written dotfile beside the user's config.
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `path` with a symlink chain followed to the file it names, or `path` itself when it is not a
/// link (including when it does not exist yet — the ordinary first-connect case).
///
/// `symlink_metadata` rather than `metadata`, because the question is what `path` IS, not what it
/// points at. A **dangling** link is resolved by hand from its target text: `canonicalize` refuses
/// a target that does not exist, but `fs::write` through such a link creates it, and this write
/// stands in for that one.
fn follow_link(path: &Path) -> std::path::PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => std::fs::canonicalize(path)
            .or_else(|_| {
                std::fs::read_link(path).map(|target| {
                    if target.is_absolute() {
                        target
                    } else {
                        // A relative link is relative to the DIRECTORY holding it.
                        path.parent().unwrap_or_else(|| Path::new(".")).join(target)
                    }
                })
            })
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_block_is_appended_and_the_rest_survives() {
        let user = "[somethingelse]\nkeep=me\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "cat x");
        assert!(
            out.contains("[somethingelse]\nkeep=me"),
            "user content kept"
        );
        assert!(out.contains(
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x"
        ));
    }

    #[test]
    fn an_existing_key_is_replaced_in_place() {
        let user = "[screencast]\nchooser_type=simple\nchooser_cmd=OLD\noutput_name=DP-1\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "NEW");
        assert!(out.contains("chooser_cmd=NEW"));
        // OLD survives ONLY as the restore marker — no live assignment may still name it.
        assert!(!out.lines().any(|l| l.trim() == "chooser_cmd=OLD"));
        assert!(out.contains("# punktfunk: previous chooser_cmd = OLD"));
        assert!(out.contains("chooser_type=simple"), "sibling key kept");
        assert!(out.contains("output_name=DP-1"), "sibling key kept");
    }

    #[test]
    fn a_key_missing_from_an_existing_block_is_inserted_into_it() {
        let user = "[screencast]\nchooser_type=simple\n\n[other]\nx=1\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "cat x");
        let cmd = out.find("chooser_cmd").expect("inserted");
        let other = out.find("[other]").expect("kept");
        assert!(
            cmd < other,
            "must land inside [screencast], not after [other]"
        );
        assert!(out.contains("x=1"), "the later section survives");
    }

    #[test]
    fn hyprlang_blocks_are_edited_in_place_too() {
        let user = "misc {\n    keep = 1\n}\n\nscreencopy {\n    custom_picker_binary = OLD\n    allow_token_by_default = true\n}\n";
        let out = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/shim.sh",
        );
        assert!(out.contains("custom_picker_binary = /run/user/1000/shim.sh"));
        assert!(!out
            .lines()
            .any(|l| l.trim() == "custom_picker_binary = OLD"));
        assert!(out.contains("# punktfunk: previous custom_picker_binary = OLD"));
        assert!(
            out.contains("allow_token_by_default = true"),
            "sibling kept"
        );
        assert!(out.contains("misc {\n    keep = 1\n}"), "other block kept");
    }

    #[test]
    fn a_hyprlang_key_missing_from_its_block_lands_before_the_brace() {
        let user = "screencopy {\n    allow_token_by_default = true\n}\n";
        let out = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        let key = out.find("custom_picker_binary").expect("inserted");
        let brace = out.find('}').expect("kept");
        assert!(key < brace, "must land inside the block");
    }

    /// Idempotence is what keeps the caller from restarting the portal on every connect.
    #[test]
    fn a_second_pass_changes_nothing() {
        let once = upsert("", Block::Ini("screencast"), "chooser_cmd", "cat x");
        let twice = upsert(&once, Block::Ini("screencast"), "chooser_cmd", "cat x");
        assert_eq!(once, twice);
        let h1 = upsert(
            "",
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        let h2 = upsert(
            &h1,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/x",
        );
        assert_eq!(h1, h2);
    }

    /// An empty file must not gain a leading blank line.
    #[test]
    fn an_empty_file_yields_just_the_block() {
        assert_eq!(
            upsert("", Block::Ini("screencast"), "k", "v"),
            "[screencast]\n# punktfunk: previous k = (none)\nk=v\n"
        );
    }

    // ── the takeover is reversible (design D6) ─────────────────────────────────────────────────
    //
    // Omarchy ships its OWN `~/.config/hypr/xdph.conf` naming
    // `custom_picker_binary = hyprland-preview-share-picker` — the picker every Chromium share on
    // the box goes through. Taking that key over without a way back is not a cosmetic leftover: it
    // is "screen sharing stopped working on this machine" for as long as the config survives, i.e.
    // past a reboot and past an uninstall.

    /// The round trip that matters: their picker → ours → theirs, byte-identical.
    #[test]
    fn an_omarchy_picker_survives_the_round_trip() {
        let user = "screencopy {\n    allow_token_by_default = true\n    custom_picker_binary = hyprland-preview-share-picker\n}\n";
        let ours = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/pf-picker.sh",
        );
        assert!(ours.contains("custom_picker_binary = /run/user/1000/pf-picker.sh"));
        assert_eq!(
            prior_value(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            Some(Some("hyprland-preview-share-picker".to_string()))
        );
        let back = restore(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary")
            .expect("a file we took over is restorable");
        assert_eq!(
            back, user,
            "the user's file must come back exactly as it was"
        );
    }

    /// A second takeover (the shim path moves with `$XDG_RUNTIME_DIR`) must not record OUR path as
    /// theirs — that is how a restore puts back a dead runtime path instead of their picker.
    #[test]
    fn a_second_takeover_keeps_the_first_prior_value() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        let once = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/a",
        );
        let twice = upsert(
            &once,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/b",
        );
        assert!(twice.contains("custom_picker_binary = /run/b"));
        assert_eq!(
            restore(
                &twice,
                Block::Hyprlang("screencopy"),
                "custom_picker_binary"
            )
            .as_deref(),
            Some(user)
        );
    }

    /// When the key did not exist before us, restoring REMOVES it — putting an empty or defaulted
    /// value there would be a setting the user never had.
    #[test]
    fn a_key_we_invented_is_removed_on_restore_not_blanked() {
        let user = "screencopy {\n    allow_token_by_default = true\n}\n";
        let ours = upsert(
            user,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/a",
        );
        assert_eq!(
            prior_value(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            Some(None)
        );
        assert_eq!(
            restore(&ours, Block::Hyprlang("screencopy"), "custom_picker_binary").as_deref(),
            Some(user)
        );
    }

    /// Restoring a file we never touched must be a no-op, not a deletion — this runs at teardown
    /// on every Hyprland box, including ones whose config is entirely the user's.
    #[test]
    fn a_file_without_our_marker_is_not_ours_to_restore() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        assert_eq!(
            restore(user, Block::Hyprlang("screencopy"), "custom_picker_binary"),
            None
        );
        assert_eq!(restore("", Block::Ini("screencast"), "chooser_cmd"), None);
    }

    /// The INI half (xdpw) reverses identically — the two backends share this module precisely so
    /// a fix on one is not a fix on one.
    #[test]
    fn the_ini_grammar_reverses_too() {
        let user = "[screencast]\nchooser_type=simple\nchooser_cmd=slurp\noutput_name=DP-1\n";
        let ours = upsert(user, Block::Ini("screencast"), "chooser_cmd", "/run/a");
        assert_eq!(
            restore(&ours, Block::Ini("screencast"), "chooser_cmd").as_deref(),
            Some(user)
        );
    }

    #[test]
    fn current_value_reads_what_is_there_now() {
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        assert_eq!(
            current_value(user, Block::Hyprlang("screencopy"), "custom_picker_binary").as_deref(),
            Some("theirs")
        );
        assert_eq!(
            current_value(user, Block::Hyprlang("screencopy"), "nope"),
            None
        );
    }
}

/// [`ensure_key`] itself — the half that touches the user's disk.
///
/// The merge above was pinned by seven cases while the I/O wrapper around it, which is where the
/// destructive behaviour lives (the read, the once-only backup, the replacing write), had none. That
/// is backwards: `upsert` can at worst return a wrong string, `ensure_key` can delete a config.
/// Filesystem-only — no compositor, no portal — so these run on every platform, like the merge tests.
#[cfg(test)]
mod io_tests {
    use super::*;

    /// A scratch directory removed on drop. `tempfile` is deliberately not a dependency of this
    /// crate; the temp-dir + pid + counter convention is the one `proc.rs`'s fixtures already use.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("pf-vd-portalcfg-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn backup_of(p: &Path) -> std::path::PathBuf {
        p.with_extension("punktfunk-backup")
    }

    /// The data-loss case. A config that cannot be decoded must be left EXACTLY as it is: the old
    /// `unwrap_or_default()` turned it into an empty string, wrote a file holding only our block,
    /// skipped the backup (nothing to back up, as far as it could tell) and returned `Ok(true)`.
    #[test]
    fn a_non_utf8_config_is_refused_not_replaced() {
        let s = Scratch::new("nonutf8");
        let p = s.path("config");
        // A Latin-1 'ÿ' in a comment — the whole file is otherwise perfectly ordinary.
        let raw: &[u8] = b"[screencast]\n# r\xffgler\nchooser_type=simple\noutput_name=DP-1\n";
        std::fs::write(&p, raw).expect("seed");
        let err = ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x")
            .expect_err("an unreadable config must not be rewritten");
        assert!(
            format!("{err:#}").contains("not UTF-8"),
            "the error must name the real cause: {err:#}"
        );
        assert_eq!(
            std::fs::read(&p).expect("still there"),
            raw,
            "byte-identical"
        );
        assert!(
            !backup_of(&p).exists(),
            "nothing was edited, so nothing is owed a backup"
        );
    }

    /// The ordinary first-connect path: no file yet, so one is created — and there is no original
    /// to preserve, so no backup is left lying beside it.
    #[test]
    fn a_missing_file_is_created_without_a_backup() {
        let s = Scratch::new("missing");
        let p = s.path("nested").join("config");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write"));
        assert_eq!(
            std::fs::read_to_string(&p).expect("created"),
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x\n"
        );
        assert!(!backup_of(&p).exists());
    }

    /// The on-disk half of the D6 round trip, including the case that made it a hard requirement:
    /// an Omarchy box whose `xdph.conf` names their own share picker. After `restore_key` the file
    /// must be byte-identical to what they shipped.
    #[test]
    fn restore_key_puts_the_users_picker_back_byte_for_byte() {
        let s = Scratch::new("restore");
        let p = s.path("xdph.conf");
        let user = "screencopy {\n    allow_token_by_default = true\n    custom_picker_binary = hyprland-preview-share-picker\n}\n";
        std::fs::write(&p, user).expect("seed");
        let block = Block::Hyprlang("screencopy");
        assert!(
            ensure_key(&p, block, "custom_picker_binary", "/run/user/1000/pf.sh").expect("take")
        );
        assert_eq!(
            peek(&p, block, "custom_picker_binary"),
            (
                Some("/run/user/1000/pf.sh".to_string()),
                Some(Some("hyprland-preview-share-picker".to_string()))
            )
        );
        assert!(restore_key(&p, block, "custom_picker_binary").expect("restore"));
        assert_eq!(std::fs::read_to_string(&p).expect("restored"), user);
        // Idempotent, and safe to call on a file that is no longer ours.
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("second restore"));
        assert_eq!(std::fs::read_to_string(&p).expect("unchanged"), user);
    }

    /// Teardown calls this on every Hyprland box. A config that was never ours — and a config that
    /// does not exist — must come through untouched.
    #[test]
    fn restore_key_is_a_no_op_on_a_config_we_never_took_over() {
        let s = Scratch::new("restore-noop");
        let p = s.path("xdph.conf");
        let block = Block::Hyprlang("screencopy");
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("absent file"));
        assert!(!p.exists(), "restoring must not CREATE a config");
        let user = "screencopy {\n    custom_picker_binary = theirs\n}\n";
        std::fs::write(&p, user).expect("seed");
        assert!(!restore_key(&p, block, "custom_picker_binary").expect("not ours"));
        assert_eq!(std::fs::read_to_string(&p).expect("intact"), user);
    }

    /// `create_new` is what makes the backup once-only, and this is the invariant it buys: after a
    /// second edit (a new `$XDG_RUNTIME_DIR`, so a new value) the backup must still hold the user's
    /// PRISTINE file — not our own previous output.
    #[test]
    fn the_backup_holds_the_original_across_two_edits() {
        let s = Scratch::new("backup");
        let p = s.path("config");
        let pristine = "[screencast]\nchooser_type=simple\noutput_name=DP-1\n";
        std::fs::write(&p, pristine).expect("seed");
        assert!(
            ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat /run/a").expect("1st")
        );
        assert!(
            ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat /run/b").expect("2nd")
        );
        assert_eq!(
            std::fs::read_to_string(backup_of(&p)).expect("backup"),
            pristine
        );
        let now = std::fs::read_to_string(&p).expect("edited");
        assert!(
            now.contains("chooser_cmd=cat /run/b"),
            "the second value won"
        );
        assert!(
            now.contains("output_name=DP-1"),
            "the user's other keys survived"
        );
    }

    /// Idempotence at the I/O level: an already-correct file is not rewritten and reports `false`,
    /// because the caller RESTARTS the portal on `true` — a spurious `true` restarts xdpw/xdph on
    /// every connect.
    #[test]
    fn an_unchanged_file_returns_false_and_does_not_rewrite() {
        let s = Scratch::new("unchanged");
        let p = s.path("config");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("1st"));
        let after_first = std::fs::read_to_string(&p).expect("written");
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .expect("mtime");
        assert!(
            !ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("2nd"),
            "an unchanged config must report no change"
        );
        assert_eq!(
            std::fs::read_to_string(&p).expect("still there"),
            after_first
        );
        assert_eq!(
            std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .expect("mtime"),
            mtime,
            "the file must not have been touched at all"
        );
    }

    /// The write publishes the WHOLE new file or nothing (temp + rename), and it leaves no debris
    /// beside the config — a stray dotfile in `~/.config/hypr` is the kind of thing that outlives
    /// several releases.
    #[test]
    fn the_write_is_atomic_and_leaves_no_temp_behind() {
        let s = Scratch::new("atomic");
        let p = s.path("config");
        std::fs::write(&p, "[other]\nkeep=me\n").expect("seed");
        assert!(ensure_key(&p, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write"));
        let names: Vec<String> = std::fs::read_dir(&s.0)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".tmp")),
            "temp file left behind: {names:?}"
        );
        assert!(std::fs::read_to_string(&p)
            .expect("edited")
            .contains("keep=me"));
    }

    /// A user who manages dotfiles (stow, chezmoi, home-manager) has `~/.config/hypr/xdph.conf` as
    /// a SYMLINK into their repo. The edit has to land in the repo file with the link intact:
    /// `fs::write` followed the link, the temp-file + `rename` that replaced it does not, and a
    /// detached link is a config the user's tooling then fights us over on every connect.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_edited_through_the_link() {
        let s = Scratch::new("symlink");
        let repo = s.path("dotfiles");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let real = repo.join("xdph.conf");
        std::fs::write(
            &real,
            "screencopy {\n    allow_token_by_default = true\n}\n",
        )
        .expect("seed");
        let link = s.path("xdph.conf");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(ensure_key(
            &link,
            Block::Hyprlang("screencopy"),
            "custom_picker_binary",
            "/run/user/1000/shim.sh",
        )
        .expect("write"));

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the dotfiles link was replaced by a detached regular file"
        );
        let target = std::fs::read_to_string(&real).expect("the repo file");
        assert!(
            target.contains("custom_picker_binary = /run/user/1000/shim.sh"),
            "the edit never reached the repo file: {target}"
        );
        assert!(
            target.contains("allow_token_by_default = true"),
            "the user's own keys survived"
        );
    }

    /// The link may point at a file that does not exist yet (a repo checkout that has not been
    /// populated). `fs::write` created the target through it, so this must too — replacing the
    /// link would again detach it.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_written_through_to_its_target() {
        let s = Scratch::new("dangling");
        let repo = s.path("dotfiles");
        std::fs::create_dir_all(&repo).expect("repo dir");
        let real = repo.join("config");
        let link = s.path("config");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(
            ensure_key(&link, Block::Ini("screencast"), "chooser_cmd", "cat x").expect("write")
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the link was replaced instead of written through"
        );
        assert_eq!(
            std::fs::read_to_string(&real).expect("target created"),
            "[screencast]\n# punktfunk: previous chooser_cmd = (none)\nchooser_cmd=cat x\n"
        );
    }
}
