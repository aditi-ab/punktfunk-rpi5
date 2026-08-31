//! Punktfunk in the Omarchy menu (Super+Space): the CLIENT's rows.
//!
//! One managed block in `~/.config/omarchy/extensions/omarchy-menu.jsonc` — a root
//! `punktfunk` submenu, "Open Punktfunk", the couch console, and one connect row per saved
//! host (plus a wake row where a MAC is known), so typing a host's name in the menu's search
//! and pressing enter starts the stream. Reusing the same `"punktfunk"` root id the HOST's
//! `punktfunk-omarchy setup` writes is deliberate and documented by the extension format
//! itself ("reuse an existing id to override/extend it"): on a box running both, the two
//! blocks merge into one submenu.
//!
//! The menu cannot generate rows itself — its `provider` field resolves only against a map
//! baked into the shell (verified in `Menu.qml`, Omarchy 4.0.1) — so the rows are STATIC and
//! this module keeps them true instead: [`sync_if_enabled`] rewrites the block from the
//! known-hosts store, and `trust::KnownHosts::save` calls it, so pairing a host in ANY of
//! our binaries updates the menu, and `omarchy-menu refresh` repaints an open shell.
//!
//! Opt-in, like every Omarchy edit we make: nothing is written until the operator flips the
//! preferences switch or runs `punktfunk-client --omarchy-menu on`, and `off` removes exactly
//! our block. The editing rules mirror `packaging/linux/omarchy/punktfunk-omarchy`'s
//! `setup_menu`, traps included: the file is a SINGLE document where one parse error drops
//! every row the user owns, so edits are validated on a copy and an unparsable file is left
//! completely alone — it is not ours to repair.

use crate::trust::KnownHosts;
use std::path::{Path, PathBuf};

const BEGIN: &str =
    "// >>> punktfunk-client (managed by punktfunk-client --omarchy-menu — do not edit between these markers)";
const END: &str = "// <<< punktfunk-client";

/// Where the Omarchy menu reads extensions. `None` when no home is resolvable at all.
fn menu_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("omarchy/extensions/omarchy-menu.jsonc"))
}

/// Is our block installed? This is the consent bit everything else keys off.
pub fn enabled() -> bool {
    menu_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|t| t.contains(BEGIN))
}

/// Install (or refresh) the block from the current known-hosts store.
pub fn enable() -> Result<(), String> {
    let p = menu_path().ok_or("no home directory")?;
    write_rows(&p, Some(&rows_for(&KnownHosts::load())))
}

/// Remove exactly our block; everything else in the file is untouched.
pub fn disable() -> Result<(), String> {
    let Some(p) = menu_path() else { return Ok(()) };
    if !p.exists() {
        return Ok(());
    }
    write_rows(&p, None)
}

/// Bring the block in line with the store, when the operator opted in — the quiet form
/// `trust::KnownHosts::save` calls, so a pairing made anywhere lands in the menu.
pub fn sync_if_enabled() {
    if !enabled() {
        return;
    }
    if let Err(e) = enable() {
        tracing::warn!("omarchy menu sync: {e}");
    }
}

/// Rewrite the file with our block replaced by `rows` (removed, for `None`). Validated on a
/// copy first, both before and after: a file that does not parse is not touched, and an edit
/// that would not parse is not written.
fn write_rows(path: &Path, rows: Option<&str>) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{\n}\n".to_string(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !jsonc_valid(&text) {
        return Err(format!(
            "{} does not parse as JSONC — fix it, then re-run",
            path.display()
        ));
    }
    let stripped = strip_block(&text);
    let next = match rows {
        Some(r) => insert_block(&stripped, r)
            .ok_or_else(|| format!("could not find the closing brace in {}", path.display()))?,
        None => stripped,
    };
    if !jsonc_valid(&next) {
        return Err("the edit would not have parsed — your file is untouched".to_string());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    crate::trust::write_atomic(path, next.as_bytes()).map_err(|e| e.to_string())?;
    refresh();
    Ok(())
}

