//! The dmabuf import cache's policy: which import a frame reuses, which imports a miss frees,
//! and when the device idles first.
//!
//! Pure: no `ash`, so the table below runs on every target. [`Policy::step`] takes what a
//! shell saw ([`Event`]) and answers what it does next ([`Action`]). The shell is
//! `ImportCache` in `vk_util.rs`: it imports, idles the device where told, and destroys what
//! the policy hands back.
//!
//! Invariants: a hit is the same buffer at the same extent and is not fresh; a miss keeps the
//! `keep` most recently used imports, which the frames in flight read, and frees the rest,
//! which are back with the producer; an import a frame in flight reads is freed only after the
//! device idled. Each row of `import_table` names the fix it pins. Design:
//! `design/pf-encode-overhaul.md` §4.4.

/// A buffer's `(st_dev, st_ino)`, or a per-frame sentinel when `fstat` failed.
pub(crate) type Key = (u64, u64);
/// The width and height a buffer was imported at.
pub(crate) type Extent = (u32, u32);

/// The cached imports, least recently used first.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Policy<T> {
    entries: Vec<(Key, Extent, T)>,
}

/// What a shell saw.
#[derive(Debug)]
pub(crate) enum Event<'a, T> {
    /// A frame reads the buffer `key` at `extent`.
    Want(Key, Extent),
    /// The import an [`Action::Import`] asked for succeeded. `keep` counts the frames in
    /// flight; `busy` lists the cached keys they read.
    Imported {
        key: Key,
        extent: Extent,
        item: T,
        keep: usize,
        busy: &'a [Key],
    },
}

/// What a shell does next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action<T> {
    /// Hand out the cached import. It is not fresh: it holds the producer's last contents.
    Reuse(T),
    /// Import the buffer. `stale` is its import at another extent: idle the device, free it.
    Import { stale: Option<T> },
    /// Free `evict`, idling the device first when `idle_first`; the new import is fresh.
    Insert { evict: Vec<T>, idle_first: bool },
}

