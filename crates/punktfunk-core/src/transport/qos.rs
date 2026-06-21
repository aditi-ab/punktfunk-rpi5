//! Shared UDP socket tuning for the media planes: send/recv buffer growth + best-effort link-layer
//! QoS.
//!
//! [`grow_socket_buffers`] is the `SO_SNDBUF`/`SO_RCVBUF` growth the native data plane applies; the
//! GameStream video/audio sockets reuse it so they don't go ENOBUFS-bound at high bitrate.
//!
//! [`set_media_qos`] DSCP-tags the latency-sensitive video/audio traffic (+ Linux `SO_PRIORITY`) so a
//! QoS-aware path (Wi-Fi WMM access categories, a managed switch, a shaped uplink) can prioritize it
//! over bulk flows. Mirrors what Apollo/Sunshine tag — DSCP **CS5** for video, **CS6** for audio. It
//! is **opt-in** (`PUNKTFUNK_DSCP=1`): DSCP can interact badly with some consumer ISPs/routers, and on
//! Windows a plain `IP_TOS` is silently stripped unless a qWAVE policy is active (Apollo uses the
//! qWAVE API there — that port is a follow-up; today this is a no-op on the wire on Windows).

use std::net::UdpSocket;

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
/// `punktfunk1.rs::paced_submit` — spreads a big frame's overflow, so this buffer mostly absorbs the
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
    const fn dscp(self) -> u32 {
        match self {
            MediaClass::Video => 40, // CS5
            MediaClass::Audio => 48, // CS6
        }
    }
}

/// Whether DSCP/QoS marking is enabled (`PUNKTFUNK_DSCP=1`). Off by default.
pub(crate) fn dscp_enabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_DSCP").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

/// Best-effort: tag `socket`'s outgoing packets for prioritized delivery of its media class. A no-op
/// unless `PUNKTFUNK_DSCP=1`. Every step is best-effort (failures logged at debug, never fatal) — QoS
/// is a nicety, not required for correctness.
///
/// IPv4 only (all current media sockets bind `0.0.0.0`); a v6 socket simply isn't tagged. On Windows
/// the `IP_TOS` set succeeds but the OS doesn't tag the wire without a qWAVE policy (follow-up).
pub fn set_media_qos(socket: &UdpSocket, class: MediaClass) {
    if dscp_enabled() {
        apply_media_qos(socket, class);
    }
}

/// The unconditional QoS application, factored out of [`set_media_qos`] so it is directly testable
/// without touching the process-global `PUNKTFUNK_DSCP` env. Best-effort (every step logs-and-continues).
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
        // No PUNKTFUNK_DSCP in the test env → early return; must not panic regardless.
        set_media_qos(&sock, MediaClass::Video);
        set_media_qos(&sock, MediaClass::Audio);
        grow_socket_buffers(&sock);
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
