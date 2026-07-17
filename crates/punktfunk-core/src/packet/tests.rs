use super::reassemble::LOSS_WINDOW_NS;
use super::*;
use crate::config::{Config, FecScheme};
use crate::fec::coder_for;
use crate::stats::StatsCounters;
use zerocopy::{FromBytes, IntoBytes};

fn limits() -> ReassemblerLimits {
    ReassemblerLimits {
        shard_bytes: 16,
        max_data_shards: 8,
        max_total_shards: 12,
        max_blocks: 4,
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
        key: [0u8; 16],
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
        key: [0u8; 16],
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
    let lim = limits(); // max_frame_bytes 4096, shards 16 B, ≤8 data shards × ≤4 blocks
    let mut r = Reassembler::new(lim);
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    // Largest geometry-consistent frame: 4 blocks × 8 shards × 16 B = 512 B per buffer.
    // Budget = 4 × 4096 = 16384 B → exactly 32 such frames fit; the 33rd must be refused.
    for i in 0..33u32 {
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
