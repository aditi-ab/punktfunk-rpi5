//! Corpus replay: walk a captured real-host stream through the planners.
//!
//! The M0 capture hook (`PUNKTFUNK_DUMP_VIDEO=<dir>` on any desktop client) writes
//! the exact decoder input of a live session — `au-<stamp>.<codec>` plus an `.idx`
//! sidecar carrying `offset len flags complete` per AU. This harness feeds those AUs
//! back through [`pf_bitstream::h264::H264Planner`] / [`pf_bitstream::h265::H265Planner`]
//! and asserts the planner survives a REAL host stream: every AU plans (bar the
//! deliberate skips), no panic, and the warnings are only the ones a clean capture may
//! legitimately produce.
//!
//! Why this exists separately from the vendored conformance vectors: those prove we
//! match the spec's own test streams, and the on-glass sessions prove the whole pipe —
//! but between the two sits "does the planner handle what OUR five host encoder
//! families actually emit", which is the question the corpus was captured to answer.
//! For HEVC this is the ONLY pre-wiring validation against real host output (the
//! client's HEVC rung is still being built), so it runs long before M3 finishes.
//!
//! Ignored by default: captures are hundreds of megabytes and live outside the repo.
//! Run one explicitly —
//!
//! ```text
//! PF_CORPUS=/path/to/au-1785970273.h265 \
//!   cargo test -p pf-bitstream --test corpus_replay -- --ignored --nocapture
//! ```
//!
//! The `.idx` sidecar is found next to the data file (`<data>.idx`); the codec comes
//! from the extension, matching the capture hook's own naming convention.

use std::path::Path;
use std::path::PathBuf;

/// One captured access unit: its byte range in the data file, plus the wire bits the
/// byte stream itself cannot carry.
struct CapturedAu {
    offset: usize,
    len: usize,
    /// The wire `flags` byte (`USER_FLAG_*`) — kept for the RFI/intra-refresh legs,
    /// which discriminate on it.
    _flags: u32,
    complete: bool,
}

/// Parse the `.idx` sidecar: one `offset len flags complete` line per AU, `#` comments
/// and blank lines skipped (the hook writes none today, but a hand-trimmed corpus file
/// is a thing a human will produce).
///
/// A malformed FINAL line is dropped with a note instead of failing: ending a capture
/// means killing the client, so the last buffered line is routinely half-written (the
/// hook's own docs call a truncated last AU acceptable). Anywhere else a malformed line
/// means the sidecar is corrupt and the run must not quietly replay a subset.
fn read_index(path: &Path) -> Vec<CapturedAu> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read the index sidecar {}: {e}", path.display()));
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect();
    let last = lines.len().saturating_sub(1);
    let mut out = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        match parse_index_line(line) {
            Some(au) => out.push(au),
            None if n == last => {
                println!("note: dropping a truncated final index line ({line:?})");
            }
            None => panic!("index line {n} is malformed: {line:?}"),
        }
    }
    out
}

/// One `offset len flags complete` line, or `None` when it is not four parsable fields.
fn parse_index_line(line: &str) -> Option<CapturedAu> {
    let mut it = line.split_whitespace();
    let num = |raw: &str| -> Option<u64> {
        match raw.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => raw.parse().ok(),
        }
    };
    let offset = num(it.next()?)?;
    let len = num(it.next()?)?;
    let flags = num(it.next()?)?;
    let complete = num(it.next()?)?;
    Some(CapturedAu {
        offset: offset as usize,
        len: len as usize,
        _flags: flags as u32,
        complete: complete != 0,
    })
}

/// The capture named by `PF_CORPUS`, or `None` when the variable is unset.
fn corpus_from_env() -> Option<(PathBuf, Vec<u8>, Vec<CapturedAu>)> {
    let path = PathBuf::from(std::env::var_os("PF_CORPUS")?);
    let data = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("cannot read the capture {}: {e}", path.display()));
    let mut idx = path.clone().into_os_string();
    idx.push(".idx");
    let mut index = read_index(Path::new(&idx));
    // Same truncation story on the data side: the final AU's bytes may not all have
    // reached the file before the client died. Drop AUs the data cannot cover — but
    // only from the tail, so a short file can never silently hide a middle gap.
    let covered = index
        .iter()
        .take_while(|au| au.offset.saturating_add(au.len) <= data.len())
        .count();
    if covered < index.len() {
        println!(
            "note: dropping {} index entr{} past the end of the data file (truncated capture)",
            index.len() - covered,
            if index.len() - covered == 1 {
                "y"
            } else {
                "ies"
            },
        );
        index.truncate(covered);
    }
    assert!(!index.is_empty(), "the capture's index is empty");
    Some((path, data, index))
}

