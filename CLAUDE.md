# CLAUDE.md — punktfunk

Low-latency desktop/game streaming stack, Linux-first, with a shared Rust protocol core
(`punktfunk-core`) exposed over a C ABI and native clients per platform. Full design:
[`docs/implementation-plan.md`](docs/implementation-plan.md). Status table: `README.md`.

## Where the work stands

- **Core (`punktfunk-core` + C ABI): complete and hardened.** FEC recovery, loopback-under-loss,
  proptests, C ABI harness all green; 13 adversarial-review findings fixed +
  regression-tested (`a913042`).
- **GameStream host: working end-to-end with a stock Moonlight client.** Validated live
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
- **Native protocol (`punktfunk/1`): full session planes, validated live.** QUIC
  control plane (`punktfunk-core` `quic` feature: Hello{mode}/Welcome{full Config}/Start), data
  plane = the hardened core `Session` over raw UDP with **GF(2¹⁶) Leopard FEC + AES-GCM**
  (inexpressible in GameStream), host creates the native virtual output at the client's
  requested mode. `punktfunk1-host` is a **persistent listener** (sessions back to back;
  `--max-sessions`). QUIC datagrams carry the side planes, demuxed by first byte: input
  0xC8 (incl. **gamepads** — incremental events accumulated into the uinput xpad), **Opus
  audio** 0xC9 (48 kHz stereo, 5 ms, host→client), **rumble** 0xCA (host→client). **Trust:**
  host serves its persistent identity (`~/.config/punktfunk/cert.pem`, shared with GameStream
  pairing) and logs the SHA-256 fingerprint; clients pin it, established by a **SPAKE2 PIN pairing
  ceremony** (host arms pairing and displays a 4-digit PIN; a PAKE binds both cert fingerprints so an
  attacker gets one online guess, no offline dictionary attack) — PIN pairing is the default for new
  hosts. **TOFU on first connect** (`endpoint::client_pinned`) stays as an explicit host opt-in
  (`punktfunk1-host --allow-tofu` / `serve --open`, advertised as `pair=optional`) for fully trusted LANs;
  clients only offer the TOFU "Trust" path for a host that advertised `pair=optional`, route every
  other new host straight to the PIN ceremony, and on a pinned-fingerprint change force re-pairing
  (no re-TOFU shortcut). Clients present persistent identities via QUIC client auth, the host stores
  paired fingerprints (`punktfunk1-paired.json`) and gates sessions with `--require-pairing` (the
  default; `--allow-tofu`/`--open` accept unpaired clients).
  **LAN auto-discovery**: both `serve --native` and `punktfunk1-host` advertise the native service over
  mDNS (`_punktfunk._udp`, `crate::discovery`) with TXT `proto`/`fp`(cert fingerprint to
  pin)/`pair`(required|optional)/`id`; `punktfunk-probe --discover` lists hosts, Apple clients
  browse the same service via NWBrowser (validated cross-LAN 2026-06-12).
  **Mid-stream mode renegotiation**: `Reconfigure` on the still-open control stream — the
  host rebuilds output+encoder at the new mode in ~90 ms while the data plane runs on
  (validated live: one .h265 with 720p and 1080p segments). Measured on-box at 720p120: 1680/1680 frames, **p50 0.83 ms**
  capture→…→reassembled; audio measured live (~200 pkts/s). A **wall-clock skew handshake**
  (`ClockProbe`/`ClockEcho`, 8 NTP rounds after `Start`, `clock_offset_ns`) aligns the client to the
  host clock, so that latency is now valid **cross-machine** (`skew_corrected=true`) — measured GNOME
  box → dev box over the LAN: **p50 1.30 ms** (the −1.57 ms inter-box clock offset removed).
  `punktfunk-probe` is the
  working reference client (`--pin`, datagram counters, `--input-test` incl. gamepad).
  The embeddable connector (`NativeClient`) exposes it all over the C ABI: `punktfunk_connect`
  (pin/TOFU) + `next_au`/`next_audio`/`next_rumble`/`next_hidout`/`send_input`/
  `send_rich_input`. **Client-negotiated virtual pad type**: the Hello carries a gamepad
  preference byte (same trailing-byte back-compat pattern as the compositor), the Welcome
  echoes the resolved backend — precedence: explicit client choice > `PUNKTFUNK_GAMEPAD`
  env > uinput Xbox 360; DualSense (UHID) only on Linux hosts.

