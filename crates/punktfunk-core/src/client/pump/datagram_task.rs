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
    pad_audio_tx: std::sync::mpsc::SyncSender<crate::quic::PadAudioFrame>,
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
    // Redundant-audio-plane rebuild (`0xD2`). Recovery happens HERE rather than in the four
    // client decoders: the recovered frame is re-inserted into this queue in order, so every
    // embedder gets a complete stream without knowing the plane exists.
    let mut audio_red = crate::audio::AudioRedRecovery::new();
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
            Some(&crate::quic::AUDIO_RED_MAGIC) => {
                if let Some((seq, pts_ns, opus, prev)) = crate::quic::decode_audio_red_datagram(&d)
                {
                    if audio_red.recover_before(seq, prev.is_some()) {
                        // The copy is the frame BEFORE this one, so it carries the previous
                        // sequence and presentation time — one protocol frame earlier.
                        let _ = audio_tx.try_send(AudioPacket {
                            seq: seq.wrapping_sub(1),
                            pts_ns: pts_ns
                                .saturating_sub(crate::audio::FRAME_MS as u64 * 1_000_000),
                            data: prev.unwrap_or_default().to_vec(),
                        });
                    }
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: opus.to_vec(),
                    });
                }
            }
            Some(&crate::quic::RUMBLE_MAGIC) => {
                if let Some(u) = crate::quic::decode_rumble_envelope(&d) {
                    // A pad index the client cannot represent is dropped outright, before either
                    // consumer sees it. It used to be waved through: the seq gate was skipped (its
                    // per-pad cursor has no slot for it) and it was handed to the legacy queue,
                    // while the policy engine silently discarded it on its own bounds check — so
                    // "both consumers are fed" below was false for exactly these, and an embedder
                    // draining the queue could be handed an index it would use to subscript its
                    // own per-pad array. The host never emits one; this is malformed or hostile.
                    let idx = u.pad as usize;
                    if idx >= crate::input::MAX_PADS {
                        continue;
                    }
                    // Gate v2 envelopes on their per-pad seq; forward v1 (envelope: None) as-is.
                    let fresh = match u.envelope {
                        Some(env) => {
                            if crate::input::GamepadSnapshot::seq_newer(
                                env.seq,
                                rumble_last_seq[idx],
                            ) {
                                rumble_last_seq[idx] = Some(env.seq);
                                true
                            } else {
                                false // reordered/duplicate — drop, keep the newer state
                            }
                        }
                        None => true,
                    };
                    if fresh {
                        let ttl = u.envelope.map(|e| e.ttl_ms);
                        // Both consumers are fed; an embedder drains exactly one of them
                        // (the legacy queue, or the policy engine's command API).
                        //
                        // Only the policy engine carries `u.left_trigger`/`u.right_trigger` (the
                        // v3 impulse-trigger tail). The legacy queue's tuple is the shape two
                        // frozen C entry points read through fixed out-params
                        // (`punktfunk_connection_next_rumble`/`_next_rumble2`), so it stays at the
                        // two handle levels forever: an out-of-tree embedder on those symbols must
                        // keep behaving exactly as it did. That is the §5 compatibility table's
                        // "new host, old client" cell, and it is now a per-API property rather
                        // than a per-client one — the same session can serve both.
                        let _ = rumble_tx.try_send((u.pad, u.low, u.high, ttl));
                        rumble_feed.wire_update(
                            u.pad,
                            u.low,
                            u.high,
                            u.left_trigger,
                            u.right_trigger,
                            ttl,
                        );
                    }
                }
            }
            Some(&crate::quic::HIDOUT_MAGIC) => {
                if let Some(h) = HidOutput::decode(&d) {
                    let _ = hidout_tx.try_send(h);
                }
            }
            Some(&crate::quic::PAD_AUDIO_MAGIC) => {
                if let Some(f) = crate::quic::decode_pad_audio_datagram(&d) {
                    let _ = pad_audio_tx.try_send(f);
                }
            }
            // The lossless plane feeds the SAME queue as `0xC9`, deliberately: the header is
            // identical by design, so seq/pts (and therefore the gap tracker, the de-jitter
            // policy and A/V sync) mean exactly what they mean on the Opus plane, and only the
            // payload format differs. Keeping one queue means the whole downstream pipeline —
            // `AUDIO_QUEUE`, `next_audio`, the in-core decode in `abi.rs` — is unchanged, and the
            // format that tells a consumer how to read `data` is the session-wide
            // `Welcome::audio_codec` rather than anything per-packet.
            //
            // A session runs one plane or the other for its whole life, so the two arms can never
            // interleave into that queue. `AudioRedRecovery` is not involved: `0xD2` redundancy is
            // undefined for this plane and never sent with it (it would double a bitrate that is
            // already the largest on the connection), so a lost datagram here is concealed by
            // `pcm::PcmConceal` at the decode site instead of reconstructed here.
            Some(&crate::quic::AUDIO_PCM_MAGIC) => {
                if let Some((seq, pts_ns, pcm)) = crate::quic::decode_audio_pcm_datagram(&d) {
                    let _ = audio_tx.try_send(AudioPacket {
                        seq,
                        pts_ns,
                        data: pcm.to_vec(),
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `0xD3` datagram must land in the SAME queue `0xC9` feeds, with its sequence and
    /// presentation time intact and its payload byte-for-byte what the host put on the wire.
    ///
    /// Driven through the REAL demux loop over a real QUIC connection rather than by calling the
    /// decoder directly, because the decoder is not what this arm adds: the arm is a tag, a sink
    /// and an absence (no `AudioRedRecovery`), and only the loop can be wrong about those. The
    /// endpoint pair is the one `endpoint`'s own MTU measurement uses; a single datagram over
    /// loopback costs milliseconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lossless_datagram_reaches_the_audio_sink() {
        let server = crate::quic::endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let client = crate::quic::endpoint::client_insecure().unwrap();
        let accept = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming");
            (server, incoming.await.expect("host side connects"))
        });
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server_ep, host_conn) = accept.await.unwrap();

        // Every plane's sink, so nothing the loop touches is a closed channel — the receivers
        // must outlive the task or `try_send` would fail for a reason the test does not intend.
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(8);
        let (rumble_tx, _rumble_rx) = std::sync::mpsc::sync_channel::<RumbleUpdate>(8);
        let (hidout_tx, _hidout_rx) = std::sync::mpsc::sync_channel(8);
        let (pad_audio_tx, _pad_audio_rx) = std::sync::mpsc::sync_channel(8);
        let (hdr_meta_tx, _hdr_meta_rx) = std::sync::mpsc::sync_channel(8);
        let (host_timing_tx, _host_timing_rx) = std::sync::mpsc::sync_channel(8);
        let (cursor_state_tx, _cursor_state_rx) = std::sync::mpsc::sync_channel(8);
        let rumble_feed =
            super::super::rumble::RumbleFeed(Arc::new(super::super::rumble::RumbleShared::new()));
        tokio::spawn(run(
            client_conn,
            audio_tx,
            rumble_tx,
            rumble_feed,
            hidout_tx,
            pad_audio_tx,
            hdr_meta_tx,
            host_timing_tx,
            Arc::new(Mutex::new(
                super::super::frame_channel::EncodeLatAcc::default(),
            )),
            cursor_state_tx,
        ));

        // One frame of 48 kHz/24-bit stereo, sized the way the host is REQUIRED to size it: from
        // the connection's own `max_datagram_size`, because this plane is never fragmented and an
        // oversized datagram is not sent at all. Hardcoding 5 ms here fails outright — 1440 B of
        // payload does not fit before MTU discovery settles, which is exactly the trap §4.2
        // warns the host about, reproduced by accident on the first attempt at this test.
        let bits = crate::audio::pcm::BITS_24;
        let max_dg = host_conn
            .max_datagram_size()
            .expect("datagrams are enabled");
        let frame_us =
            crate::audio::pcm::frame_us_for(crate::audio::SAMPLE_RATE_HZ, bits, 2, max_dg)
                .expect("some rung of the ladder fits");
        let n = crate::audio::pcm::samples_per_frame(crate::audio::SAMPLE_RATE_HZ, frame_us, 2);
        let samples: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.8).collect();
        let mut wire = Vec::new();
        crate::audio::pcm::from_f32(&samples, bits, &mut wire);
        assert_eq!(wire.len(), n * 3);
        host_conn
            .send_datagram(crate::quic::encode_audio_pcm_datagram(7, 1_234_567, &wire).into())
            .expect("datagram fits the path");

        let got = tokio::task::spawn_blocking(move || {
            audio_rx.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .unwrap()
        .expect("the 0xD3 arm must feed the audio sink");
        assert_eq!(got.seq, 7);
        assert_eq!(got.pts_ns, 1_234_567);
        assert_eq!(got.data, wire, "the payload must cross unmodified");
    }
}
