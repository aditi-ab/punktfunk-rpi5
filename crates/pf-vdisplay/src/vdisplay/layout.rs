//! Pure display-**arrangement** engine (design: `design/display-management.md` §6.2). Given a
//! group's members (in acquire order) and the `layout` policy, compute each member's top-left
//! origin in the desktop coordinate space. No I/O, no OS types — the registry (for the
//! `/display/state` readout) and the per-backend position apply both consume it, so the auto-row /
//! manual math is defined and tested in exactly one place (the `pick_gamescope_mode` / `wiring_plan`
//! discipline).
//!
//! * **auto-row** — left-to-right in acquire order, top-aligned: member *i* sits at
//!   `x = Σ widths[0..i]`, `y = 0`. This is what compositors mostly do by default, made
//!   deterministic.
//! * **manual** — per-identity-slot offsets from [`Layout::positions`] (console-arranged): a member
//!   whose stable identity slot has a stored position sits there; a member with no pin (no stored
//!   position, or a shared/anonymous identity that has no slot) is **packed clear of the pins** —
//!   rowed left-to-right starting past the rightmost pinned edge — so a half-arranged group neither
//!   collapses everything onto the origin nor drops an unpinned display exactly on top of a pinned
//!   one. The pins themselves are reproduced verbatim: where two of them overlap, that is the
//!   operator's own arrangement and not ours to second-guess.
//!
//! Members carry no height, so "clear of the pins" is decided on the x axis alone and every pin
//! counts regardless of its `y` — a vertically-stacked arrangement therefore packs further right than
//! it strictly needs to. That is the conservative direction: a gap is a cosmetic waste of desktop
//! coordinate space, an overlap is two desktops fighting over the same pixels.
//!
//! Group membership + acquire order live in the registry ([`super::registry`]); this file only turns
//! that ordered member list into positions.

use super::policy::{Layout, LayoutMode};

/// One display in a group, as the arranger sees it (given in acquire order).
#[derive(Clone, Copy, Debug)]
pub struct Member {
    /// Stable per-client identity slot — the manual-layout key. `None` for a shared/anonymous
    /// identity (no per-client slot), which can't carry a manual pin and therefore always auto-rows.
    pub identity_slot: Option<u32>,
    /// The member's width **in the same coordinate space the resulting [`Placement`] is expressed
    /// in**, for row `x` accumulation. Clamped at 0 (a bogus negative never shifts a sibling left).
    ///
    /// ⚠ Every fill site currently uses the requested *mode* width, i.e. pixels. On Windows
    /// that is also the desktop space (CCD geometry is pixels), so the two agree; on KWin the
    /// placement is handed to `config.position()`, which is the compositor's **logical** space — the
    /// two coincide only at scale 1.0, and a per-output scale is exactly what the identity machinery
    /// exists to make KDE reapply. A 150 %-scaled 2560-wide output occupies 1707 logical px, so
    /// auto-rowing past it by 2560 leaves an 853-px dead band. Fixing that means dividing by the
    /// output's applied scale at the KWin fill site (`kwin_output_mgmt` already reads `scale` into
    /// its device state); this type stays unit-agnostic, and the contract is that whoever fills it
    /// speaks the consumer's space.
    pub width: i32,
}

/// A member's resolved desktop-space top-left origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
}

/// The auto-row origin of member `i`: the summed width of every prior member, top-aligned.
/// `saturating_add` because the widths are client-supplied through the requested mode — an absurd
/// one must produce an absurd coordinate, not a debug-build panic inside the state readout.
fn auto_row_x(members: &[Member], i: usize) -> i32 {
    members[..i]
        .iter()
        .fold(0i32, |x, m| x.saturating_add(m.width.max(0)))
}

/// The manual pin for `m`, if its identity slot carries one. The lookup is an exact string match on
/// the canonical decimal slot id — `DisplayPolicy::sanitized` re-keys the table to that form on
/// write, so a `"01"` typed into a hand-edited settings file still resolves here.
fn pin_of(m: &Member, layout: &Layout) -> Option<Placement> {
    m.identity_slot
        .and_then(|slot| layout.positions.get(&slot.to_string()))
        .map(|p| Placement { x: p.x, y: p.y })
}

/// Arrange `members` (in acquire order) per `layout`, returning one [`Placement`] per member in the
/// same order. Pure — the single source of truth for auto-row / manual placement, shared by the
/// state readout and (KWin) the per-backend position apply.
pub fn arrange(members: &[Member], layout: &Layout) -> Vec<Placement> {
    match layout.mode {
        LayoutMode::AutoRow => (0..members.len())
            .map(|i| Placement {
                x: auto_row_x(members, i),
                y: 0,
            })
            .collect(),
        LayoutMode::Manual => arrange_manual(members, layout),
    }
}

