//! Equal shares of one path, for the host that can see every session on it.
//!
//! A client's controller reads one session's window and cannot tell its own
//! wall from a sibling's queue. The host can: sessions from one client address
//! form a group, and [`shares`] divides what the group is getting between the
//! Automatic ones. A fixed-rate session takes what it is set to and is never
//! told anything; an idle one lends what it is not using and takes it back
//! when it asks.
//!
//! A share is a ceiling, not a target. The client still earns every step up to
//! it through its own evidence, and nothing here moves a share by less than
//! the band. One address is one NAT and not proof of one bottleneck, so the
//! arithmetic is wrong in the safe direction: a member that is getting what it
//! asked for is never cut, whatever a sibling is doing on its own air.
//!
//! The host calls this on every delivery report and the simulator calls it on
//! its own clock. One implementation, two callers — which is what lets the
//! simulator show that the host's loop above the client's does not oscillate.

use super::controller::FLOOR_KBPS;
use std::time::Duration;

/// How often an up-move may go out. Down-moves apply as soon as the evidence
/// does; a share that climbed every report window would be the client's own
/// growth law again, one round-trip behind it.
pub const SHARE_CLOCK: Duration = Duration::from_secs(5);

/// A share moves only when it differs from the standing one by more than a
/// tenth. Under that the two controllers trade the same kilobits back and
/// forth for the life of the session.
const SHARE_BAND_DIV: u32 = 10;

/// Delivered within an eighth of the rate it is set to: the path is carrying
/// what this session offers it. The same band the decode cap calls "the same
/// rate".
const SHORT_DIV: u32 = 8;

/// A share of `0` releases the ceiling: the group is gone, or this session is
/// alone on the path again.
pub const NO_SHARE_KBPS: u32 = 0;

/// One session of a group, as the host knows it.
///
/// Every field is something the host has without asking the client: the
/// encoder target it set, the delivered rate its `DeliveryReport`s come to,
/// and whether it is repeating a keepalive instead of encoding motion.
#[derive(Clone, Copy, Debug)]
pub struct Member {
    /// `false` = an explicit bitrate. Never clamped, never acked.
    pub automatic: bool,
    /// Encoder target, kbps.
    pub current_kbps: u32,
    /// What reached the client over the last report window, kbps.
    pub delivered_kbps: u32,
    /// The host is sending keepalive repeats: this session is not asking for
    /// its share.
    pub idle: bool,
    /// The share this session was last told. `None` = it has never had one.
    pub share_kbps: Option<u32>,
}

/// The path refused some of what this session offered it.
fn short(m: &Member) -> bool {
    !m.idle && m.delivered_kbps < m.current_kbps - m.current_kbps / SHORT_DIV
}

/// Shares for a group, in the members' own order. `Some(kbps)` is a ceiling to
/// send; `None` leaves the standing one alone.
///
/// `may_raise` is the [`SHARE_CLOCK`] tick: without it only cuts go out.
/// Fewer than two members is not a group, and every standing share is
/// released — which is also what keeps a single session's decisions
/// byte-identical to an ungoverned build.
pub fn shares(members: &[Member], may_raise: bool) -> Vec<Option<u32>> {
    let mut out = vec![None; members.len()];
    if members.len() < 2 {
        for (o, m) in out.iter_mut().zip(members) {
            if m.automatic && m.share_kbps.is_some() {
                *o = Some(NO_SHARE_KBPS);
            }
        }
        return out;
    }
    let auto: Vec<usize> = (0..members.len())
        .filter(|&i| members[i].automatic)
        .collect();
    if auto.is_empty() {
        return out;
    }
    let budget = budget_kbps(members);
    let equal = (budget / auto.len() as u64) as u32;
    // Max-min fair: a member that wants less than an equal share takes what it
    // wants, and the rest split what it left behind.
    let mut order = auto.clone();
    order.sort_by_key(|&i| demand(&members[i], equal));
    let (mut left, mut rest) = (auto.len() as u64, budget);
    for &i in &order {
        let take = u64::from(demand(&members[i], equal)).min(rest / left);
        rest -= take;
        left -= 1;
        // Never under what this one is already delivering: it proved the path
        // had that much, and cutting it would punish the evidence.
        let share = (take as u32).max(members[i].delivered_kbps).max(FLOOR_KBPS);
        out[i] = send(members[i].share_kbps, share, may_raise);
    }
    out
}

