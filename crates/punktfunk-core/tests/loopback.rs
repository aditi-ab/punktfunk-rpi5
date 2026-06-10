//! M1 acceptance: round-trip access units through the full host→client path
//! (packetize → FEC → loopback with simulated loss → recover → reassemble) and assert
//! byte-exact recovery, for both FEC schemes, with and without encryption. Plus
//! property tests over the FEC layer's loss patterns.

use proptest::prelude::*;
use punktfunk_core::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use punktfunk_core::fec::coder_for;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::session::Session;
use punktfunk_core::transport::loopback_pair;

fn config(role: Role, scheme: FecScheme, encrypt: bool, drop_period: u32) -> Config {
    Config {
        role,
        phase: match scheme {
            FecScheme::Gf8 => ProtocolPhase::P1GameStream,
            FecScheme::Gf16 => ProtocolPhase::P2Punktfunk,
        },
        fec: FecConfig {
            scheme,
            fec_percent: 25,
            max_data_per_block: 32,
        },
        shard_payload: 1024,
        max_frame_bytes: 8 * 1024 * 1024,
        encrypt,
        key: [7u8; 16],
        salt: [1, 2, 3, 4],
        loopback_drop_period: drop_period,
    }
}

/// Drive `frames` access units host→client over a lossy loopback and assert each one
/// comes back byte-identical. Returns the client's final stats.
fn run_stream(
    scheme: FecScheme,
    encrypt: bool,
    drop_period: u32,
    frames: &[Vec<u8>],
) -> punktfunk_core::Stats {
    let (host_tp, client_tp) = loopback_pair(drop_period, 0);
    let mut host = Session::new(
        config(Role::Host, scheme, encrypt, drop_period),
        Box::new(host_tp),
    )
    .unwrap();
    let mut client = Session::new(
        config(Role::Client, scheme, encrypt, drop_period),
        Box::new(client_tp),
    )
    .unwrap();

    for (i, frame) in frames.iter().enumerate() {
        host.submit_frame(frame, i as u64 * 1_000_000, 0).unwrap();
        let got = client
            .poll_frame()
            .expect("frame should recover despite loss");
        assert_eq!(&got.data, frame, "frame {i} mismatched after recovery");
        assert_eq!(got.frame_index, i as u32);
        assert_eq!(got.pts_ns, i as u64 * 1_000_000);
    }
    client.stats()
}

fn sample_frames() -> Vec<Vec<u8>> {
    (0..5usize)
        .map(|f| {
            let len = 1 + f * 40_000; // 1, 40k, 80k, 120k, 160k → single- and multi-block
            (0..len)
                .map(|b| (b.wrapping_mul(31).wrapping_add(f * 7)) as u8)
                .collect()
        })
        .collect()
}

#[test]
fn gf8_stream_recovers_under_loss() {
    let frames = sample_frames();
    // drop_period 8 deletes the 1st of every 8 packets → real data-shard loss.
    let stats = run_stream(FecScheme::Gf8, false, 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert!(
        stats.fec_recovered_shards > 0,
        "loss should have forced FEC recovery"
    );
}

#[test]
fn gf16_stream_recovers_under_loss() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf16, false, 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert!(stats.fec_recovered_shards > 0);
}

#[test]
fn encrypted_stream_recovers_under_loss() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf8, true, 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
}

#[test]
fn lossless_stream_is_exact() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf16, false, 0, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert_eq!(
        stats.fec_recovered_shards, 0,
        "no loss → nothing to recover"
    );
}

#[test]
fn input_round_trips_client_to_host() {
    let (host_tp, client_tp) = loopback_pair(0, 0);
    let mut host = Session::new(
        config(Role::Host, FecScheme::Gf8, false, 0),
        Box::new(host_tp),
    )
    .unwrap();
    let mut client = Session::new(
        config(Role::Client, FecScheme::Gf8, false, 0),
        Box::new(client_tp),
    )
    .unwrap();

    let sent = InputEvent {
        kind: InputKind::MouseMove,
        _pad: [0; 3],
        code: 0,
        x: -7,
        y: 13,
        flags: 0,
    };
    client.send_input(&sent).unwrap();
    let got = host
        .poll_input()
        .unwrap()
        .expect("host should receive the input event");
    assert_eq!(got, sent);
}

// ---- property tests over the FEC layer --------------------------------------

proptest! {
    /// For random shard counts and an erasure set within the recovery budget, every
    /// original shard is reconstructed byte-identically — for both backends.
    #[test]
    fn fec_recovers_any_loss_within_budget(
        k in 1usize..40,
        extra in 0usize..16,        // recovery beyond the bare minimum
        shard_half in 1usize..64,   // shard_len = 2*shard_half (even)
        seed in any::<u64>(),
    ) {
        let m = (extra + 1).min(40);
        let shard_len = shard_half * 2;
        for coder in [coder_for(FecScheme::Gf8), coder_for(FecScheme::Gf16)] {
            // Gf8 ceiling: data + recovery <= 255.
            if matches!(coder.scheme(), FecScheme::Gf8) && k + m > 255 { continue; }

            let data: Vec<Vec<u8>> = (0..k)
                .map(|i| (0..shard_len).map(|b| (i ^ b).wrapping_add(seed as usize) as u8).collect())
                .collect();
            let recovery = coder.encode(&data, m).unwrap();

            let mut received: Vec<Option<Vec<u8>>> =
                data.iter().cloned().map(Some).chain(recovery.into_iter().map(Some)).collect();

            // Erase up to `m` shards chosen by a cheap PRNG over the seed.
            let total = k + m;
            let lose = (seed as usize % (m + 1)).min(m);
            let mut s = seed | 1;
            for _ in 0..lose {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                let idx = (s >> 33) as usize % total;
                received[idx] = None;
            }

            let restored = coder.reconstruct(k, m, &mut received).unwrap();
            prop_assert_eq!(restored, data);
        }
    }
}
