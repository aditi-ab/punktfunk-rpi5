use super::reassemble::LOSS_WINDOW_NS;
use super::*;
use crate::config::{Config, FecScheme};
use crate::crypto::SessionKey;
use crate::fec::coder_for;
use crate::stats::StatsCounters;
use zerocopy::{FromBytes, IntoBytes};

fn limits() -> ReassemblerLimits {
    // `min == max` pins the whole stream to 16-byte shards — the strictest geometry, so the
    // firewall tests below exercise the bounds checks; per-frame-pinning tests build their own
    // limits with a real range. Derived per-frame block ceiling: 4096/16 = 256 shards → 32.
    ReassemblerLimits {
        min_shard_bytes: 16,
        max_shard_bytes: 16,
        max_data_shards: 8,
        max_total_shards: 12,
        max_frame_bytes: 4096,
    }
}

fn base_header() -> PacketHeader {
    PacketHeader {
        pts_ns: 0,
        frame_index: 0,
        stream_seq: 0,
        frame_bytes: 16,
        user_flags: 0,
        block_index: 0,
        block_count: 1,
        data_shards: 1,
        recovery_shards: 0,
        shard_index: 0,
        shard_bytes: 16,
        magic: PUNKTFUNK_MAGIC,
        version: 1,
        fec_scheme: 0,
        flags: FLAG_PIC,
    }
}

fn packet(h: PacketHeader) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(h.as_bytes());
    p.extend_from_slice(&vec![0xAB; h.shard_bytes as usize]);
    p
}

/// A header advertising 65535+65535 shards must be dropped, not allocate gigabytes.
#[test]
fn rejects_oversized_shard_counts() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.data_shards = 65535;
    h.recovery_shards = 65535;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// A second packet for a block whose geometry differs from the first must be dropped
/// — never index past the block's allocated shard vector (the old OOB panic).
#[test]
fn rejects_inconsistent_block_geometry_without_panicking() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    let mut h1 = base_header();
    h1.data_shards = 4;
    h1.recovery_shards = 2; // block sized to 6 slots
    h1.frame_bytes = 64;
    assert!(r
        .push(&packet(h1), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    // Same block, different geometry, shard_index valid for ITS total (8) but past
    // the established block's 6 slots.
    let mut h2 = base_header();
    h2.data_shards = 6;
    h2.recovery_shards = 2;
    h2.shard_index = 7;
    h2.frame_bytes = 64;
    assert!(r
        .push(&packet(h2), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// The loss window is TIME-based: an incomplete frame survives newer frames arriving within
/// [`LOSS_WINDOW_NS`] of its capture pts (a 33 ms-late shard at 120 fps is late, not lost —
/// the old 4-INDEX window wrongly killed it), is declared lost once the newest pts moves past
/// the window (`frames_dropped`), and a straggler shard can't resurrect it afterwards.
#[test]
fn incomplete_frames_age_out_by_capture_time_not_frame_count() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    const FRAME_NS: u64 = 8_333_333; // 120 fps

    // Frame 0: one of its two shards arrives — incomplete.
    let mut h = base_header();
    h.data_shards = 2;
    h.frame_bytes = 32;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    // Frames 1..=8 complete around it (well past the old 4-index window, inside 120 ms):
    // frame 0 must still be alive — no drop counted.
    for i in 1..=8u32 {
        let mut h = base_header();
        h.frame_index = i;
        h.pts_ns = i as u64 * FRAME_NS;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_some());
    }
    assert_eq!(stats.snapshot().frames_dropped, 0);

    // Frame 0's second shard arrives 8 frames late (~66 ms at 120 fps) — completes fine.
    let mut h = base_header();
    h.data_shards = 2;
    h.frame_bytes = 32;
    h.shard_index = 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_some());

    // Frame 20: incomplete again; then a frame lands past the 120 ms window → declared lost.
    let mut h = base_header();
    h.frame_index = 20;
    h.pts_ns = 20 * FRAME_NS;
    h.data_shards = 2;
    h.frame_bytes = 32;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let mut h = base_header();
    h.frame_index = 21;
    h.pts_ns = 20 * FRAME_NS + LOSS_WINDOW_NS + 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(stats.snapshot().frames_dropped, 1);

    // A straggler shard for the abandoned frame 20 is dropped, never resurrected.
    let mut h = base_header();
    h.frame_index = 20;
    h.pts_ns = 20 * FRAME_NS;
    h.data_shards = 2;
    h.frame_bytes = 32;
    h.shard_index = 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().frames_dropped, 1, "no double-count");
}

/// The explicit-index path stamps the caller's `frame_index` and leaves the internal video
/// counter untouched — the punktfunk/1 encode loop owns the numbering, and mixing must not
/// perturb the legacy self-numbering path (tests/ABI/synthetic).
#[test]
fn explicit_frame_index_is_stamped_and_internal_counter_untouched() {
    use crate::config::{FecConfig, FecScheme, ProtocolPhase, Role};
    let cfg = Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 0,
            max_data_per_block: 8,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    };
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut seen = Vec::new();
    pk.packetize_each(&[1u8; 16], 0, 0, Some(4242), coder.as_ref(), |hdr, _| {
        seen.push(hdr.frame_index);
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, vec![4242]);
    // The legacy wrapper still numbers from the untouched internal counter.
    let pkts = pk.packetize(&[1u8; 16], 0, 0, coder.as_ref()).unwrap();
    let hdr = PacketHeader::read_from_bytes(&pkts[0][..HEADER_LEN]).unwrap();
    assert_eq!(hdr.frame_index, 0);
    // The probe space is a third, independent counter.
    assert_eq!(pk.alloc_probe_index(), 0);
    assert_eq!(pk.alloc_probe_index(), 1);
}

/// Probe filler (FLAG_PROBE in user_flags) reassembles in its OWN window: a probe frame whose
/// index is far behind the video stream's completes anyway (an old client's single window
/// would drop it as stale), and video frames complete undisturbed around it.
#[test]
fn probe_frames_reassemble_in_their_own_window() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    // Establish a video stream far into its index space.
    let mut v = base_header();
    v.frame_index = 100_000;
    v.pts_ns = 1_000_000_000;
    assert!(r
        .push(&packet(v), coder.as_ref(), &stats)
        .unwrap()
        .is_some());

    // A probe frame at index 0 — 100k "behind" the video window — must still complete.
    let mut p = base_header();
    p.frame_index = 0;
    p.pts_ns = 1_000_000_100;
    p.user_flags = FLAG_PROBE as u32;
    let got = r.push(&packet(p), coder.as_ref(), &stats).unwrap();
    assert!(got.is_some(), "probe frame must complete in its own window");
    assert_eq!(got.unwrap().flags & FLAG_PROBE as u32, FLAG_PROBE as u32);

    // The probe burst must not have advanced the VIDEO window: the next video frame is
    // contiguous and completes, with nothing counted dropped.
    let mut v2 = base_header();
    v2.frame_index = 100_001;
    v2.pts_ns = 1_000_000_200;
    assert!(r
        .push(&packet(v2), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// An incomplete probe frame aging out of the probe window is NOT a video `frames_dropped`
/// (which would fire the client's loss recovery) — probe loss is measured bytes-wise by the
/// probe accumulator.
#[test]
fn aged_out_probe_frames_do_not_count_as_dropped() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    // Probe frame 0: one of two shards — incomplete.
    let mut p = base_header();
    p.user_flags = FLAG_PROBE as u32;
    p.data_shards = 2;
    p.frame_bytes = 32;
    assert!(r
        .push(&packet(p), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    // A much newer probe frame ages it out of the probe window.
    let mut p2 = base_header();
    p2.user_flags = FLAG_PROBE as u32;
    p2.frame_index = 1;
    p2.pts_ns = LOSS_WINDOW_NS + 1;
    assert!(r
        .push(&packet(p2), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(
        stats.snapshot().frames_dropped,
        0,
        "probe-window drops must not fire video loss recovery"
    );
}

/// Build a host config for the end-to-end roundtrips: 16-byte shards, 4-data-shard blocks.
fn e2e_config(scheme: FecScheme, fec_percent: u8) -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme,
            fec_percent,
            max_data_per_block: 4,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

/// Packetize a synthetic AU, deliver a mangled subset (losses within the FEC budget,
/// optionally reversed, with a duplicate), and assert the reassembled AU is byte-identical
/// to the source — the shards landed straight in the frame buffer at the right offsets and
/// FEC filled the holes.
///
/// `fec_recovered_shards` accounting: with in-order delivery it equals the kill count
/// exactly (and nothing is late). With reversed delivery parity arrives first, so the
/// `data + recovery ≥ k` trigger reconstructs EARLY and restores late-not-lost shards too —
/// deliberate (latency), but each such shard's later arrival must count `fec_late_shards`
/// so the NET (`recovered - late`) still equals the true kill count: reordering alone must
/// not read as loss (it pollutes LossReports → adaptive FEC + the ABR controller).
fn e2e_roundtrip(
    scheme: FecScheme,
    frame_len: usize,
    fec_percent: u8,
    kill: &[usize],
    reverse: bool,
) {
    let cfg = e2e_config(scheme, fec_percent);
    let coder = coder_for(scheme);
    let mut pk = Packetizer::new(&cfg);
    let src: Vec<u8> = (0..frame_len).map(|i| (i * 131 + 7) as u8).collect();
    let pkts = pk.packetize(&src, 12345, 0, coder.as_ref()).unwrap();

    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .enumerate()
        .filter(|(i, _)| !kill.contains(i))
        .map(|(_, p)| p.clone())
        .collect();
    if reverse {
        delivery.reverse(); // recovery shards (and the tail) arrive first
    }
    if let Some(dup) = delivery.first().cloned() {
        delivery.push(dup); // a duplicate must be ignored, not double-counted
    }

    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &delivery {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    let f = got.expect("frame must complete within the FEC budget");
    assert_eq!(f.data, src, "reassembled AU must be byte-identical");
    assert_eq!(f.pts_ns, 12345);
    let snap = stats.snapshot();
    let (recovered, late) = (snap.fec_recovered_shards, snap.fec_late_shards);
    if reverse {
        assert!(
            recovered >= kill.len() as u64,
            "early reconstruct counts more"
        );
    } else {
        assert_eq!(recovered, kill.len() as u64);
    }
    assert_eq!(
        recovered - late,
        kill.len() as u64,
        "net recovered (recovered - late) must equal the true loss regardless of order \
             (recovered={recovered} late={late} killed={})",
        kill.len()
    );
}

/// Multi-block frame with a partial tail shard, heavy loss, both delivery orders + dups.
/// 100 bytes / 16 = 7 shards → blocks of (4 data + 2 rec) and (3 data + 2 rec).
#[test]
fn e2e_multiblock_loss_reorder_dup_gf16() {
    // Data-first wire order (T1.3): blk0 data = idx 0..4, blk1 data = idx 4..7,
    // blk0 rec = idx 7..9, blk1 rec = idx 9..11.
    // Kill 2 data in block 0 and 1 data in block 1 — all within the 50% budget.
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 5], false);
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 5], true);
}

#[test]
fn e2e_multiblock_loss_reorder_dup_gf8() {
    e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 6], false);
    e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 6], true);
}

