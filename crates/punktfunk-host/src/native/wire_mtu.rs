//! MTU resilience for the video data plane (the "connects fine, black screen forever" field
//! shape).
//!
//! Video datagrams are sealed at a per-session `shard_payload` sized for a clean 1500-byte MTU
//! (1472-byte UDP payloads). A host whose route to the client runs through a smaller-MTU hop —
//! a VPN/overlay adapter (Tailscale/WARP/ZeroTier default to 1280) claiming the LAN route, or a
//! lowered NIC MTU — delivers every SMALL flow (QUIC control, hole punch, input, audio) while
//! 100 % of video datagrams die by fragmentation or local `WSAEMSGSIZE`: the client sits on a
//! black screen reporting `loss_ppm=0` (it can't see gaps in packets it never saw any of) and
//! the host streams into the void with every gauge green. Neither side observes the failure
//! directly — but the control connection CAN: its MTU discovery probes up to exactly the sealed
//! video-datagram size ([`video_datagram_udp_ceiling`], set in `quic/endpoint.rs`), so its
//! settled MTU is a verdict on the path.
//!
//! Three legs, none of which changes a session on a healthy path:
//! - **`PUNKTFUNK_WIRE_MTU=<bytes>`** — operator override; the shard payload is derived from
//!   the given on-wire IP MTU. Wire-compatible with every deployed client:
//!   `Welcome::shard_payload` is already negotiated per session (the v4/v6 split ships two
//!   values today) and clients follow the negotiated value.
//! - **Watch** — a per-session task samples the control connection's discovered MTU once the
//!   search has had time to finish. A connection still alive that settled BELOW the ceiling is
//!   proof the path can't carry full-size video: log an actionable WARN and record the measured
//!   budget for the peer.
//! - **Heal** — the next handshake from that peer clamps `shard_payload` to the recorded
//!   budget, so a reconnect fixes the stream. A later session that reaches the ceiling erases
//!   the record (the learn/heal loop is self-correcting in both directions).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};

use punktfunk_core::config::{
    jumbo_shard_payload_for, jumbo_wire_mtu, mtu1500_shard_payload_for, sealed_datagram_bytes,
    shard_payload_for_udp_budget, shard_payload_for_wire_mtu, video_datagram_udp_ceiling,
};

/// Everything the MID-SESSION renegotiation driver needs (design/shard-payload-reneg.md
/// Phase 2) — `None` at [`spawn_watch`] makes the watcher observe-and-learn only (leg-1
/// behavior). Constructed ONLY when the client's `Hello::max_shard_payload` advertised
/// per-frame geometry AND the session's wire is not chunk-aligned: a PyroWave client parses
/// chunk-aligned AUs in windows of the `Welcome` value pinned at session start (Apple
/// `Stage2Pipeline` / `pf-client-core` video.rs read it once over the C ABI), so re-keying
/// such a session mid-stream would corrupt its parse — those sessions keep the leg-1
/// next-session clamp instead.
pub(super) struct ShardReneg {
    /// The client's advertised receive ceiling (bytes of shard; > 0 by construction).
    pub client_ceiling: u16,
    /// → control task (the control stream's sole writer): send `ShardPayloadChanged{n}`.
    pub change_tx: tokio::sync::mpsc::UnboundedSender<u16>,
    /// ← control task: the client's `ShardPayloadAck`s (the grow gate).
    pub ack_rx: tokio::sync::mpsc::UnboundedReceiver<u16>,
    /// → data plane: apply [`Session::set_shard_payload`] between AUs
    /// (drained next to `bitrate_rx` in the encode loop).
    pub apply_tx: std::sync::mpsc::Sender<usize>,
}

/// Measured UDP-payload budget per peer IP, learned from live control connections whose MTU
/// discovery settled below the video-datagram ceiling. In-memory only: a host restart
/// re-learns in one session, and entries self-correct (a later ceiling-hit erases, a lower
/// re-measure overwrites).
fn learned() -> &'static Mutex<HashMap<IpAddr, u16>> {
    static LEARNED: OnceLock<Mutex<HashMap<IpAddr, u16>>> = OnceLock::new();
    LEARNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shard payload for a new session to `peer`: `PUNKTFUNK_WIRE_MTU` override, else the
/// peer's learned path budget, else the family default (today's exact behavior). Logs whenever
/// the result differs from the default.
pub(super) fn negotiated_shard_payload(peer: IpAddr) -> usize {
    let env = match std::env::var("PUNKTFUNK_WIRE_MTU") {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(mtu) => Some(mtu),
            Err(_) => {
                tracing::warn!(value = %v, "PUNKTFUNK_WIRE_MTU is not a number — ignoring it");
                None
            }
        },
        Err(_) => None,
    };
    let learned_budget = learned().lock().unwrap().get(&peer).copied();
    resolve(env, learned_budget, peer)
}