/// Manual placement: pins verbatim, everything else rowed out past them.
///
/// The unpinned fallback used to be the unconditional auto-row prefix sum — computed as if the pins
/// did not exist — so an unpinned display could land exactly on top of a pinned sibling with nothing
/// downstream noticing (the arrangement is only ever *reported* and *applied*, never validated). One
/// number in this crate's own fixture separated the tested case from that collision. Rowing the
/// unpinned members from the rightmost pinned edge instead makes the overlap unrepresentable while
/// keeping every property the fallback had: deterministic, acquire-ordered, and identical to plain
/// auto-row when nothing is pinned.
fn arrange_manual(members: &[Member], layout: &Layout) -> Vec<Placement> {
    let pins: Vec<Option<Placement>> = members.iter().map(|m| pin_of(m, layout)).collect();
    // Start the unpinned row at the desktop origin, or past the rightmost pinned edge when there is
    // one. `max(0)` on the width keeps a bogus negative from pulling the cursor back over a pin.
    let mut cursor = pins
        .iter()
        .zip(members)
        .filter_map(|(pin, m)| pin.map(|p| p.x.saturating_add(m.width.max(0))))
        .fold(0i32, i32::max);
    pins.iter()
        .zip(members)
        .map(|(pin, m)| match pin {
            Some(p) => *p,
            None => {
                let at = Placement { x: cursor, y: 0 };
                cursor = cursor.saturating_add(m.width.max(0));
                at
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Position;
    use std::collections::BTreeMap;

    fn m(slot: Option<u32>, width: i32) -> Member {
        Member {
            identity_slot: slot,
            width,
        }
    }

    fn manual(pairs: &[(&str, i32, i32)]) -> Layout {
        let mut positions = BTreeMap::new();
        for (k, x, y) in pairs {
            positions.insert(k.to_string(), Position { x: *x, y: *y });
        }
        Layout {
            mode: LayoutMode::Manual,
            positions,
        }
    }

    #[test]
    fn auto_row_accumulates_widths_top_aligned() {
        let members = [m(Some(1), 2560), m(Some(2), 1920), m(None, 1280)];
        let out = arrange(&members, &Layout::default()); // default = AutoRow
        assert_eq!(
            out,
            vec![
                Placement { x: 0, y: 0 },
                Placement { x: 2560, y: 0 },
                Placement { x: 4480, y: 0 },
            ]
        );
    }

    #[test]
    fn manual_honors_pins_by_identity_slot() {
        let members = [m(Some(1), 2560), m(Some(7), 1920)];
        // Client 7 arranged to the LEFT of client 1 (crossing order reversed vs auto-row).
        let layout = manual(&[("1", 1920, 0), ("7", 0, 0)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 1920, y: 0 });
        assert_eq!(out[1], Placement { x: 0, y: 0 });
    }

    #[test]
    fn manual_unpinned_and_slotless_pack_clear_of_the_pins() {
        let members = [m(Some(1), 2560), m(Some(9), 1920), m(None, 1280)];
        // Only slot 1 is pinned; slot 9 has no stored pin; the third has no slot at all.
        let layout = manual(&[("1", 100, 50)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 100, y: 50 }, "pinned");
        // The pin occupies [100, 2660); the unpinned members row out from its right edge in acquire
        // order, NOT from the pin-blind prefix sum (which would have put the first one at 2560 —
        // inside the pin).
        assert_eq!(
            out[1],
            Placement { x: 2660, y: 0 },
            "unpinned → past the pin"
        );
        assert_eq!(out[2], Placement { x: 4580, y: 0 }, "slotless → past both");
    }

    #[test]
    fn manual_with_no_pins_at_all_is_plain_auto_row() {
        // The fallback must not drift from auto-row when the manual table happens to be empty (the
        // state a group is in the instant `manual` is selected and nothing has been arranged yet).
        let members = [m(Some(1), 2560), m(Some(2), 1920), m(None, 1280)];
        let out = arrange(&members, &manual(&[]));
        assert_eq!(out, arrange(&members, &Layout::default()));
    }

    #[test]
    fn a_manual_pin_that_would_collide_with_an_auto_row_sibling_is_packed_clear() {
        // The exact geometry §13 11.8 names: a pin sitting where the pin-blind auto-row would have
        // put the unpinned sibling. Two displays on one origin = two desktops on the same pixels.
        let members = [m(Some(1), 2560), m(Some(9), 1920)];
        let layout = manual(&[("1", 2560, 0)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 2560, y: 0 }, "pin honored verbatim");
        assert_ne!(
            out[1], out[0],
            "the unpinned sibling must not land on the pin"
        );
        assert_eq!(
            out[1],
            Placement { x: 5120, y: 0 },
            "past the pin's right edge"
        );
    }

    #[test]
    fn a_pin_left_of_the_origin_still_leaves_the_unpinned_row_at_zero() {
        // A negative pin is legal (KWin's global space extends left of 0). Its right edge is what
        // matters: at -3000+2560 = -440 it constrains nothing, so the row still starts at the origin.
        let members = [m(Some(1), 2560), m(Some(9), 1920)];
        let out = arrange(&members, &manual(&[("1", -3000, 0)]));
        assert_eq!(out[0], Placement { x: -3000, y: 0 });
        assert_eq!(out[1], Placement { x: 0, y: 0 });
    }

    #[test]
    fn absurd_widths_saturate_instead_of_panicking() {
        // Widths originate in the client-requested mode; a hostile or corrupt one must produce an
        // absurd coordinate, not an overflow panic inside the `/display/state` readout.
        let members = [m(Some(1), i32::MAX), m(Some(2), i32::MAX), m(None, 4096)];
        let out = arrange(&members, &Layout::default());
        assert_eq!(out[2], Placement { x: i32::MAX, y: 0 });
        let out = arrange(&members, &manual(&[("1", i32::MAX, 0)]));
        assert_eq!(out[2], Placement { x: i32::MAX, y: 0 });
    }

    /// Property (deterministic seeded walk): across arbitrary member widths, slot assignments and pin
    /// tables, **no unpinned member may share desktop space with any sibling**. Overlap between two
    /// *pins* is excluded from the invariant — that is the operator's own arrangement, faithfully
    /// reproduced. Members carry no height, so "share space" is decided on the x interval alone,
    /// which is the strictest reading available here.
    #[test]
    fn no_unpinned_member_overlaps_a_sibling_under_any_layout() {
        // Tiny deterministic LCG (Numerical Recipes) — reproducible, no dependency. Same shape as
        // `lifecycle`'s property walk.
        let mut rng: u64 = 0x0bad_f00d_dead_beef;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };

        for _ in 0..20_000 {
            let count = (next() % 6) as usize;
            let members: Vec<Member> = (0..count)
                .map(|_| {
                    // A slot only sometimes, and from a small pool so collisions with the pin table
                    // are frequent; widths include 0 and the odd negative.
                    let slot = match next() % 4 {
                        0 => None,
                        _ => Some(next() % 6 + 1),
                    };
                    let width = match next() % 8 {
                        0 => 0,
                        1 => -((next() % 4000) as i32),
                        _ => (next() % 4000) as i32,
                    };
                    m(slot, width)
                })
                .collect();
            let mut pairs: Vec<(String, i32, i32)> = Vec::new();
            for slot in 1..=6u32 {
                if next() % 2 == 0 {
                    let x = (next() % 8000) as i32 - 2000;
                    let y = ((next() % 3) * 1440) as i32;
                    pairs.push((slot.to_string(), x, y));
                }
            }
            let borrowed: Vec<(&str, i32, i32)> =
                pairs.iter().map(|(k, x, y)| (k.as_str(), *x, *y)).collect();

            for layout in [Layout::default(), manual(&borrowed)] {
                let out = arrange(&members, &layout);
                assert_eq!(out.len(), members.len());
                let pinned: Vec<bool> = members
                    .iter()
                    .map(|mem| pin_of(mem, &layout).is_some())
                    .collect();
                for i in 0..out.len() {
                    for j in (i + 1)..out.len() {
                        if pinned[i] && pinned[j] {
                            continue; // the operator's own arrangement
                        }
                        let span = |k: usize| {
                            let x = out[k].x as i64;
                            (x, x + members[k].width.max(0) as i64)
                        };
                        let (ai, bi) = span(i);
                        let (aj, bj) = span(j);
                        // Empty spans (a zero/negative width) can't collide with anything.
                        if ai >= bi || aj >= bj {
                            continue;
                        }
                        assert!(
                            bi <= aj || bj <= ai,
                            "members {i} {:?} and {j} {:?} overlap under {layout:?} \
                             (widths {} / {})",
                            out[i],
                            out[j],
                            members[i].width,
                            members[j].width
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_group_is_empty() {
        assert!(arrange(&[], &Layout::default()).is_empty());
        assert!(arrange(&[], &manual(&[("1", 0, 0)])).is_empty());
    }

    #[test]
    fn negative_width_never_shifts_siblings_left() {
        let members = [m(Some(1), -100), m(Some(2), 1920)];
        let out = arrange(&members, &Layout::default());
        let origin = Placement { x: 0, y: 0 };
        assert_eq!(out[0], origin);
        assert_eq!(out[1], origin, "clamped width contributes 0");
    }
}
