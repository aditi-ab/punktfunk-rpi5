# CLAUDE.md — lumen

Low-latency desktop/game streaming stack, Linux-first, with a shared Rust protocol core
(`lumen-core`) exposed over a C ABI and native clients per platform. Full design:
[`docs/implementation-plan.md`](docs/implementation-plan.md). Status table: `README.md`.

## Where the work stands

- **M1 (`lumen-core` + C ABI): complete and hardened.** FEC recovery, loopback-under-loss,
  proptests, C ABI harness all green; 13 adversarial-review findings fixed +
  regression-tested (`a913042`).
- **M2 (GameStream host): working end-to-end with a stock Moonlight client.** Validated live
  on this box: pairing (persists across restarts), serverinfo/applist (app catalog from
  `~/.config/lumen/apps.json` → each entry picks a compositor + nested command), RTSP, ENet
  control, audio, and video at the **client's native resolution and refresh** — the host
  creates a per-session virtual output via per-compositor `VirtualDisplay` backends:
  **KWin** (`zkde_screencast stream_virtual_output`, needs KWin ≥ 6.5.6 headless; >60 Hz via
  custom modes), **gamescope** (spawned headless at WxH@Hz, its PipeWire node captured, needs
  gamescope ≥ 3.16.22 — older deadlocks on PipeWire ≥ 1.6), **Mutter** (D-Bus
  `RecordVirtual`; implemented, live validation pending a gnome-shell install).
  Performance work landed and measured: GPU **zero-copy** on all paths (tiled dmabuf →
  EGL/GL → CUDA; LINEAR dmabuf → **Vulkan bridge** → CUDA → NVENC), auto 2-way NVENC
  split-encode above ~1 Gpix/s (5K@240), infinite GOP + RFI keyframes (killed the periodic
  freeze), encode|send thread split with `sendmmsg` batching. Stable 240 fps at 5120×1440.
  Input: mouse/keyboard (libei via RemoteDesktop portal on KWin/GNOME, gamescope's own EIS
  socket, wlr protocols on Sway) and **gamepads** (uinput X-Box-360 pads + rumble
  back-channel; live validation pending the udev rule below). Management REST API +
  checked-in OpenAPI doc (`mgmt.rs`).
- **M3 (`lumen/1`, the native protocol): seeded and validated.** QUIC control plane
  (`lumen-core` `quic` feature: Hello{mode}/Welcome{full Config}/Start), data plane = the
  hardened M1 `Session` over raw UDP with **GF(2¹⁶) Leopard FEC + AES-GCM** (inexpressible
  in GameStream), input over **QUIC datagrams**, host creates the native virtual output at
  the client's requested mode. Measured on-box at 720p120: 1680/1680 frames, **p50 0.83 ms**
  capture→encode→FEC→crypto→UDP→reassembled. `lumen-client-rs` is a working (headless)
  reference client. Trust is seed-stage (self-signed / accept-any).

## What's left

1. **M4 — client decode + present**: the SwiftUI client is scaffolded and handed off —
   the lumen/1 connector is in the C ABI (`lumen_connect` & co., ABI-roundtrip-tested) with
   an xcframework build script + LumenKit Swift package; **see
   [`clients/apple/README.md`](clients/apple/README.md) for the Mac-side pickup**. Then
   glass-to-glass numbers via `tools/latency-probe` (scaffold). The Linux reference client
   (`lumen-client-rs`) gets VAAPI + wgpu on the same connector later.
2. **Sub-frame pipelining**: overlap encode and transmit within a frame. Requires a direct
   NVENC SDK wrapper (libavcodec only emits whole AUs) — the next big latency lever (~2–4 ms
   at high res).
3. **lumen/1 trust model**: pairing + certificate pinning to replace accept-any.
4. **M2 polish**: wlroots/Sway `VirtualDisplay` backend (deferred; swaymsg `create_output`),
   GNOME live validation, gamepad live validation, HDR/10-bit/AV1 negotiation, surround
   audio, reconnect-at-new-mode robustness.
5. **Native clients** (`clients/{apple,android}` scaffolds) consuming `lumen_core.h`.
6. **This box, one-time setup still pending**: `sudo cp scripts/60-lumen.rules
   /etc/udev/rules.d/` + user into `input` group (gamepads); `sudo ninja -C
   /tmp/gamescope-src/build install` (the fixed gamescope ≥ 3.16.22 — until then use
   `PATH=/tmp/gamescope-src/build/src:$PATH`); `apt install gnome-shell` (Mutter validation).

## Build / test / run

```sh
cargo build --workspace          # green on Linux and macOS
cargo test  --workspace          # unit + loopback + proptest + C ABI harness (~92 tests)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p loss-harness        # FEC loss-resilience sweep (no network needed)
bash crates/lumen-core/tests/c/run.sh   # standalone C-ABI link + round-trip proof
```

