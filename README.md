# punktfunk

*A ground-up low-latency desktop streaming stack, built Linux-first, with a shared Rust
protocol core and native clients per platform.*

`punktfunk` is a placeholder codename. The bet: ship a **Linux virtual-display streaming
host** that speaks the existing Moonlight protocol (every Moonlight/Artemis client works
day one), then break the ~1 Gbps FEC wall with a **GF(2¹⁶) Leopard-RS** transport as a
negotiated extension. See [`docs/implementation-plan.md`](docs/implementation-plan.md).

## Status

| Milestone | State |
|-----------|-------|
| **M1 — `punktfunk-core` + C ABI** | ✅ done & tested (FEC, packetization, crypto, session, `punktfunk_core.h`) |
| **M0 — pipeline spike** (wlroots→PipeWire→NVENC→file→`punktfunk-core`) | ✅ done & verified on NVIDIA (RTX 5070 Ti / driver 595) |
| M2 — P1 host → stock Moonlight | 🟡 capture+encode landed in M0; pairing/RTSP/vdisplay pending |
| M3 — measurement harness | 🟡 `tools/loss-harness` runs; `latency-probe` scaffolded |
| M4 — P2 transport + Rust client | 🟡 GF(2¹⁶) core done; `punktfunk-client-rs` scaffolded |
| M5 — Apple client | 🟡 macOS first light: HEVC on glass + input over `punktfunk/1` (`clients/apple`) |

`punktfunk-core` is complete and verified: it builds and its full test suite (FEC recovery,
loopback round-trip under loss, property tests, and a **C ABI harness**) passes on
macOS/aarch64. **M0 is done:** `punktfunk-host` captures a headless wlroots output via the
ScreenCast portal + PipeWire, encodes it with NVENC, writes a playable H.265 file, and
round-trips every access unit through a `punktfunk_core` host→client session (see
`docs/linux-setup.md`). M2 is in flight: the GameStream control plane (`gamestream/`) and
the management REST API (`mgmt.rs`, OpenAPI spec in `docs/api/`) are implemented; the
remaining Linux host backends (KWin/Mutter virtual displays, libei input) are
`#[cfg(target_os = "linux")]` seams — defined and compiling, implementations pending.

## Layout

```
crates/
  punktfunk-core/        protocol · FEC · pacing · crypto — the C ABI (lib + cdylib + staticlib)
  punktfunk-host/        Linux host: vdisplay · capture · encode · inject · gamestream · mgmt
  punktfunk-client-rs/   reference client (M4): VAAPI decode + wgpu present
clients/{apple,android}/   native client scaffolds (import punktfunk_core.h)
include/punktfunk_core.h       cbindgen-generated C header (checked in)
tools/{latency-probe,loss-harness}/   measurement (plan §10)
docs/implementation-plan.md
```

## Build & test

```sh
cargo build --workspace          # green on Linux and macOS
cargo test  --workspace          # unit + loopback + proptest + C ABI harness
cargo clippy --workspace --all-targets

cargo run -p loss-harness        # FEC loss-resilience sweep (no network needed)
bash crates/punktfunk-core/tests/c/run.sh   # standalone C-ABI link+round-trip proof
```

The C header regenerates from `crates/punktfunk-core/src/abi.rs` on every build (cbindgen via
`build.rs`) into `include/punktfunk_core.h`.

## Design invariants

- **One core, linked everywhere.** Protocol/FEC/crypto/pacing live in `punktfunk-core` exactly
  once, exposed over a stable, versioned C ABI (`punktfunk_abi_version()`, `PunktfunkConfig`
  carries its own `struct_size`).
- **No async on the hot path.** The per-frame pipeline uses native threads only;
  `tokio`/`quinn` are gated behind the off-by-default `quic` feature (control plane only).
- **FEC is the wall-breaker.** GF(2⁸) (≤255 shards/block) for Moonlight compat;
  GF(2¹⁶) (≤65535 shards/block, SIMD, O(n log n)) to push past ~1 Gbps.

## License

MIT OR Apache-2.0.