/// T1.3 pin: the wire order is DATA-FIRST — every block's data shards in block order, then
/// every block's parity in block order — so the lossless-completion-gating packet (the last
/// data shard) never sits behind parity in the paced spread. SOF on the first emitted packet,
/// EOF on the last (a parity shard whenever the frame carries FEC).
#[test]
fn packetize_emits_all_data_before_any_parity() {
    use zerocopy::FromBytes;
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    // 100 B / 16 → 7 data shards → blocks (4 data + 2 rec) + (3 data + 2 rec).
    let src: Vec<u8> = (0..100).map(|i| (i * 31 + 3) as u8).collect();
    let pkts = pk.packetize(&src, 1, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts.len(), 11);
    let hdrs: Vec<PacketHeader> = pkts
        .iter()
        .map(|p| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap())
        .collect();
    // (block_index, shard_index) in emission order.
    let layout: Vec<(u16, u16)> = hdrs
        .iter()
        .map(|h| (h.block_index, h.shard_index))
        .collect();
    assert_eq!(
        layout,
        vec![
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3), // blk0 data
            (1, 0),
            (1, 1),
            (1, 2), // blk1 data
            (0, 4),
            (0, 5), // blk0 parity
            (1, 3),
            (1, 4), // blk1 parity
        ],
        "data-first wire order"
    );
    // A shard is parity iff shard_index >= data_shards; no parity may precede any data.
    let first_parity = hdrs
        .iter()
        .position(|h| h.shard_index >= h.data_shards)
        .unwrap();
    assert!(
        hdrs[first_parity..]
            .iter()
            .all(|h| h.shard_index >= h.data_shards),
        "no data shard after the first parity shard"
    );
    // Stream seqs stay strictly sequential in emission order (the nonce contract).
    for (i, w) in hdrs.windows(2).enumerate() {
        assert_eq!(w[1].stream_seq, w[0].stream_seq + 1, "seq gap at {i}");
    }
    assert_eq!(hdrs[0].flags & FLAG_SOF, FLAG_SOF, "SOF on first packet");
    assert_eq!(
        hdrs.last().unwrap().flags & FLAG_EOF,
        FLAG_EOF,
        "EOF on last (parity) packet"
    );
    assert_eq!(
        hdrs.iter().filter(|h| h.flags & FLAG_EOF != 0).count(),
        1,
        "exactly one EOF"
    );

    // FEC-free frame: EOF falls on the last data shard instead.
    let cfg0 = e2e_config(FecScheme::Gf16, 0);
    let mut pk0 = Packetizer::new(&cfg0);
    let pkts0 = pk0.packetize(&src, 2, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts0.len(), 7, "no parity at 0% FEC");
    let last = PacketHeader::read_from_bytes(&pkts0.last().unwrap()[..HEADER_LEN]).unwrap();
    assert_eq!(last.flags & FLAG_EOF, FLAG_EOF, "EOF on last data shard");
    assert!(last.shard_index < last.data_shards, "last packet is data");
}

/// Zero losses, in order: the pure fast path (no codec call, recovered == 0) must still
/// emit an identical AU.
#[test]
fn e2e_clean_delivery_gf16() {
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[], false);
}

/// An empty AU rides one zero-padded shard and reassembles to zero bytes.
#[test]
fn e2e_empty_frame() {
    let cfg = e2e_config(FecScheme::Gf16, 0);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let pkts = pk.packetize(&[], 7, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts.len(), 1);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = r
        .push(&pkts[0], coder.as_ref(), &stats)
        .unwrap()
        .expect("empty frame completes");
    assert!(f.data.is_empty());
}

/// Loss beyond the FEC budget: the frame never emits, ages out as dropped, and the
/// unrecoverable-block path must not fire (block never gathers k shards at all).
#[test]
fn e2e_unrecoverable_loss_ages_out() {
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let src = vec![0x5Au8; 64]; // one block: 4 data + 2 recovery
    let pkts = pk.packetize(&src, 1_000, 0, coder.as_ref()).unwrap();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    // Deliver only 3 of 6 shards (k=4): can never reconstruct.
    for p in &pkts[..3] {
        assert!(r.push(p, coder.as_ref(), &stats).unwrap().is_none());
    }
    // A newer frame past the loss window ages it out as a video drop.
    let next = pk
        .packetize(&src, 1_000 + LOSS_WINDOW_NS + 1, 0, coder.as_ref())
        .unwrap();
    let mut done = false;
    for p in &next {
        done |= r.push(p, coder.as_ref(), &stats).unwrap().is_some();
    }
    assert!(done);
    assert_eq!(stats.snapshot().frames_dropped, 1);
}

/// The in-flight buffer budget: a window of tiny first-shards all declaring max-size frames
/// stops allocating at [`IN_FLIGHT_BUF_FACTOR`] × max_frame_bytes instead of committing
/// gigabytes (the eager whole-frame buffer's amplification defense).
#[test]
fn in_flight_buffer_budget_bounds_allocation() {
    // limits(): max_frame_bytes 4096, shards 16 B, ≤8 data shards × ≤4 blocks → budget 16384 B.
    let lim = limits();
    let budget = IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes;
    // What ONE such frame commits: the largest geometry-consistent buffer (4 blocks × 8 shards
    // × 16 B = 512 B) plus the state of the single block this first shard opens. Both are sized
    // from header fields, so the firewall meters both — counting only the buffer is precisely
    // the hole security-review 2026-08-15 #11 closed, and the boundary moved when it did.
    let per_frame = 512 + block_state_bytes(8, 0);
    let fits = budget / per_frame;
    let mut r = Reassembler::new(lim);
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    for i in 0..=fits as u32 {
        let mut h = base_header();
        h.frame_index = i;
        h.frame_bytes = 512;
        h.block_count = 4;
        h.data_shards = 8;
        r.push(&packet(h), coder.as_ref(), &stats).unwrap();
    }
    assert_eq!(
        stats.snapshot().packets_dropped,
        1,
        "the frame past the budget is dropped, everything under it accepted"
    );
    // The point of the whole exercise: whatever the geometry, the commitment stays under the
    // ceiling. Asserted on the live figure, so a release site that forgets half the cost (the
    // 0.23.0 accounting-drift lesson on `in_flight`) fails here and not in the field.
    assert!(
        r.in_flight() <= budget,
        "in-flight commitment {} must never exceed the {budget} B budget",
        r.in_flight(),
    );
}