/// What there is to divide.
///
/// The group's own delivery, because that is the one wall every member is
/// measuring together and it is re-taken every window (L1). A wall one member
/// measured beside a sibling read that sibling's residual, not the path.
/// Nothing about a session that is getting what it asked for says the path is
/// full, so a group with no short member asks for a notch more; one short
/// member pins the budget at what arrived. A fixed-rate session's rate comes
/// off the top: it is not in the division.
fn budget_kbps(members: &[Member]) -> u64 {
    let delivered: u64 = members.iter().map(|m| u64::from(m.delivered_kbps)).sum();
    let headroom: u64 = if members.iter().any(short) {
        0
    } else {
        members
            .iter()
            .filter(|m| !m.idle)
            .map(|m| u64::from(m.current_kbps) / 8)
            .sum()
    };
    let fixed: u64 = members
        .iter()
        .filter(|m| !m.automatic)
        .map(|m| u64::from(m.current_kbps))
        .sum();
    (delivered + headroom).saturating_sub(fixed)
}

/// What a member would use if the path were free. `u32::MAX` = everything it
/// can get.
///
/// A session that is idle, still, or held under its share by its own decoder
/// lends the difference; one the path is refusing bytes to is not lending, it
/// is being starved, and asks for the whole share.
fn demand(m: &Member, equal_kbps: u32) -> u32 {
    if m.idle {
        return m.delivered_kbps + m.delivered_kbps / 4;
    }
    if !short(m) && m.current_kbps < equal_kbps {
        return m.current_kbps + m.current_kbps / 8;
    }
    u32::MAX
}

