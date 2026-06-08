# CLAUDE.md — lumen

Low-latency desktop streaming stack, Linux-first, with a shared Rust protocol core
(`lumen-core`) exposed over a C ABI and native clients per platform. Full design:
[`docs/implementation-plan.md`](docs/implementation-plan.md). Status table: `README.md`.

## Where the work stands

- **M1 (`lumen-core` + C ABI) is complete, tested, and hardened.** It builds and its full
  suite passes (FEC recovery, loopback-under-loss, proptests, a C ABI harness). It was put
  through an adversarial review and 13 verified findings were fixed + regression-tested
  (commit `a913042`).
- **The host backends are `#[cfg(target_os = "linux")]` stubs.** They compile everywhere
  but `bail!` until implemented. This is the next work (**M0**, then **M2**) and needs a
  real Linux GPU + Wayland stack — which is why this repo is being moved to the NVIDIA
  Ubuntu VM.

## Build / test / run

```sh
cargo build --workspace          # green on Linux and macOS
cargo test  --workspace          # unit + loopback + proptest + C ABI harness
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p loss-harness        # FEC loss-resilience sweep (no network needed)
bash crates/lumen-core/tests/c/run.sh   # standalone C-ABI link + round-trip proof
```

`include/lumen_core.h` is generated from `crates/lumen-core/src/abi.rs` by cbindgen
(`build.rs`) on every build and is **checked in**; CI fails if it drifts, so commit the
regenerated header when the ABI changes.

## Layout

```
crates/lumen-core/   protocol · FEC · pacing · crypto — the C ABI (lib + cdylib + staticlib)
crates/lumen-host/   Linux host: vdisplay · capture · encode · inject · web · pipeline (cfg-gated)
crates/lumen-client-rs/   reference client (M4)
tools/{loss-harness,latency-probe}/   measurement (plan §10)
clients/{apple,android}/   native client scaffolds (import lumen_core.h)
include/lumen_core.h   generated C header
```

## Design invariants — do not regress

- **One core, linked everywhere.** Protocol/FEC/crypto/pacing live only in `lumen-core`,
  behind a stable, versioned C ABI (`lumen_abi_version`, `LumenConfig.struct_size`).
- **No async on the hot path.** The per-frame pipeline uses native threads only;
  `tokio`/`quinn` are gated behind the off-by-default `quic` feature (control plane only).
- **FEC is the wall-breaker.** GF(2⁸) (≤255 shards/block, Moonlight-compatible) and
  GF(2¹⁶) Leopard-RS (≤65535 shards/block, SIMD) — the latter removes the ~1 Gbps ceiling.
- **Security hardening from the M1 review must stay intact:** the reassembler bounds every
  attacker-controlled header field against negotiated limits *before allocating*
  (`ReassemblerLimits` in `packet.rs`); AES-GCM uses per-direction nonce salts + seq-as-AAD
  (`crypto.rs`); the ABI enforces `struct_size` and range-checks inputs. There are
  regression tests for these — keep them green.

## Next: M0 (the pipeline spike) on this VM

**Start here on the NVIDIA Ubuntu VM:** [`docs/linux-setup.md`](docs/linux-setup.md), then
run `bash scripts/bootstrap-ubuntu.sh` (verifies NVIDIA/NVENC, installs the Rust/PipeWire/
wlroots/FFmpeg-dev deps) and bring up headless Sway with `scripts/headless/`.

Per plan §8/§12: drive a headless Sway/wlroots output → capture via PipeWire (ScreenCast
portal, `ashpd` 0.13 + `pipewire` 0.10) → encode with NVENC (`ffmpeg-next` 7.x,
`hevc_nvenc`) → write a playable H.265 file. Then wire that pipeline into a `lumen-core`
host `Session` (M2). The module seams exist in
`crates/lumen-host/src/{vdisplay,capture,encode,inject,pipeline}.rs`. **Budget for the
CPU-copy fallback first** — dmabuf→NVENC zero-copy import is unreliable across NVIDIA
driver versions (plan §9 risk); the setup doc covers it.

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
