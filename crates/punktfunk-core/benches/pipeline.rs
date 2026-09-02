//! GPU-free microbenchmarks for the punktfunk/1 hot path; they run in ordinary CI.
//!
//! `crypto/*` — AES-128-GCM and ChaCha20-Poly1305 on one ~MTU shard.
//! `pipeline/*` — one frame through FEC encode → seal → packetize → loopback → reassemble →
//! FEC decode → open. A core throughput/latency regression shows up here.
//!
//! GPU capture / NVENC is out of scope (no GPU in CI). Run with
//! `cargo bench -p punktfunk-core`.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use punktfunk_core::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use punktfunk_core::crypto::{SessionCrypto, SessionKey};
use punktfunk_core::session::Session;
use punktfunk_core::transport::loopback_pair;
// Not `criterion::black_box`: deprecated in 0.8 (forwards here). Benches compile under
// `--all-targets -D warnings`, so that import is an error, not a warning.
use std::hint::black_box;

const TAG_LEN: usize = 16; // AEAD authentication tag (GCM and Poly1305 share the size)
const SHARD: usize = punktfunk_core::config::mtu1500_shard_payload();

fn cfg(role: Role, scheme: FecScheme) -> Config {
    Config {
        role,
        phase: match scheme {
            FecScheme::Gf8 => ProtocolPhase::P1GameStream,
            FecScheme::Gf16 => ProtocolPhase::P2Punktfunk,
        },
        fec: FecConfig {
            scheme,
            fec_percent: 25,
            // GF(2^8) ≤255 shards/block (Moonlight); GF(2^16) Leopard goes higher.
            max_data_per_block: match scheme {
                FecScheme::Gf8 => 128,
                FecScheme::Gf16 => 4096,
            },
        },
        shard_payload: SHARD,
        max_frame_bytes: 8 * 1024 * 1024,
        encrypt: true, // bench the real path — crypto is always on for punktfunk/1
        key: SessionKey::Aes128Gcm([7u8; 16]),
        salt: [1, 2, 3, 4],
        loopback_drop_period: 0, // throughput run: no induced loss (loss-harness covers recovery)
    }
}

fn bench_crypto(c: &mut Criterion) {
    let mut g = c.benchmark_group("crypto");
    g.throughput(Throughput::Bytes(SHARD as u64));
    // Both negotiated AEADs. The `_chacha20` series is the host sealing-cost check for the
    // soft-AES-armv7 path (`design/chacha20-session-cipher.md`). AES keeps unsuffixed names so
    // the CI regression compare retains its history.
    for (suffix, key) in [
        ("", SessionKey::Aes128Gcm([7u8; 16])),
        ("_chacha20", SessionKey::ChaCha20Poly1305([7u8; 32])),
    ] {
        let host = SessionCrypto::new(&key, [1, 2, 3, 4], Role::Host);
        let client = SessionCrypto::new(&key, [1, 2, 3, 4], Role::Client);
        let payload = vec![0xABu8; SHARD];
        let sealed = host.seal(0, &payload).unwrap();

        g.bench_function(format!("seal{suffix}"), |b| {
            let mut seq = 0u64;
            b.iter(|| {
                let ct = host.seal(seq, black_box(&payload)).unwrap();
                seq += 1;
                black_box(ct)
            })
        });
        g.bench_function(format!("seal_in_place{suffix}"), |b| {
            let mut seq = 0u64;
            let mut buf = vec![0xABu8; SHARD + TAG_LEN];
            b.iter(|| {
                host.seal_in_place(seq, black_box(&mut buf)).unwrap();
                seq += 1;
            })
        });
        g.bench_function(format!("open{suffix}"), |b| {
            b.iter(|| black_box(client.open(0, black_box(&sealed)).unwrap()))
        });
        g.bench_function(format!("open_in_place{suffix}"), |b| {
            // In-place open consumes the buffer, so each iteration restores the ciphertext first.
            let mut buf = sealed.clone();
            b.iter(|| {
                buf.copy_from_slice(black_box(&sealed));
                black_box(client.open_in_place(0, &mut buf).unwrap());
            })
        });
    }
    g.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    let mut g = c.benchmark_group("pipeline");
    // 64 KB ≈ a steady-state P-frame; 1 MB ≈ a keyframe / scene-cut.
    for scheme in [FecScheme::Gf8, FecScheme::Gf16] {
        let label = match scheme {
            FecScheme::Gf8 => "gf8",
            FecScheme::Gf16 => "gf16",
        };
        for &size in &[64 * 1024usize, 1024 * 1024] {
            g.throughput(Throughput::Bytes(size as u64));
            g.bench_with_input(BenchmarkId::new(label, size), &size, |b, &size| {
                let (h, cl) = loopback_pair(0, 0);
                let mut host = Session::new(cfg(Role::Host, scheme), Box::new(h)).unwrap();
                let mut client = Session::new(cfg(Role::Client, scheme), Box::new(cl)).unwrap();
                let frame = vec![0x5Au8; size];
                let mut seq = 0u64;
                b.iter(|| {
                    host.submit_frame(black_box(&frame), seq, 0).unwrap();
                    let f = client.poll_frame().unwrap();
                    seq += 1;
                    black_box(f)
                })
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench_crypto, bench_pipeline);
criterion_main!(benches);
