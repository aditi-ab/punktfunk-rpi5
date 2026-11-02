//! The bring-up rung a device proved, kept across sessions so the next video start begins
//! there. [`RungFloor`](crate::input_stall::RungFloor) lives for one surface; without this a
//! device whose first rung stops its decoder pays that freeze on every start.
//!
//! The file holds one line per (build fingerprint, decoder, codec): `\t`-separated, the fields
//! of [`Entry`] in order. A line that does not parse is dropped. A rung is written only when the
//! floor moved (two clean failures) and a decoder then settled on it ([`outcome`]). An entry
//! from another OS build is dropped on the next write; one [`MAX_AGE_SECS`] old or from another
//! app version gives rung 0 one more chance ([`Start::Retry`]).
//!
//! Rungs are ladder indices, and the low-latency toggle reshapes the ladder. Toggled off, the
//! session takes the platform's default decoder (keyed `default`), so it keys its own entry.
//! Plan: `design/android-presenter-overhaul-v3.md` §3.4.

use crate::input_stall::SETTLED_FRAMES;

/// How long a proven rung holds before rung 0 gets another chance: 30 days.
const MAX_AGE_SECS: u64 = 30 * 86_400;

/// One remembered rung: a line of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) fingerprint: String,
    pub(crate) decoder: String,
    pub(crate) codec: String,
    pub(crate) rung: usize,
    /// Unix seconds when the rung was proven.
    pub(crate) proven_at_secs: u64,
    /// The app version that proved it.
    pub(crate) version: String,
}

/// What an entry is remembered for.
#[derive(Clone, Default)]
pub(crate) struct Key {
    pub(crate) fingerprint: String,
    pub(crate) decoder: String,
    pub(crate) codec: String,
}

impl Entry {
    fn is(&self, key: &Key) -> bool {
        self.fingerprint == key.fingerprint
            && self.decoder == key.decoder
            && self.codec == key.codec
    }
}

/// Where a codec's first decoder starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// Nothing remembered: rung 0.
    Fresh,
    /// A current entry: its rung.
    Remembered(usize),
    /// An old entry, or one from another app version: rung 0, the entry kept in hand.
    Retry(usize),
}

impl Start {
    /// The rung the floor begins at.
    pub(crate) fn rung(self) -> usize {
        match self {
            Start::Remembered(rung) => rung,
            Start::Fresh | Start::Retry(_) => 0,
        }
    }
}

/// What a decoder that showed frames does to the memory ([`outcome`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Keep,
    Record(usize),
    Forget,
}

pub(crate) fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let [fingerprint, decoder, codec, rung, proven_at_secs, version] =
        <[&str; 6]>::try_from(line.split('\t').collect::<Vec<_>>()).ok()?;
    Some(Entry {
        fingerprint: fingerprint.to_owned(),
        decoder: decoder.to_owned(),
        codec: codec.to_owned(),
        rung: rung.parse().ok()?,
        proven_at_secs: proven_at_secs.parse().ok()?,
        version: version.to_owned(),
    })
}

pub(crate) fn render(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                e.fingerprint, e.decoder, e.codec, e.rung, e.proven_at_secs, e.version
            )
        })
        .collect()
}

pub(crate) fn find<'a>(entries: &'a [Entry], key: &Key) -> Option<&'a Entry> {
    entries.iter().find(|e| e.is(key))
}

/// Where `key`'s first decoder starts. A stamp in the future (the clock went back) counts as old.
pub(crate) fn lookup(entries: &[Entry], key: &Key, now_secs: u64, version: &str) -> Start {
    let Some(e) = find(entries, key) else {
        return Start::Fresh;
    };
    let fresh = now_secs
        .checked_sub(e.proven_at_secs)
        .is_some_and(|age| age < MAX_AGE_SECS);
    if fresh && e.version == version {
        Start::Remembered(e.rung)
    } else {
        Start::Retry(e.rung)
    }
}

/// Entries from another OS build go; `true`: some did.
fn drop_other_builds(entries: &mut Vec<Entry>, key: &Key) -> bool {
    let len = entries.len();
    entries.retain(|e| e.fingerprint == key.fingerprint);
    entries.len() != len
}

/// Stamp `rung` for `key`, replacing its entry. `true`: the list changed.
pub(crate) fn record(
    entries: &mut Vec<Entry>,
    key: &Key,
    rung: usize,
    now_secs: u64,
    version: &str,
) -> bool {
    let dropped = drop_other_builds(entries, key);
    let entry = Entry {
        fingerprint: key.fingerprint.clone(),
        decoder: key.decoder.clone(),
        codec: key.codec.clone(),
        rung,
        proven_at_secs: now_secs,
        version: version.to_owned(),
    };
    match entries.iter_mut().find(|e| e.is(key)) {
        Some(e) if *e == entry => dropped,
        Some(e) => {
            *e = entry;
            true
        }
        None => {
            entries.push(entry);
            true
        }
    }
}

/// Remove `key`'s entry. `true`: the list changed.
pub(crate) fn forget(entries: &mut Vec<Entry>, key: &Key) -> bool {
    let dropped = drop_other_builds(entries, key);
    let len = entries.len();
    entries.retain(|e| !e.is(key));
    dropped || entries.len() != len
}

