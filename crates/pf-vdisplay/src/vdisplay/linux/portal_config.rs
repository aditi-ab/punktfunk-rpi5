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

/// Set `key` to `value` inside `block`, preserving every other line.
///
/// Three cases, all of which the tests pin: the block is absent (append it), the block has the key
/// (replace that one line, keeping its indentation), and the block lacks the key (insert before the
/// block ends).
pub(crate) fn upsert(existing: &str, block: Block<'_>, key: &str, value: &str) -> String {
    let sep = match block {
        Block::Ini(_) => "=",
        Block::Hyprlang(_) => " = ",
    };
    let assignment = |indent: &str| format!("{indent}{key}{sep}{value}");

    let lines: Vec<&str> = existing.lines().collect();
    let Some(open_at) = lines.iter().position(|l| opens(l, block)) else {
        // Absent: append the whole block, keeping the user's file intact above it.
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&match block {
            Block::Ini(name) => format!("[{name}]\n{}\n", assignment("")),
            Block::Hyprlang(name) => format!("{name} {{\n{}\n}}\n", assignment("    ")),
        });
        return out;
    };

    // Where the block ends: the next `[`-header for INI, the closing brace for hyprlang, else EOF.
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

    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    if let Some(i) = (open_at + 1..end_at).find(|&i| assigns(lines[i], key)) {
        let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
        out[i] = assignment(&indent);
    } else {
        let indent = match block {
            Block::Ini(_) => "",
            Block::Hyprlang(_) => "    ",
        };
        out.insert(end_at, assignment(indent));
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
pub(crate) fn ensure_key(path: &Path, block: Block<'_>, key: &str, value: &str) -> Result<bool> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = upsert(&existing, block, key, value);
    if updated == existing {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    // One-time backup. `create_new` makes this genuinely once: a later edit must not overwrite the
    // user's ORIGINAL with our own previous output.
    if !existing.is_empty() {
        let backup = path.with_extension("punktfunk-backup");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = f.write_all(existing.as_bytes());
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
    std::fs::write(path, &updated).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
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
        assert!(out.contains("[screencast]\nchooser_cmd=cat x"));
    }

    #[test]
    fn an_existing_key_is_replaced_in_place() {
        let user = "[screencast]\nchooser_type=simple\nchooser_cmd=OLD\noutput_name=DP-1\n";
        let out = upsert(user, Block::Ini("screencast"), "chooser_cmd", "NEW");
        assert!(out.contains("chooser_cmd=NEW"));
        assert!(!out.contains("OLD"));
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
        assert!(!out.contains("OLD"));
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
            "[screencast]\nk=v\n"
        );
    }
}
