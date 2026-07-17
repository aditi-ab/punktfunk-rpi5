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
//!   position, or a shared/anonymous identity that has no slot) falls back to its auto-row origin, so
//!   a half-arranged group never collapses everything onto the origin.
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
    /// Pixel width, for auto-row `x` accumulation. Clamped at 0 (a bogus negative never shifts a
    /// sibling left).
    pub width: i32,
}

/// A member's resolved desktop-space top-left origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
}

/// The auto-row origin of member `i`: the summed width of every prior member, top-aligned.
fn auto_row_x(members: &[Member], i: usize) -> i32 {
    members[..i].iter().map(|m| m.width.max(0)).sum()
}

/// Arrange `members` (in acquire order) per `layout`, returning one [`Placement`] per member in the
/// same order. Pure — the single source of truth for auto-row / manual placement, shared by the
/// state readout and (KWin) the per-backend position apply.
pub fn arrange(members: &[Member], layout: &Layout) -> Vec<Placement> {
    members
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let auto = Placement {
                x: auto_row_x(members, i),
                y: 0,
            };
            match layout.mode {
                LayoutMode::AutoRow => auto,
                // A pinned member sits at its stored offset; an unpinned one falls back to auto-row.
                LayoutMode::Manual => m
                    .identity_slot
                    .and_then(|slot| layout.positions.get(&slot.to_string()))
                    .map(|p| Placement { x: p.x, y: p.y })
                    .unwrap_or(auto),
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
    fn manual_unpinned_and_slotless_fall_back_to_auto_row() {
        let members = [m(Some(1), 2560), m(Some(9), 1920), m(None, 1280)];
        // Only slot 1 is pinned; slot 9 has no stored pin; the third has no slot at all.
        let layout = manual(&[("1", 100, 50)]);
        let out = arrange(&members, &layout);
        assert_eq!(out[0], Placement { x: 100, y: 50 }, "pinned");
        assert_eq!(out[1], Placement { x: 2560, y: 0 }, "unpinned → auto-row");
        assert_eq!(out[2], Placement { x: 4480, y: 0 }, "slotless → auto-row");
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