/// A decoder showed `rendered` frames with the floor at `floor_rung`, under a codec that
/// started as `start`. Only a settled decoder ([`SETTLED_FRAMES`]) changes anything: one that
/// settled on a floor that moved records it; a retry that settled on rung 0 clears the entry.
pub(crate) fn outcome(start: Start, floor_rung: usize, rendered: u64) -> Outcome {
    if rendered < SETTLED_FRAMES {
        Outcome::Keep
    } else if floor_rung != start.rung() {
        Outcome::Record(floor_rung)
    } else if matches!(start, Start::Retry(_)) {
        Outcome::Forget
    } else {
        Outcome::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "0.45.0";
    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    fn key(decoder: &str) -> Key {
        Key {
            fingerprint: "jmgo/n1s/1".into(),
            decoder: decoder.into(),
            codec: "video/av01".into(),
        }
    }

    fn entry(key: &Key, rung: usize, proven_at_secs: u64, version: &str) -> Entry {
        Entry {
            fingerprint: key.fingerprint.clone(),
            decoder: key.decoder.clone(),
            codec: key.codec.clone(),
            rung,
            proven_at_secs,
            version: version.into(),
        }
    }

    #[test]
    fn entries_round_trip_and_a_bad_line_is_dropped() {
        let entries = vec![
            entry(&key("OMX.MS.av1"), 2, NOW, V),
            entry(&key("default"), 3, NOW - DAY, "0.44.1"),
        ];
        assert_eq!(parse(&render(&entries)), entries);
        let text = format!(
            "{}a\tb\tc\tnot-a-rung\t1\tv\nshort\tline\na\tb\tc\t1\t1\tv\textra\n{}",
            render(&entries[..1]),
            render(&entries[1..]).trim_end()
        );
        assert_eq!(parse(&text), entries);
    }

    #[test]
    fn a_current_entry_is_remembered_and_an_old_one_retried() {
        let k = key("OMX.MS.av1");
        assert_eq!(lookup(&[], &k, NOW, V), Start::Fresh);
        let at = |proven, version| lookup(&[entry(&k, 2, proven, version)], &k, NOW, V);
        assert_eq!(at(NOW - 29 * DAY, V), Start::Remembered(2));
        assert_eq!(at(NOW - 30 * DAY, V), Start::Retry(2));
        assert_eq!(at(NOW - DAY, "0.44.1"), Start::Retry(2));
        assert_eq!(at(NOW + DAY, V), Start::Retry(2));
        // Another decoder, codec or OS build is another key.
        let other = [entry(&key("c2.android.av1.decoder"), 2, NOW, V)];
        assert_eq!(lookup(&other, &k, NOW, V), Start::Fresh);
        let mut build = k.clone();
        build.fingerprint = "jmgo/n1s/2".into();
        assert_eq!(
            lookup(&[entry(&k, 2, NOW, V)], &build, NOW, V),
            Start::Fresh
        );
        assert_eq!(Start::Remembered(2).rung(), 2);
        assert_eq!(Start::Retry(2).rung(), 0);
    }

    #[test]
    fn a_write_replaces_the_keys_entry_and_drops_other_builds() {
        let k = key("OMX.MS.av1");
        let mut entries = vec![entry(&key("default"), 3, NOW, V)];
        assert!(record(&mut entries, &k, 2, NOW, V));
        assert!(!record(&mut entries, &k, 2, NOW, V));
        assert!(record(&mut entries, &k, 3, NOW + 1, V));
        assert_eq!(
            entries,
            [entry(&key("default"), 3, NOW, V), entry(&k, 3, NOW + 1, V)]
        );
        let mut updated = k.clone();
        updated.fingerprint = "jmgo/n1s/2".into();
        assert!(forget(&mut entries, &updated));
        assert!(entries.is_empty());
        let mut entries = vec![entry(&k, 2, NOW, V)];
        assert!(forget(&mut entries, &k));
        assert!(!forget(&mut entries, &k));
    }

    #[test]
    fn a_floor_that_moved_and_settled_is_recorded() {
        assert_eq!(outcome(Start::Fresh, 2, SETTLED_FRAMES), Outcome::Record(2));
        assert_eq!(outcome(Start::Fresh, 0, SETTLED_FRAMES), Outcome::Keep);
        assert_eq!(
            outcome(Start::Remembered(2), 3, SETTLED_FRAMES),
            Outcome::Record(3)
        );
    }

    #[test]
    fn a_floor_that_moved_but_never_settled_records_nothing() {
        for start in [Start::Fresh, Start::Remembered(2), Start::Retry(2)] {
            assert_eq!(outcome(start, 4, SETTLED_FRAMES - 1), Outcome::Keep);
        }
        assert_eq!(
            outcome(Start::Retry(2), 0, SETTLED_FRAMES - 1),
            Outcome::Keep
        );
    }

    #[test]
    fn a_remembered_start_that_settles_on_its_rung_keeps_the_entry() {
        assert_eq!(
            outcome(Start::Remembered(2), 2, SETTLED_FRAMES),
            Outcome::Keep
        );
    }

    #[test]
    fn a_retry_that_settles_on_rung_0_clears_the_memory() {
        assert_eq!(outcome(Start::Retry(2), 0, SETTLED_FRAMES), Outcome::Forget);
    }

    #[test]
    fn a_retry_whose_floor_moves_again_is_re_stamped() {
        assert_eq!(
            outcome(Start::Retry(2), 2, SETTLED_FRAMES),
            Outcome::Record(2)
        );
        assert_eq!(
            outcome(Start::Retry(2), 3, SETTLED_FRAMES),
            Outcome::Record(3)
        );
    }
}
