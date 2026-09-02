//! Embedder events → QUIC datagrams.
//!
//! Toward `HOST_CAP_GAMEPAD_STATE`, per-transition gamepad events fold into seq-stamped
//! `GamepadSnapshot`s. The datagram plane drops, reorders, and sheds oldest-first at
//! the 4 KiB send cap, so a lost edge would leave a held trigger stuck until the next
//! change. Snapshots heal on the next send; seq drops stale reorders; a 100 ms refresh
//! of every touched pad bounds loss to one interval (host rumble refresh is the same
//! idea at 500 ms). Keyboard/mouse/touch pass through. An older host keeps the legacy
//! per-transition gamepad events. `HOST_CAP_PAD_AUDIO` gates flags bits 8/9; without
//! it the whole flags word is the pad index.

use super::*;

pub(super) async fn run(
    conn: quinn::Connection,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    gamepad_snapshots: bool,
    // HOST_CAP_PAD_AUDIO: only then do arrivals carry flags 8/9. An older host
    // reads the whole flags word as the pad index and would drop the kind.
    pad_audio: bool,
    // bit0 haptics, bit1 speaker. Fed by [`NativeClient::set_pad_audio_caps`] and
    // by arrival events that already carry the bits.
    pad_audio_caps: std::sync::Arc<[std::sync::atomic::AtomicU8; crate::input::MAX_PADS]>,
) {
    use crate::input::{GamepadSnapshot, InputKind, MAX_PADS};
    use std::sync::atomic::Ordering;
    // Slot appears on the first event for that index; refresh never invents a pad.
    let mut pads: [Option<GamepadSnapshot>; MAX_PADS] = [None; MAX_PADS];
    // Wrapping seq persists across remove/re-add on the same index. Removal takes
    // seq+1; a re-add continues, so the host does not reject a restarted-at-0 seq.
    let mut seq: [u8; MAX_PADS] = [0; MAX_PADS];
    // Removal re-sends still owed. A single lost removal strands a ghost pad; a few
    // time-spread seq-rising repeats, canceled the moment the pad is driven again.
    const REMOVE_RESENDS: u8 = 2;
    let mut remove_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    // Declared kind + owed re-sends. The host needs the kind before the first
    // frame (mixed types); same lossy-plane burst as removal.
    const ARRIVAL_RESENDS: u8 = 2;
    let mut arrival: [Option<u8>; MAX_PADS] = [None; MAX_PADS];
    let mut arrival_owed: [u8; MAX_PADS] = [0; MAX_PADS];
    // Caps the last arrival actually carried. `set_pad_audio_caps` cannot reach this
    // task, so the tick re-arms the burst when the live registry moves.
    let mut arrival_caps_sent: [u8; MAX_PADS] = [0; MAX_PADS];
    let caps_now = |idx: usize| -> u8 {
        if pad_audio {
            pad_audio_caps[idx].load(Ordering::Relaxed)
        } else {
            0
        }
    };
    // Index plus bits 8/9 toward a PAD_AUDIO host; else byte-identical to the index.
    let arrival_flags = |idx: usize| -> u32 {
        let caps = caps_now(idx);
        crate::input::encode_gamepad_arrival(idx as u8, caps)
    };
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
                    // Driven again: cancel owed removal (fresh snapshot seq already wins).
                    remove_owed[idx] = 0;
                    let snap = pads[idx].get_or_insert(GamepadSnapshot {
                        pad: idx as u8,
                        ..Default::default()
                    });
                    // Unknown axis: don't send (host legacy fold drops them too).
                    if snap.fold(&ev) {
                        seq[idx] = seq[idx].wrapping_add(1);
                        snap.seq = seq[idx];
                        let _ = conn
                            .send_datagram(snap.to_event().encode().to_vec().into());
                    }
                    continue;
                }
                if gamepad_snapshots && ev.kind == InputKind::GamepadRemove && idx < MAX_PADS {
                    // Seq-stamped removal in the shared seq space so no reorder resurrects
                    // the pad. Arm the burst; drop owed arrival (a re-plug sends its own).
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
                if gamepad_snapshots && ev.kind == InputKind::GamepadArrival {
                    // Index is the low byte; bits 8/9 may carry caps (raw events). Fold
                    // them into the registry so the burst keeps them.
                    let (pad, ev_caps) = crate::input::decode_gamepad_arrival(ev.flags);
                    let idx = pad as usize;
                    if idx < MAX_PADS {
                        if ev_caps != 0 {
                            pad_audio_caps[idx].fetch_or(ev_caps, Ordering::Relaxed);
                        }
                        // Kind + burst so the host learns it before the first frame under loss.
                        arrival[idx] = Some(ev.code as u8);
                        arrival_owed[idx] = ARRIVAL_RESENDS;
                        arrival_caps_sent[idx] = caps_now(idx);
                        let arr = crate::input::InputEvent {
                            flags: arrival_flags(idx),
                            ..ev
                        };
                        let _ = conn.send_datagram(arr.encode().to_vec().into());
                        continue;
                    }
                }
                let _ = conn.send_datagram(ev.encode().to_vec().into());
            }
            _ = refresh.tick() => {
                for idx in 0..MAX_PADS {
                    // Caps moved after the burst drained: re-arm. Live declared pads only;
                    // a steady session sends nothing.
                    if arrival[idx].is_some()
                        && arrival_owed[idx] == 0
                        && caps_now(idx) != arrival_caps_sent[idx]
                    {
                        arrival_owed[idx] = ARRIVAL_RESENDS;
                    }
                    // Owed kind, even if the pad is still idle. Idempotent on the host.
                    if arrival_owed[idx] > 0 {
                        if let Some(kind) = arrival[idx] {
                            arrival_owed[idx] -= 1;
                            arrival_caps_sent[idx] = caps_now(idx);
                            let arr = crate::input::InputEvent {
                                kind: InputKind::GamepadArrival,
                                _pad: [0; 3],
                                code: kind as u32,
                                x: 0,
                                y: 0,
                                flags: arrival_flags(idx),
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
                        // Fresh-seq removal. Host no-op if already gone; a re-plug still wins by seq.
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
