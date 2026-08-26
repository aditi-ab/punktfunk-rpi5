//! Shared UDP socket tuning for the media planes: send/recv buffer growth + best-effort link-layer
//! QoS.
//!
//! [`grow_socket_buffers`] is the `SO_SNDBUF`/`SO_RCVBUF` growth the native data plane applies; the
//! GameStream video/audio sockets reuse it so they don't go ENOBUFS-bound at high bitrate.
//!
//! [`set_media_qos`] DSCP-tags the latency-sensitive video/audio traffic (+ Linux `SO_PRIORITY`) so a
//! QoS-aware path (Wi-Fi WMM access categories, a managed switch, a shaped uplink) can prioritize it
//! over bulk flows. Mirrors what Apollo/Sunshine tag — DSCP **CS5** for video, **CS6** for audio.
//! Default: **on toward private-network peers** (RFC1918 / ULA / link-local / loopback — ABR
//! overhaul RFC §2.5), where the AP-side WMM mapping is a real airtime-priority win and the
//! documented bleach/reject risk (some consumer ISPs/routers on the WAN path) cannot apply; off
//! toward anything routable. `PUNKTFUNK_DSCP=1` forces it on everywhere, `=0` is the kill switch,
//! and [`set_dscp_default`] (the Android low-latency tie-in) still forces it on regardless of the
//! peer. On Windows a plain `IP_TOS` is silently stripped from the wire, so the marking
//! goes through qWAVE flows instead (see [`super::qos_windows`]) — the caller holds the returned
//! [`QosFlow`] guard for as long as the socket sends media.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};

/// Target kernel socket-buffer size (`SO_SNDBUF`/`SO_RCVBUF`). A high-resolution frame is a burst (a
/// 5120×1440 keyframe is ~130 packets the send thread hands to `sendmmsg` at once); the default UDP
/// buffer (~208 KB on Linux) overflows on it, which EAGAINs the host send (dropping packets) or drops
/// on the client recv — and with infinite-GOP a single lost frame freezes the decode until the next
/// RFI refresh. Requested large; the OS clamps to `net.core.{wmem,rmem}_max` (Linux) /
/// `kern.ipc.maxsockbuf` (macOS).
///
/// Sized for 1 Gbps+: at ~1.2 Gbps on the wire an 8 MB buffer is only ~49 ms of steady state, and a
/// single multi-MB IDR keyframe (~4 MB ≈ 3300 packets) instantly fills most of it. 32 MB gives ~200 ms
/// of headroom and absorbs a keyframe burst without EAGAIN/ENOBUFS drops. (Paced sending —
/// `native.rs::paced_submit` — spreads a big frame's overflow, so this buffer mostly absorbs the
/// immediate microburst rather than a whole unpaced frame.)
pub(crate) const TARGET_SOCKBUF: usize = 32 * 1024 * 1024;

/// Best-effort grow of `SO_SNDBUF`/`SO_RCVBUF` to [`TARGET_SOCKBUF`]. A failure isn't fatal (the
/// stream just runs lossier); a grant far below the request means the OS cap is too low for clean
/// 4K/5K streaming, so warn with the knob to raise.
pub fn grow_socket_buffers(socket: &UdpSocket) {
    let sock = socket2::SockRef::from(socket);
    let _ = sock.set_send_buffer_size(TARGET_SOCKBUF);
    let _ = sock.set_recv_buffer_size(TARGET_SOCKBUF);
    // The kernel reports back the (possibly clamped, Linux-doubled) granted size.
    let granted = sock
        .send_buffer_size()
        .unwrap_or(0)
        .min(sock.recv_buffer_size().unwrap_or(0));
    if granted < TARGET_SOCKBUF / 4 {
        tracing::warn!(
            granted_kb = granted / 1024,
            "UDP socket buffer capped well below target — high-resolution streaming may drop \
             frames; raise net.core.wmem_max / net.core.rmem_max (Linux) for clean 4K/5K"
        );
    }
}

/// Media class of a socket — selects the DSCP code point (and Linux `SO_PRIORITY`), matching Apollo's
/// mapping: video = CS5, audio = CS6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaClass {
    Video,
    Audio,
}

impl MediaClass {
    /// DSCP code point (the high 6 bits of the IPv4 TOS / IPv6 traffic-class byte).
    pub(super) const fn dscp(self) -> u32 {
        match self {
            MediaClass::Video => 40, // CS5
            MediaClass::Audio => 48, // CS6
        }
    }
}