/// A header whose (data_shards, block_count) disagree with the geometry derived from its own
/// frame_bytes is dropped — the derived-offset invariant that lets shards land directly in
/// the frame buffer.
#[test]
fn rejects_geometry_inconsistent_with_frame_bytes() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.frame_bytes = 16; // exactly one shard…
    h.data_shards = 2; // …but claims two
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

#[test]
fn rejects_wrong_shard_bytes_and_oversized_frame() {
    let coder = coder_for(FecScheme::Gf8);

    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.shard_bytes = 8; // != negotiated 16
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);

    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.frame_bytes = 1_000_000; // > max_frame_bytes
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// Adaptive FEC raises `fec_percent` mid-session while the receiver's per-block acceptance
/// ceiling is frozen at session construction and never renegotiated. A maximal block must
/// therefore still land: the sender clamps its parity to the ceiling
/// (`Packetizer::recovery_for`), and the receiver sizes that ceiling from the whole clamp range
/// rather than the start percentage. Regression guard for the wedge this caused — every packet
/// of a large block failing `total > max_total_shards`, so the frame never completed and the
/// resulting loss drove adaptive FEC higher still.
#[test]
fn adaptive_fec_ramp_keeps_maximal_blocks_within_the_peers_ceiling() {
    let cfg = e2e_config(FecScheme::Gf16, 10);
    let coder = coder_for(FecScheme::Gf16);
    let lim = ReassemblerLimits::from_config(&cfg);
    let mut pk = Packetizer::new(&cfg);

    // Ramp far past the negotiated 10% — exactly what `apply_fec_target` does under loss.
    pk.set_fec_percent(50);

    // A frame of full `max_data_per_block` blocks: where the ceiling actually binds.
    let frame_len = cfg.shard_payload * cfg.fec.max_data_per_block as usize * 2;
    let src: Vec<u8> = (0..frame_len).map(|i| (i * 131 + 7) as u8).collect();
    let pkts = pk.packetize(&src, 1, 0, coder.as_ref()).unwrap();

    let k = cfg.fec.max_data_per_block as usize;
    let mut clamped = false;
    for p in &pkts {
        let hdr = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        let total = hdr.data_shards as usize + hdr.recovery_shards as usize;
        assert!(
            total <= lim.max_total_shards,
            "block total {total} exceeds the peer's ceiling {} — every packet of this block \
             would be dropped",
            lim.max_total_shards
        );
        // The unclamped 50% would put 2 parity on a full block; the negotiated 10% ceiling
        // leaves room for 1. Proves the clamp actually bound rather than passing vacuously.
        if hdr.data_shards as usize == k {
            assert!(
                (hdr.recovery_shards as usize) < cfg.fec.recovery_for(k).max(1) + 1,
                "parity must be clamped to the peer's ceiling"
            );
            clamped = true;
        }
    }
    assert!(clamped, "test must exercise a maximal block");

    // And the frame still reassembles byte-identically.
    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &pkts {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            got = Some(f);
        }
    }
    assert_eq!(
        got.expect("frame must complete after an adaptive-FEC ramp")
            .data,
        src
    );
}

// ---------------------------------------------------------------------------
// Streamed access units (VIDEO_CAP_STREAMED_AU — nvenc-subframe-slice-output.md Phase 2)
// ---------------------------------------------------------------------------

/// Packetize one streamed AU from `chunks` via begin/push/finish, returning the emitted wire
/// packets (header ++ shard) and the concatenated source bytes.
fn streamed_packets(
    scheme: FecScheme,
    fec_percent: u8,
    chunks: &[&[u8]],
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let cfg = e2e_config(scheme, fec_percent);
    let coder = coder_for(scheme);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(12345, 0, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in chunks {
        src.extend_from_slice(c);
        // slice_end = true with USER_FLAG_SLICE_STREAM unset: must be inert (the flag is the
        // gate) — every legacy-shape assertion downstream proves it.
        pk.push_streamed(
            &mut au,
            c,
            true,
            coder.as_ref(),
            |h: &PacketHeader, b: &[u8]| {
                let mut p = Vec::with_capacity(HEADER_LEN + b.len());
                p.extend_from_slice(h.as_bytes());
                p.extend_from_slice(b);
                pkts.push(p);
                Ok(())
            },
        )
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h: &PacketHeader, b: &[u8]| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// Deliver a streamed AU's packets (optionally with kills within the FEC budget, reversed
/// order, and a duplicate) and assert byte-identical completion. Reversed order is the
/// critical case: the FINAL block's real-total headers arrive FIRST, the frame opens
/// legacy-shaped, and the sentinels must still be accepted against the pinned totals.
fn streamed_roundtrip(scheme: FecScheme, kill: &[usize], reverse: bool) {
    let chunks: Vec<Vec<u8>> = (0..3)
        .map(|c| (0..50).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(scheme, 50, &chunk_refs);
    // 150 B / 16 B shards / 4-shard blocks → sentinel blocks 0,1 (4 data + 2 rec each) +
    // final block (2 data + 1 rec) = 15 packets.
    assert_eq!(
        pkts.len(),
        15,
        "expected geometry changed — update the kills"
    );

    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .enumerate()
        .filter(|(i, _)| !kill.contains(i))
        .map(|(_, p)| p.clone())
        .collect();
    if reverse {
        delivery.reverse();
    }
    if let Some(dup) = delivery.first().cloned() {
        delivery.push(dup);
    }

    let cfg = e2e_config(scheme, 50);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(scheme);
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &delivery {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    let f = got.expect("streamed frame must complete within the FEC budget");
    assert_eq!(
        f.data, src,
        "reassembled streamed AU must be byte-identical"
    );
    assert_eq!(f.pts_ns, 12345);
    assert!(f.complete);
}

#[test]
fn streamed_roundtrip_clean_and_reversed() {
    streamed_roundtrip(FecScheme::Gf16, &[], false);
    streamed_roundtrip(FecScheme::Gf16, &[], true);
    streamed_roundtrip(FecScheme::Gf8, &[], true);
}

/// Loss within each block's FEC budget: one data shard from a sentinel block, one from the
/// final block — in both delivery orders.
#[test]
fn streamed_roundtrip_survives_loss_and_reorder() {
    // Wire order: blk0 = 0..4 data + 4..6 rec, blk1 = 6..10 data + 10..12 rec,
    // final = 12..14 data + 14 rec.
    streamed_roundtrip(FecScheme::Gf16, &[1, 12], false);
    streamed_roundtrip(FecScheme::Gf16, &[1, 12], true);
}

/// The wire shape of a streamed AU: sentinel headers (block_count = 0, frame_bytes = 0,
/// full-K) on every non-final block, real totals + EOF on the final block, SOF on the very
/// first packet only.
#[test]
fn streamed_headers_sentinel_then_final() {
    let chunks: Vec<Vec<u8>> = (0..3).map(|_| vec![0xA5u8; 50]).collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &chunk_refs);
    let mut saw_final = false;
    for (i, p) in pkts.iter().enumerate() {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_eq!(
            h.flags & FLAG_SOF != 0,
            i == 0,
            "SOF exactly on the first packet"
        );
        assert_eq!(
            h.flags & FLAG_EOF != 0,
            i + 1 == pkts.len(),
            "EOF exactly on the last packet"
        );
        if h.block_index < 2 {
            assert_eq!(
                h.block_count, 0,
                "non-final block must ride sentinel headers"
            );
            assert_eq!(h.frame_bytes, 0);
            assert_eq!(h.data_shards, 4, "sentinel blocks are exactly full-K");
        } else {
            saw_final = true;
            assert_eq!(h.block_count, 3, "final block carries the real block count");
            assert_eq!(h.frame_bytes as usize, src.len(), "and the real AU size");
        }
    }
    assert!(saw_final);
}

/// A streamed AU smaller than one block emits NO sentinels — its single (final) block is
/// byte-identical in shape to a legacy frame, so small frames pay zero streaming overhead
/// and any receiver accepts them.
#[test]
fn streamed_small_frame_degenerates_to_legacy() {
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &[&[0x5Au8; 40]]);
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_eq!(
            h.block_count, 1,
            "single-block streamed AU must be legacy-shaped"
        );
        assert_eq!(h.frame_bytes as usize, src.len());
    }
}

/// Sentinel firewall: a sentinel header that is not exactly full-K, or claims a non-zero
/// total, or sits where the final block could no longer follow, is dropped before any
/// allocation happens.
#[test]
fn streamed_sentinel_firewall_bounds() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let sentinel = |f: fn(&mut PacketHeader)| {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8; // limits().max_data_shards — the only legal sentinel K
        h.recovery_shards = 0;
        f(&mut h);
        h
    };
    // Not full-K.
    let h = sentinel(|h| h.data_shards = 7);
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // Claims a total.
    let h = sentinel(|h| h.frame_bytes = 64);
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // Sits on the last block the limits allow (no room for the final block after it).
    let h = sentinel(|h| h.block_index = 31); // derived max_blocks == 32 (see `limits()`)
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 3);
    // A conformant sentinel IS accepted (proves the rejections above weren't vacuous).
    let h = sentinel(|_| {});
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        3,
        "conformant sentinel accepted"
    );
}

