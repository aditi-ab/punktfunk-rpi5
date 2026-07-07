//! Tier-1 microbenchmarks for the punktfunk/1 hot path — GPU-free, so they run in normal CI.
//!
//! Two layers:
//!  - `crypto/*`  — the isolated AES-128-GCM primitives on one ~MTU shard.
//!  - `pipeline/*`— a whole frame through the real per-frame path end to end over the in-process
//!    loopback transport: FEC encode → AES-GCM seal → packetize → (loopback) → reassemble →
//!    FEC decode → open. This is what a throughput/latency regression in the core would show up in.
//!
//! The GPU capture/NVENC encode path is deliberately out of scope here (no GPU in CI) — that's the
//! Tier-3 stream benchmark on a self-hosted GPU runner. Run locally with `cargo bench -p punktfunk-core`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use punktfunk_core::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use punktfunk_core::crypto::SessionCrypto;
use punktfunk_core::session::Session;
use punktfunk_core::transport::loopback_pair;

const TAG_LEN: usize = 16; // AES-GCM authentication tag
const SHARD: usize = punktfunk_core::config::mtu1500_shard_payload(); // one MTU-safe data shard

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
            // GF(2^8) is capped at ≤255 shards/block (Moonlight-compatible); GF(2^16) Leopard goes
            // far higher. Use a realistic, valid block size for each.
            max_data_per_block: match scheme {
                FecScheme::Gf8 => 128,
                FecScheme::Gf16 => 4096,
            },
        },
        shard_payload: SHARD,
        max_frame_bytes: 8 * 1024 * 1024,
        encrypt: true, // bench the real path — crypto is always on for punktfunk/1
        key: [7u8; 16],
        salt: [1, 2, 3, 4],
        loopback_drop_period: 0, // throughput run: no induced loss (loss-harness covers recovery)
    }
}

fn bench_crypto(c: &mut Criterion) {
    let host = SessionCrypto::new(&[7u8; 16], [1, 2, 3, 4], Role::Host);
    let client = SessionCrypto::new(&[7u8; 16], [1, 2, 3, 4], Role::Client);
    let payload = vec![0xABu8; SHARD];
    let sealed = host.seal(0, &payload).unwrap();

    let mut g = c.benchmark_group("crypto");
    g.throughput(Throughput::Bytes(SHARD as u64));
    g.bench_function("seal", |b| {
        let mut seq = 0u64;
        b.iter(|| {
            let ct = host.seal(seq, black_box(&payload)).unwrap();
            seq += 1;
            black_box(ct)
        })
    });
    g.bench_function("seal_in_place", |b| {
        let mut seq = 0u64;
        let mut buf = vec![0xABu8; SHARD + TAG_LEN];
        b.iter(|| {
            host.seal_in_place(seq, black_box(&mut buf)).unwrap();
            seq += 1;
        })
    });
    g.bench_function("open", |b| {
        b.iter(|| black_box(client.open(0, black_box(&sealed)).unwrap()))
    });
    g.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    let mut g = c.benchmark_group("pipeline");
    // 64 KB ≈ a steady-state P-frame; 1 MB ≈ a keyframe/scene-cut. Both FEC schemes (GF(2^8)
    // GameStream-compat vs GF(2^16) Leopard, the wall-breaker).
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
