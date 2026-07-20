//! Input task: embedder events → QUIC datagrams. Toward a host that advertised
//! HOST_CAP_GAMEPAD_STATE, the per-transition gamepad events every embedder still emits are
//! folded into idempotent, sequence-numbered full-state snapshots (`GamepadSnapshot`): the
//! datagram plane drops and reorders (and sheds oldest-first at the 4 KiB send cap), so a lost
//! per-transition event would corrupt held pad state until the *next* change — a held trigger
//! stuck wrong indefinitely. Snapshots heal on the next send, the seq lets the host drop stale
//! reorders, and a periodic refresh of every touched pad bounds any loss to one refresh
//! interval — the same idempotent-state discipline as the host's 500 ms rumble refresh.
//! Keyboard/mouse/touch events pass through unchanged; an older host (no caps bit) keeps
//! getting the legacy per-transition gamepad events.

use super::super::*;
use super::*;

pub(super) async fn run(
    conn: quinn::Connection,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    gamepad_snapshots: bool,
) {
    use crate::input::{GamepadSnapshot, InputKind, MAX_PADS};
    // Touched pads only: an entry appears on the first gamepad event for that index, so the
    // refresh never conjures a virtual pad the embedder didn't drive.
    let mut pads: [Option<GamepadSnapshot>; MAX_PADS] = [None; MAX_PADS];
    // Per-pad wrapping seq that PERSISTS across a pad's remove/re-add on the same index (the
    // snapshot itself is cleared to `None` on removal). A removal takes `seq[idx] + 1` so it
    // supersedes every prior snapshot; the re-added pad's first snapshot takes the next value
    // after that, so the host's seq gate accepts it instead of rejecting a restarted-at-0 seq.
    let mut seq: [u8; MAX_PADS] = [0; MAX_PADS];
    // Re-sends of a removal still owed on refresh ticks (the removal rides the lossy datagram
    // plane; a single lost one would silently strand a ghost pad on the host — the exact bug
    // the removal fixes). Mirrors the host's rumble stop burst: a few time-spread re-sends,
    // each with a fresh (higher) seq, and canceled the moment the pad is driven again.
    const REMOVE_RESENDS: u8 = 2;
    let mut remove_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    // Per-pad declared controller kind ([`GamepadArrival`]) + its owed re-sends: the host needs
    // the kind before the pad's first frame to build a matching virtual device (mixed types), so
    // like the removal it rides the lossy plane with a small time-spread re-send burst.
    const ARRIVAL_RESENDS: u8 = 2;
    let mut arrival: [Option<u8>; MAX_PADS] = [None; MAX_PADS];
    let mut arrival_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    let mut refresh = tokio::time::interval(Duration::from_millis(100));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = input_rx.recv() => {
                let Some(ev) = ev else { break };
                let idx = ev.flags as usize;
                if gamepad_snapshots
                    && matches!(ev.kind, InputKind::GamepadButton | InputKind::GamepadAxis)
                    && idx < MAX_PADS
                {
                    // The pad is being driven — cancel any owed removal (a re-plug on this
                    // index; its fresh snapshot seq already supersedes the removal's).
                    remove_owed[idx] = 0;
                    let snap = pads[idx].get_or_insert(GamepadSnapshot {
                        pad: idx as u8,
                        ..Default::default()
                    });
                    // Unknown axis ids don't send (the host's legacy fold drops them too).
                    if snap.fold(&ev) {
                        seq[idx] = seq[idx].wrapping_add(1);
                        snap.seq = seq[idx];
                        let _ = conn
                            .send_datagram(snap.to_event().encode().to_vec().into());
                    }
                    continue;
                }
                if gamepad_snapshots && ev.kind == InputKind::GamepadRemove && idx < MAX_PADS {
                    // Stop refreshing the pad and forward a seq-stamped removal (in the shared
                    // seq space) so the host tears its virtual device down and no reordered
                    // snapshot can resurrect it; arm the re-send burst against datagram loss.
                    // Drop any owed kind declaration too — a re-plug on this index sends its own.
                    pads[idx] = None;
                    arrival[idx] = None;
                    arrival_owed[idx] = 0;
                    seq[idx] = seq[idx].wrapping_add(1);
                    remove_owed[idx] = REMOVE_RESENDS;
                    let rem = crate::input::InputEvent {
                        flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                        ..ev
                    };
                    let _ = conn.send_datagram(rem.encode().to_vec().into());
                    continue;
                }
                if gamepad_snapshots && ev.kind == InputKind::GamepadArrival && idx < MAX_PADS {
                    // Remember the declared kind (`code`) and forward it, arming a re-send burst
                    // so the host learns it before the pad's first frame even under loss.
                    arrival[idx] = Some(ev.code as u8);
                    arrival_owed[idx] = ARRIVAL_RESENDS;
                    let _ = conn.send_datagram(ev.encode().to_vec().into());
                    continue;
                }
                let _ = conn.send_datagram(ev.encode().to_vec().into());
            }
            _ = refresh.tick() => {
                for idx in 0..MAX_PADS {
                    // Re-send an owed kind declaration (independent of whether the pad has state
                    // yet — it may be idle-but-connected). Idempotent on the host.
                    if arrival_owed[idx] > 0 {
                        if let Some(kind) = arrival[idx] {
                            arrival_owed[idx] -= 1;
                            let arr = crate::input::InputEvent {
                                kind: InputKind::GamepadArrival,
                                _pad: [0; 3],
                                code: kind as u32,
                                x: 0,
                                y: 0,
                                flags: idx as u32,
                            };
                            let _ = conn.send_datagram(arr.encode().to_vec().into());
                        } else {
                            arrival_owed[idx] = 0;
                        }
                    }
                    if let Some(snap) = pads[idx].as_mut() {
                        seq[idx] = seq[idx].wrapping_add(1);
                        snap.seq = seq[idx];
                        let _ = conn.send_datagram(snap.to_event().encode().to_vec().into());
                    } else if remove_owed[idx] > 0 {
                        // Idempotent removal re-send with a fresh seq (the host drops it as a
                        // no-op once the pad is already gone, but a re-plug's later snapshot
                        // still wins by seq).
                        remove_owed[idx] -= 1;
                        seq[idx] = seq[idx].wrapping_add(1);
                        let rem = crate::input::InputEvent {
                            kind: InputKind::GamepadRemove,
                            _pad: [0; 3],
                            code: 0,
                            x: 0,
                            y: 0,
                            flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                        };
                        let _ = conn.send_datagram(rem.encode().to_vec().into());
                    }
                }
            }
        }
    }
}
