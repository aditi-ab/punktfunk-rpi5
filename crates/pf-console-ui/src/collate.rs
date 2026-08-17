//! Sorting and grouping the library — all policy, no Skia.
//!
//! Kept a separate, pure module for two reasons. It is the half of "alternate views and
//! group & sort" that has to be identical on every client, so this file is also the
//! portable SPEC the Apple and Android ports implement; and grouping rules are exactly the
//! kind of thing that reads obviously correct and is quietly wrong (a platform-less Steam
//! library collapsing into one "Unknown" heap, a fold that files "The Witcher" under T).
//! Both are cheap to test here and expensive to notice on a TV.
//!
//! Everything returns INDICES into the caller's slice. The screens' art cache, fetch pump
//! and cursor arithmetic all key off the shared model's ordering, so a collation that
//! handed back cloned games would fork the identity of every title in the shelf.

use crate::library::{store_label, LibraryGame};

/// How titles are ordered within a group.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SortKey {
    /// The host's own order, untouched. The DEFAULT, and byte-identical to the shelf as it
    /// has always been — a user who never opens the sort pills must see no change at all.
    #[default]
    HostOrder,
    /// A–Z, case- and diacritic-relaxed, with the leading article folded away.
    Title,
    /// Platform label A–Z, then title within it.
    Platform,
    /// Store label A–Z, then title.
    Store,
}

impl SortKey {
    /// Parse the persisted `library_sort` value. Lenient by design: an unknown string is a
    /// newer client's key, and the right answer to one is today's shelf rather than an
    /// error — the same rule `ui_palette` follows.
    pub(crate) fn parse(s: &str) -> SortKey {
        match s {
            "title" => SortKey::Title,
            "platform" => SortKey::Platform,
            "store" => SortKey::Store,
            _ => SortKey::HostOrder,
        }
    }

    /// The persisted name. A FILE FORMAT — renaming one silently resets every user's
    /// chosen sort to the default on their next launch.
    pub(crate) fn id(self) -> &'static str {
        match self {
            SortKey::HostOrder => "host",
            SortKey::Title => "title",
            SortKey::Platform => "platform",
            SortKey::Store => "store",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SortKey::HostOrder => "Default",
            SortKey::Title => "A–Z",
            SortKey::Platform => "Platform",
            SortKey::Store => "Store",
        }
    }

    /// The pills, in the order they are offered.
    pub(crate) const ALL: [SortKey; 4] = [
        SortKey::HostOrder,
        SortKey::Title,
        SortKey::Platform,
        SortKey::Store,
    ];
}

/// What a group IS, kept as data rather than a formatted string so a filtered library can
/// be compared against it without re-parsing a label.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum GroupKey {
    /// The launcher entries (design D4's leading group).
    Launchers,
    Platform(String),
    Store(String),
}

/// One collated bucket: what it is, what to call it, and which games are in it.
#[derive(Clone, Debug)]
pub(crate) struct Group {
    pub key: GroupKey,
    pub label: String,
    /// Indices into the slice passed to [`collate`], in display order.
    pub games: Vec<usize>,
}

/// What to bucket by. `None` = one group holding everything (the plain shelf).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GroupBy {
    Platform,
    Store,
}

/// Fold a title down to something sortable: lowercase, diacritics relaxed to their base
/// letter, punctuation dropped, and a leading article removed.
///
/// The article fold is what a user actually means by A–Z. "The Witcher 3" belongs under W;
/// left alone, every "The …" in a library piles up under T and the sort is useless exactly
/// where it is most needed. English articles only — the host's titles are whatever the
/// store called them, and inventing rules for languages we cannot detect would file things
/// under letters nobody expects.
pub(crate) fn sort_title(title: &str) -> String {
    let relaxed: String = title
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        })
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let trimmed = relaxed.trim();
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = trimmed.strip_prefix(article) {
            // Guard against a title that IS an article ("The", "A Way Out" keeps "way out",
            // but a bare "The" must not sort as an empty string and float to the front).
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    trimmed.to_string()
}