/// Pure resolution (env override > learned budget > family default) — the tested core of
/// [`negotiated_shard_payload`].
fn resolve(env_wire_mtu: Option<usize>, learned_udp_budget: Option<u16>, peer: IpAddr) -> usize {
    let default = mtu1500_shard_payload_for(peer);
    if let Some(mtu) = env_wire_mtu {
        let p = shard_payload_for_wire_mtu(mtu, peer);
        if p != default {
            tracing::info!(
                wire_mtu = mtu,
                shard_payload = p,
                default,
                "wire MTU: shard payload set from PUNKTFUNK_WIRE_MTU"
            );
        }
        return p;
    }
    if let Some(budget) = learned_udp_budget {
        let p = shard_payload_for_udp_budget(budget as usize, peer);
        if p != default {
            tracing::info!(
                peer = %peer,
                udp_budget = budget,
                shard_payload = p,
                default,
                "wire MTU: shard payload clamped to this peer's measured path MTU (learned \
                 from a prior session's QUIC MTU discovery) — video datagrams now fit the \
                 constrained hop"
            );
            return p;
        }
    }
    default
}

/// Sample the control connection's discovered MTU after the search has settled and turn it
/// into a verdict — and, with a [`ShardReneg`] driver, act on it MID-SESSION
/// (design/shard-payload-reneg.md Phase 2): a below-ceiling verdict shrinks the live wire at
/// the ~3–10 s mark (session 1 heals instead of staying black), and a settled-at-jumbo
/// verdict grows it, ack-gated, when the operator opted in. Spawned once per negotiated
/// session; without a grow the task ends after the final sample (bounded ~10 s lifetime,
/// holding only a cheap `Connection` handle) — after a grow it stays as the revert guard
/// until the connection closes.
pub(super) fn spawn_watch(
    conn: quinn::Connection,
    session_shard_payload: usize,
    reneg: Option<ShardReneg>,
) {
    tokio::spawn(async move {
        let peer = conn.remote_address().ip();
        let ceiling = video_datagram_udp_ceiling() as u16;
        // Discovery finishes in a handful of RTTs on a LAN (well under the first sample) but
        // needs a loss timeout per failed probe on a constrained path — the second sample
        // covers that with margin. Max, because discovery only ever raises `current_mtu`
        // (the post-grow revert guard below re-reads it live, where blackhole detection CAN
        // lower it again).
        let mut settled = 0u16;
        for wait_s in [3u64, 7] {
            tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
            settled = settled.max(conn.stats().path.current_mtu);
            if settled >= ceiling {
                break;
            }
        }
        // The wire this session is CURRENTLY sealed at — moves on a mid-session shrink/grow.
        let mut current = session_shard_payload;
        let mut reneg = reneg;
        if settled >= ceiling {
            // The path carries full-size video datagrams — erase any stale learned clamp so
            // the next session returns to the default wire.
            if learned().lock().unwrap().remove(&peer).is_some() {
                tracing::info!(peer = %peer,
                    "wire MTU: path re-measured at full size — learned clamp cleared");
            }
        } else {
            // A closed connection stops discovering, so a session that ended before the final
            // sample proves nothing (a healthy high-RTT path could still be mid-search): learn
            // only from a connection that stayed alive through the whole window.
            if conn.close_reason().is_some() {
                return;
            }
            learned().lock().unwrap().insert(peer, settled);
            if sealed_datagram_bytes(current) <= settled as usize {
                // This session was already clamped small enough — the path is still constrained
                // (keep the record fresh) but video fits, so no alarm.
                tracing::info!(peer = %peer, discovered_udp_mtu = settled,
                    "wire MTU: constrained path re-measured; this session's video is sized to fit");
            } else {
                tracing::warn!(
                    peer = %peer,
                    discovered_udp_mtu = settled,
                    needed_udp_mtu = ceiling,
                    "wire MTU: this path CANNOT carry full-size video datagrams — the control \
                     plane works but every video packet is oversized for a hop, which streams as \
                     an endless black screen with zero reported loss. Typical cause: a VPN/overlay \
                     adapter (Tailscale / Cloudflare WARP / ZeroTier) claiming the LAN route, or a \
                     lowered NIC MTU — compare `ping <client> -f -l 1450` vs `-l 1200` and check \
                     `netsh interface ipv4 show subinterfaces` (Windows) / `ip link` (Linux). The \
                     measured budget is recorded: the NEXT session from this client sizes video to \
                     fit automatically. To pin it for all sessions set PUNKTFUNK_WIRE_MTU."
                );
                // Phase 2 down-leg: heal THIS session at the verdict mark. Shrink is sent
                // and applied immediately — per-frame pinning on the client makes ordering
                // irrelevant and smaller always fits; the ack is telemetry. The learned
                // record above still makes session 2 START right.
                if let Some(r) = reneg.as_ref() {
                    let target = shard_payload_for_udp_budget(settled as usize, peer);
                    if target < current
                        && r.change_tx.send(target as u16).is_ok()
                        && r.apply_tx.send(target).is_ok()
                    {
                        tracing::info!(
                            peer = %peer,
                            shard_payload = target,
                            was = current,
                            "wire MTU: video re-keyed mid-session to fit the constrained path \
                             — the stream heals now instead of on the next connect"
                        );
                        current = target;
                    }
                }
            }
        }
        // Phase 2 up-leg: jumbo grow — operator opt-in (PUNKTFUNK_JUMBO / PUNKTFUNK_WIRE_MTU
        // > 1500, which also raised the endpoint's probe ceiling so `settled` can even reach
        // here), client-advertised headroom, and a settled-at-jumbo proof. The grow is
        // ACK-GATED: not one sealed datagram above the old size leaves before the client's
        // ack, even though its buffers are statically sized — the rule must not erode.
        let (Some(mtu), Some(r)) = (jumbo_wire_mtu(), reneg.as_mut()) else {
            return;
        };
        let target = jumbo_shard_payload_for(mtu, peer).min(r.client_ceiling as usize);
        let target = target - target % 2;
        if target <= current || (settled as usize) < sealed_datagram_bytes(target) {
            return;
        }
        if r.change_tx.send(target as u16).is_err() {
            return;
        }
        let acked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(v) = r.ack_rx.recv().await {
                if v as usize == target {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if !acked {
            tracing::warn!(peer = %peer, shard_payload = target,
                "wire MTU: jumbo grow not acked — staying at the current wire");
            return;
        }
        if r.apply_tx.send(target).is_err() {
            return;
        }
        tracing::info!(
            peer = %peer,
            shard_payload = target,
            was = current,
            wire_mtu = mtu,
            "wire MTU: jumbo grow acked and applied — packets-per-frame cut ~6×"
        );
        current = target;
        // Revert guard: a mis-proven jumbo hop must self-correct instead of blackholing.
        // quinn's PMTU blackhole detection lowers `current_mtu` when the big packets start
        // vanishing; sample it and shrink back through the same path the down-leg uses.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if conn.close_reason().is_some() {
                return;
            }
            let mtu_now = conn.stats().path.current_mtu;
            if (mtu_now as usize) < sealed_datagram_bytes(current) {
                let back = shard_payload_for_udp_budget(mtu_now as usize, peer);
                tracing::warn!(peer = %peer, discovered_udp_mtu = mtu_now,
                    shard_payload = back, was = current,
                    "wire MTU: jumbo path stopped fitting — reverting the wire to match");
                if r.change_tx.send(back as u16).is_err() || r.apply_tx.send(back).is_err() {
                    return;
                }
                current = back;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
    const V6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

    #[test]
    fn default_when_nothing_known() {
        assert_eq!(resolve(None, None, V4), mtu1500_shard_payload_for(V4));
        assert_eq!(resolve(None, None, V6), mtu1500_shard_payload_for(V6));
    }

    #[test]
    fn env_override_beats_learned() {
        // 1280 wire − 28 IP/UDP − 64 header/crypto = 1188.
        assert_eq!(resolve(Some(1280), Some(1472), V4), 1188);
    }

    #[test]
    fn learned_budget_clamps() {
        // A WARP-shaped path: 1280-byte UDP budget → 1280 − 64 = 1216.
        assert_eq!(resolve(None, Some(1280), V4), 1216);
    }

    #[test]
    fn learned_at_or_above_ceiling_is_the_default_wire() {
        assert_eq!(resolve(None, Some(1472), V4), mtu1500_shard_payload_for(V4));
        assert_eq!(resolve(None, Some(2000), V4), mtu1500_shard_payload_for(V4));
    }

    #[test]
    fn env_full_mtu_is_the_default_wire_both_families() {
        assert_eq!(resolve(Some(1500), None, V4), mtu1500_shard_payload_for(V4));
        assert_eq!(resolve(Some(1500), None, V6), mtu1500_shard_payload_for(V6));
    }
}
