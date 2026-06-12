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
| **M1 — `punktfunk-core` + C ABI** | ✅ done & hardened (FEC, packetization, AES-GCM, session, adversarial-review fixes, `punktfunk_core.h`) |
| **M2 — GameStream host → stock Moonlight** | ✅ live end-to-end: pairing, RTSP, audio, per-client virtual output at native res, GPU zero-copy NVENC, gamepads |
| **M3 — `punktfunk/1` native protocol** | ✅ validated live: QUIC control + GF(2¹⁶) FEC/AES data plane, SPAKE2 PIN pairing, mid-stream mode renegotiation |
| **M4 — client decode + present (Apple)** | 🟡 macOS first light: AnnexB→VideoToolbox HEVC on glass + input/pairing over `punktfunk/1` (`clients/apple`); iOS + presenter next |
| **Web console + management API** | ✅ TanStack web console (`web/`) over the OpenAPI mgmt API: host status, paired devices, on-demand native pairing (arm → show PIN) |

The **GameStream host works with a stock Moonlight client** — validated live on NVIDIA
(RTX 5070 Ti & RTX 4090, driver 595): trust-on-first-use pairing that persists, an app
catalog, RTSP/ENet/audio, and **video at the client's exact resolution and refresh** via a
per-session virtual output (KWin, gamescope, Mutter, Sway backends), encoded with GPU
**zero-copy** (dmabuf → CUDA/Vulkan → NVENC) at up to 5120×1440@240. The native
**`punktfunk/1`** protocol adds a QUIC control plane and a GF(2¹⁶) Leopard-FEC + AES-GCM data
plane (p50 ~0.8 ms capture→reassembled at 720p120), with a SPAKE2 PIN pairing ceremony. Both
run from **one process** (`serve --native`), managed through a REST API + web console. Builds
against FFmpeg 7 or 8; deployed live on Bazzite. Full status: [`CLAUDE.md`](CLAUDE.md);
roadmap, setup guides & progress: the docs site ([`docs-site/`](docs-site) — Fumadocs;
`bun run dev`), with the canonical [roadmap](docs-site/content/docs/roadmap.md) and
[status](docs-site/content/docs/status.md) there. Design notes stay in [`docs/`](docs).

## Layout

```
crates/
  punktfunk-core/        protocol · FEC · pacing · crypto · quic — the C ABI (lib + cdylib + staticlib)
  punktfunk-host/        Linux host: vdisplay · capture · encode · inject · gamestream · m3 · mgmt · native_pairing
  punktfunk-client-rs/   punktfunk/1 reference client (M3 headless; M4 adds decode+present)
clients/{apple,android}/   native client scaffolds (import punktfunk_core.h); apple = macOS first light
web/                       TanStack web console (host status · paired devices · pairing) over the mgmt API
packaging/                 Fedora/Bazzite RPM · bootc image · COPR (see packaging/bazzite/README.md)
include/punktfunk_core.h       cbindgen-generated C header (checked in)
tools/{latency-probe,loss-harness}/   measurement (plan §10)
docs/{implementation-plan,roadmap,windows-host,dualsense-haptics}.md
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