/// Retro-validation: final-block totals under which an already-received sentinel block is
/// out of range (or mis-sized) kill the WHOLE frame — no spliced delivery — and the killed
/// index cannot be resurrected by stragglers.
#[test]
fn streamed_lying_final_totals_kill_the_frame_wholesale() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    // Two sentinel blocks (indexes 0 and 1) open the frame and land shards.
    for bi in 0..2u16 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = bi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    // A "final" header claiming the whole AU is ONE 16-byte shard: geometry-valid on its own
    // (expect_blocks = 1, K = 1), but it disowns both sentinel blocks → the frame dies.
    let mut lying = base_header();
    lying.block_count = 1;
    lying.frame_bytes = 16;
    lying.data_shards = 1;
    lying.recovery_shards = 0;
    assert!(r
        .push(&packet(lying), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let snap = stats.snapshot();
    assert_eq!(
        snap.frames_dropped, 1,
        "the lying frame must be counted lost"
    );
    // A straggler sentinel for the killed index must not resurrect it.
    let mut h = base_header();
    h.block_count = 0;
    h.frame_bytes = 0;
    h.data_shards = 8;
    h.recovery_shards = 0;
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        before + 1,
        "straggler for a killed frame must be dropped, not re-open it"
    );
}

// ---------------------------------------------------------------------------
// Slice-granularity streamed AUs (USER_FLAG_SLICE_STREAM — nvenc-subframe P2b)
// ---------------------------------------------------------------------------

/// A block geometry big enough that slice cuts land INSIDE a block — variable-K sentinel
/// blocks with non-uniform bases, the shape the uniform derivation can't describe.
fn slice_config() -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 50,
            max_data_per_block: 64,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

/// Slice chunks chosen to exercise every packetizer path: an exact-shard slice, a slice with
/// a sub-shard remainder, a slice below [`MIN_STREAM_BLOCK_SHARDS`] that must accumulate,
/// and a finish tail. 1023 B total → blocks (K, base-shard): (19, 0), (26, 19), (18, 45),
/// final (1, 63) with block_count 4. Chunk 0 is an exact 20-shard multiple and flushes 19:
/// a flush never drains `pending` to empty, so `finish_streamed` always seals real bytes.
fn slice_chunks() -> Vec<Vec<u8>> {
    [320usize, 403, 100, 200]
        .iter()
        .enumerate()
        .map(|(c, &n)| (0..n).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect()
}

/// Packetize one SLICE-streamed AU (every chunk is a slice boundary), returning the wire
/// packets and concatenated source.
fn slice_streamed_packets() -> (Vec<Vec<u8>>, Vec<u8>) {
    let cfg = slice_config();
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(12345, USER_FLAG_SLICE_STREAM, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in slice_chunks() {
        src.extend_from_slice(&c);
        pk.push_streamed(&mut au, &c, true, coder.as_ref(), |h, b| {
            let mut p = Vec::with_capacity(HEADER_LEN + b.len());
            p.extend_from_slice(h.as_bytes());
            p.extend_from_slice(b);
            pkts.push(p);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

fn push_all(
    r: &mut Reassembler,
    coder: &dyn crate::fec::ErasureCoder,
    stats: &StatsCounters,
    delivery: &[Vec<u8>],
) -> Option<crate::session::Frame> {
    let mut got = None;
    for p in delivery {
        if let Some(f) = r.push(p, coder, stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    got
}

/// The slice wire shape: the flag on EVERY packet, sentinel `frame_bytes` = shard-aligned
/// block base, variable K per block, real totals only on the final block — and the AU
/// reassembles byte-identically from in-order delivery.
#[test]
fn slice_streamed_wire_shape_and_roundtrip() {
    let (pkts, src) = slice_streamed_packets();
    assert_eq!(src.len(), 1023);
    // (block_index, K, base bytes) — chunk 2 (100 B) accumulated instead of flushing (6
    // whole shards < MIN_STREAM_BLOCK_SHARDS) and rode into block 2 with chunk 3's bytes.
    // Block 0 keeps one shard back (chunk 0 is an exact multiple), which rides into block 1.
    let expect = [(0u16, 19u16, 0u32), (1, 26, 304), (2, 18, 720)];
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_ne!(
            h.user_flags & USER_FLAG_SLICE_STREAM,
            0,
            "the marker must ride EVERY packet — reorder can deliver any of them first"
        );
        if h.block_count == 0 {
            let (_, k, base) = expect[h.block_index as usize];
            assert_eq!(h.data_shards, k, "block {} K", h.block_index);
            assert_eq!(h.frame_bytes, base, "block {} base", h.block_index);
            assert_eq!(base % 16, 0, "sentinel bases are shard-aligned");
        } else {
            assert_eq!(h.block_index, 3);
            assert_eq!(h.block_count, 4);
            assert_eq!(h.frame_bytes as usize, src.len());
            assert_eq!(h.data_shards, 1);
        }
    }

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
        .expect("slice-streamed frame must complete");
    assert_eq!(f.data, src, "reassembled slice AU must be byte-identical");
    assert_ne!(f.flags & USER_FLAG_SLICE_STREAM, 0);
}

/// Loss inside two different variable-K blocks, delivered fully REVERSED (the final block's
/// totals arrive first and pin the frame; every sentinel is then accepted against them) with
/// a duplicate — still byte-identical.
#[test]
fn slice_streamed_survives_loss_and_reorder() {
    let (pkts, src) = slice_streamed_packets();
    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
            // Kill data shards 2/5/17 of block 0 and 3/7 of block 1 — within the 50% parity.
            !(h.block_count == 0
                && ((h.block_index == 0 && [2, 5, 17].contains(&h.shard_index))
                    || (h.block_index == 1 && [3, 7].contains(&h.shard_index))))
        })
        .cloned()
        .collect();
    delivery.reverse();
    let dup = delivery.first().cloned().unwrap();
    delivery.push(dup);

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &delivery)
        .expect("slice-streamed frame must complete within the FEC budget");
    assert_eq!(f.data, src);
}

/// Post-pin range firewall: once the final block pinned the totals, a sentinel whose base
/// would reach into (or past) the final block's range is dropped — and the honest copy of
/// that block still assembles the frame.
#[test]
fn slice_streamed_post_pin_out_of_range_sentinel_dropped() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    // Final block first — pins totals (64 data shards, final K = 1).
    let finals: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count != 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &finals).is_none());

    // A block-0 packet whose base claims shard 60: 60 + 20 > 63 (the final block's base) —
    // it would overlap the final block's landed shards. Must drop WITHOUT killing the frame.
    let mut evil = pkts
        .iter()
        .find(|p| {
            let h = hdr_of(p);
            h.block_count == 0 && h.block_index == 0 && h.shard_index == 0
        })
        .cloned()
        .unwrap();
    let mut h = PacketHeader::read_from_bytes(&evil[..HEADER_LEN]).unwrap();
    h.frame_bytes = 60 * 16;
    evil[..HEADER_LEN].copy_from_slice(h.as_bytes());
    let before = stats.snapshot().packets_dropped;
    assert!(r.push(&evil, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);

    // The honest packets (including the real block-0) still complete the frame.
    let rest: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count == 0)
        .cloned()
        .collect();
    let f = push_all(&mut r, coder.as_ref(), &stats, &rest)
        .expect("the honest blocks must still complete the frame");
    assert_eq!(f.data, src);
}

/// Slice retro-validation: final totals under which an already-landed sentinel block would
/// overlap the final block's range kill the WHOLE frame, and stragglers can't resurrect it.
#[test]
fn slice_streamed_lying_final_kills_frame() {
    let (pkts, _) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    // All sentinel blocks land first.
    let sentinels: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count == 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &sentinels).is_none());

    // A final header claiming K = 30 puts the final base at shard 34 — under block 2's
    // landed range (base 45, K 18 → needs base ≥ 63). The frame dies wholesale.
    let mut lying = pkts
        .iter()
        .find(|p| {
            let h = hdr_of(p);
            h.block_count != 0 && h.shard_index == 0
        })
        .cloned()
        .unwrap();
    let mut h = PacketHeader::read_from_bytes(&lying[..HEADER_LEN]).unwrap();
    h.data_shards = 30;
    lying[..HEADER_LEN].copy_from_slice(h.as_bytes());
    assert!(r.push(&lying, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(
        stats.snapshot().frames_dropped,
        1,
        "the lying frame must be counted lost"
    );

    // The honest final packets are stragglers for a killed index now — no resurrection.
    let finals: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count != 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &finals).is_none());
    assert_eq!(stats.snapshot().frames_dropped, 1);
}

