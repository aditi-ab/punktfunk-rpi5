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
//! - **Grow** (PW7a) — the mirror image, for the jumbo half: a connection whose discovery
//!   settles at the sealed JUMBO size has proven the path carries ~8.9 KB video datagrams, and
//!   the next session on that same path *starts* there instead of at the 1500-byte default.
//!   PyroWave sessions cannot be re-keyed mid-stream (the client's parse window is the
//!   `Welcome` value, read once over the C ABI), so the session-start value is the ONLY way
//!   they ever reach jumbo — and it is exactly where ~6× fewer datagrams per frame is worth
//!   the most. See [`jumbo_session_start`] for why a remembered verdict alone is never
//!   allowed to seal one byte above the default.

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

/// Identity of a PATH, not of a peer — the key the jumbo verdict is filed under.
///
/// The clamp above is keyed by peer IP alone, and that is safe *because being wrong is benign*:
/// a stale clamp only makes video datagrams smaller than they had to be. A stale GROW is the
/// opposite — one oversized datagram on a 1500-byte path is silently dropped, which is the
/// "connects fine, black screen forever" shape this whole module exists to kill. So the grow
/// keys strictly: a verdict earned over the host's 10 GbE NIC does not apply to the same peer
/// IP reached over the host's Wi-Fi or a VPN adapter, because those are different routes with
/// different MTUs.
///
/// `local` is `Connection::local_ip()` (the address the connection was actually received on);
/// `None` where the platform can't report it, which degrades this key to the clamp's — safely,
/// because the live re-proof in [`jumbo_session_start`] is what actually protects the grow.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct PathKey {
    local: Option<IpAddr>,
    peer: IpAddr,
}

/// A path that a completed MTU-discovery search proved carries jumbo video datagrams.
#[derive(Clone, Copy, Debug)]
struct JumboVerdict {
    /// The settled UDP-payload budget the proof measured.
    udp_budget: u16,
    /// The operator's jumbo target when the proof was taken. A changed `PUNKTFUNK_JUMBO` /
    /// `PUNKTFUNK_WIRE_MTU` invalidates it rather than being silently reinterpreted.
    target_wire_mtu: usize,
    /// When it was taken ([`JUMBO_VERDICT_TTL`]).
    at: std::time::Instant,
}

/// How long a jumbo verdict may be redeemed for. Contrary evidence erases it long before this
/// (any settle below the sealed target, on any later session over the same path — the same
/// self-correction the clamp has), so the TTL is not the safety mechanism; it is a bound on how
/// stale an *unrefreshed* memory can get, for the case where the path changes while no session
/// is running.
const JUMBO_VERDICT_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// How long the `Welcome` may wait for THIS connection's MTU discovery to re-prove a jumbo
/// path.
///
/// The wait is structural, not laziness: every connection restarts discovery from ~1200 bytes,
/// so the live proof the grow requires does not exist yet when the `Welcome` is built — and the
/// binary search up to sealed-jumbo needs an ACKED probe per step, each of which a peer may sit
/// on for its ack delay. Without a wait the gate would never pass and the feature would be dead.
///
/// It is honestly on the bring-up critical path (`handshake.rs` sends the `Welcome` and only
/// THEN kicks the display prep), so it is bounded, returns the instant the proof lands, and is
/// entered ONLY for a path a previous session already proved jumbo — i.e. an opted-in operator
/// on a jumbo LAN, never anyone else. The worst case (the full wait, no proof) is the moved
/// laptop, and it is self-limiting: that session's watcher erases the verdict, so the next
/// connect doesn't wait at all.
const JUMBO_PROOF_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
const JUMBO_PROOF_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Proven-jumbo paths. Same lifetime rules as [`learned`] — in-memory, re-earned in one session
/// after a host restart.
fn jumbo_verdicts() -> &'static Mutex<HashMap<PathKey, JumboVerdict>> {
    static JUMBO: OnceLock<Mutex<HashMap<PathKey, JumboVerdict>>> = OnceLock::new();
    JUMBO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn path_key(conn: &quinn::Connection) -> PathKey {
    PathKey {
        local: conn.local_ip(),
        peer: conn.remote_address().ip(),
    }
}