/// Repaint an open shell, best-effort. Headless (ssh) has no shell to answer, and a failed
/// spawn is not an error: the file is the source of truth and the next shell start reads it.
fn refresh() {
    let mut cmd = std::process::Command::new("omarchy-menu");
    // The shell-IPC wrapper wants OMARCHY_PATH and a login session exports it; a plain shell
    // (ssh, a bare TTY) does not, and the wrapper dies on the missing var rather than on the
    // missing shell. Seed the packaged default so refresh works from anywhere the shell runs.
    if std::env::var_os("OMARCHY_PATH").is_none() {
        cmd.env("OMARCHY_PATH", "/usr/share/omarchy");
    }
    let _ = cmd
        .arg("refresh")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Drop a previous block of ours — begin marker through end marker, inclusive.
fn strip_block(text: &str) -> String {
    let mut out = Vec::new();
    let mut skip = false;
    for l in text.lines() {
        if l.contains(BEGIN) {
            skip = true;
        }
        if skip {
            if l.contains(END) {
                skip = false;
            }
            continue;
        }
        out.push(l);
    }
    let mut s = out.join("\n");
    if text.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Insert the block before the document's LAST closing brace — same rule as the host tool.
fn insert_block(text: &str, rows: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let close = lines.iter().rposition(|l| l.trim() == "}")?;
    let mut out: Vec<&str> = lines[..close].to_vec();
    out.push(BEGIN);
    let rows = rows.trim_end();
    out.extend(rows.lines());
    out.push(END);
    out.extend(&lines[close..]);
    let mut s = out.join("\n");
    s.push('\n');
    Some(s)
}

/// Does the file parse as the JSONC the menu reads? Refusal is the safe direction: a file
/// that fails here is left completely alone.
fn jsonc_valid(text: &str) -> bool {
    jsonc_parse(text).is_some()
}

/// The parse behind [`jsonc_valid`]: strip `//` comments, forgive trailing commas, then
/// `serde_json`. Both transforms are STRING-AWARE — a `//` inside a string is data, not a
/// comment. That is not pedantry: the HOST's deployed menu block carries
/// `… || echo https://localhost:47992` inside an action, and the first naive version of this
/// validator refused that real file outright (as does `punktfunk-omarchy`'s original sed
/// pipeline, fixed alongside this). Byte-level scan: every delimiter is ASCII, so multi-byte
/// glyphs pass through untouched.
fn jsonc_parse(text: &str) -> Option<serde_json::Value> {
    let b = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    // A comma seen outside a string is HELD until the next significant byte: dropped if that
    // byte closes a scope (the trailing comma being forgiven), emitted otherwise. Comments
    // and whitespace between the comma and the brace fall out of this for free.
    let mut held_comma = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' {
                if let Some(&n) = b.get(i + 1) {
                    out.push(n);
                    i += 2;
                    continue;
                }
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b',' => {
                if held_comma {
                    out.push(b',');
                }
                held_comma = true;
                i += 1;
            }
            b'}' | b']' => {
                held_comma = false;
                out.push(c);
                i += 1;
            }
            _ => {
                if !c.is_ascii_whitespace() && held_comma {
                    out.push(b',');
                    held_comma = false;
                }
                if c == b'"' {
                    in_str = true;
                }
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
}

/// A menu id fragment from a host's name: lowercase alphanumerics and dashes, collapsed.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "host".to_string()
    } else {
        s
    }
}

/// The block's rows. Every value that came from the store passes through JSON escaping —
/// a host is free to be named `"};evil`.
fn rows_for(known: &KnownHosts) -> String {
    // JSON string literal, quotes included.
    let j = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let mut out = String::new();
    // The same root row the host block writes: on a box running both, the ids merge.
    out.push_str(
        "  \"punktfunk\": {\"icon\":\"\u{f0379}\",\"label\":\"Punktfunk\",\"aliases\":[\"streaming\",\"stream\"]},\n",
    );
    out.push_str(
        "  \"punktfunk.app\": {\"icon\":\"\u{f11b}\",\"label\":\"Open Punktfunk\",\"description\":\"Hosts, pairing, settings\",\"action\":\"uwsm-app -- punktfunk-client\"},\n",
    );
    out.push_str(
        "  \"punktfunk.couch\": {\"icon\":\"\u{f0eb5}\",\"label\":\"Game console\",\"description\":\"The couch UI — library, hosts, pairing\",\"action\":\"uwsm-app -- punktfunk-client --browse\"},\n",
    );
    let mut hosts: Vec<_> = known.hosts.iter().collect();
    // Most recent first, so the menu's order is the reach-for-it order.
    hosts.sort_by_key(|h| std::cmp::Reverse(h.last_used.unwrap_or(0)));
    let mut taken = std::collections::HashSet::new();
    for h in hosts {
        let mut id = slug(if h.name.is_empty() { &h.addr } else { &h.name });
        while !taken.insert(id.clone()) {
            id.push('-');
        }
        let label = if h.name.is_empty() { &h.addr } else { &h.name };
        let target = format!("{}:{}", h.addr, h.port);
        out.push_str(&format!(
            "  \"punktfunk.connect-{id}\": {{\"icon\":\"\u{f0379}\",\"label\":{},\"description\":{},\"aliases\":[\"connect\"],\"action\":{}}},\n",
            j(label),
            j(&format!("Connect and stream — {target}")),
            j(&format!("uwsm-app -- punktfunk-client --connect '{target}'")),
        ));
        // Wake without connecting: the "start the rig, then walk over" move. Only where a
        // MAC was ever learned — a row that cannot work is worse than no row.
        if !h.mac.is_empty() {
            out.push_str(&format!(
                "  \"punktfunk.wake-{id}\": {{\"icon\":\"\u{f0425}\",\"label\":{},\"description\":\"Send Wake-on-LAN\",\"action\":{}}},\n",
                j(&format!("Wake {label}")),
                j(&format!("punktfunk-client --wake '{target}'")),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::KnownHost;

    fn host(name: &str, addr: &str, mac: bool) -> KnownHost {
        KnownHost {
            name: name.into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: "ab".repeat(32),
            paired: true,
            last_used: None,
            mac: if mac {
                vec!["aa:bb:cc:dd:ee:ff".into()]
            } else {
                vec![]
            },
            os: String::new(),
            mgmt_port: None,
            clipboard_sync: false,
            profile_id: None,
            pinned_profiles: vec![],
            id: None,
        }
    }

    fn known(hosts: Vec<KnownHost>) -> KnownHosts {
        KnownHosts { hosts }
    }

    #[test]
    fn the_block_round_trips_and_leaves_the_users_rows_alone() {
        let theirs = "{\n  // a comment\n  \"personal.notes\": {\"label\":\"Notes\"},\n}\n";
        let with_ours = insert_block(
            &strip_block(theirs),
            &rows_for(&known(vec![host("Desk", "10.0.0.9", true)])),
        )
        .unwrap();
        assert!(jsonc_valid(&with_ours), "{with_ours}");
        assert!(with_ours.contains("personal.notes"), "their row survives");
        assert!(with_ours.contains("punktfunk.connect-desk"));
        assert!(with_ours.contains("punktfunk.wake-desk"));
        // Idempotent: a second pass replaces, never stacks.
        let again = insert_block(&strip_block(&with_ours), &rows_for(&known(vec![]))).unwrap();
        assert_eq!(again.matches(BEGIN).count(), 1);
        assert!(
            !again.contains("punktfunk.connect-desk"),
            "the stale host is gone"
        );
        // And removal restores their document exactly.
        assert_eq!(strip_block(&again), theirs);
    }

    #[test]
    fn a_hostile_host_name_cannot_escape_the_row() {
        // Quotes and braces arrive escaped: the document parses, and the name comes back
        // out as DATA, byte-identical, under the id its slug predicts.
        let spiky = "a\"b}c";
        let doc = insert_block(
            "{\n}\n",
            &rows_for(&known(vec![host(spiky, "10.0.0.9", false)])),
        )
        .unwrap();
        let v = jsonc_parse(&doc).expect("parses");
        assert_eq!(v["punktfunk.connect-a-b-c"]["label"].as_str(), Some(spiky));

        // `//` and a comma inside a string are DATA — the string-aware strip must not eat
        // them (the naive version did, and refused the box's real menu file over a URL).
        let commenty = "x // y, }";
        let doc = insert_block(
            "{\n}\n",
            &rows_for(&known(vec![host(commenty, "10.0.0.9", false)])),
        )
        .unwrap();
        let v = jsonc_parse(&doc).expect("a URL-ish name still parses");
        assert_eq!(v["punktfunk.connect-x-y"]["label"].as_str(), Some(commenty));
    }

    #[test]
    fn the_boxes_real_file_shapes_parse() {
        // The exact row that broke the first validator on glass (2026-08-31): a URL inside
        // an action string. Plus a trailing comma separated from its brace by a comment.
        let real = concat!(
            "{\n",
            "  // header comment with an example: \"a\": {\"b\":1},\n",
            "  \"punktfunk.console\": {\"label\":\"Open console\",\"action\":\"sh -c 'omarchy-launch-webapp \\\"$(punktfunk-host ctl console-url || echo https://localhost:47992)\\\"'\"},\n",
            "  \"last\": {\"label\":\"x\"}, // trailing, then a comment\n",
            "}\n"
        );
        let v = jsonc_parse(real).expect("the deployed shapes are valid");
        assert!(
            v["punktfunk.console"]["action"]
                .as_str()
                .unwrap()
                .contains("https://localhost:47992"),
            "the URL survives as data"
        );
    }

    #[test]
    fn duplicate_names_and_empty_names_get_distinct_ids() {
        let rows = rows_for(&known(vec![
            host("Desk", "10.0.0.1", false),
            host("Desk", "10.0.0.2", false),
            host("", "10.0.0.3", false),
        ]));
        assert!(rows.contains("punktfunk.connect-desk\""));
        assert!(rows.contains("punktfunk.connect-desk-\""));
        assert!(rows.contains("punktfunk.connect-10-0-0-3\""));
    }

    #[test]
    fn an_unparsable_file_is_refused_not_repaired() {
        let dir = std::env::temp_dir().join(format!("pf-menu-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("omarchy-menu.jsonc");
        std::fs::write(&p, "{ this is not jsonc").unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(write_rows(&p, Some("  \"punktfunk\": {},\n")).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