/// One slice bigger than a whole FEC block must cut MULTIPLE blocks from a single push (the
/// flush loop) — the final block can never be left oversized.
#[test]
fn slice_streamed_giant_slice_cuts_multiple_blocks() {
    let cfg = slice_config(); // max_data_per_block 64
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(1, USER_FLAG_SLICE_STREAM, Some(0));
    let src: Vec<u8> = (0..70 * 16).map(|i| (i * 131 + 7) as u8).collect();
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    pk.push_streamed(&mut au, &src, true, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        if h.block_count == 0 {
            assert_eq!((h.block_index, h.data_shards, h.frame_bytes), (0, 64, 0));
        } else {
            assert_eq!((h.block_index, h.block_count, h.data_shards), (1, 2, 6));
            assert_eq!(h.frame_bytes as usize, src.len());
        }
    }
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("must complete");
    assert_eq!(f.data, src);
}

/// `max_data_per_block` SMALLER than [`MIN_STREAM_BLOCK_SHARDS`]: one slice cut shatters into
/// several full-K blocks (the flush floor clamps to the block size) and the mirrored
/// block-count ceilings must still admit the frame end to end.
#[test]
fn slice_streamed_small_kmax_roundtrip() {
    let cfg = e2e_config(FecScheme::Gf16, 50); // max_data_per_block 4 < 16
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(1, USER_FLAG_SLICE_STREAM, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in 0..2usize {
        let chunk: Vec<u8> = (0..320 + c * 83)
            .map(|i| (c * 57 + i * 131 + 7) as u8)
            .collect();
        src.extend_from_slice(&chunk);
        pk.push_streamed(&mut au, &chunk, true, coder.as_ref(), |h, b| {
            let mut p = Vec::with_capacity(HEADER_LEN + b.len());
            p.extend_from_slice(h.as_bytes());
            p.extend_from_slice(b);
            pkts.push(p);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        if h.block_count == 0 {
            assert_eq!(h.data_shards, 4, "floor clamps to full-K blocks");
            assert_eq!(h.frame_bytes % (4 * 16), 0, "bases advance block-wise");
        }
    }
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("must complete");
    assert_eq!(f.data, src);
}

/// A frame's packets must agree on the slice marker: a legacy-shaped header for a
/// slice-opened frame (or vice versa) is dropped before it can pin or place anything under
/// the wrong rule.
#[test]
fn slice_streamed_mixed_flag_packet_dropped() {
    let (pkts, _) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    // Open the frame with one honest slice sentinel packet.
    let first = pkts
        .iter()
        .find(|p| hdr_of(p).block_count == 0)
        .cloned()
        .unwrap();
    assert!(r.push(&first, coder.as_ref(), &stats).unwrap().is_none());

    // A LEGACY final for the same frame index that PASSES the legacy firewall (one 16-byte
    // shard, one block): only the flag-consistency check stands between it and pinning the
    // slice-opened frame under uniform rules — which would then "catch" the sentinel lying
    // and kill the frame. It must be dropped as a packet, neither pinning nor killing.
    let mut h = hdr_of(&first);
    h.user_flags &= !USER_FLAG_SLICE_STREAM;
    h.block_index = 0;
    h.block_count = 1;
    h.frame_bytes = 16;
    h.data_shards = 1;
    h.recovery_shards = 0;
    h.shard_index = 0;
    let mut legacy = Vec::with_capacity(HEADER_LEN + 16);
    legacy.extend_from_slice(h.as_bytes());
    legacy.extend_from_slice(&[0xEE; 16]);
    let before = stats.snapshot().packets_dropped;
    assert!(r.push(&legacy, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);
    assert_eq!(stats.snapshot().frames_dropped, 0, "dropped, not killed");
}

// ---------------------------------------------------------------------------
// Slice-progressive prefix delivery (Frame::part — P2c)
// ---------------------------------------------------------------------------

/// Push packets collecting EVERY delivery (parts and completions), returning them in order.
fn push_collect(
    r: &mut Reassembler,
    coder: &dyn crate::fec::ErasureCoder,
    stats: &StatsCounters,
    delivery: &[Vec<u8>],
) -> Vec<crate::session::Frame> {
    let mut out = Vec::new();
    for p in delivery {
        if let Some(f) = r.push(p, coder, stats).unwrap() {
            out.push(f);
        }
    }
    out
}

/// In-order slice delivery streams one part per completed block, offsets tiling exactly, the
/// final delivery carrying only the suffix with `last` + `complete`.
#[test]
fn parts_stream_in_order() {
    let (pkts, src) = slice_streamed_packets();
    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert_eq!(
        got.len(),
        4,
        "three sentinel-block parts + the final suffix"
    );
    let mut rebuilt = Vec::new();
    for (i, f) in got.iter().enumerate() {
        let part = f
            .part
            .expect("parts mode: every delivery carries part meta");
        assert_eq!(
            part.offset as usize,
            rebuilt.len(),
            "parts tile with no gaps"
        );
        assert_eq!(part.first, i == 0);
        assert_eq!(part.last, i + 1 == got.len());
        assert_eq!(
            f.complete, part.last,
            "complete rides exactly the last part"
        );
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(rebuilt, src, "concatenated parts must be the byte-exact AU");
    assert_eq!(
        stats.snapshot().frames_completed,
        0,
        "the reassembler leaves the completion count to the session boundary"
    );
}

/// A block completing BEHIND the prefix emits nothing; the block that closes the gap emits
/// ONE coalesced part spanning everything unlocked.
#[test]
fn parts_coalesce_across_reordered_blocks() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
    // Blocks 1 and 2 fully first, then block 0, then the final block.
    let mut delivery: Vec<Vec<u8>> = Vec::new();
    for want in [1u16, 2, 0] {
        delivery.extend(
            pkts.iter()
                .filter(|p| {
                    let h = hdr_of(p);
                    h.block_count == 0 && h.block_index == want
                })
                .cloned(),
        );
    }
    delivery.extend(pkts.iter().filter(|p| hdr_of(p).block_count != 0).cloned());

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &delivery);
    assert_eq!(got.len(), 2, "one coalesced prefix part + the final suffix");
    let p0 = got[0].part.unwrap();
    assert_eq!((p0.offset, p0.first, p0.last), (0, true, false));
    assert_eq!(got[0].data.len(), 720 + 288, "blocks 0-2 in one part");
    let p1 = got[1].part.unwrap();
    assert!(p1.last && got[1].complete);
    let mut rebuilt = got[0].data.clone();
    rebuilt.extend_from_slice(&got[1].data);
    assert_eq!(rebuilt, src);
}

/// Loss inside a block delays its part until FEC reconstructs it — the part then carries the
/// recovered bytes, still byte-exact.
#[test]
fn parts_wait_for_fec_reconstruction() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
    // Kill two data shards of block 0 — within the 50% parity budget.
    let delivery: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            let h = hdr_of(p);
            !(h.block_count == 0 && h.block_index == 0 && [1, 7].contains(&h.shard_index))
        })
        .cloned()
        .collect();
    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &delivery);
    let mut rebuilt = Vec::new();
    for f in &got {
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(
        rebuilt, src,
        "reconstructed prefix parts must be byte-exact"
    );
    assert!(got.last().unwrap().complete);
}