/// Everything the session-start jumbo decision reads. Every field but `proven_udp_budget` is
/// observed on THIS connection during THIS handshake — which is the point (see
/// [`jumbo_session_start`]).
#[derive(Clone, Copy, Debug)]
struct JumboStart {
    /// The host operator's opt-in ([`jumbo_wire_mtu`]) — `None` = no jumbo, ever.
    target_wire_mtu: Option<usize>,
    /// `Hello::max_shard_payload`: the client's own receive ceiling (0 = legacy client, which
    /// never gets a geometry it didn't ask for).
    client_ceiling: u16,
    /// `conn.stats().path.current_mtu` right now: the largest UDP payload quinn has had ACKED
    /// on this connection.
    live_udp_mtu: u16,
    /// What a previous session over this same [`PathKey`] settled at, if any.
    proven_udp_budget: Option<u16>,
    /// The constrained-path clamp [`learned`] for this peer, if any. Contradictory evidence
    /// (this peer black-screened on a small MTU recently) vetoes the grow — the two memories
    /// are keyed differently and the safe one wins.
    clamped_udp_budget: Option<u16>,
}

/// The jumbo shard payload a session to `peer` could use, or `None` when there is nothing to
/// gain (no opt-in, a legacy/low client ceiling, or a target that isn't bigger than the family
/// default). Shared by the decision, the wait, and the watcher so all three agree on the number.
fn jumbo_target(
    target_wire_mtu: Option<usize>,
    client_ceiling: u16,
    peer: IpAddr,
) -> Option<usize> {
    let mtu = target_wire_mtu?;
    let t = jumbo_shard_payload_for(mtu, peer).min(client_ceiling as usize);
    let t = t - t % 2; // FEC requires even shards
    (t > mtu1500_shard_payload_for(peer)).then_some(t)
}

/// The session-START jumbo decision: `Some(shard_payload)` only when every gate below holds.
///
/// **Why a remembered verdict is never enough.** A laptop that proved jumbo on the wired LAN
/// and comes back on Wi-Fi, a switch that lost its jumbo config, a client IP recycled by DHCP —
/// all of them present a path that cannot carry an 8.9 KB datagram, and a PyroWave session
/// sealed at that size cannot be re-keyed mid-stream, so it would black-screen for its whole
/// life. The memory therefore only decides whether it is worth WAITING for a proof; what
/// actually authorises the grow is `live_udp_mtu` — a datagram of exactly that size, acked by
/// this client, on this connection, seconds ago. That is why this is as safe as the clamp
/// despite the failure modes being opposite: a wrong memory cannot produce a jumbo `Welcome`,
/// only a live measurement can.
///
/// The gates, in order: the host operator opted in; the client advertised enough receive
/// headroom; the target beats the family default (nothing to gain otherwise); no constrained-path
/// clamp contradicts it; a prior session over this exact path settled at or above the sealed
/// target; and this connection has re-proven it live.
fn jumbo_session_start(i: JumboStart, peer: IpAddr) -> Option<usize> {
    let target = jumbo_target(i.target_wire_mtu, i.client_ceiling, peer)?;
    let sealed = sealed_datagram_bytes(target);
    if let Some(clamp) = i.clamped_udp_budget {
        if (clamp as usize) < sealed {
            return None;
        }
    }
    if (i.proven_udp_budget? as usize) < sealed {
        return None;
    }
    if (i.live_udp_mtu as usize) < sealed {
        return None;
    }
    Some(target)
}

/// The shard payload for a new session on `conn`: a proven-jumbo grow, else the
/// `PUNKTFUNK_WIRE_MTU` override, else the peer's learned path budget, else the family default
/// (today's exact behavior). Logs whenever the result differs from the default.
///
/// `client_ceiling` is the client's `Hello::max_shard_payload`. Async only for the bounded
/// [`JUMBO_PROOF_WAIT`], which is entered *only* on a path a previous session already proved
/// jumbo — every other session resolves without awaiting anything.
pub(super) async fn negotiated_shard_payload(
    conn: &quinn::Connection,
    client_ceiling: u16,
) -> usize {
    let peer = conn.remote_address().ip();
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
    let target_wire_mtu = jumbo_wire_mtu();
    let proven_udp_budget = fresh_verdict(path_key(conn), target_wire_mtu);
    let mut jumbo = JumboStart {
        target_wire_mtu,
        client_ceiling,
        live_udp_mtu: conn.stats().path.current_mtu,
        proven_udp_budget,
        clamped_udp_budget: learned_budget,
    };
    // A proven path is worth waiting a moment for: MTU discovery starts when the handshake
    // completes and needs an acked probe per binary-search step, so at `Welcome` time it may
    // simply not have got there yet. Bounded, and only on paths that already proved it once.
    let awaited_proof = proven_udp_budget
        .and_then(|_| jumbo_target(target_wire_mtu, client_ceiling, peer))
        .map(|t| sealed_datagram_bytes(t) as u16);
    if let Some(sealed) = awaited_proof {
        if jumbo.live_udp_mtu < sealed {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < JUMBO_PROOF_WAIT {
                tokio::time::sleep(JUMBO_PROOF_POLL).await;
                jumbo.live_udp_mtu = conn.stats().path.current_mtu;
                if jumbo.live_udp_mtu >= sealed {
                    break;
                }
            }
            tracing::debug!(
                peer = %peer,
                waited_ms = t0.elapsed().as_millis() as u64,
                live_udp_mtu = jumbo.live_udp_mtu,
                needed = sealed,
                "wire MTU: waited for this connection to re-prove its jumbo path"
            );
        }
    }
    resolve(env, learned_budget, jumbo, peer)
}

