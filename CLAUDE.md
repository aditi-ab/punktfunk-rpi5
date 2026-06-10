# CLAUDE.md — punktfunk

Low-latency desktop/game streaming stack, Linux-first, with a shared Rust protocol core
(`punktfunk-core`) exposed over a C ABI and native clients per platform. Full design:
[`docs/implementation-plan.md`](docs/implementation-plan.md). Status table: `README.md`.

## Where the work stands

- **M1 (`punktfunk-core` + C ABI): complete and hardened.** FEC recovery, loopback-under-loss,
  proptests, C ABI harness all green; 13 adversarial-review findings fixed +
  regression-tested (`a913042`).
- **M2 (GameStream host): working end-to-end with a stock Moonlight client.** Validated live
  on this box: pairing (persists across restarts), serverinfo/applist (app catalog from
  `~/.config/punktfunk/apps.json` → each entry picks a compositor + nested command), RTSP, ENet
  control, audio, and video at the **client's native resolution and refresh** — the host
  creates a per-session virtual output via per-compositor `VirtualDisplay` backends:
  **KWin** (`zkde_screencast stream_virtual_output`, needs KWin ≥ 6.5.6 headless; >60 Hz via
  custom modes), **gamescope** (spawned headless at WxH@Hz, its PipeWire node captured, needs
  gamescope ≥ 3.16.22 — older deadlocks on PipeWire ≥ 1.6), **Mutter** (D-Bus
  `RecordVirtual` virtual monitor; validated live on headless GNOME Shell 50, zero-copy),
  **Sway/wlroots** (`swaymsg create_output` + custom mode, xdpw portal capture with a
  managed chooser config; validated live on sway 1.11, zero-copy).
  Performance work landed and measured: GPU **zero-copy** on all paths (tiled dmabuf →
  EGL/GL → CUDA; LINEAR dmabuf → **Vulkan bridge** → CUDA → NVENC), auto 2-way NVENC
  split-encode above ~1 Gpix/s (5K@240), infinite GOP + RFI keyframes (killed the periodic
  freeze), encode|send thread split with `sendmmsg` batching. Stable 240 fps at 5120×1440.
  Input: mouse/keyboard (libei via RemoteDesktop portal on KWin/GNOME, gamescope's own EIS
  socket, wlr protocols on Sway) and **gamepads** (uinput X-Box-360 pads + rumble
  back-channel; validated live — pad created/destroyed with the session). Management REST API +
  checked-in OpenAPI doc (`mgmt.rs`).
- **M3 (`punktfunk/1`, the native protocol): full session planes, validated live.** QUIC
  control plane (`punktfunk-core` `quic` feature: Hello{mode}/Welcome{full Config}/Start), data
  plane = the hardened M1 `Session` over raw UDP with **GF(2¹⁶) Leopard FEC + AES-GCM**
  (inexpressible in GameStream), host creates the native virtual output at the client's
  requested mode. `m3-host` is a **persistent listener** (sessions back to back;
  `--max-sessions`). QUIC datagrams carry the side planes, demuxed by first byte: input
  0xC8 (incl. **gamepads** — incremental events accumulated into the uinput xpad), **Opus
  audio** 0xC9 (48 kHz stereo, 5 ms, host→client), **rumble** 0xCA (host→client). **Trust:**
  host serves its persistent identity (`~/.config/punktfunk/cert.pem`, shared with GameStream
  pairing) and logs the SHA-256 fingerprint; clients pin it (TOFU on first connect —
  `endpoint::client_pinned`), and a **PIN pairing ceremony** (host displays a 4-digit PIN,
  proof = HMAC over both cert fingerprints, single attempt) establishes mutual trust:
  clients present persistent identities via QUIC client auth, the host stores paired
  fingerprints (`punktfunk1-paired.json`) and can gate sessions with `--require-pairing`.
  **Mid-stream mode renegotiation**: `Reconfigure` on the still-open control stream — the
  host rebuilds output+encoder at the new mode in ~90 ms while the data plane runs on
  (validated live: one .h265 with 720p and 1080p segments). Measured on-box at 720p120: 1680/1680 frames, **p50 0.83 ms**
  capture→…→reassembled; audio measured live (~200 pkts/s). `punktfunk-client-rs` is the
  working reference client (`--pin`, datagram counters, `--input-test` incl. gamepad).
  The embeddable connector (`NativeClient`) exposes it all over the C ABI: `punktfunk_connect`
  (pin/TOFU) + `next_au`/`next_audio`/`next_rumble`/`send_input`.

## What's left

1. **M4 — client decode + present: macOS stage 1 done, first light achieved
   (2026-06-10).** PunktfunkKit compiles and is tested on macOS (AnnexB → VideoToolbox →
   `AVSampleBufferDisplayLayer`, GCMouse/GCKeyboard capture, `PunktfunkClient` app shell);
   validated live Mac ↔ this box at 720p60 — vkcube on glass, input injected via gamescope
   EIS. Tests: `swift test` in `clients/apple` (unit + real-codec round trip),
   `test-loopback.sh` (Swift client vs synthetic m3-host on loopback — runs on macOS),
   `RemoteFirstLightTests` (full pipeline over the LAN). See
   [`clients/apple/README.md`](clients/apple/README.md). Next: stage 2 presenter
   (`VTDecompressionSession` + `CAMetalLayer` frame pacing), glass-to-glass numbers via
   `tools/latency-probe` (scaffold), iOS variant. The Linux reference client
   (`punktfunk-client-rs`) gets VAAPI + wgpu on the same connector later.