/// The bucket label for one game under `by`.
///
/// The interesting case is a game with no platform. It does NOT go to "Unknown": a Steam
/// library is entirely platform-less, and one giant "Unknown" heap would be a worse view
/// than no grouping at all. A store-front game buckets under its STORE instead ("Steam"),
/// which is both true and useful, and only an entry with neither lands in "Other".
fn bucket(g: &LibraryGame, by: GroupBy) -> GroupKey {
    match by {
        GroupBy::Platform => match g
            .platform
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(p) => GroupKey::Platform(p.to_string()),
            None => match store_label(&g.store) {
                "Game" => GroupKey::Platform("Other".to_string()),
                store => GroupKey::Store(store.to_string()),
            },
        },
        GroupBy::Store => GroupKey::Store(store_label(&g.store).to_string()),
    }
}

fn label_of(key: &GroupKey) -> String {
    match key {
        GroupKey::Launchers => "Launchers".to_string(),
        GroupKey::Platform(p) | GroupKey::Store(p) => p.clone(),
    }
}

/// Collate `games` into display groups.
///
/// Launchers always form the leading group, which is how design D4's "launcher entries come
/// first" invariant survives grouping BY CONSTRUCTION rather than by every caller
/// remembering it. Sorting applies WITHIN groups, never across them.
pub(crate) fn collate(
    games: &[LibraryGame],
    sort: SortKey,
    group_by: Option<GroupBy>,
) -> Vec<Group> {
    let mut launchers: Vec<usize> = Vec::new();
    // Insertion-ordered rather than a map, so groups appear in the order the library first
    // mentions them and two runs over the same library agree.
    let mut buckets: Vec<(GroupKey, Vec<usize>)> = Vec::new();

    for (i, g) in games.iter().enumerate() {
        if g.launcher {
            launchers.push(i);
            continue;
        }
        let key = match group_by {
            Some(by) => bucket(g, by),
            // Ungrouped: one bucket holding the whole shelf. Its label is never drawn (the
            // shelf has no heading when there is only one group), so it names itself
            // honestly rather than inventing a title.
            None => GroupKey::Platform("All".to_string()),
        };
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(i),
            None => buckets.push((key, vec![i])),
        }
    }

    let order = |a: usize, b: usize| -> std::cmp::Ordering {
        let (ga, gb) = (&games[a], &games[b]);
        match sort {
            // Untouched: the host's order IS the order, so the comparator never fires.
            SortKey::HostOrder => a.cmp(&b),
            SortKey::Title => sort_title(&ga.title)
                .cmp(&sort_title(&gb.title))
                .then(a.cmp(&b)),
            SortKey::Platform => ga
                .platform
                .as_deref()
                .unwrap_or("")
                .cmp(gb.platform.as_deref().unwrap_or(""))
                .then_with(|| sort_title(&ga.title).cmp(&sort_title(&gb.title)))
                .then(a.cmp(&b)),
            SortKey::Store => store_label(&ga.store)
                .cmp(store_label(&gb.store))
                .then_with(|| sort_title(&ga.title).cmp(&sort_title(&gb.title)))
                .then(a.cmp(&b)),
        }
    };

    let mut out: Vec<Group> = Vec::with_capacity(buckets.len() + 1);
    if !launchers.is_empty() {
        // Launchers keep the host's order whatever the sort says: there are two or three of
        // them, they are a fixed set, and shuffling them by title makes muscle memory
        // useless for no gain.
        out.push(Group {
            key: GroupKey::Launchers,
            label: label_of(&GroupKey::Launchers),
            games: launchers,
        });
    }
    for (key, mut idx) in buckets {
        // `sort_by` is stable, and every comparator falls back to the index, so equal keys
        // keep the host's order rather than an arbitrary one.
        idx.sort_by(|&a, &b| order(a, b));
        out.push(Group {
            label: label_of(&key),
            key,
            games: idx,
        });
    }
    // Groups themselves go A–Z by label, launchers excepted — they were pushed first and
    // `sort_by` is stable, so pinning them by key keeps them leading.
    out.sort_by(|a, b| {
        let lead = |g: &Group| u8::from(g.key != GroupKey::Launchers);
        lead(a).cmp(&lead(b)).then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// The flat index list for a group filter — `None` = the whole library, in collated order.
pub(crate) fn filtered(
    games: &[LibraryGame],
    sort: SortKey,
    filter: Option<&GroupKey>,
) -> Vec<usize> {
    let by = match filter {
        Some(GroupKey::Platform(_)) => Some(GroupBy::Platform),
        Some(GroupKey::Store(_)) => Some(GroupBy::Store),
        Some(GroupKey::Launchers) | None => None,
    };
    let groups = collate(games, sort, by);
    match filter {
        None => groups.into_iter().flat_map(|g| g.games).collect(),
        Some(want) => groups
            .into_iter()
            .find(|g| &g.key == want)
            .map(|g| g.games)
            .unwrap_or_default(),
    }
}

/// Is there anything worth browsing? A library with one platform and one store has nothing
/// to drill INTO, and a screen that opens onto a single tile is worse than no screen —
/// so the button that reaches it is hidden rather than made to disappoint.
pub(crate) fn worth_browsing(games: &[LibraryGame]) -> bool {
    collate(games, SortKey::HostOrder, Some(GroupBy::Platform))
        .iter()
        .filter(|g| g.key != GroupKey::Launchers)
        .count()
        >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_SORT: [SortKey; 4] = SortKey::ALL;

    fn game(
        id: &str,
        title: &str,
        store: &str,
        platform: Option<&str>,
        launcher: bool,
    ) -> LibraryGame {
        LibraryGame {
            id: id.into(),
            title: title.into(),
            store: store.into(),
            launcher,
            icon: String::new(),
            platform: platform.map(str::to_string),
            running: false,
        }
    }

    #[test]
    fn article_fold_files_titles_where_a_reader_looks_for_them() {
        assert_eq!(sort_title("The Witcher 3"), "witcher 3");
        assert_eq!(sort_title("A Way Out"), "way out");
        assert_eq!(sort_title("An Untitled Story"), "untitled story");
        // Not an article, just a word starting with one.
        assert_eq!(sort_title("Theme Hospital"), "theme hospital");
        assert_eq!(sort_title("Anno 1800"), "anno 1800");
        // Diacritics relax; punctuation goes.
        assert_eq!(sort_title("Pokémon: Red!"), "pokemon red");
        // A title that is ONLY an article keeps something to sort on.
        assert_eq!(sort_title("The"), "the");
    }

    #[test]
    fn platform_less_store_games_bucket_under_their_store_not_unknown() {
        let games = [
            game("a", "Dota 2", "steam", None, false),
            game("b", "Half-Life", "steam", None, false),
            game("c", "Shadow of the Colossus", "custom", Some("PS2"), false),
            // Neither a platform nor a store we have a label for.
            game("d", "Mystery", "wat", None, false),
        ];
        let groups = collate(&games, SortKey::HostOrder, Some(GroupBy::Platform));
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert!(labels.contains(&"Steam"), "{labels:?}");
        assert!(labels.contains(&"PS2"), "{labels:?}");
        assert!(labels.contains(&"Other"), "{labels:?}");
        assert!(
            !labels.contains(&"Unknown"),
            "a platform-less Steam library must not become one heap: {labels:?}"
        );
        let steam = groups.iter().find(|g| g.label == "Steam").unwrap();
        assert_eq!(steam.games, vec![0, 1]);
    }

    #[test]
    fn launchers_always_lead_whatever_the_sort() {
        let games = [
            game("z", "Zed", "steam", Some("PC"), false),
            game("l", "Steam", "steam", None, true),
            game("a", "Aaa", "steam", Some("PC"), false),
        ];
        for sort in EVERY_SORT {
            let groups = collate(&games, sort, Some(GroupBy::Platform));
            assert_eq!(groups[0].key, GroupKey::Launchers, "{sort:?}");
            assert_eq!(groups[0].games, vec![1], "{sort:?}");
        }
    }

    #[test]
    fn host_order_is_byte_identical_to_no_sorting_at_all() {
        let games = [
            game("c", "Zed", "steam", Some("PC"), false),
            game("a", "Aaa", "steam", Some("PC"), false),
            game("b", "Mmm", "steam", Some("PC"), false),
        ];
        // The default must leave the shelf exactly as the host handed it over — a user who
        // never touches the sort pills sees no change whatever.
        assert_eq!(filtered(&games, SortKey::HostOrder, None), vec![0, 1, 2]);
        assert_eq!(filtered(&games, SortKey::Title, None), vec![1, 2, 0]);
    }

    #[test]
    fn equal_keys_keep_the_hosts_order() {
        let games = [
            game("a", "Same", "steam", Some("PC"), false),
            game("b", "Same", "steam", Some("PC"), false),
            game("c", "Same", "steam", Some("PC"), false),
        ];
        assert_eq!(filtered(&games, SortKey::Title, None), vec![0, 1, 2]);
    }

    #[test]
    fn filtering_returns_only_that_groups_games() {
        let games = [
            game("a", "Ico", "custom", Some("PS2"), false),
            game("b", "Dota", "steam", None, false),
            game("c", "SotC", "custom", Some("PS2"), false),
        ];
        let want = GroupKey::Platform("PS2".into());
        assert_eq!(
            filtered(&games, SortKey::HostOrder, Some(&want)),
            vec![0, 2]
        );
        // A filter naming a group that no longer exists yields nothing, rather than
        // silently showing everything — a stale filter must not look like a working shelf.
        let gone = GroupKey::Platform("PS3".into());
        assert!(filtered(&games, SortKey::HostOrder, Some(&gone)).is_empty());
    }

    #[test]
    fn empty_and_single_group_libraries_are_not_worth_browsing() {
        assert!(!worth_browsing(&[]));
        assert!(!worth_browsing(&[game("l", "Steam", "steam", None, true)]));
        // One store, no platforms: nothing to choose between, so the button stays hidden.
        let one = [
            game("a", "Dota", "steam", None, false),
            game("b", "HL", "steam", None, false),
        ];
        assert!(!worth_browsing(&one));
        // A second group earns the screen.
        let two = [
            game("a", "Dota", "steam", None, false),
            game("b", "Ico", "custom", Some("PS2"), false),
        ];
        assert!(worth_browsing(&two));
        // Launchers alone never count — every library has them, and a screen offering
        // only "Launchers" is the single-tile screen this rule exists to prevent.
        let launcher_plus_one = [
            game("l", "Steam", "steam", None, true),
            game("a", "Dota", "steam", None, false),
        ];
        assert!(!worth_browsing(&launcher_plus_one));
    }

    /// The persisted strings, pinned literally. They are a FILE FORMAT: renaming one here
    /// silently resets every user's chosen sort to the default on their next launch.
    #[test]
    fn sort_keys_parse_from_their_stored_names_and_unknown_falls_back() {
        assert_eq!(SortKey::parse("title"), SortKey::Title);
        assert_eq!(SortKey::parse("platform"), SortKey::Platform);
        assert_eq!(SortKey::parse("store"), SortKey::Store);
        for k in EVERY_SORT {
            assert_eq!(SortKey::parse(k.id()), k, "{} round-trips", k.label());
        }
        // Anything else is a newer client's key, and the right answer to one is today's
        // shelf rather than an error — the rule `ui_palette` already follows.
        assert_eq!(SortKey::parse("something-newer"), SortKey::HostOrder);
        assert_eq!(SortKey::parse(""), SortKey::HostOrder);
        assert_eq!(SortKey::default(), SortKey::HostOrder);
    }
}