/// Whether this share is worth an ack: outside the band, and a rise only on
/// the clock.
fn send(standing: Option<u32>, share: u32, may_raise: bool) -> Option<u32> {
    let Some(old) = standing else {
        return Some(share);
    };
    if share.abs_diff(old) <= old / SHARE_BAND_DIV {
        return None;
    }
    (share < old || may_raise).then_some(share)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(current_kbps: u32, delivered_kbps: u32) -> Member {
        Member {
            automatic: true,
            current_kbps,
            delivered_kbps,
            idle: false,
            share_kbps: None,
        }
    }

    /// Two Automatic sessions on a path carrying 18 Mbps split it, and a pair
    /// already sitting at that share is left alone.
    #[test]
    fn two_automatic_sessions_split_what_the_path_delivers() {
        let g = [auto(9_000, 9_000), auto(9_000, 9_000)];
        assert_eq!(shares(&g, true), [Some(10_125), Some(10_125)]);
        let held = [
            Member {
                share_kbps: Some(10_000),
                ..g[0]
            },
            Member {
                share_kbps: Some(10_000),
                ..g[1]
            },
        ];
        assert_eq!(shares(&held, true), [None, None], "inside the band");
    }

    /// The fixed session's rate comes off the top and it is never told
    /// anything; the Automatic one gets what is left, which is the cut that
    /// drains the queue it built.
    #[test]
    fn a_fixed_rate_session_is_never_touched_and_takes_its_rate_off_the_top() {
        let fixed = Member {
            automatic: false,
            current_kbps: 8_000,
            delivered_kbps: 8_000,
            idle: false,
            share_kbps: None,
        };
        let out = shares(&[auto(14_000, 10_000), fixed], true);
        assert_eq!(out[1], None, "a fixed session never gets a governor ack");
        assert_eq!(
            out[0],
            Some(10_000),
            "18 Mbps delivered less the fixed 8 leaves the Automatic one 10"
        );
    }

    /// An idle sibling lends what it is not using, and asks for it back by
    /// producing frames again.
    #[test]
    fn an_idle_session_lends_its_share_and_takes_it_back() {
        let still = Member {
            idle: true,
            ..auto(9_000, 300)
        };
        let out = shares(&[auto(9_000, 9_000), still], true);
        assert_eq!(out[1], Some(FLOOR_KBPS), "the lender keeps only the floor");
        assert!(
            out[0].is_some_and(|k| k > 9_000),
            "the active session takes the rest: {out:?}"
        );
        let back = [auto(9_000, 9_000), auto(9_000, 9_000)];
        assert_eq!(shares(&back, true), [Some(10_125), Some(10_125)]);
    }

    /// A member held under its share by its own bounds lends the difference;
    /// one the path is refusing bytes to is starved, not lending, and keeps
    /// its whole share.
    #[test]
    fn a_self_bounded_member_lends_and_a_starved_one_does_not() {
        let out = shares(&[auto(4_000, 4_000), auto(14_000, 14_000)], true);
        assert_eq!(out[0], Some(4_500), "its own bound plus a notch");
        assert!(out[1].is_some_and(|k| k > 14_000), "the rest: {out:?}");

        let out = shares(&[auto(12_000, 4_000), auto(14_000, 14_000)], true);
        assert_eq!(out[0], Some(9_000), "half of the 18 Mbps that arrived");
    }

    /// Delivering above its share proves the path had more: the budget grows
    /// with the evidence and the member is never cut for producing it.
    #[test]
    fn a_member_delivering_above_its_share_raises_the_budget() {
        let g = [
            Member {
                share_kbps: Some(9_000),
                ..auto(14_000, 14_000)
            },
            Member {
                share_kbps: Some(9_000),
                ..auto(9_000, 4_000)
            },
        ];
        let out = shares(&g, true);
        assert!(
            out[0].is_some_and(|k| k >= 14_000),
            "the clean member keeps what it delivered: {out:?}"
        );
    }

    /// A newcomer opens on the share the path has room for rather than the
    /// rate it negotiated blind, and its sibling comes down to what is
    /// reaching it instead of cascading.
    #[test]
    fn a_newcomer_gets_a_share_without_flooring_its_sibling() {
        let sibling = Member {
            share_kbps: Some(17_000),
            ..auto(17_000, 12_000)
        };
        let out = shares(&[sibling, auto(20_000, 6_000)], true);
        assert_eq!(out[0], Some(12_000), "down to what arrived, no further");
        assert_eq!(out[1], Some(9_000), "the newcomer opens at half the path");
    }

    /// The band and the clock: a small move never goes out, a rise waits for
    /// the clock, a cut does not.
    #[test]
    fn a_share_moves_by_more_than_the_band_and_rises_only_on_the_clock() {
        assert_eq!(send(Some(10_000), 10_900, true), None, "inside the band");
        assert_eq!(send(Some(10_000), 12_000, true), Some(12_000));
        assert_eq!(send(Some(10_000), 12_000, false), None, "a rise waits");
        assert_eq!(send(Some(10_000), 8_000, false), Some(8_000), "a cut now");
        assert_eq!(send(None, 8_000, false), Some(8_000), "the first share");
    }

    /// One session is not a group: nothing is governed, and a session left
    /// alone on the path gets its ceiling back at once.
    #[test]
    fn one_session_is_not_a_group() {
        assert_eq!(shares(&[auto(20_000, 9_000)], true), [None]);
        let survivor = Member {
            share_kbps: Some(9_000),
            ..auto(9_000, 9_000)
        };
        assert_eq!(shares(&[survivor], false), [Some(NO_SHARE_KBPS)]);
    }
}