/// Embedder force-on for DSCP marking when `PUNKTFUNK_DSCP` is unset (see [`set_dscp_default`]).
/// `false` (the default) is AUTO — marking toward private-network peers only, the RFC §2.5
/// default; `true` marks toward every peer (the caller has judged its path, e.g. the Android
/// low-latency mode over a VPN the address math can't recognize as local).
static DSCP_DEFAULT: AtomicBool = AtomicBool::new(false);

/// Force DSCP marking on for sockets created from now on (or back to the private-peer AUTO
/// default with `false`). Must be called BEFORE connecting — the tag is applied at socket
/// creation. The Android client ties this to its experimental low-latency mode;
/// `PUNKTFUNK_DSCP` still overrides in either direction.
pub fn set_dscp_default(enabled: bool) {
    DSCP_DEFAULT.store(enabled, Ordering::Relaxed);
}

/// The DSCP decision (pure — unit-tested): the env override wins in either direction
/// (`1`/`true`/`on` forces on everywhere, `0`/`false`/`off` is the kill switch — e.g. to rule
/// QoS out while debugging a flaky AP); else the embedder's force-on; else AUTO — mark exactly
/// when the peer is a private-network address.
fn dscp_decision(env: Option<&str>, embedder_on: bool, peer_private: bool) -> bool {
    match env {
        Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        _ => embedder_on || peer_private,
    }
}

/// [`dscp_decision`] against the process env, the embedder default, and the socket's connected
/// peer.
pub(crate) fn dscp_enabled_for(peer: Option<std::net::SocketAddr>) -> bool {
    dscp_decision(
        std::env::var("PUNKTFUNK_DSCP").ok().as_deref(),
        DSCP_DEFAULT.load(Ordering::Relaxed),
        peer.is_some_and(|p| is_private_peer(&p)),
    )
}

