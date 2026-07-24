//! Datagram demux: host → client audio/rumble (try_send: a lagging embedder drops the
//! newest packet rather than backing up the QUIC receive path).

use super::*;

// One parameter per demuxed plane — grouping them into a struct would just move the field
// list one hop away from the single call site.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    conn: quinn::Connection,
    audio_tx: std::sync::mpsc::SyncSender<AudioPacket>,
    rumble_tx: std::sync::mpsc::SyncSender<RumbleUpdate>,
    rumble_feed: super::super::rumble::RumbleFeed,
    hidout_tx: std::sync::mpsc::SyncSender<crate::quic::HidOutput>,
    hdr_meta_tx: std::sync::mpsc::SyncSender<crate::quic::HdrMeta>,
    host_timing_tx: std::sync::mpsc::SyncSender<crate::quic::HostTiming>,
    // The ABR encode signal's accumulator (see [`EncodeLatAcc`]) — fed HERE, not off
    // `host_timing_tx`: that channel is the overlay's, lossy and embedder-drained.
    encode_lat: Arc<Mutex<super::super::frame_channel::EncodeLatAcc>>,
    cursor_state_tx: std::sync::mpsc::SyncSender<crate::quic::CursorState>,
) {
    // Per-pad reorder gate for v2 rumble envelopes (the seq analog of the host's gamepad-state
    // gate): a datagram the network reordered must not roll a stopped motor back on. Legacy v1
    // datagrams carry no seq and bypass it (an old host's own periodic re-send is the only heal).
    let mut rumble_last_seq: [Option<u8>; crate::input::MAX_PADS] = [None; crate::input::MAX_PADS];
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&crate::quic::AUDIO_MAGIC) => {
                if let Some((seq, pts_ns, opus)) = crate::quic::decode_audio_datagram(&d) {
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                    });
                }
            }
            Some(&crate::quic::RUMBLE_MAGIC) => {
                if let Some(u) = crate::quic::decode_rumble_envelope(&d) {
                    // Gate v2 envelopes on their per-pad seq; forward v1 (envelope: None) as-is.
                    let fresh = match u.envelope {
                        Some(env) => {
                            let idx = u.pad as usize;
                            if idx < crate::input::MAX_PADS {
                                if crate::input::GamepadSnapshot::seq_newer(
                                    env.seq,
                                    rumble_last_seq[idx],
                                ) {
                                    rumble_last_seq[idx] = Some(env.seq);
                                    true
                                } else {
                                    false // reordered/duplicate — drop, keep the newer state
                                }
                            } else {
                                true // out-of-range pad (host never sends these): no gate
                            }
                        }
                        None => true,
                    };
                    if fresh {
                        let ttl = u.envelope.map(|e| e.ttl_ms);
                        // Both consumers are fed; an embedder drains exactly one of them
                        // (the legacy queue, or the policy engine's command API).
                        let _ = rumble_tx.try_send((u.pad, u.low, u.high, ttl));
                        rumble_feed.wire_update(u.pad, u.low, u.high, ttl);
                    }
                }
            }
            Some(&crate::quic::HIDOUT_MAGIC) => {
                if let Some(h) = HidOutput::decode(&d) {
                    let _ = hidout_tx.try_send(h);
                }
            }
            Some(&crate::quic::HDR_META_MAGIC) => {
                if let Some(m) = crate::quic::decode_hdr_meta_datagram(&d) {
                    let _ = hdr_meta_tx.try_send(m);
                }
            }
            Some(&crate::quic::HOST_TIMING_MAGIC) => {
                if let Some(t) = crate::quic::decode_host_timing_datagram(&d) {
                    if let Some(s) = &t.stages {
                        let mut acc = encode_lat.lock().unwrap();
                        acc.sum_us += s.encode_us as u64;
                        acc.count += 1;
                    }
                    let _ = host_timing_tx.try_send(t);
                }
            }
            Some(&crate::quic::CURSOR_STATE_MAGIC) => {
                if let Some(s) = crate::quic::decode_cursor_state_datagram(&d) {
                    let _ = cursor_state_tx.try_send(s);
                }
            }
            _ => {} // unknown tag — a newer host; ignore
        }
    }
}
