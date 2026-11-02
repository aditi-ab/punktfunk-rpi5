//! Windows' queue of file operations for the next restart, `PendingFileRenameOperations`, as
//! data: decode it, drop the deletes under a path, encode it. An uninstall queues the files it
//! can't delete, and nothing takes them out when an install writes those paths again: the
//! restart then deletes the new files. Registry access stays with the caller.

/// The key under `HKLM` that holds the queue.
pub const KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";
/// The `REG_MULTI_SZ` value: a source, then its destination, per operation.
pub const VALUE: &str = "PendingFileRenameOperations";

/// The value's strings. A delete's destination is an empty string, so every empty string stays
/// but the list's own terminator, the last NUL.
pub fn decode(data: &[u16]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let end = data[at..]
            .iter()
            .position(|&unit| unit == 0)
            .map_or(data.len(), |nul| at + nul);
        if end > at || end + 1 < data.len() {
            strings.push(String::from_utf16_lossy(&data[at..end]));
        }
        at = end + 1;
    }
    strings
}

/// The strings as `REG_MULTI_SZ` data, each NUL-terminated, then the list's terminator.
pub fn encode(ops: &[String]) -> Vec<u16> {
    let mut data: Vec<u16> = ops
        .iter()
        .flat_map(|op| op.encode_utf16().chain(std::iter::once(0)))
        .collect();
    data.push(0);
    data
}

/// The queue without the deletes of `path` and of everything under it. `None` when it queues
/// none, or isn't source and destination pairs. Renames stay: their file is still wanted.
pub fn without_deletes_under(ops: &[String], path: &str) -> Option<Vec<String>> {
    let ops = paired(ops)?;
    let kept: Vec<String> = ops
        .chunks_exact(2)
        .filter(|pair| !(pair[1].is_empty() && under(target(&pair[0]), path)))
        .flatten()
        .cloned()
        .collect();
    (kept.len() != ops.len()).then_some(kept)
}

/// Whether a delete of exactly `path` is queued.
pub fn deletes(ops: &[String], path: &str) -> bool {
    paired(ops).is_some_and(|ops| {
        ops.chunks_exact(2)
            .any(|pair| pair[1].is_empty() && same(target(&pair[0]), path))
    })
}

/// The queue as whole pairs. Data that ends on a delete without the list's terminator decodes
/// one string short: that delete gets its empty destination back.
fn paired(ops: &[String]) -> Option<Vec<String>> {
    let mut ops = ops.to_vec();
    if !ops.len().is_multiple_of(2) {
        if ops.last().is_none_or(String::is_empty) {
            return None;
        }
        ops.push(String::new());
    }
    Some(ops)
}

/// A source's path: past the `*<digits>` tag newer Windows writes and the `\??\` prefix.
fn target(source: &str) -> &str {
    let untagged = source.strip_prefix('*').map_or(source, |rest| {
        rest.trim_start_matches(|c: char| c.is_ascii_digit())
    });
    untagged.strip_prefix(r"\??\").unwrap_or(untagged)
}

fn same(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

fn under(file: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('\\');
    same(file, dir)
        || file
            .get(..dir.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(dir))
            && file[dir.len()..].starts_with('\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(ops: &[&str]) -> Vec<String> {
        ops.iter().map(|op| op.to_string()).collect()
    }

    #[test]
    fn a_delete_at_the_end_keeps_its_empty_destination() {
        let ops = queue(&[r"\??\C:\a\old.dll", "", r"\??\C:\b\new", r"!\??\C:\b\dest"]);
        assert_eq!(decode(&encode(&ops)), ops);
        let ends_with_delete = queue(&[r"\??\C:\b\new", r"!\??\C:\b\dest", r"\??\C:\a", ""]);
        assert_eq!(decode(&encode(&ends_with_delete)), ends_with_delete);
        assert!(decode(&[0]).is_empty());
    }

    #[test]
    fn a_reinstall_takes_its_folder_out_of_the_queue() {
        let ops = queue(&[
            r"*1\??\C:\Windows\System32\punktfunk\pf_seat_audio-949f.dll",
            "",
            r"*1\??\C:\Program Files\punktfunk\unins000.exe",
            "",
            r"*1\??\C:\Program Files\punktfunk",
            "",
            r"*1\??\C:\Program Files\punktfunk-other\keep.exe",
            "",
            r"*1\??\C:\Temp\new.exe",
            r"*1!\??\C:\Program Files\punktfunk\replaced.exe",
        ]);
        let kept = without_deletes_under(&ops, r"c:\program files\PUNKTFUNK\").unwrap();
        assert_eq!(
            kept,
            queue(&[
                r"*1\??\C:\Windows\System32\punktfunk\pf_seat_audio-949f.dll",
                "",
                r"*1\??\C:\Program Files\punktfunk-other\keep.exe",
                "",
                r"*1\??\C:\Temp\new.exe",
                r"*1!\??\C:\Program Files\punktfunk\replaced.exe",
            ])
        );
        assert_eq!(
            without_deletes_under(&kept, r"C:\Program Files\punktfunk"),
            None
        );
    }

    #[test]
    fn a_last_delete_without_the_terminator_still_counts() {
        let data: Vec<u16> = r"\??\C:\Program Files\punktfunk\unins000.exe"
            .encode_utf16()
            .chain([0, 0])
            .collect();
        let ops = decode(&data);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            without_deletes_under(&ops, r"C:\Program Files\punktfunk"),
            Some(Vec::new())
        );
        let broken = queue(&[r"\??\C:\a", "", ""]);
        assert_eq!(without_deletes_under(&broken, r"C:\a"), None);
    }

    #[test]
    fn only_the_exact_file_counts_as_queued() {
        let ops = queue(&[r"*1\??\C:\Windows\System32\punktfunk\a.dll", ""]);
        assert!(deletes(&ops, r"C:\Windows\System32\punktfunk\A.DLL"));
        assert!(!deletes(&ops, r"C:\Windows\System32\punktfunk"));
    }
}