Generated artifacts are **checked in** and CI fails on drift: `include/lumen_core.h`
(cbindgen from `lumen-core/src/abi.rs`) and `docs/api/openapi.json` (regenerate with
`cargo run -p lumen-host -- openapi > docs/api/openapi.json`; spec lives in `mgmt.rs`).

## Layout

```
crates/lumen-core/        protocol · FEC · crypto · quic (lumen/1 control plane, feature-gated)
crates/lumen-host/
  gamestream/             Moonlight compat: nvhttp · pairing · rtsp · control · stream · gamepad · apps
  vdisplay/{kwin,gamescope,mutter}.rs   per-compositor client-sized virtual outputs
  zerocopy/{egl,cuda,vulkan}.rs         dmabuf → CUDA → NVENC (tiled via EGL/GL, LINEAR via Vulkan)
  inject/{libei,wlr,gamepad}.rs         input backends (+ uinput virtual gamepads)
  capture.rs · encode.rs · audio.rs · m0.rs · m3.rs · mgmt.rs
crates/lumen-client-rs/   lumen/1 reference client (M3 headless; M4 adds decode+present)
tools/{loss-harness,latency-probe}/     measurement (plan §10)
scripts/                  60-lumen.rules · lumen-host.service · host.env.example · headless/
include/lumen_core.h      generated C header
```

## Design invariants — do not regress

- **One core, linked everywhere.** Protocol/FEC/crypto live only in `lumen-core`, behind a
  stable, versioned C ABI. `tokio`/`quinn` exist only behind the `quic` feature (control
  plane); **no async on the per-frame path** — native threads only.
- **Native client resolution, no scaling.** A session gets a virtual output at exactly the
  client's WxH@Hz via the `VirtualDisplay` trait (`create(mode) → VirtualOutput { node_id,
  remote_fd, preferred_mode, keepalive }`, RAII teardown). There is no cross-compositor
  protocol for this — each compositor keeps its own backend.
- **FEC is the wall-breaker.** GF(2⁸) (≤255 shards/block, Moonlight-compatible) and GF(2¹⁶)
  Leopard (≤65535 shards/block) — lumen/1 negotiates the latter, removing the ~1 Gbps
  ceiling.
- **M1 security hardening stays intact**: reassembler bounds attacker-controlled fields
  before allocating (`ReassemblerLimits`); AES-GCM per-direction nonce salts + seq-as-AAD;
  ABI `struct_size` checks. Regression tests exist — keep them green.
- **PipeWire consumer discipline**: our capture streams set `node.dont-reconnect` and tear
  down promptly on negotiation timeout — one wedged link head-blocks the daemon's shared
  work queue system-wide.

## Running on this box

Headless QEMU VM (Ubuntu 26.04, kernel 7.0), passthrough RTX 5070 Ti (driver 595 **open**
module — a kernel update silently drops it; reinstall `nvidia-driver-595-open`), no KMS
scanout → KWin `--drm` impossible; everything renders offscreen via `renderD128`.

```sh
# compositor session (shell 1, or the systemd unit in scripts/):
XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
XDG_CURRENT_DESKTOP=KDE KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
kwin_wayland --virtual --width 1920 --height 1080 --no-lockscreen --socket wayland-kde \
  --exit-with-session wev

# host (shell 2; gamescope entries need the PATH override until ninja install):
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE LUMEN_VIDEO_SOURCE=virtual \
LUMEN_ZEROCOPY=1 PATH=/tmp/gamescope-src/build/src:$PATH cargo run -rp lumen-host -- serve

# lumen/1 native loopback test (no Moonlight needed):
cargo run -rp lumen-host -- m3-host --source virtual --seconds 10   # + LUMEN_COMPOSITOR=gamescope etc.
cargo run -rp lumen-client-rs -- --mode 1280x720x120 --out /tmp/a.h265 --input-test
```

Pinned crate facts: `ashpd` 0.13 + `pipewire` 0.9 (must match ashpd's) + `ffmpeg-next` 8.x
(system FFmpeg 8 / libavcodec 62). Env knobs: `LUMEN_VIDEO_SOURCE=virtual|portal`,
`LUMEN_COMPOSITOR=kwin|gamescope|mutter`, `LUMEN_ZEROCOPY=1`, `LUMEN_GAMESCOPE_APP=...`,
`LUMEN_INPUT_BACKEND=...`, `LUMEN_PERF=1` (per-stage timing), `LUMEN_VIDEO_DROP=N` (FEC
test), `LUMEN_FEC_PCT=N`.

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
- `pkill` caution on this box: match exact comm names (`pkill -x gamescope-wl`,
  `pkill -x lumen-host`) — `pkill -f` self-matches the invoking shell.
