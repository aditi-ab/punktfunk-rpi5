//! Which folder ACLs a converge changes. An ACE with inheritance makes Windows rewrite every
//! file below the folder, so all the SIDs one folder needs share one walk per verb.

use std::collections::BTreeMap;

/// SID → folder → ACE.
pub(crate) type Aces<A> = BTreeMap<String, BTreeMap<String, A>>;

/// One folder's changes: the SIDs taken off in one walk, the ACEs put on in another.
#[derive(Debug, PartialEq)]
pub(crate) struct FolderStep<A> {
    pub(crate) path: String,
    pub(crate) remove: Vec<String>,
    pub(crate) place: Vec<(String, A)>,
}

/// What a converge does: the walks, and the record changes that need none.
#[derive(Debug, PartialEq)]
pub(crate) struct Plan<A> {
    pub(crate) steps: Vec<FolderStep<A>>,
    /// Wanted and already on the folder: recorded as placed.
    pub(crate) kept: Vec<(String, String, A)>,
    /// Stale and already off the folder: dropped from the record.
    pub(crate) dropped: Vec<(String, String)>,
}

/// The walks that turn `had` into `want`, folder by folder. A wanted ACE already `in_place`
/// needs none, and neither does a stale one already `gone`. Each predicate runs once per ACE.
pub(crate) fn plan<A: Copy>(
    want: &Aces<A>,
    had: &Aces<A>,
    mut in_place: impl FnMut(&str, &str, A) -> bool,
    mut gone: impl FnMut(&str, &str) -> bool,
) -> Plan<A> {
    let mut steps: BTreeMap<String, FolderStep<A>> = BTreeMap::new();
    let (mut kept, mut dropped) = (Vec::new(), Vec::new());
    for (sid, paths) in had {
        for path in paths.keys() {
            if want.get(sid).is_some_and(|w| w.contains_key(path)) {
                continue;
            }
            if gone(sid, path) {
                dropped.push((sid.clone(), path.clone()));
            } else {
                step(&mut steps, path).remove.push(sid.clone());
            }
        }
    }
    for (sid, paths) in want {
        for (path, &ace) in paths {
            if in_place(sid, path, ace) {
                kept.push((sid.clone(), path.clone(), ace));
            } else {
                step(&mut steps, path).place.push((sid.clone(), ace));
            }
        }
    }
    Plan {
        steps: steps.into_values().collect(),
        kept,
        dropped,
    }
}

fn step<'a, A>(
    steps: &'a mut BTreeMap<String, FolderStep<A>>,
    path: &str,
) -> &'a mut FolderStep<A> {
    steps.entry(path.to_string()).or_insert_with(|| FolderStep {
        path: path.to_string(),
        remove: Vec::new(),
        place: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aces(rows: &[(&str, &str, u8)]) -> Aces<u8> {
        let mut out = Aces::new();
        for &(sid, path, ace) in rows {
            out.entry(sid.to_string())
                .or_default()
                .insert(path.to_string(), ace);
        }
        out
    }

    #[test]
    fn one_folder_takes_every_sid_in_one_step() {
        let want = aces(&[("runner", "D:/lib", 1), ("pkg", "D:/lib", 1)]);
        let steps = plan(&want, &Aces::new(), |_, _, _| false, |_, _| false).steps;
        assert_eq!(
            steps,
            [FolderStep {
                path: "D:/lib".into(),
                remove: vec![],
                place: vec![("pkg".into(), 1), ("runner".into(), 1)],
            }]
        );
    }

    #[test]
    fn what_is_in_place_or_gone_costs_no_walk() {
        let had = aces(&[("runner", "D:/lib", 1), ("pkg", "D:/old", 1)]);
        let want = aces(&[("runner", "D:/lib", 1), ("pkg", "D:/lib", 1)]);
        // The decision placed both on D:/lib and took the package off D:/old itself.
        let quiet = plan(&want, &had, |_, p, _| p == "D:/lib", |_, p| p == "D:/old");
        assert!(quiet.steps.is_empty(), "{:?}", quiet.steps);
        assert_eq!(quiet.kept.len(), 2);
        assert_eq!(quiet.dropped, [("pkg".to_string(), "D:/old".to_string())]);
        // Nothing noted: D:/old loses the package, D:/lib gains it, the runner stays.
        let had_only =
            |sid: &str, path: &str, ace| had.get(sid).and_then(|p| p.get(path)) == Some(&ace);
        let steps = plan(&want, &had, had_only, |_, _| false).steps;
        assert_eq!(
            steps,
            [
                FolderStep {
                    path: "D:/lib".into(),
                    remove: vec![],
                    place: vec![("pkg".into(), 1)],
                },
                FolderStep {
                    path: "D:/old".into(),
                    remove: vec!["pkg".into()],
                    place: vec![],
                },
            ]
        );
    }

    #[test]
    fn a_changed_ace_is_placed_again() {
        let had = aces(&[("runner", "D:/lib", 2)]);
        let want = aces(&[("runner", "D:/lib", 1)]);
        let same =
            |sid: &str, path: &str, ace| had.get(sid).and_then(|p| p.get(path)) == Some(&ace);
        let steps = plan(&want, &had, same, |_, _| false).steps;
        assert_eq!(steps[0].place, [("runner".into(), 1)]);
        assert!(steps[0].remove.is_empty());
    }
}