/// A peer address the local network owns: RFC1918 / link-local / loopback IPv4, and
/// loopback / ULA (`fc00::/7`) / link-local (`fe80::/10`) IPv6, including v4-mapped forms.
/// The DSCP bleach/reject risk lives on ISP paths — none of these ever cross one.
fn is_private_peer(addr: &std::net::SocketAddr) -> bool {
    fn v4(ip: std::net::Ipv4Addr) -> bool {
        ip.is_private() || ip.is_loopback() || ip.is_link_local()
    }
    match addr.ip() {
        std::net::IpAddr::V4(ip) => v4(ip),
        std::net::IpAddr::V6(ip) => {
            let seg0 = ip.segments()[0];
            ip.is_loopback()
                || (seg0 & 0xfe00) == 0xfc00
                || (seg0 & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

/// RAII token for a socket's QoS marking. On Windows it is the qWAVE flow membership
/// ([`super::qos_windows::QosFlow`]) — dropping it removes the marking, so hold it for as long
/// as the socket sends media. Elsewhere DSCP rides the socket option itself and the token is
/// inert (and never constructed — [`set_media_qos`] returns `None`).
#[cfg(windows)]
pub use super::qos_windows::QosFlow;
#[cfg(not(windows))]
pub struct QosFlow {
    _never: std::convert::Infallible,
}

/// Best-effort: tag `socket`'s outgoing packets for prioritized delivery of its media class. A
/// no-op toward a non-private peer unless forced (see [`dscp_decision`]). Every step is
/// best-effort (failures logged at debug, never fatal) — QoS is a nicety, not required for
/// correctness.
///
/// The socket must already be `connect`ed (Windows derives the qWAVE flow from the connected
/// 5-tuple; the private-peer default reads the same connected address). IPv4 only (all current
/// media sockets bind `0.0.0.0`); a v6 socket simply isn't tagged. Returns the [`QosFlow`]
/// guard on Windows — keep it alive with the socket; `None` elsewhere (the marking is a plain
/// socket option) and whenever a step refused.
pub fn set_media_qos(socket: &UdpSocket, class: MediaClass) -> Option<QosFlow> {
    if !dscp_enabled_for(socket.peer_addr().ok()) {
        return None;
    }
    #[cfg(windows)]
    {
        super::qos_windows::add_media_flow(socket, class)
    }
    #[cfg(not(windows))]
    {
        apply_media_qos(socket, class);
        None
    }
}

/// The unconditional QoS application, factored out of [`set_media_qos`] so it is directly testable
/// without touching the process-global `PUNKTFUNK_DSCP` env. Best-effort (every step logs-and-continues).
#[cfg_attr(windows, allow(dead_code))]
fn apply_media_qos(socket: &UdpSocket, class: MediaClass) {
    let sock = socket2::SockRef::from(socket);
    // DSCP occupies the high 6 bits of the TOS byte → shift left 2.
    if let Err(e) = sock.set_tos_v4(class.dscp() << 2) {
        tracing::debug!(error = %e, ?class, "set IP_TOS (DSCP) failed — QoS marking skipped");
    }
    // SO_PRIORITY must be set AFTER IP_TOS (setting TOS resets SO_PRIORITY to 0 on Linux). Linux-only;
    // 6 is the highest priority allowed without CAP_NET_ADMIN, so video=5 / audio=6 (Apollo's scheme).
    #[cfg(target_os = "linux")]
    {
        let prio = match class {
            MediaClass::Video => 5,
            MediaClass::Audio => 6,
        };
        if let Err(e) = sock.set_priority(prio) {
            tracing::debug!(error = %e, "set SO_PRIORITY failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dscp_code_points_match_apollo() {
        // CS5 video / CS6 audio, shifted into the TOS byte (high 6 bits).
        assert_eq!(MediaClass::Video.dscp(), 40);
        assert_eq!(MediaClass::Audio.dscp(), 48);
        assert_eq!(MediaClass::Video.dscp() << 2, 0xA0);
        assert_eq!(MediaClass::Audio.dscp() << 2, 0xC0);
    }

    #[test]
    fn qos_and_buffer_growth_are_best_effort_and_never_panic() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Unconnected socket: no peer → no locality → the AUTO default stays off (and no
        // PUNKTFUNK_DSCP in the test env); must not panic regardless.
        assert!(set_media_qos(&sock, MediaClass::Video).is_none());
        assert!(set_media_qos(&sock, MediaClass::Audio).is_none());
        grow_socket_buffers(&sock);
    }

    /// RFC §2.5: the default marks exactly the peers the local network owns — the env still
    /// wins in either direction and the embedder hook still forces on (a VPN path the address
    /// math can't recognize).
    #[test]
    fn dscp_defaults_on_for_private_peers_only() {
        // The pure decision.
        assert!(dscp_decision(Some("1"), false, false));
        assert!(
            !dscp_decision(Some("0"), true, true),
            "the kill switch beats everything"
        );
        assert!(dscp_decision(None, true, false), "embedder force-on");
        assert!(
            dscp_decision(None, false, true),
            "AUTO: a private peer marks"
        );
        assert!(
            !dscp_decision(None, false, false),
            "AUTO: a routable peer does not"
        );

        // The address classifier.
        use std::net::SocketAddr;
        for a in [
            "192.168.1.20:47999",
            "10.0.0.7:1",
            "172.16.3.4:5",
            "169.254.10.1:2",
            "127.0.0.1:9",
            "[fe80::1]:1",
            "[fd12:3456::1]:1",
            "[::1]:1",
            "[::ffff:192.168.1.2]:1",
        ] {
            assert!(
                is_private_peer(&a.parse::<SocketAddr>().unwrap()),
                "{a} is local"
            );
        }
        for a in [
            "1.1.1.1:53",
            "84.23.10.9:47999",
            "172.32.0.1:1", // one past RFC1918's 172.16/12
            "[2001:db8::1]:1",
            "[::ffff:8.8.8.8]:1",
        ] {
            assert!(
                !is_private_peer(&a.parse::<SocketAddr>().unwrap()),
                "{a} is routable"
            );
        }
    }

    /// The AUTO path end to end: a CONNECTED loopback socket has a private peer, so the
    /// default now marks it (the unconnected socket above stays unmarked — no peer).
    #[test]
    fn a_connected_loopback_socket_marks_by_default() {
        let target = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(target.local_addr().unwrap()).unwrap();
        let _ = set_media_qos(&sock, MediaClass::Video);
        #[cfg(target_os = "linux")]
        {
            let s = socket2::SockRef::from(&sock);
            assert_eq!(s.tos_v4().unwrap(), 0xA0, "AUTO marked the private peer");
        }
    }

    #[test]
    fn apply_qos_tags_the_socket() {
        // Exercise the enabled path directly (no env), and read the options back where we can.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        apply_media_qos(&sock, MediaClass::Video);
        #[cfg(target_os = "linux")]
        {
            let s = socket2::SockRef::from(&sock);
            assert_eq!(s.tos_v4().unwrap(), 0xA0, "video → CS5 in the TOS byte");
            assert_eq!(s.priority().unwrap(), 5, "video → SO_PRIORITY 5");
        }
    }
}