/// Per-AU outcome tally — what the run reports and asserts on.
#[derive(Default)]
struct Tally {
    planned: usize,
    skipped: usize,
    errors: Vec<String>,
    warnings: Vec<String>,
    partial: usize,
}

impl Tally {
    /// A clean capture of a healthy session must plan every complete AU. Errors are
    /// hard failures; warnings are printed and capped — `MissingReference` on a stream
    /// that never lost a packet would mean the planner invented a gap.
    fn assert_clean(&self, total: usize) {
        println!(
            "planned {} / skipped {} / partial-AUs-ignored {} / errors {} / warnings {} \
             (of {total} captured AUs)",
            self.planned,
            self.skipped,
            self.partial,
            self.errors.len(),
            self.warnings.len(),
        );
        for w in self.warnings.iter().take(20) {
            println!("  warning: {w}");
        }
        for e in self.errors.iter().take(20) {
            println!("  ERROR: {e}");
        }
        assert!(
            self.errors.is_empty(),
            "{} AUs failed to plan — first: {}",
            self.errors.len(),
            self.errors[0],
        );
        assert!(
            self.warnings.is_empty(),
            "{} planner warnings on a clean capture — first: {}",
            self.warnings.len(),
            self.warnings[0],
        );
        assert!(self.planned > 0, "no AU planned at all");
    }
}

#[test]
#[ignore = "needs a capture: PF_CORPUS=<au-file> (see the module docs)"]
fn a_captured_host_stream_replays_through_the_planner() {
    let Some((path, data, index)) = corpus_from_env() else {
        panic!("PF_CORPUS is unset — see the module docs for the invocation");
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_owned();
    println!(
        "replaying {} ({} bytes, {} AUs, codec {ext})",
        path.display(),
        data.len(),
        index.len(),
    );

    let mut tally = Tally::default();
    // The planners take one COMPLETE AU. A partial AU (the wire's shard split) is the
    // pump's business, not the planner's — count and skip rather than feed a fragment.
    let complete: Vec<&CapturedAu> = index.iter().filter(|au| au.complete).collect();
    tally.partial = index.len() - complete.len();

    match ext.as_str() {
        "h265" => {
            let mut planner = pf_bitstream::h265::H265Planner::new();
            for (i, au) in complete.iter().enumerate() {
                let bytes = &data[au.offset..au.offset + au.len];
                match planner.plan_au(bytes) {
                    Ok(plan) => {
                        tally.planned += 1;
                        for w in &plan.warnings {
                            tally.warnings.push(format!("AU {i}: {w:?}"));
                        }
                    }
                    // The spec's own skip (8.1.3): decode nothing, show nothing, the
                    // stream is healthy — never an error (the WP-2 contract note).
                    Err(pf_bitstream::h265::PlanError::RaslSkipped { .. }) => tally.skipped += 1,
                    Err(e) => tally.errors.push(format!("AU {i}: {e}")),
                }
            }
        }
        "h264" => {
            let mut planner = pf_bitstream::h264::H264Planner::new();
            for (i, au) in complete.iter().enumerate() {
                let bytes = &data[au.offset..au.offset + au.len];
                match planner.plan_au(bytes) {
                    Ok(plan) => {
                        tally.planned += 1;
                        for w in &plan.warnings {
                            tally.warnings.push(format!("AU {i}: {w:?}"));
                        }
                    }
                    Err(e) => tally.errors.push(format!("AU {i}: {e}")),
                }
            }
        }
        other => panic!("no planner for a .{other} capture (h264/h265 only today)"),
    }

    tally.assert_clean(index.len());
}