2. **Sub-frame pipelining**: overlap encode and transmit within a frame. Requires a direct
   NVENC SDK wrapper (libavcodec only emits whole AUs) — the next big latency lever (~2–4 ms
   at high res).
3. **punktfunk/1 protocol growth**: concurrent sessions (today: one at a time, extras wait
   in the accept queue); mgmt REST endpoints for the punktfunk/1 paired-client list.
4. **M2 polish**: HDR negotiation, reconnect-at-new-mode robustness.
5. **Native clients** (`clients/{apple,android}` scaffolds) consuming `punktfunk_core.h`.

Box one-time setup is complete: udev rule + `input` group (gamepads validated live),
gamescope 3.16.22 installed system-wide (no PATH override), gnome-shell installed (Mutter
backend validated live). All three compositor backends are live-validated.

## Build / test / run

```sh
cargo build --workspace          # green on Linux and macOS
cargo test  --workspace          # unit + loopback + proptest + C ABI harness (~100 tests)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p loss-harness        # FEC loss-resilience sweep (no network needed)
bash crates/punktfunk-core/tests/c/run.sh   # standalone C-ABI link + round-trip proof
```

Generated artifacts are **checked in** and CI fails on drift: `include/punktfunk_core.h`
(cbindgen from `punktfunk-core/src/abi.rs`) and `docs/api/openapi.json` (regenerate with
`cargo run -p punktfunk-host -- openapi > docs/api/openapi.json`; spec lives in `mgmt.rs`).

## Layout

```
crates/punktfunk-core/        protocol · FEC · crypto · quic (punktfunk/1 control plane, feature-gated)
crates/punktfunk-host/
  gamestream/             Moonlight compat: nvhttp · pairing · rtsp · control · stream · gamepad · apps
  vdisplay/{kwin,gamescope,mutter,wlroots}.rs   per-compositor client-sized virtual outputs
  zerocopy/{egl,cuda,vulkan}.rs         dmabuf → CUDA → NVENC (tiled via EGL/GL, LINEAR via Vulkan)
  inject/{libei,wlr,gamepad}.rs         input backends (+ uinput virtual gamepads)
  capture.rs · encode.rs · audio.rs · m0.rs · m3.rs · mgmt.rs
crates/punktfunk-client-rs/   punktfunk/1 reference client (M3 headless; M4 adds decode+present)
tools/{loss-harness,latency-probe}/     measurement (plan §10)
scripts/                  60-punktfunk.rules · punktfunk-host.service · host.env.example · headless/
include/punktfunk_core.h      generated C header
```

## Design invariants — do not regress

- **One core, linked everywhere.** Protocol/FEC/crypto live only in `punktfunk-core`, behind a
  stable, versioned C ABI. `tokio`/`quinn` exist only behind the `quic` feature (control
  plane); **no async on the per-frame path** — native threads only.
- **Native client resolution, no scaling.** A session gets a virtual output at exactly the
  client's WxH@Hz via the `VirtualDisplay` trait (`create(mode) → VirtualOutput { node_id,
  remote_fd, preferred_mode, keepalive }`, RAII teardown). There is no cross-compositor
  protocol for this — each compositor keeps its own backend.
- **FEC is the wall-breaker.** GF(2⁸) (≤255 shards/block, Moonlight-compatible) and GF(2¹⁶)
  Leopard (≤65535 shards/block) — punktfunk/1 negotiates the latter, removing the ~1 Gbps
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
# compositor session (shell 1, or the systemd unit in scripts/): full headless Plasma.
# The script sets XDG_MENU_PREFIX=plasma- & co. — without it plasmashell runs but the
# launcher menu is EMPTY (no apps, no System Settings).
bash scripts/headless/run-headless-kde.sh 1920x1080

# host (shell 2):
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE PUNKTFUNK_VIDEO_SOURCE=virtual \
PUNKTFUNK_ZEROCOPY=1 cargo run -rp punktfunk-host -- serve

# punktfunk/1 native loopback test (no Moonlight needed; same env as serve, listener persists
# across sessions — bound it with --max-sessions):
cargo run -rp punktfunk-host -- m3-host --source virtual --seconds 10 --max-sessions 1
cargo run -rp punktfunk-client-rs -- --mode 1280x720x120 --out /tmp/a.h265 --input-test  # + --pin HEX
```

Pinned crate facts: `ashpd` 0.13 + `pipewire` 0.9 (must match ashpd's) + `ffmpeg-next` 8.x
(system FFmpeg 8 / libavcodec 62). Env knobs: `PUNKTFUNK_VIDEO_SOURCE=virtual|portal`,
`PUNKTFUNK_COMPOSITOR=kwin|gamescope|mutter`, `PUNKTFUNK_ZEROCOPY=1`, `PUNKTFUNK_GAMESCOPE_APP=...`,
`PUNKTFUNK_INPUT_BACKEND=...`, `PUNKTFUNK_PERF=1` (per-stage timing), `PUNKTFUNK_VIDEO_DROP=N` (FEC
test), `PUNKTFUNK_FEC_PCT=N`.

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
- `pkill` caution on this box: match exact comm names (`pkill -x gamescope-wl`,
  `pkill -x punktfunk-host`) — `pkill -f` self-matches the invoking shell.