## What's left

1. **Native clients — decode + present: macOS stage 1 done, first light achieved
   (2026-06-10).** PunktfunkKit compiles and is tested on macOS (AnnexB → VideoToolbox →
   `AVSampleBufferDisplayLayer`, GCMouse/GCKeyboard capture, `PunktfunkClient` app shell);
   validated live Mac ↔ this box at 720p60 — vkcube on glass, input injected via gamescope
   EIS. The app speaks the full ABI v2 trust surface: Keychain-persisted client identity
   presented on every connect, SPAKE2 PIN pairing UI (host-card context menu + the trust
   prompt's "Pair with PIN instead…"), TOFU fingerprint prompt. **Gamepads (2026-06-11):**
   controller discovery + selection in Settings (`GamepadManager` — exactly one pad
   forwarded as pad 0, auto or pinned; pad TYPE auto-resolves from the physical
   controller, user-overridable), capture incl. DualSense touchpad/motion
   (`GamepadCapture`/`GamepadWire`), feedback rendering (rumble → CoreHaptics; lightbar /
   player LEDs / adaptive triggers → `GCDeviceLight`/`playerIndex`/
   `GCDualSenseAdaptiveTrigger` via the table-driven `DualSenseTriggerEffect` parser).
   Loopback-tested end to end (`PUNKTFUNK_TEST_FEEDBACK=1` scripted burst); DualSense
   motion sign/scale derived, not yet live-verified. Tests: `swift test` in
   `clients/apple` (unit + real-codec round trip),
   `test-loopback.sh` (Swift client vs synthetic punktfunk1-hosts on loopback — runs on macOS;
   includes the pairing ceremony + `--require-pairing` gate),
   `RemoteFirstLightTests` (full pipeline over the LAN). See
   [`clients/apple/README.md`](clients/apple/README.md). Next: stage 2 presenter
   (`VTDecompressionSession` + `CAMetalLayer` frame pacing), glass-to-glass numbers via
   `tools/latency-probe` (scaffold), iOS variant.
   **Linux stage 1 done, first light 2026-06-12** (`clients/linux`, binary
   `punktfunk-client`): GTK4/libadwaita shell linking `punktfunk-core` directly (no C ABI;
   `NativeClient` is now `Sync` — mutexed plane receivers), mDNS host list, TOFU + SPAKE2
   PIN dialogs (identity shared with client-rs), FFmpeg software HEVC decode (LOW_DELAY,
   slice threads) → `GtkGraphicsOffload`-wrapped picture, PipeWire playback (mic-player
   jitter ring inverted), SDL3 gamepad capture + rumble/lightbar feedback, keyboard via
   exact inverse of the host VK table, absolute mouse + 120-unit scroll. Validated live
   against `serve --native` on this box: 1080p60, steady 60 fps, capture→decoded p50
   ≈6.4 ms (debug build). `--connect host[:port]` for scripting. **Swift-parity batch +
   stage 1.5 (2026-06-12 evening)**: capture state machine (click-to-capture,
   Ctrl+Alt+Shift+Q / focus-loss release, held-state flush), app-lifetime SDL gamepad
   service (pad pin UI, auto type from the physical pad, DualSense touchpad/motion 0xCC +
   raw-DS5-effects trigger/player-LED replay — needs a physical pad to live-verify), mic
   uplink (validated live), per-host speed test, compositor pref, native-display mode
   default, saved-hosts list, .deb + RPM-subpackage CI (deb.yml/rpm.yml). **VAAPI decode
   → DRM-PRIME dmabuf → `GdkDmabufTexture`** (BT.709 color state; Tier-1 zero-copy on
   Intel/AMD, `PUNKTFUNK_DECODER=software|vaapi` override) with a proven fallback ladder —
   no VAAPI device (NVIDIA) or mid-session VAAPI error → software decode. **First AMD test
   (Steam Deck) hit a green-screen bug, fixed:** FFmpeg's VAAPI export uses
   `SEPARATE_LAYERS`, so NV12 arrives as two single-plane layers (R8 luma + GR88 chroma,
   one shared fd); the mapper took `layers[0]` only → GTK got a luma-only R8 texture, chroma
   read as 0 → green field / red whites. Fix derives the combined fourcc from the decoder
   `sw_format` (→ `DRM_FORMAT_NV12`) and flattens all planes across all layers (mpv's
   pattern); a first-frame descriptor dump logs the real layout. Awaiting Steam Deck
   reconfirm. Next: the stage-2 raw-Wayland
   presenter (wp_presentation feedback, tearing-control, Vulkan Video on NVIDIA) —
   **wgpu/winit rejected** (no dmabuf import / presentation feedback / shortcuts-inhibit).
   **Windows stage 1 done 2026-06-15** (`clients/windows`, binary
   `punktfunk-client`): pure-Rust **WinUI 3** UI via **windows-reactor** (a declarative React-like
   framework backed by WinUI; PR #4499 added the `SwapChainPanel` widget + `set_swap_chain`). The
   video is a **`SwapChainPanel`** bound to a **D3D11 composition swapchain** (WARP fallback for
   the GPU-less dev box; runtime-compiled fullscreen-triangle shaders, Contain-fit letterbox),
   driven by reactor's per-frame `on_rendering`. **FFmpeg HEVC decode with a D3D11VA
   zero-copy hardware path** (`gpu.rs` shares one D3D11 device — hardware+`VIDEO_SUPPORT`, WARP
   fallback, multithread-protected — between the decoder and presenter; the decoder outputs
   NV12/P010 `ID3D11Texture2D` array slices with `BIND_SHADER_RESOURCE` and the presenter samples
   them via per-plane SRVs + YUV→RGB shaders — NV12/BT.709, P010/BT.2020-PQ; **software CPU decode
   stays as the robust fallback**, auto-selected with a `DecoderPref` override). **HDR10**: the
   client advertises 10-bit/HDR (Settings toggle), detects PQ in-band (`transfer == SMPTE2084`),
   and flips the swapchain to `R10G10B10A2` + ST.2084 with HDR10 metadata. **WASAPI** render + mic
   capture, **SDL3** gamepads (rumble/lightbar/DualSense), `mdns-sd` discovery, and the full trust
   surface — all **in-app**: a polished WinUI shell (host cards w/ monogram + status pills,
   `InfoBar` errors/hints, `ToggleSwitch` settings, status-chip stream HUD showing GPU/CPU decode +
   HDR), host list (live mDNS + saved + manual), settings (resolution/refresh/decoder/bitrate/HDR/
   mic), SPAKE2 PIN pairing screen, TOFU, pinned-fp-mismatch re-pair. **(D3D11VA + HDR present + the
   GUI polish are written against the windows-rs/reactor APIs but not yet on-glass validated — the
   dev VM is headless/WARP; needs the RTX box.)** **Stream input** is Win32 low-level hooks (`WH_KEYBOARD_LL`/`WH_MOUSE_LL`) — reactor
   exposes no raw key/pointer events; native Windows VK + absolute mouse (client-rect Contain-fit) +
   wheel, Ctrl+Alt+Shift+Q capture toggle. `--headless`/`--discover` keep CLI paths. Builds + clippy
   + fmt green on `x86_64-pc-windows-msvc` (on the dev VM). **windows-reactor is unpublished** (git
   dep pinned to commit `b4129fcc`; `windows` pinned to the SAME commit so `IDXGISwapChain1` unifies
   with `set_swap_chain`); its `build.rs` downloads the Win App SDK NuGets + needs `CARGO_WORKSPACE_DIR`
   set (in the VM build env; `/temp`+`/winmd` gitignored). Gotcha: `CARGO_HOME` must be an ASCII path
   — the `ü` in the dev box's username breaks SDL3's MSVC precompiled-header build. Next: **on-glass
   validation** of the D3D11VA decode + HDR present + GUI on the RTX box (the dev VM is
   headless/Session-0/WARP → the WinUI window + hardware decode need a real display+GPU: RDP or the
   RTX box), then RAWINPUT relative-mouse pointer-lock and a per-host speed test in the UI.
2. **Sub-frame pipelining**: overlap encode and transmit within a frame. Requires a direct
   NVENC SDK wrapper (libavcodec only emits whole AUs) — the next big latency lever (~2–4 ms
   at high res).
3. **punktfunk/1 protocol growth**: concurrent sessions (today: one at a time, extras wait
   in the accept queue). **Done:** unified host (`serve --native` runs GameStream + the
   punktfunk/1 QUIC host in one process) with native pairing driven over the mgmt API /
   web console (`mod native_pairing`: arm-on-demand → display PIN, paired-device list).
   **Done:** PIN pairing is the default, host-gated — the host requires pairing and advertises
   `pair=required` unless opted out with `--allow-tofu`/`--open` (then `pair=optional`, accepts
   unpaired clients); clients render TOFU only for a `pair=optional` host and force re-pairing on a
   fingerprint change. Next (see roadmap): **delegated pairing approval** (an already-paired device
   approves a new one).
4. **GameStream host polish**: HDR/10-bit (needs HDR capture + metadata plumbing; `av1_nvenc
   -highbitdepth 1` already encodes Main10 from 8-bit input on this box),
   reconnect-at-new-mode robustness. AV1 negotiation and surround audio are implemented
   and unit/live-capture tested — both still need a live Moonlight confirmation (select
   AV1 in a stock client; a real 5.1/7.1 listen incl. FEC under loss).
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

CI is Gitea Actions (`.gitea/workflows/`, guide: docs-site `ci.md`): `ci.yml` runs the
workspace checks inside the `git.unom.io/unom/punktfunk-rust-ci` image plus web/docs-site
build+typecheck; `docker.yml` builds+pushes the web/docs/rust-ci images (host and native
clients are deliberately NOT containerized); `apple.yml` builds the xcframework and runs
`swift build`/`swift test` on the `macos-arm64` host-mode runner (home-mac-mini-1,
provisioned by `scripts/ci/setup-macos-runner.sh`).

## Layout

```
crates/punktfunk-core/        protocol · FEC · crypto · quic (punktfunk/1 control plane, feature-gated)
crates/punktfunk-host/
  gamestream/             Moonlight compat: nvhttp · pairing · rtsp · control · stream · gamepad · apps
  vdisplay/{kwin,gamescope,mutter,wlroots}.rs   per-compositor client-sized virtual outputs
  zerocopy/{egl,cuda,vulkan}.rs         dmabuf → CUDA → NVENC (tiled via EGL/GL, LINEAR via Vulkan)
  inject/{libei,wlr,gamepad,dualsense}.rs   input backends (uinput xpad + UHID DualSense)
  capture.rs · encode.rs · audio.rs · spike.rs · punktfunk1.rs · mgmt.rs · native_pairing.rs
clients/probe/    punktfunk/1 reference/probe client (headless test/measurement tool)
clients/linux/    native Linux client (GTK4/libadwaita · FFmpeg · PipeWire · SDL3)
clients/windows/  native Windows client (WinUI 3 via windows-reactor · D3D11 · WASAPI · SDL3)
clients/apple/    native macOS/iOS client (Swift · VideoToolbox · GameController)
clients/android/  native Android client (Kotlin app + native/ Rust JNI core over punktfunk-core)
clients/decky/    Steam Deck Decky plugin
web/                          TanStack web console over the mgmt API (status · devices · pairing)
packaging/                    Fedora/Bazzite RPM · bootc · COPR (packaging/bazzite/README.md)
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
- **Core security hardening stays intact**: reassembler bounds attacker-controlled fields
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
cargo run -rp punktfunk-host -- punktfunk1-host --source virtual --seconds 10 --max-sessions 1
cargo run -rp punktfunk-probe -- --mode 1280x720x120 --out /tmp/a.h265 --input-test  # + --pin HEX
```

Pinned crate facts: `ashpd` 0.13 + `pipewire` 0.9 (must match ashpd's) + `ffmpeg-next` 8.x
(`ffmpeg-sys-next` auto-detects the system FFmpeg, so it builds against **FFmpeg 7.x/libavcodec 61
or 8.x/libavcodec 62** — validated live on Ubuntu 26.04 (8) and Bazzite F43 (7.1); the zero-copy
FFI also link-needs `libGL`/`libgbm`/`libcuda` at build time). Env knobs: `PUNKTFUNK_VIDEO_SOURCE=virtual|portal`,
`PUNKTFUNK_COMPOSITOR=kwin|gamescope|mutter`, `PUNKTFUNK_ZEROCOPY=1`, `PUNKTFUNK_GAMESCOPE_APP=...`,
`PUNKTFUNK_INPUT_BACKEND=...`, `PUNKTFUNK_PERF=1` (per-stage timing), `PUNKTFUNK_VIDEO_DROP=N` (FEC
test), `PUNKTFUNK_FEC_PCT=N`.

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
- `pkill` caution on this box: match exact comm names (`pkill -x gamescope-wl`,
  `pkill -x punktfunk-host`) — `pkill -f` self-matches the invoking shell.