/// The peer's jumbo verdict if it is still redeemable: same operator target, inside the TTL.
/// A verdict that fails either test is dropped on the spot rather than left to rot.
fn fresh_verdict(key: PathKey, target_wire_mtu: Option<usize>) -> Option<u16> {
    let target = target_wire_mtu?;
    let mut map = jumbo_verdicts().lock().unwrap();
    let v = *map.get(&key)?;
    if v.target_wire_mtu != target || v.at.elapsed() > JUMBO_VERDICT_TTL {
        map.remove(&key);
        return None;
    }
    Some(v.udp_budget)
}

/// Pure resolution (proven jumbo > env override > learned budget > family default) — the tested
/// core of [`negotiated_shard_payload`].
fn resolve(
    env_wire_mtu: Option<usize>,
    learned_udp_budget: Option<u16>,
    jumbo: JumboStart,
    peer: IpAddr,
) -> usize {
    let default = mtu1500_shard_payload_for(peer);
    // First, because the two are mutually exclusive by construction: `jumbo_wire_mtu()` only
    // fires above 1500, and the env branch below CLAMPS to the family default, so a
    // `PUNKTFUNK_WIRE_MTU=9000` operator would otherwise get 1408 and never a jumbo start.
    if let Some(p) = jumbo_session_start(jumbo, peer) {
        tracing::info!(
            peer = %peer,
            shard_payload = p,
            default,
            live_udp_mtu = jumbo.live_udp_mtu,
            proven_udp_budget = jumbo.proven_udp_budget,
            "wire MTU: session starts at the JUMBO shard — this path proved it in a previous \
             session AND re-proved it live on this connection (~6× fewer datagrams per frame)"
        );
        return p;
    }
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
/// verdict grows it, ack-gated, when the operator opted in. The same settled-at-jumbo reading
/// also writes this path's next-session verdict (PW7a) — `client_ceiling` is the client's
/// `Hello::max_shard_payload`, which decides what "jumbo" is worth proving for this peer.
/// Spawned once per negotiated session; without a grow the task ends after the final sample
/// (bounded ~10 s lifetime, holding only a cheap `Connection` handle) — after a grow, or on a
/// session that STARTED jumbo, it stays as the revert guard until the connection closes.
pub(super) fn spawn_watch(
    conn: quinn::Connection,
    session_shard_payload: usize,
    client_ceiling: u16,
    reneg: Option<ShardReneg>,
) {
    tokio::spawn(async move {
        let peer = conn.remote_address().ip();
        let ceiling = video_datagram_udp_ceiling() as u16;
        // The sealed size a JUMBO proof has to reach on this path (PW7a) — `None` unless the
        // operator opted in AND this client advertised the headroom. Read once: the verdict
        // records the target it was proven under, and the two must be the same number.
        let target_wire_mtu = jumbo_wire_mtu();
        let jumbo_proof =
            jumbo_target(target_wire_mtu, client_ceiling, peer).map(sealed_datagram_bytes);
        // Discovery finishes in a handful of RTTs on a LAN (well under the first sample) but
        // needs a loss timeout per failed probe on a constrained path — the second sample
        // covers that with margin. Max, because discovery only ever raises `current_mtu`
        // (the post-grow revert guard below re-reads it live, where blackhole detection CAN
        // lower it again). Stop early only once nothing more is expected: with a jumbo opt-in
        // the search keeps climbing past the 1500-byte ceiling, and stopping there would throw
        // away the very measurement the proof needs.
        let goal = jumbo_proof
            .unwrap_or(ceiling as usize)
            .max(ceiling as usize) as u16;
        let mut settled = 0u16;
        for wait_s in [3u64, 7] {
            tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
            settled = settled.max(conn.stats().path.current_mtu);
            if settled >= goal {
                break;
            }
        }
        // The wire this session is CURRENTLY sealed at — moves on a mid-session shrink/grow.
        let mut current = session_shard_payload;
        let mut reneg = reneg;
        // PW7a bookkeeping, before anything else can return: this is where a jumbo path earns
        // its next-session verdict — and, far more importantly, where it LOSES it. Recording
        // needs a live connection that reached the sealed target; anything else (a lower
        // settle, a connection that died before the window closed, i.e. exactly what a client
        // staring at a black screen does) erases, so the next session falls back to the
        // 1500-byte default and has to prove itself again from scratch.
        if let Some(need) = jumbo_proof {
            let key = path_key(&conn);
            if settled as usize >= need && conn.close_reason().is_none() {
                jumbo_verdicts().lock().unwrap().insert(
                    key,
                    JumboVerdict {
                        udp_budget: settled,
                        target_wire_mtu: target_wire_mtu.unwrap_or_default(),
                        at: std::time::Instant::now(),
                    },
                );
                tracing::info!(peer = %peer, discovered_udp_mtu = settled, needed = need,
                    "wire MTU: this path carries JUMBO video datagrams — the next session over \
                     it starts at the big shard (it still has to re-prove the path live)");
            } else if jumbo_verdicts().lock().unwrap().remove(&key).is_some() {
                tracing::info!(peer = %peer, discovered_udp_mtu = settled, needed = need,
                    "wire MTU: jumbo verdict cleared — this path no longer proves it");
            }
        }
        if settled >= ceiling {
            // The path carries full-size video datagrams — erase any stale learned clamp so
            // the next session returns to the default wire.
            if learned().lock().unwrap().remove(&peer).is_some() {
                tracing::info!(peer = %peer,
                    "wire MTU: path re-measured at full size — learned clamp cleared");
            }
            // …but "full size" is the 1500-byte ceiling, and this session may have STARTED
            // above it (a PW7a jumbo start whose path changed since the proof, or a client
            // that roamed onto a 1500-MTU link). Then every video datagram is dying right now.
            // The verdict is already erased above; heal the live wire if this session can be
            // re-keyed at all — a PyroWave client cannot (its parse window is the `Welcome`
            // value), so for those the WARN plus a corrected next session is all there is.
            if sealed_datagram_bytes(current) > settled as usize {
                tracing::warn!(
                    peer = %peer,
                    discovered_udp_mtu = settled,
                    shard_payload = current,
                    "wire MTU: this session started at a JUMBO shard but the path does not \
                     carry it — video datagrams are oversized for a hop, which streams as a \
                     black screen with zero reported loss. The jumbo verdict for this path is \
                     cleared: the next connect starts at the standard 1500-byte wire."
                );
                if let Some(r) = reneg.as_ref() {
                    let back = shard_payload_for_udp_budget(settled as usize, peer);
                    if back < current
                        && r.change_tx.send(back as u16).is_ok()
                        && r.apply_tx.send(back).is_ok()
                    {
                        tracing::info!(peer = %peer, shard_payload = back, was = current,
                            "wire MTU: video re-keyed mid-session back to the standard wire");
                        current = back;
                    }
                }
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
        // PW7a revert guard for a session that STARTED jumbo and has no re-key channel (the
        // PyroWave case, and the only reason the session-start grow exists). Nothing can save
        // this session if the path stops fitting mid-stream — but the NEXT one must not repeat
        // it, so keep sampling and drop the verdict the moment quinn's blackhole detection or
        // a re-search says the path shrank. Cheap: one `Connection` handle, one sample per 5 s.
        // Only for a session that is currently FITTING — one that already failed the check
        // above has been warned about and had its verdict erased there.
        if current > mtu1500_shard_payload_for(peer)
            && reneg.is_none()
            && sealed_datagram_bytes(current) <= settled as usize
        {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if conn.close_reason().is_some() {
                    return;
                }
                let mtu_now = conn.stats().path.current_mtu;
                if (mtu_now as usize) < sealed_datagram_bytes(current) {
                    jumbo_verdicts().lock().unwrap().remove(&path_key(&conn));
                    tracing::warn!(peer = %peer, discovered_udp_mtu = mtu_now,
                        shard_payload = current,
                        "wire MTU: the jumbo path this session started on stopped fitting — this \
                         session cannot be re-keyed (chunk-aligned client parse window), so it \
                         will not recover, but the verdict is cleared and the next connect \
                         starts at the standard wire");
                    return;
                }
            }
        }
        // Phase 2 up-leg: jumbo grow — operator opt-in (PUNKTFUNK_JUMBO / PUNKTFUNK_WIRE_MTU
        // > 1500, which also raised the endpoint's probe ceiling so `settled` can even reach
        // here), client-advertised headroom, and a settled-at-jumbo proof. The grow is
        // ACK-GATED: not one sealed datagram above the old size leaves before the client's
        // ack, even though its buffers are statically sized — the rule must not erode.
        let (Some(mtu), Some(r)) = (target_wire_mtu, reneg.as_mut()) else {
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
    /// No jumbo anywhere — what every session that isn't on an opted-in jumbo LAN passes.
    const NO_JUMBO: JumboStart = JumboStart {
        target_wire_mtu: None,
        client_ceiling: 0,
        live_udp_mtu: 0,
        proven_udp_budget: None,
        clamped_udp_budget: None,
    };
    /// A 9000-MTU LAN, a modern client, a path proven last session and re-proven live now.
    fn proven_jumbo() -> JumboStart {
        JumboStart {
            target_wire_mtu: Some(9000),
            client_ceiling: punktfunk_core::config::max_shard_payload() as u16,
            live_udp_mtu: 8972,
            proven_udp_budget: Some(8972),
            clamped_udp_budget: None,
        }
    }

    #[test]
    fn default_when_nothing_known() {
        assert_eq!(
            resolve(None, None, NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(None, None, NO_JUMBO, V6),
            mtu1500_shard_payload_for(V6)
        );
    }

    #[test]
    fn env_override_beats_learned() {
        // 1280 wire − 28 IP/UDP − 64 header/crypto = 1188.
        assert_eq!(resolve(Some(1280), Some(1472), NO_JUMBO, V4), 1188);
    }

    #[test]
    fn learned_budget_clamps() {
        // A WARP-shaped path: 1280-byte UDP budget → 1280 − 64 = 1216.
        assert_eq!(resolve(None, Some(1280), NO_JUMBO, V4), 1216);
    }

    #[test]
    fn learned_at_or_above_ceiling_is_the_default_wire() {
        assert_eq!(
            resolve(None, Some(1472), NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(None, Some(2000), NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
    }

    #[test]
    fn env_full_mtu_is_the_default_wire_both_families() {
        assert_eq!(
            resolve(Some(1500), None, NO_JUMBO, V4),
            mtu1500_shard_payload_for(V4)
        );
        assert_eq!(
            resolve(Some(1500), None, NO_JUMBO, V6),
            mtu1500_shard_payload_for(V6)
        );
    }

    /// The happy path, both families: 9000 − 28 (IPv4) − 64 = 8908, and 9000 − 48 − 64 = 8888.
    #[test]
    fn proven_and_reproven_path_starts_jumbo() {
        assert_eq!(jumbo_session_start(proven_jumbo(), V4), Some(8908));
        let mut v6 = proven_jumbo();
        v6.live_udp_mtu = 8952;
        v6.proven_udp_budget = Some(8952);
        assert_eq!(jumbo_session_start(v6, V6), Some(8888));
        // …and it is what `resolve` returns, ahead of the env branch that would clamp a
        // >1500 `PUNKTFUNK_WIRE_MTU` back down to the family default.
        assert_eq!(resolve(Some(9000), None, proven_jumbo(), V4), 8908);
    }

    /// THE guard: the laptop that proved jumbo on the wired LAN and came back on a 1500-MTU
    /// link. The memory still says jumbo; the live connection says otherwise; the live one
    /// wins, every time. This is what makes the grow as safe as the clamp.
    #[test]
    fn a_remembered_verdict_never_grows_without_a_live_reproof() {
        let mut moved = proven_jumbo();
        moved.live_udp_mtu = 1472; // a clean 1500-MTU path, freshly measured
        assert_eq!(jumbo_session_start(moved, V4), None);
        assert_eq!(
            resolve(None, None, moved, V4),
            mtu1500_shard_payload_for(V4)
        );
        // Not even one byte of headroom short of the sealed target is enough.
        let mut nearly = proven_jumbo();
        nearly.live_udp_mtu = 8971;
        assert_eq!(jumbo_session_start(nearly, V4), None);
    }

    /// …and the mirror: a live-proven path with no prior verdict still starts at the default.
    /// Both halves are required, so a single fluke on either side cannot seal a jumbo wire.
    #[test]
    fn a_live_proof_alone_does_not_grow() {
        let mut first_ever = proven_jumbo();
        first_ever.proven_udp_budget = None;
        assert_eq!(jumbo_session_start(first_ever, V4), None);
        let mut weak_memory = proven_jumbo();
        weak_memory.proven_udp_budget = Some(1472);
        assert_eq!(jumbo_session_start(weak_memory, V4), None);
    }

    /// The two memories are keyed differently (clamp: peer; verdict: route), so they can
    /// disagree. When they do, the one that keeps datagrams small wins.
    #[test]
    fn a_constrained_path_clamp_vetoes_the_grow() {
        let mut contradicted = proven_jumbo();
        contradicted.clamped_udp_budget = Some(1280);
        assert_eq!(jumbo_session_start(contradicted, V4), None);
        // A clamp that is itself at or above the sealed target isn't contrary evidence.
        let mut roomy = proven_jumbo();
        roomy.clamped_udp_budget = Some(8972);
        assert_eq!(jumbo_session_start(roomy, V4), Some(8908));
    }

    #[test]
    fn without_the_operator_opt_in_nothing_grows() {
        let mut no_optin = proven_jumbo();
        no_optin.target_wire_mtu = None;
        assert_eq!(jumbo_session_start(no_optin, V4), None);
    }

    /// A legacy client (no `Hello::max_shard_payload`) is never handed a geometry it did not
    /// advertise, and a client whose ceiling lands under the family default is left alone
    /// rather than being "grown" to something smaller.
    #[test]
    fn the_client_ceiling_is_binding() {
        let mut legacy = proven_jumbo();
        legacy.client_ceiling = 0;
        assert_eq!(jumbo_session_start(legacy, V4), None);
        let mut small = proven_jumbo();
        small.client_ceiling = 1408;
        assert_eq!(jumbo_session_start(small, V4), None);
        // A ceiling between the default and the path target caps the grow — and the proof
        // then only has to cover the SMALLER sealed size.
        let mut capped = proven_jumbo();
        capped.client_ceiling = 4000;
        assert_eq!(jumbo_session_start(capped, V4), Some(4000));
    }

    /// Every shard payload the grow can produce is even (Leopard FEC splits shards in halves)
    /// and fits the receive ceiling every client sizes its buffers from.
    #[test]
    fn grown_shards_stay_even_and_inside_the_receive_ceiling() {
        for mtu in [2000usize, 4000, 4001, 9000, 9216, 64000] {
            for peer in [V4, V6] {
                let Some(t) = jumbo_target(Some(mtu), u16::MAX, peer) else {
                    continue;
                };
                assert_eq!(t % 2, 0, "odd shard for mtu {mtu}");
                assert!(t <= punktfunk_core::config::max_shard_payload());
                assert!(t > mtu1500_shard_payload_for(peer));
                assert!(
                    sealed_datagram_bytes(t) <= punktfunk_core::packet::MAX_DATAGRAM_BYTES,
                    "sealed datagram overflows the receive ceiling at mtu {mtu}"
                );
            }
        }
        // Below the family default there is nothing to grow to.
        assert_eq!(jumbo_target(Some(1500), u16::MAX, V4), None);
        assert_eq!(jumbo_target(None, u16::MAX, V4), None);
    }

    /// A path is a (local interface, peer) pair, not a peer: the same client reached over the
    /// host's other NIC is a different route with a different MTU.
    #[test]
    fn the_verdict_key_separates_routes_to_the_same_peer() {
        let over_10g = PathKey {
            local: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            peer: V4,
        };
        let over_wifi = PathKey {
            local: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            peer: V4,
        };
        assert_ne!(over_10g, over_wifi);
        assert_ne!(
            over_10g,
            PathKey {
                local: None,
                peer: V4
            }
        );
    }
}
