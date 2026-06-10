# CLAUDE.md — lumen

Low-latency desktop streaming stack, Linux-first, with a shared Rust protocol core
(`lumen-core`) exposed over a C ABI and native clients per platform. Full design:
[`docs/implementation-plan.md`](docs/implementation-plan.md). Status table: `README.md`.

## Where the work stands

- **M1 (`lumen-core` + C ABI) is complete, tested, and hardened.** It builds and its full
  suite passes (FEC recovery, loopback-under-loss, proptests, a C ABI harness). It was put
  through an adversarial review and 13 verified findings were fixed + regression-tested
  (commit `a913042`).
- **M0 (the pipeline spike) is done and verified** on the NVIDIA box (Ubuntu 25.10, RTX
  5070 Ti, driver 595): `lumen-host m0` captures a headless wlroots output via the
  ScreenCast portal + PipeWire, NVENC-encodes it, writes a playable H.265 file, and
  round-trips every access unit through a `lumen_core` host→client session (0 mismatches).
  See [`docs/linux-setup.md`](docs/linux-setup.md); the code is in
  `crates/lumen-host/src/{m0,capture,encode}.rs` (+ `capture/linux.rs`, `encode/linux.rs`).
- **M2 is in flight.** The GameStream control plane lives in `gamestream/` (mDNS,
  serverinfo, pairing, RTSP, ENet control, video/audio streams) and the management REST
  API in `mgmt.rs`; the remaining `#[cfg(target_os = "linux")]` backends — KWin/Mutter
  virtual displays (`vdisplay.rs`), libei/uinput input (`inject.rs`) — compile everywhere
  and `bail!` where unimplemented.

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
regenerated header when the ABI changes. Same deal for the management API's OpenAPI
document: `docs/api/openapi.json` is **checked in** for client codegen and a test fails if
it drifts — regenerate with `cargo run -p lumen-host -- openapi > docs/api/openapi.json`
(the spec lives in `crates/lumen-host/src/mgmt.rs`).

## Layout

```
crates/lumen-core/   protocol · FEC · pacing · crypto — the C ABI (lib + cdylib + staticlib)
crates/lumen-host/   Linux host: vdisplay · capture · encode · inject · gamestream · mgmt · pipeline
docs/api/openapi.json   generated management-API spec (codegen input)
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

## Running the M0 spike on this box

[`docs/linux-setup.md`](docs/linux-setup.md) is the reference. One-time: `bash
scripts/bootstrap-ubuntu.sh` (verifies NVIDIA/NVENC, installs deps incl. `libnvidia-gl`,
adds the `render`/`video` groups — re-login after). Then per run: `bash
scripts/headless/run-headless-sway.sh` (shell 1) and `bash
scripts/headless/prepare-session.sh` (shell 2), then `cargo run -p lumen-host -- m0
--source portal --out /tmp/lumen-m0.h265`. `--source synthetic` needs no capture session.

M0 uses the **CPU-copy capture path** (portal → PipeWire shm, packed `RGB` on wlroots →
NVENC `rgb0`); dmabuf→NVENC zero-copy is deferred (plan §9). Pinned crate facts (the setup
doc has the why): `ashpd` **0.13** (`screencast` feature, options-struct API, multi-thread
tokio runtime) + `pipewire` **0.9** (must match ashpd's; not 0.10) + `ffmpeg-next` **8.x**
(binds the system FFmpeg **8.x** / libavcodec 62 on Ubuntu 26.04; bumped from 7.x).

## Next: M2 — P1 host to a stock Moonlight client

Wire M0's capture→encode pipeline (`m0.rs` / `pipeline.rs`) into a streaming host: KWin
virtual output (`vdisplay.rs`, study KRdp), `serverinfo`/pairing/RTSP
(`gamestream/{nvhttp,pairing,rtsp}.rs`) enough for a real Moonlight client, input via
reis/uinput (`inject.rs`), management/config REST API (`mgmt.rs`).

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
