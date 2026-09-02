//! Sorting and grouping the library — policy only, no Skia.
//!
//! Identical on every client: Apple and Android implement this file. Pin the groups
//! with `clients/shared/library-collate-vectors.json` (`vectors_match_the_shared_file`).
//! Change a rule here, regenerate that file in the same commit.
//!
//! Returns indices into the caller's slice. Art cache, fetch pump and cursor arithmetic
//! all key off the shared model's order.

use crate::library::{store_label, LibraryGame};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SortKey {
    /// Default. Must match the host's list so an unused sort is a no-op.
    #[default]
    HostOrder,
    /// A–Z after [`sort_title`].
    Title,
    Platform,
    Store,
}

impl SortKey {
    /// Persisted `library_sort`. Unknown strings (a newer client's key) fall back to
    /// [`SortKey::HostOrder`], same as `ui_palette`.
    pub(crate) fn parse(s: &str) -> SortKey {
        match s {
            "title" => SortKey::Title,
            "platform" => SortKey::Platform,
            "store" => SortKey::Store,
            _ => SortKey::HostOrder,
        }
    }

    /// Persisted id. Renaming one resets every stored sort on next launch.
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

    pub(crate) const ALL: [SortKey; 4] = [
        SortKey::HostOrder,
        SortKey::Title,
        SortKey::Platform,
        SortKey::Store,
    ];
}