impl<T: Copy> Policy<T> {
    pub(crate) fn step(&mut self, event: Event<T>) -> Action<T> {
        match event {
            Event::Want(key, extent) => {
                let Some(pos) = self.entries.iter().position(|e| e.0 == key) else {
                    return Action::Import { stale: None };
                };
                let entry = self.entries.remove(pos);
                // The inode names another allocation now: never hand it out.
                if entry.1 != extent {
                    return Action::Import {
                        stale: Some(entry.2),
                    };
                }
                self.entries.push(entry);
                Action::Reuse(entry.2)
            }
            Event::Imported {
                key,
                extent,
                item,
                keep,
                busy,
            } => {
                // The frames in flight are the last ones submitted: their imports are the newest.
                let n = self.entries.len().saturating_sub(keep);
                let evict: Vec<_> = self.entries.drain(..n).collect();
                self.entries.push((key, extent, item));
                Action::Insert {
                    idle_first: evict.iter().any(|e| busy.contains(&e.0)),
                    evict: evict.into_iter().map(|e| e.2).collect(),
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.entries.iter().map(|e| e.0)
    }

    /// Empty the cache, handing out every import.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.entries.drain(..).map(|e| e.2)
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Event::*;
    use super::*;

    const A: Key = (7, 1);
    const B: Key = (7, 2);
    const C: Key = (7, 3);
    const HD: Extent = (1920, 1080);
    const UHD: Extent = (3840, 2160);

    /// A cache holding `entries`, least recently used first.
    fn cache(entries: &[(Key, Extent, char)]) -> Policy<char> {
        Policy {
            entries: entries.to_vec(),
        }
    }

    /// `item` imported for `key` at HD with `keep` frames in flight reading `busy`.
    fn got(key: Key, item: char, keep: usize, busy: &[Key]) -> Event<'_, char> {
        Imported {
            key,
            extent: HD,
            item,
            keep,
            busy,
        }
    }

    const fn insert(evict: Vec<char>, idle_first: bool) -> Action<char> {
        Insert { evict, idle_first }
    }

    const MISS: Action<char> = Import { stale: None };

    /// A row: the rule it pins, the cache it starts from, each step's answer, the cache it ends as.
    type Row = (
        &'static str,
        Policy<char>,
        Vec<(Event<'static, char>, Action<char>)>,
        Policy<char>,
    );

    #[test]
    fn import_table() {
        // A failed `fstat` keys the import per frame. The slot stores that same sentinel as its
        // key, so the in-flight rule protects the import.
        let (s1, s2, s3) = ((u64::MAX, 1), (u64::MAX, 2), (u64::MAX, 3));
        let rows: Vec<Row> = vec![
            (
                // Depth 1 records with nothing in flight: a pool of two misses every frame.
                "keep only in-flight dmabuf imports resident",
                cache(&[(A, HD, 'a')]),
                vec![
                    (Want(B, HD), MISS),
                    (got(B, 'b', 0, &[]), insert(vec!['a'], false)),
                    (Want(A, HD), MISS),
                    (got(A, 'a', 0, &[]), insert(vec!['b'], false)),
                ],
                cache(&[(A, HD, 'a')]),
            ),
            (
                // A failed record left B's import newer than the one slot 0 still reads.
                "idle the device before evicting an import",
                cache(&[(A, HD, 'a'), (B, HD, 'b')]),
                vec![
                    (Want(C, HD), MISS),
                    (got(C, 'c', 1, &[A]), insert(vec!['a'], true)),
                ],
                cache(&[(B, HD, 'b'), (C, HD, 'c')]),
            ),
            (
                // Least recently used, not first imported: the hit in flight survives.
                "idle the device before evicting a dmabuf import",
                cache(&[(A, HD, 'a'), (B, HD, 'b')]),
                vec![
                    (Want(A, HD), Reuse('a')),
                    (Want(C, HD), MISS),
                    (got(C, 'c', 1, &[A]), insert(vec!['b'], false)),
                ],
                cache(&[(A, HD, 'a'), (C, HD, 'c')]),
            ),
            (
                // A hit is not fresh: the consumer acquires it keeping the producer's contents.
                "acquire a cached dmabuf on every frame",
                cache(&[(A, HD, 'a')]),
                vec![(Want(A, HD), Reuse('a')), (Want(A, HD), Reuse('a'))],
                cache(&[(A, HD, 'a')]),
            ),
            (
                "recheck the extent of a cached PyroWave dmabuf import",
                cache(&[(A, HD, 'a'), (B, HD, 'b')]),
                vec![
                    (Want(A, UHD), Import { stale: Some('a') }),
                    (
                        Imported {
                            key: A,
                            extent: UHD,
                            item: 'A',
                            keep: 1,
                            busy: &[],
                        },
                        insert(vec![], false),
                    ),
                ],
                cache(&[(B, HD, 'b'), (A, UHD, 'A')]),
            ),
            (
                "a failed import frees only the stale extent",
                cache(&[(B, HD, 'b'), (A, HD, 'a')]),
                vec![
                    (Want(A, UHD), Import { stale: Some('a') }),
                    (Want(A, UHD), MISS),
                ],
                cache(&[(B, HD, 'b')]),
            ),
            (
                "keep the sentinel of a failed fstat while its frame is in flight",
                cache(&[(s1, HD, 's')]),
                vec![
                    (Want(s2, HD), MISS),
                    (got(s2, 't', 1, &[]), insert(vec![], false)),
                    (Want(s3, HD), MISS),
                    (got(s3, 'u', 1, &[]), insert(vec!['s'], false)),
                ],
                cache(&[(s2, HD, 't'), (s3, HD, 'u')]),
            ),
        ];
        for (name, mut p, steps, end) in rows {
            for (i, (event, want)) in steps.into_iter().enumerate() {
                let shown = format!("{event:?}");
                assert_eq!(p.step(event), want, "{name}: step {i} ({shown})");
            }
            assert_eq!(p, end, "{name}: end state");
        }
    }
}