/// With parts on, a legacy single-block frame degenerates to ONE whole-AU delivery carrying
/// `{offset 0, first, last}` — the consumer's feed logic stays uniform.
#[test]
fn parts_degenerate_whole_frame() {
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let src: Vec<u8> = (0..40).map(|i| i as u8).collect();
    let pkts = pk.packetize(&src, 7, 0, coder.as_ref()).unwrap();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert_eq!(got.len(), 1);
    let f = &got[0];
    assert_eq!(
        f.part,
        Some(crate::session::FramePart {
            offset: 0,
            first: true,
            last: true
        })
    );
    assert!(f.complete);
    assert_eq!(f.data, src);
}

/// Parts also flow for the LEGACY streamed shape (uniform full-K sentinels) — the prefix
/// cursor rides `base_shard`, which both wire shapes maintain.
#[test]
fn parts_flow_for_legacy_streamed_frames() {
    let chunks: Vec<Vec<u8>> = (0..3)
        .map(|c| (0..50).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &chunk_refs);
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert!(got.len() > 1, "sentinel blocks must deliver early parts");
    let mut rebuilt = Vec::new();
    for f in &got {
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(rebuilt, src);
    assert!(got.last().unwrap().complete);
}

/// A one-datagram open commits only the buffer its OWN header proves it needs, and the
/// in-flight budget still bounds the ones that claim a lot.
///
/// Both halves matter. A sentinel that claims little must cost little: sizing every
/// sentinel-opened frame at `max_frame_bytes` (the 0.23.0 shape) was survivable only while
/// sentinels were rare, and the slice wire made every ordinary AU one — after which the budget
/// was spent on ~3 frames and everything else on the link was dropped. A sentinel that claims a
/// lot must still be bounded: its wire base can point near the frame ceiling, which is the
/// amplification this budget exists for.
#[test]
fn streamed_open_commits_its_own_extent_and_stays_bounded() {
    let coder = coder_for(FecScheme::Gf8);
    // limits(): shard 16 B, max_data_shards 8, max_frame_bytes 4096 → budget = 4 × 4096.
    // Modest legacy sentinels (block 0, full K = 8 → 128 B each): far more than
    // IN_FLIGHT_BUF_FACTOR of them must fit, because none of them claims the ceiling.
    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    for fi in 0..32u32 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.frame_index = fi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert_eq!(
        stats.snapshot().packets_dropped,
        0,
        "ordinary one-datagram opens must not exhaust the in-flight budget"
    );

    // A SLICE sentinel whose wire base sits just under the ceiling really does commit a
    // max-sized frame (base 3968 B + K 8 = 256 shards = 4096 B), plus the state of the block it
    // opens — so the budget takes fewer of these than the buffer alone would suggest, and the
    // first one past it must be refused.
    let lim = limits();
    let budget = IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes;
    let fits = budget / (4096 + block_state_bytes(8, 0));
    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();
    for fi in 0..=fits as u32 {
        let mut h = base_header();
        h.user_flags = USER_FLAG_SLICE_STREAM;
        h.block_count = 0;
        h.frame_bytes = 4096 - 8 * 16;
        h.block_index = 1;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.frame_index = fi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert!(
        r.in_flight() <= budget,
        "in-flight commitment {} must never exceed the {budget} B budget",
        r.in_flight(),
    );
    assert_eq!(
        stats.snapshot().packets_dropped,
        1,
        "the first ceiling-claiming open past the budget must be refused"
    );
}

/// The final-first order's single buffer-safety guard (2026-07 security review finding 3): a
/// frame opened by its FINAL block allocates an EXACT-sized buffer; a sentinel aimed at (or
/// past) the pinned final slot must be dropped — without the guard its full-K write would land
/// outside that buffer. And the reject must not corrupt the frame: it still completes.
#[test]
fn streamed_out_of_range_sentinel_after_final_first_is_dropped() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    // Final block opens the frame: block_count = 1, frame_bytes = 32 → K = 2. Send shard 0
    // only, so the frame stays in flight with the totals pinned.
    let mut fin = base_header();
    fin.block_count = 1;
    fin.frame_bytes = 32;
    fin.data_shards = 2;
    fin.recovery_shards = 0;
    fin.shard_index = 0;
    assert!(r
        .push(&packet(fin), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // Sentinels at the final slot (0) and past it (1): both non-final-impossible under the
    // pinned block_count = 1 → dropped, never written into the 32-byte buffer.
    for bi in 0..2u16 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = bi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert_eq!(stats.snapshot().packets_dropped, 2);
    // The frame is unharmed: its real second shard completes it at the exact pinned length.
    let mut fin2 = fin;
    fin2.shard_index = 1;
    let got = r
        .push(&packet(fin2), coder.as_ref(), &stats)
        .unwrap()
        .expect("frame must still complete after the rejected sentinels");
    assert_eq!(got.data.len(), 32);
    assert!(got.complete);
}

/// A second "final" header with DIFFERENT totals must be rejected once a streamed frame is
/// pinned (re-pinning would re-interpret already-landed shards), and the frame must still
/// complete under the first totals.
#[test]
fn streamed_second_final_with_different_totals_is_rejected() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let sentinel_shard = |shard_index: u16| {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = 0;
        h.shard_index = shard_index;
        h
    };
    let final_shard = |frame_bytes: u32, data_shards: u16, shard_index: u16| {
        let mut h = base_header();
        h.block_count = 2;
        h.frame_bytes = frame_bytes;
        h.data_shards = data_shards;
        h.recovery_shards = 0;
        h.block_index = 1;
        h.shard_index = shard_index;
        h
    };
    // Sentinel opens block 0, then the real final pins totals: 10 shards = 160 bytes.
    assert!(r
        .push(&packet(sentinel_shard(0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert!(r
        .push(&packet(final_shard(160, 2, 0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // A second final claiming 144 bytes (K = 1): geometry-valid alone, but it contradicts the
    // pinned totals → dropped.
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&packet(final_shard(144, 1, 0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);
    // The frame still completes under the FIRST totals: the rest of block 0 + the final tail.
    let mut got = None;
    for s in 1..8u16 {
        assert!(got.is_none());
        got = r
            .push(&packet(sentinel_shard(s)), coder.as_ref(), &stats)
            .unwrap();
    }
    assert!(got.is_none(), "block 1 still owes a shard");
    let got = r
        .push(&packet(final_shard(160, 2, 1)), coder.as_ref(), &stats)
        .unwrap()
        .expect("frame completes under the first pinned totals");
    assert_eq!(got.data.len(), 160);
}

/// Production-shaped slice geometry: a 1500-MTU shard payload and the smallest frame ceiling
/// the QUIC handshake ever negotiates (`max_frame_bytes` is clamped to ≥ 8 MiB there).
fn prod_slice_config() -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 20,
            max_data_per_block: 200,
        },
        shard_payload: crate::config::mtu1500_shard_payload(),
        max_frame_bytes: 8 << 20,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

/// Packetize one streamed AU of `chunks`, each chunk an encoder slice boundary.
fn streamed_packets_with(
    cfg: &Config,
    frame_index: u32,
    pts_ns: u64,
    slice: bool,
    chunks: &[usize],
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let coder = coder_for(cfg.fec.scheme);
    let mut pk = Packetizer::new(cfg);
    let uf = if slice { USER_FLAG_SLICE_STREAM } else { 0 };
    let mut au = pk.begin_streamed(pts_ns, uf, Some(frame_index));
    let (mut pkts, mut src) = (Vec::new(), Vec::new());
    let sink = |pkts: &mut Vec<Vec<u8>>, h: &PacketHeader, b: &[u8]| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
    };
    for (c, &n) in chunks.iter().enumerate() {
        let data: Vec<u8> = (0..n).map(|i| (c * 57 + i * 131 + 7) as u8).collect();
        src.extend_from_slice(&data);
        pk.push_streamed(&mut au, &data, true, coder.as_ref(), |h, b| {
            sink(&mut pkts, h, b);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        sink(&mut pkts, h, b);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// An AU whose length is an exact multiple of the shard payload must still reassemble.
///
/// Regression: the slice flush drained `pending` to empty, so `finish_streamed` sealed a final
/// block of one zero-padded FILLER shard. Its derived base (`total_data − 1`) overlapped the
/// sentinel block flushed a moment earlier, the receiver's retro-validation read that as a lying
/// header, and the whole AU was destroyed — one frame in every `shard_payload` (~12 s at 120 fps),
/// each costing a re-anchor freeze and a recovery keyframe.
#[test]
fn slice_streamed_exact_shard_multiple_completes() {
    let cfg = prod_slice_config();
    let coder = coder_for(FecScheme::Gf16);
    let payload = cfg.shard_payload;
    for shards in [16usize, 29, 30, 64] {
        let (pkts, src) = streamed_packets_with(&cfg, 1, 1000, true, &[shards * payload]);
        // Whatever the block split, the final block must carry real bytes — never a lone
        // zero-pad shard sitting on top of the previous block's range.
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("{shards}-shard AU (exact multiple) must complete"));
        assert_eq!(f.data, src, "{shards}-shard AU must be byte-identical");
    }
    // ...and the sweep around one of them, so an off-by-one in the keep-back can't hide.
    for extra in 0..3usize {
        let n = 30 * payload + extra;
        let (pkts, src) = streamed_packets_with(&cfg, 2, 2000, true, &[n]);
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("{n}-byte AU must complete"));
        assert_eq!(f.data, src);
    }
}

/// A slice-streamed frame must cost the reassembler its OWN size, not the negotiated ceiling.
///
/// Regression: sentinel-opened frames allocated `max_frame_bytes` (8-64 MiB) each. Since the
/// slice wire makes every ordinary AU sentinel-opened, the in-flight budget
/// (`IN_FLIGHT_BUF_FACTOR × max_frame_bytes`) was spent after ~3 concurrent frames and every
/// packet of every further frame was dropped outright — a permanent loss storm on any link with
/// normal reorder, plus a multi-megabyte zeroing per access unit.
#[test]
fn slice_streamed_in_flight_budget_matches_legacy() {
    let cfg = prod_slice_config();
    let coder = coder_for(FecScheme::Gf16);
    // A normal 40 KB access unit, opened but not completed — the shape a link with reorder
    // holds several of at once.
    for slice in [false, true] {
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        for i in 0..12u32 {
            let (pkts, _) = streamed_packets_with(&cfg, i, 1_000_000 * i as u64, slice, &[40_000]);
            r.push(&pkts[0], coder.as_ref(), &stats).unwrap();
        }
        assert_eq!(
            stats
                .packets_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "slice={slice}: 12 ordinary AUs in flight must fit the in-flight budget"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-frame shard geometry (mid-session shard-payload renegotiation — W0.1,
// design/shard-payload-reneg.md). The 0.23.0 lesson applies in full: geometry
// changes breed sizing bugs, so the slice/sentinel suite re-runs at every
// production shard size and mixed-geometry streams are tortured under reorder.
// ---------------------------------------------------------------------------

/// The shard sizes the renegotiation actually moves between: the clamp floor (512), a
/// WARP/Tailscale-shaped 1280-MTU path (1216), the 1500-MTU default (1408), and 9000-MTU
/// jumbo (8908 — sealed 8972, inside [`MAX_DATAGRAM_BYTES`]).
const PRODUCTION_SHARDS: [usize; 4] = [512, 1216, 1408, 8908];

/// [`prod_slice_config`] at an arbitrary shard payload.
fn geo_config(shard_payload: usize) -> Config {
    let mut c = prod_slice_config();
    c.shard_payload = shard_payload;
    c.validate().expect("geometry config must be valid");
    c
}

/// Packetize one legacy AU at the packetizer's CURRENT shard payload with an explicit
/// frame index, returning wire packets + source bytes.
fn legacy_packets_with(
    pk: &mut Packetizer,
    frame_index: u32,
    pts_ns: u64,
    len: usize,
    coder: &dyn crate::fec::ErasureCoder,
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let src: Vec<u8> = (0..len)
        .map(|i| (i * 131 + frame_index as usize * 7 + 3) as u8)
        .collect();
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    pk.packetize_each(&src, pts_ns, 0, Some(frame_index), coder, |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// The slice-wire regression suite re-run at every production shard size (the design's
/// non-negotiable verification): the exact-multiple sweep (the 0.23.0 filler-shard bug
/// shape), lossy + reversed slice roundtrips, the legacy-streamed sentinel path, and the
/// in-flight budget — each asserting DELIVERED byte-identical frames, never just an
/// absence of errors.
#[test]
fn slice_wire_suite_at_production_shard_sizes() {
    let coder = coder_for(FecScheme::Gf16);
    for &shard in &PRODUCTION_SHARDS {
        let cfg = geo_config(shard);

        // Exact-shard-multiple AUs + the off-by-one sweep around one of them.
        for shards in [16usize, 30, 64] {
            for extra in 0..3usize {
                let n = shards * shard + extra;
                let (pkts, src) = streamed_packets_with(&cfg, 1, 1000, true, &[n]);
                let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
                let stats = StatsCounters::default();
                let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
                    .unwrap_or_else(|| panic!("shard {shard}: {n}-byte slice AU must complete"));
                assert_eq!(
                    f.data, src,
                    "shard {shard}: {n}-byte AU must be byte-identical"
                );
                assert_eq!(
                    r.in_flight(),
                    0,
                    "shard {shard}: budget must return to zero"
                );
            }
        }

        // A multi-slice AU under loss (one data shard of the first flushed block — within
        // its ≥ 20% parity) in both delivery orders. Reversed is the critical order: the
        // final block's totals arrive first and every sentinel validates against the pin.
        for reverse in [false, true] {
            let chunks = [20 * shard + 13, 7 * shard + 1, 17 * shard];
            let (pkts, src) = streamed_packets_with(&cfg, 2, 2000, true, &chunks);
            let killed = pkts
                .iter()
                .position(|p| {
                    let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
                    h.shard_index < h.data_shards && h.recovery_shards >= 1
                })
                .expect("suite frame must have a recoverable data shard");
            let mut delivery: Vec<Vec<u8>> = pkts
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != killed)
                .map(|(_, p)| p.clone())
                .collect();
            if reverse {
                delivery.reverse();
            }
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            let f = push_all(&mut r, coder.as_ref(), &stats, &delivery).unwrap_or_else(|| {
                panic!("shard {shard} reverse={reverse}: lossy slice AU must complete")
            });
            assert_eq!(f.data, src, "shard {shard} reverse={reverse}");
            assert_eq!(r.in_flight(), 0);
        }

        // Legacy-streamed (uniform full-K sentinel) path: one AU spanning a sentinel block
        // (K = 200) plus a final block.
        {
            let (pkts, src) = streamed_packets_with(&cfg, 3, 3000, false, &[230 * shard]);
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
                .unwrap_or_else(|| panic!("shard {shard}: legacy-streamed AU must complete"));
            assert_eq!(f.data, src);
            assert_eq!(r.in_flight(), 0);
        }

        // The budget regression at this size: 12 ordinary AUs opened concurrently, no drops.
        for slice in [false, true] {
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            for i in 0..12u32 {
                let (pkts, _) =
                    streamed_packets_with(&cfg, i, 1_000_000 * i as u64, slice, &[40_000]);
                r.push(&pkts[0], coder.as_ref(), &stats).unwrap();
            }
            assert_eq!(
                stats
                    .packets_dropped
                    .load(std::sync::atomic::Ordering::Relaxed),
                0,
                "shard {shard} slice={slice}: 12 AUs in flight must fit the budget"
            );
        }
    }
}

/// One packetizer, one reassembler, one continuous stream — the shard payload swapped
/// live between AUs ([`Packetizer::set_shard_payload`], the Phase 1 host seam): every
/// frame across shrink → grow-to-jumbo → shrink-again delivers byte-identically under its
/// own per-frame pin, and the budget returns to zero.
#[test]
fn mid_stream_shard_swap_delivers_every_frame() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    // (shard size to swap to, AU length) — swaps happen between AUs, as Phase 1 will.
    let schedule = [
        (1408usize, 3 * 1408 + 100),
        (1408, 9 * 1408),
        (512, 5 * 512 + 17), // shrink (the VPN heal)
        (512, 512),
        (8908, 12 * 8908 + 1), // grow (jumbo)
        (1216, 4 * 1216 + 9),  // revert (a mis-proven jumbo hop self-corrects)
    ];
    for (i, &(shard, len)) in schedule.iter().enumerate() {
        pk.set_shard_payload(shard);
        let pts = 1_000_000 * (i as u64 + 1);
        let (pkts, src) = legacy_packets_with(&mut pk, i as u32, pts, len, coder.as_ref());
        for p in &pkts {
            let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
            assert_eq!(
                h.shard_bytes as usize, shard,
                "sender must stamp the live size"
            );
        }
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("frame {i} at shard {shard} must complete"));
        assert_eq!(
            f.data, src,
            "frame {i} at shard {shard} must be byte-identical"
        );
        assert!(f.complete);
    }
    assert_eq!(
        r.in_flight(),
        0,
        "budget must be exact across geometry swaps"
    );
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// The reorder race the design kills structurally: an old-geometry frame still in flight
/// when new-geometry frames start arriving completes under its OWN pin — its straggler
/// lands in its own buffer, not the new geometry's.
#[test]
fn old_geometry_frame_completes_after_new_geometry_arrived() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    // Frame 0 at 1408: 7 data shards + 2 parity (20% FEC), data-first wire order. Withhold
    // THREE data shards — more than parity can bridge — so the frame genuinely stays
    // incomplete until a straggler returns (fewer, and FEC would complete it early).
    let (pkts0, src0) = legacy_packets_with(&mut pk, 0, 1_000_000, 6 * 1408 + 50, coder.as_ref());
    assert_eq!(
        pkts0.len(),
        9,
        "expected geometry changed — update the split"
    );
    let head: Vec<Vec<u8>> = pkts0[..4].iter().chain(&pkts0[7..]).cloned().collect();
    let straggler = &pkts0[4];
    assert!(
        push_all(&mut r, coder.as_ref(), &stats, &head).is_none(),
        "frame 0 must still be incomplete"
    );

    // The stream re-keys to 512: frames 1..=2 arrive whole and deliver.
    pk.set_shard_payload(512);
    for i in 1..=2u32 {
        let pts = 1_000_000 + 1_000_000 * i as u64;
        let (pkts, src) = legacy_packets_with(&mut pk, i, pts, 3 * 512 + 7, coder.as_ref());
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("new-geometry frame");
        assert_eq!(f.data, src);
    }

    // Frame 0's old-geometry straggler arrives last — the frame completes byte-identically.
    let f = r
        .push(straggler, coder.as_ref(), &stats)
        .unwrap()
        .expect("old-geometry frame must complete under its own pin");
    assert_eq!(f.data, src0);
    assert_eq!(f.frame_index, 0);
    assert_eq!(r.in_flight(), 0);
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// The anti-splice pin: a packet claiming a DIFFERENT (but in-bounds) shard size for an
/// already-pinned frame is dropped — and the frame still completes from its real packets.
#[test]
fn cross_geometry_packet_for_a_pinned_frame_is_dropped() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    let mut pk_a = Packetizer::new(&geo_config(1408));
    let mut pk_b = Packetizer::new(&geo_config(1216));
    let (pkts, src) = legacy_packets_with(&mut pk_a, 0, 1_000_000, 5 * 1408 + 9, coder.as_ref());
    // The impostor: the same frame index packetized at 1216 — self-consistent (it passes
    // the firewall standalone), wrong for THIS frame's pin.
    let (impostor, _) = legacy_packets_with(&mut pk_b, 0, 1_000_000, 5 * 1216, coder.as_ref());

    assert!(r.push(&pkts[0], coder.as_ref(), &stats).unwrap().is_none());
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&impostor[1], coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        before + 1,
        "cross-geometry packet must be dropped by the frame pin"
    );
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts[1..])
        .expect("the pinned frame must still complete from its real packets");
    assert_eq!(f.data, src, "no impostor bytes may reach the frame");
}

/// The firewall bounds on a frame's pinned size: below the floor, above the receive
/// ceiling, or odd ⇒ dropped before any allocation; the exact floor and ceiling are
/// accepted AND deliver (proving the rejections aren't vacuous).
#[test]
fn shard_size_firewall_bounds() {
    let cfg = geo_config(1408);
    let lim = ReassemblerLimits::from_config(&cfg);
    assert_eq!(lim.min_shard_bytes, crate::config::MIN_SHARD_PAYLOAD);
    assert_eq!(lim.max_shard_bytes, crate::config::max_shard_payload());
    let coder = coder_for(FecScheme::Gf16);
    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();

    let single = |shard: usize, frame_index: u32| {
        let mut h = base_header();
        h.frame_index = frame_index;
        h.shard_bytes = shard as u16;
        h.frame_bytes = shard as u32;
        h
    };
    // Below the floor (even), above the ceiling (even), odd within bounds: all dropped.
    for (i, shard) in [510usize, 9154, 1409].into_iter().enumerate() {
        let before = stats.snapshot().packets_dropped;
        assert!(r
            .push(&packet(single(shard, i as u32)), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(
            stats.snapshot().packets_dropped,
            before + 1,
            "shard {shard} must be firewalled"
        );
    }
    // The exact bounds deliver whole single-shard frames.
    for (i, shard) in [
        crate::config::MIN_SHARD_PAYLOAD,
        crate::config::max_shard_payload(),
    ]
    .into_iter()
    .enumerate()
    {
        let f = r
            .push(
                &packet(single(shard, 10 + i as u32)),
                coder.as_ref(),
                &stats,
            )
            .unwrap()
            .unwrap_or_else(|| panic!("boundary shard {shard} must deliver"));
        assert_eq!(f.data.len(), shard);
    }
}

mod geometry_proptests {
    use super::*;
    use proptest::prelude::*;

    /// One generated frame: shard size, slice-vs-legacy wire, size factor, and whether to
    /// kill one recoverable data shard.
    type GenFrame = (usize, bool, usize, bool);

    fn frame_strategy() -> impl Strategy<Value = GenFrame> {
        (
            proptest::sample::select(&PRODUCTION_SHARDS[..]),
            any::<bool>(),
            1usize..30,
            any::<bool>(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Mixed-geometry reorder torture: frames of DIFFERENT shard sizes and wire shapes
        /// interleaved into one shuffled delivery, with per-frame recoverable loss — every
        /// frame must deliver byte-identically and the in-flight budget must return to
        /// exactly zero (the 0.23.0 budget-drift shape, now across geometries).
        #[test]
        fn mixed_geometry_reorder_torture(
            frames in proptest::collection::vec(frame_strategy(), 2..6),
            seed in any::<u64>(),
        ) {
            let coder = coder_for(FecScheme::Gf16);
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&geo_config(1408)));
            let stats = StatsCounters::default();

            let mut all: Vec<(u64, u32, Vec<u8>)> = Vec::new(); // (shuffle key, frame, pkt)
            let mut sources: Vec<(u32, Vec<u8>)> = Vec::new();
            for (i, &(shard, slice, factor, kill)) in frames.iter().enumerate() {
                let cfg = geo_config(shard);
                let pts = 1_000_000 * (i as u64 + 1);
                let len = factor * shard + (factor % shard.min(7));
                let (mut pkts, src) = if slice {
                    streamed_packets_with(&cfg, i as u32, pts, true, &[len.max(1)])
                } else {
                    let mut pk = Packetizer::new(&cfg);
                    legacy_packets_with(&mut pk, i as u32, pts, len.max(1), coder.as_ref())
                };
                if kill {
                    if let Some(k) = pkts.iter().position(|p| {
                        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
                        h.shard_index < h.data_shards && h.recovery_shards >= 1
                    }) {
                        pkts.remove(k);
                    }
                }
                for (j, p) in pkts.into_iter().enumerate() {
                    // Deterministic pseudo-shuffle key: interleaves frames and reorders
                    // within a frame, differently per proptest case.
                    let key = (seed | 1)
                        .wrapping_mul(j as u64 + 1)
                        .wrapping_add((i as u64) << 17)
                        .rotate_left((j % 61) as u32);
                    all.push((key, i as u32, p));
                }
                sources.push((i as u32, src));
            }
            all.sort_by_key(|(k, _, _)| *k);

            let mut delivered: std::collections::HashMap<u32, Vec<u8>> =
                std::collections::HashMap::new();
            for (_, _, p) in &all {
                if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
                    prop_assert!(f.complete);
                    prop_assert!(delivered.insert(f.frame_index, f.data).is_none(),
                        "a frame must deliver exactly once");
                }
            }
            for (i, src) in &sources {
                let got = delivered.get(i);
                prop_assert!(got.is_some(), "frame {i} must be DELIVERED, not merely error-free");
                prop_assert_eq!(got.unwrap(), src, "frame {} must be byte-identical", i);
            }
            prop_assert_eq!(r.in_flight(), 0, "budget must be exact after all frames terminate");
            prop_assert_eq!(stats.snapshot().frames_dropped, 0u64);
        }
    }
}