/// Group identity as data, not a label, so a filter can match without re-parsing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum GroupKey {
    Launchers,
    Platform(String),
    Store(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Group {
    pub key: GroupKey,
    pub label: String,
    /// Indices into the slice passed to [`collate`], display order.
    pub games: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GroupBy {
    Platform,
    Store,
}

/// "The Witcher 3" belongs under W. English articles only — titles are store strings
/// and we cannot detect language.
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
            // Bare "The" must not fold to empty and float to the front.
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    trimmed.to_string()
}

/// No platform does not mean "Unknown": a Steam library is all platform-less, so
/// store-front games bucket under the store and only a game with neither is "Other".
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

/// Launchers lead by construction; sort applies inside a group, never across.
pub(crate) fn collate(
    games: &[LibraryGame],
    sort: SortKey,
    group_by: Option<GroupBy>,
) -> Vec<Group> {
    let mut launchers: Vec<usize> = Vec::new();
    // Vec, not a map: first-seen order, so two runs over the same library agree.
    let mut buckets: Vec<(GroupKey, Vec<usize>)> = Vec::new();

    for (i, g) in games.iter().enumerate() {
        if g.launcher {
            launchers.push(i);
            continue;
        }
        let key = match group_by {
            Some(by) => bucket(g, by),
            // Ungrouped: one bucket. The label is never drawn (no heading for a single group).
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
        // Launchers stay in host order: two or three of them, and title-sort breaks muscle memory.
        out.push(Group {
            key: GroupKey::Launchers,
            label: label_of(&GroupKey::Launchers),
            games: launchers,
        });
    }
    for (key, mut idx) in buckets {
        // Stable sort plus index fallback: equal keys keep host order.
        idx.sort_by(|&a, &b| order(a, b));
        out.push(Group {
            label: label_of(&key),
            key,
            games: idx,
        });
    }
    // A–Z by label; launchers stay first via the lead key (`sort_by` is stable).
    out.sort_by(|a, b| {
        let lead = |g: &Group| u8::from(g.key != GroupKey::Launchers);
        lead(a).cmp(&lead(b)).then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// Indices for a group filter. `None` is the whole library, collated order.
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

/// Hide the browse entry when grouping would yield a single tile.
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
        assert_eq!(sort_title("Theme Hospital"), "theme hospital");
        assert_eq!(sort_title("Anno 1800"), "anno 1800");
        assert_eq!(sort_title("Pokémon: Red!"), "pokemon red");
        assert_eq!(sort_title("The"), "the");
    }

    #[test]
    fn platform_less_store_games_bucket_under_their_store_not_unknown() {
        let games = [
            game("a", "Dota 2", "steam", None, false),
            game("b", "Half-Life", "steam", None, false),
            game("c", "Shadow of the Colossus", "custom", Some("PS2"), false),
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
        // A stale filter yields empty, not the whole shelf.
        let gone = GroupKey::Platform("PS3".into());
        assert!(filtered(&games, SortKey::HostOrder, Some(&gone)).is_empty());
    }

    #[test]
    fn empty_and_single_group_libraries_are_not_worth_browsing() {
        assert!(!worth_browsing(&[]));
        assert!(!worth_browsing(&[game("l", "Steam", "steam", None, true)]));
        let one = [
            game("a", "Dota", "steam", None, false),
            game("b", "HL", "steam", None, false),
        ];
        assert!(!worth_browsing(&one));
        let two = [
            game("a", "Dota", "steam", None, false),
            game("b", "Ico", "custom", Some("PS2"), false),
        ];
        assert!(worth_browsing(&two));
        // Launchers never count: every library has them, and "Launchers" alone is one tile.
        let launcher_plus_one = [
            game("l", "Steam", "steam", None, true),
            game("a", "Dota", "steam", None, false),
        ];
        assert!(!worth_browsing(&launcher_plus_one));
    }

    #[test]
    fn sort_keys_parse_from_their_stored_names_and_unknown_falls_back() {
        assert_eq!(SortKey::parse("title"), SortKey::Title);
        assert_eq!(SortKey::parse("platform"), SortKey::Platform);
        assert_eq!(SortKey::parse("store"), SortKey::Store);
        for k in EVERY_SORT {
            assert_eq!(SortKey::parse(k.id()), k, "{} round-trips", k.label());
        }
        assert_eq!(SortKey::parse("something-newer"), SortKey::HostOrder);
        assert_eq!(SortKey::parse(""), SortKey::HostOrder);
        assert_eq!(SortKey::default(), SortKey::HostOrder);
    }

    #[test]
    fn vectors_match_the_shared_file() {
        let raw = include_str!("../../../clients/shared/library-collate-vectors.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("library-collate-vectors.json must parse");
        assert_eq!(
            file["version"], 1,
            "bump the reader when the file's version moves"
        );

        let games: Vec<LibraryGame> = file["library"]
            .as_array()
            .expect("library")
            .iter()
            .map(|e| LibraryGame {
                id: e["id"].as_str().expect("id").to_string(),
                title: e["title"].as_str().expect("title").to_string(),
                store: e["store"].as_str().expect("store").to_string(),
                launcher: e["role"].as_str() == Some("launcher"),
                icon: e["icon"].as_str().unwrap_or("").to_string(),
                platform: e["platform"].as_str().map(str::to_string),
                running: false,
            })
            .collect();
        let ids =
            |idx: &[usize]| -> Vec<&str> { idx.iter().map(|&i| games[i].id.as_str()).collect() };
        let str_list = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .expect("array")
                .iter()
                .map(|s| s.as_str().expect("string").to_string())
                .collect()
        };
        let key_of = |v: &serde_json::Value| -> GroupKey {
            let name = || v["name"].as_str().expect("name").to_string();
            match v["kind"].as_str().expect("kind") {
                "launchers" => GroupKey::Launchers,
                "platform" => GroupKey::Platform(name()),
                "store" => GroupKey::Store(name()),
                other => panic!("unknown group kind {other}"),
            }
        };
        let group_by = |v: &serde_json::Value| -> Option<GroupBy> {
            match v.as_str() {
                None => None,
                Some("platform") => Some(GroupBy::Platform),
                Some("store") => Some(GroupBy::Store),
                Some(other) => panic!("unknown group_by {other}"),
            }
        };

        for case in file["sort_title"].as_array().expect("sort_title") {
            let input = case["in"].as_str().expect("in");
            assert_eq!(
                sort_title(input),
                case["out"].as_str().expect("out"),
                "sort_title({input:?})"
            );
        }
        for (store, label) in file["store_labels"].as_object().expect("store_labels") {
            assert_eq!(
                store_label(store),
                label.as_str().expect("label"),
                "store_label({store:?})"
            );
        }
        for case in file["sort_keys"].as_array().expect("sort_keys") {
            let stored = case["stored"].as_str().expect("stored");
            assert_eq!(
                SortKey::parse(stored).id(),
                case["key"].as_str().expect("key"),
                "SortKey::parse({stored:?})"
            );
        }

        for case in file["cases"].as_array().expect("cases") {
            let name = case["name"].as_str().expect("name");
            let sort = SortKey::parse(case["sort"].as_str().expect("sort"));
            let got = collate(&games, sort, group_by(&case["group_by"]));
            let want = case["expect"].as_array().expect("expect");
            assert_eq!(got.len(), want.len(), "{name}: group count");
            for (g, w) in got.iter().zip(want) {
                assert_eq!(g.key, key_of(w), "{name}: group key");
                assert_eq!(
                    g.label,
                    w["label"].as_str().expect("label"),
                    "{name}: label"
                );
                assert_eq!(
                    ids(&g.games),
                    str_list(&w["ids"]),
                    "{name}: {} ids",
                    g.label
                );
            }
        }

        for case in file["filtered"].as_array().expect("filtered") {
            let name = case["name"].as_str().expect("name");
            let sort = SortKey::parse(case["sort"].as_str().expect("sort"));
            let filter = (!case["filter"].is_null()).then(|| key_of(&case["filter"]));
            assert_eq!(
                ids(&filtered(&games, sort, filter.as_ref())),
                str_list(&case["expect"]),
                "{name}"
            );
        }

        for case in file["worth_browsing"].as_array().expect("worth_browsing") {
            let name = case["name"].as_str().expect("name");
            let subset: Vec<LibraryGame> = match case["ids"].as_array() {
                None => games.clone(),
                Some(want) => want
                    .iter()
                    .map(|id| {
                        let id = id.as_str().expect("id");
                        games
                            .iter()
                            .find(|g| g.id == id)
                            .unwrap_or_else(|| panic!("{name}: unknown id {id}"))
                            .clone()
                    })
                    .collect(),
            };
            assert_eq!(
                worth_browsing(&subset),
                case["expect"].as_bool().expect("expect"),
                "{name}"
            );
        }
    }
}
