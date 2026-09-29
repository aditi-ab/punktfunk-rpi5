//! Carry source mtimes from one CI checkout to the next, so cargo's fingerprints survive.
//!
//! Cargo judges a path crate by mtime, and a fresh clone makes every file new: every
//! workspace crate rebuilds even when nothing changed. Run this from the repo root before the
//! first cargo step, naming the job's target dir. A path whose content matches the manifest
//! from the last run gets that run's mtime back; anything else keeps its checkout time, as a
//! persistent checkout would. The manifest is then rewritten for this tree.
//!
//! Content is a file's bytes, or a directory's entry names (cargo watches directory mtimes).
//! Every job that builds into the target dir must run this first, and no two such jobs may
//! overlap: a build from other content under restored mtimes would pass as fresh.
//! `.git` and `target` dirs are not walked.
//!
//! rustc -O scripts/ci/src-mtimes.rs -o <tmp>/src-mtimes && <tmp>/src-mtimes <target-dir>
//! Self-check: rustc --test scripts/ci/src-mtimes.rs -o <tmp>/t && <tmp>/t

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MANIFEST: &str = ".src-mtimes";

fn main() {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: src-mtimes <target-dir>");
        std::process::exit(2);
    };
    match run(Path::new("."), Path::new(&dir)) {
        Ok((kept, total)) => println!("src-mtimes: {kept} of {total} paths kept their mtime"),
        Err(e) => {
            eprintln!("src-mtimes: {e}");
            std::process::exit(1);
        }
    }
}

/// Restores what matches, writes the new manifest, returns (restored, walked).
fn run(root: &Path, target: &Path) -> io::Result<(usize, usize)> {
    fs::create_dir_all(target)?;
    let manifest = target.join(MANIFEST);
    let old = fs::read_to_string(&manifest)
        .map(|s| parse(&s))
        .unwrap_or_default();
    // Gone before any mtime moves: a run that dies half way leaves nothing to trust.
    match fs::remove_file(&manifest) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }

    let mut entries = Vec::new();
    walk(root, String::new(), &mut entries)?;
    let mut kept = 0;
    let mut out = String::new();
    for (rel, id) in &entries {
        let path = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        if let Some(&(old_id, ns)) = old.get(rel.as_str()) {
            if old_id == *id && set_mtime(&path, UNIX_EPOCH + Duration::from_nanos(ns)).is_ok() {
                kept += 1;
            }
        }
        // What is on disk now, restored or not, is what this run builds against.
        let ns = fs::metadata(&path)?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        out.push_str(&format!("{id:016x} {} {rel}\n", ns.as_nanos()));
    }
    let tmp = target.join(format!("{MANIFEST}.tmp"));
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &manifest)?;
    Ok((kept, entries.len()))
}

fn parse(s: &str) -> HashMap<String, (u64, u64)> {
    s.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, ' ');
            let id = u64::from_str_radix(f.next()?, 16).ok()?;
            let ns = f.next()?.parse().ok()?;
            Some((f.next()?.to_string(), (id, ns)))
        })
        .collect()
}

/// Every file and directory under `dir`, keyed by `/`-joined path ("" is the root).
fn walk(dir: &Path, rel: String, out: &mut Vec<(String, u64)>) -> io::Result<()> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        names.push((name, e.file_type()?));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = DefaultHasher::new();
    h.write_u8(b'd');
    for (name, ty) in &names {
        h.write(name.as_bytes());
        h.write_u8(0);
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ty.is_dir() && name != ".git" && name != "target" {
            walk(&dir.join(name), child, out)?;
        } else if ty.is_file() {
            let bytes = fs::read(dir.join(name))?;
            let mut fh = DefaultHasher::new();
            fh.write_u8(b'f');
            fh.write(&bytes);
            out.push((child, fh.finish()));
        }
    }
    out.push((rel, h.finish()));
    Ok(())
}

#[cfg(windows)]
fn set_mtime(path: &Path, t: SystemTime) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_WRITE_ATTRIBUTES, and FILE_FLAG_BACKUP_SEMANTICS so a directory opens too.
    let f = fs::OpenOptions::new()
        .access_mode(0x100)
        .custom_flags(0x0200_0000)
        .open(path)?;
    f.set_modified(t)
}

#[cfg(not(windows))]
fn set_mtime(path: &Path, t: SystemTime) -> io::Result<()> {
    fs::File::open(path)?.set_modified(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn restores_only_what_matches() {
        let base: PathBuf = std::env::temp_dir().join(format!("src-mtimes-{}", std::process::id()));
        let (src, target) = (base.join("src"), base.join("target"));
        fs::create_dir_all(src.join("keep")).unwrap();
        fs::create_dir_all(src.join("grow")).unwrap();
        fs::write(src.join("keep/a.rs"), "a").unwrap();
        fs::write(src.join("grow/b.rs"), "b").unwrap();
        fs::write(src.join("edit.rs"), "one").unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for p in ["keep", "keep/a.rs", "grow", "grow/b.rs", "edit.rs"] {
            set_mtime(&src.join(p), old).unwrap();
        }
        assert_eq!(run(&src, &target).unwrap().0, 0, "no manifest yet");

        // A fresh checkout: every mtime new, one file edited, one entry added.
        fs::write(src.join("edit.rs"), "two").unwrap();
        fs::write(src.join("grow/c.rs"), "c").unwrap();
        let now = SystemTime::now();
        for p in ["keep", "keep/a.rs", "grow", "grow/b.rs", "edit.rs"] {
            set_mtime(&src.join(p), now).unwrap();
        }
        run(&src, &target).unwrap();
        assert_eq!(mtime(&src.join("keep/a.rs")), old);
        assert_eq!(mtime(&src.join("keep")), old);
        assert_eq!(mtime(&src.join("grow/b.rs")), old);
        assert_eq!(
            mtime(&src.join("edit.rs")),
            now,
            "changed bytes keep the new mtime"
        );
        assert_eq!(
            mtime(&src.join("grow")),
            now,
            "a new entry keeps the new dir mtime"
        );

        // The manifest now describes this tree, so a rerun restores all of it.
        let (kept, total) = run(&src, &target).unwrap();
        assert_eq!(kept, total);
        fs::remove_dir_all(&base).unwrap();
    }
}
