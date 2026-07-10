//! qWAVE (qos2.h) DSCP marking — the Windows path of [`super::qos::set_media_qos`].
//!
//! On Windows a plain `IP_TOS` setsockopt succeeds but the stack strips the mark from the wire:
//! marking requires membership in a qWAVE flow, which is how Apollo/Sunshine tag (qwave.dll).
//! [`QOSAddSocketToFlow`] with a traffic type yields the OS default marking (AudioVideo → DSCP
//! 40, Voice → 56, both WMM-mapped); the follow-up `QOSSetFlow(QOSSetOutgoingDSCPValue)` pins
//! the exact CS5/CS6 code points the other platforms mark. The pin needs an elevated process or
//! the "allow non-admin DSCP" group policy and silently keeps the traffic-type default
//! otherwise — the host runs as the SYSTEM service (`PunktfunkHost`), so the exact-DSCP path
//! applies exactly where it matters (the video egress); user-mode clients keep traffic-type
//! defaults (still WMM-useful).
//!
//! Same contract as the rest of [`super::qos`]: opt-in (`dscp_enabled`), and every step
//! debug-logs and continues — QoS is a nicety, never required for correctness.

use super::qos::MediaClass;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::NetworkManagement::QoS::{
    QOSAddSocketToFlow, QOSCreateHandle, QOSRemoveSocketFromFlow, QOSSetFlow,
    QOSSetOutgoingDSCPValue, QOSTrafficTypeAudioVideo, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW,
    QOS_VERSION,
};

/// The process-wide qWAVE handle (`QOSCreateHandle` once, cached). `None` = qWAVE unavailable
/// (e.g. the QWAVE service is disabled) — every flow request then no-ops. Deliberately never
/// closed: it lives as long as the process, like the media sockets whose flows it carries.
fn qos_handle() -> Option<HANDLE> {
    static SLOT: OnceLock<Option<usize>> = OnceLock::new();
    SLOT.get_or_init(|| {
        let version = QOS_VERSION {
            MajorVersion: 1,
            MinorVersion: 0,
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: both pointers are valid for the duration of the synchronous call.
        if unsafe { QOSCreateHandle(&version, &mut handle) } == 0 {
            tracing::debug!(
                err = unsafe { GetLastError() },
                "QOSCreateHandle failed — qWAVE DSCP marking unavailable"
            );
            None
        } else {
            Some(handle as usize)
        }
    })
    .map(|h| h as HANDLE)
}

/// RAII qWAVE flow membership: while held, the socket's egress carries the flow's marking;
/// dropping removes the socket from the flow. (Closing the socket also removes it implicitly —
/// the guard makes teardown explicit and ordered, and must outlive the socket's traffic.)
pub struct QosFlow {
    /// Raw `SOCKET` value only — never dereferenced; qWAVE tolerates an already-closed socket
    /// on remove (the error is deliberately ignored).
    socket: u64,
    flow_id: u32,
}

impl Drop for QosFlow {
    fn drop(&mut self) {
        if let Some(handle) = qos_handle() {
            // SAFETY: handle/flow_id came from the successful add; a stale socket just errors.
            unsafe { QOSRemoveSocketFromFlow(handle, self.socket as _, self.flow_id, 0) };
        }
    }
}

/// Put a **connected** media socket on a qWAVE flow of its class (video →
/// `QOSTrafficTypeAudioVideo`, audio → `QOSTrafficTypeVoice`), then best-effort pin the exact
/// DSCP the other platforms mark (CS5 = 40 / CS6 = 48). Returns the flow guard, or `None` when
/// a required step refused (logged at debug).
pub(super) fn add_media_flow(socket: &UdpSocket, class: MediaClass) -> Option<QosFlow> {
    let handle = qos_handle()?;
    let traffic_type = match class {
        MediaClass::Video => QOSTrafficTypeAudioVideo,
        MediaClass::Audio => QOSTrafficTypeVoice,
    };
    let raw = socket.as_raw_socket();
    let mut flow_id = 0u32;
    // NULL destination = derive the flow's 5-tuple from the connected socket (every media
    // socket is `connect`ed before it is tagged).
    // SAFETY: the socket is live for the call; `flow_id` is a valid out-pointer.
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle,
            raw as _,
            std::ptr::null(),
            traffic_type,
            QOS_NON_ADAPTIVE_FLOW,
            &mut flow_id,
        )
    };
    if ok == 0 {
        tracing::debug!(
            err = unsafe { GetLastError() },
            ?class,
            "QOSAddSocketToFlow failed — DSCP marking skipped"
        );
        return None;
    }
    // Construct the guard FIRST so an early return below still removes the flow membership.
    // (`raw` is already `u64` — std's `RawSocket` — so no cast; win64 clippy denies same-type casts.)
    let flow = QosFlow {
        socket: raw,
        flow_id,
    };
    // Pin the exact code point. Succeeds for elevated processes or under the "allow non-admin
    // DSCP" policy; otherwise the traffic-type default marking stands (40 / 56 — WMM-useful).
    let dscp: u32 = class.dscp();
    // SAFETY: `buffer` points at 4 valid bytes for the synchronous (no OVERLAPPED) call.
    let ok = unsafe {
        QOSSetFlow(
            handle,
            flow_id,
            QOSSetOutgoingDSCPValue,
            4,
            &dscp as *const u32 as *const _,
            0,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        tracing::debug!(
            err = unsafe { GetLastError() },
            ?class,
            "QOSSetFlow(OutgoingDSCPValue) refused — traffic-type default marking stands"
        );
    } else {
        tracing::debug!(?class, dscp, flow_id, "qWAVE flow pinned to exact DSCP");
    }
    Some(flow)
}
