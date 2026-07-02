# CLAUDE.md — punktfunk

Low-latency desktop/game streaming stack, Linux-first, with a shared Rust protocol core
(`punktfunk-core`) exposed over a C ABI and native clients per platform. Full design:
[`design/implementation-plan.md`](design/implementation-plan.md). Status table: `README.md`.

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
  checked-in OpenAPI doc (`mgmt.rs`). **Web-console performance capture** (`stats_recorder.rs`,
  design: [`design/stats-capture-plan.md`](design/stats-capture-plan.md)): the operator arms stats
  recording from the web console, plays, stops, and reviews the run as graphs (per-stage latency
  breakdown · fps new/repeat · goodput · loss/FEC). A shared `Arc<StatsRecorder>` ring (the hot-path
  gate is a runtime `AtomicBool`, replacing the startup-only `PUNKTFUNK_PERF`) is fed by **both** the
  native `virtual_stream` and the GameStream encode loop at their existing ~2 s/~1 s aggregation
  boundary, and finished captures are saved as on-disk recordings
  (`~/.config/punktfunk/captures/*.json`) browsable/exportable from the console's **Performance** page
  (recharts). Endpoints `/api/v1/stats/*` (bearer-only). *Implemented; not yet on-glass validated.*
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
  **LAN auto-discovery**: both `serve` and `punktfunk1-host` advertise the native service over
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
  env > uinput Xbox 360. Backends: **Xbox 360** (uinput on Linux / the pf-xusb UMDF driver on
  Windows), **Xbox One/Series** (the same
  XInput backend with the One/Series USB identity for matching glyphs — no extra game-visible
  capability; impulse-trigger rumble is unreachable through a virtual pad), and the UHID
  `hid-playstation` pads — **DualSense** (adaptive triggers, lightbar, touchpad, motion) and
  **DualShock 4** (lightbar, touchpad, motion, rumble; DualSense minus adaptive triggers / player
  LEDs / mute). DualSense and DualShock 4 each have a Linux (UHID `hid-playstation`) **and a Windows
  (UMDF minidriver)** backend — `inject/windows/dualsense_windows.rs` + `inject/windows/dualshock4_windows.rs`, one
  driver serving either identity per a `device_type` byte the host stamps into shared memory (the DS4
  reuses the same SwDeviceCreate game-detection identity fix as the DualSense). One/Series stays
  Linux-only and folds into Xbox 360 off it. Clients auto-resolve the type from the physical controller
  (DS5→DualSense, DS4→DualShock 4, Xbox One→Xbox One). **Windows uses ZERO external gamepad
  dependencies — ViGEmBus is gone.** Xbox 360 (XInput) runs on a UMDF2 **XUSB companion** driver
  (`packaging/windows/drivers/pf-xusb/`, `inject/windows/gamepad_windows.rs`) that registers `GUID_DEVINTERFACE_XUSB`
  and answers the buffered XInput IOCTLs from a shared section, so classic `XInputGetState`/`SetState`
  work with no kernel bus driver (validated live: slot connected, state + rumble round-trip; Xbox One
  folds to this 360 path). All three UMDF drivers (DualSense/DS4 + XUSB) are built from source in CI
  (`packaging/windows/drivers/`) and installed by the Inno Setup installer via
  `punktfunk-host.exe driver install --gamepad`.
  **Multi-pad ready**: the host stamps each pad's index into the device Location (`pszDeviceLocation`),
  the driver reads it (`WdfDeviceAllocAndQueryProperty`) to map its own `*-shm-<index>`, and
  `UmdfHostProcessSharing=ProcessSharingDisabled` gives each pad its own host (per-pad statics) —
  validated live with 2 distinct XInput slots + 2 DualSense pads. (Client-side multi-pad forwarding is
  the remaining piece.)
- **Windows host: implemented and shipping (all-vendor, x64-only).** `#[cfg(windows)]` backends
  behind the same traits as Linux — **IDD-push capture** straight into the in-house all-Rust IddCx
  **pf-vdisplay** virtual display (`capture/windows/idd_push.rs`, `vdisplay/windows/pf_vdisplay.rs`;
  DXGI Desktop Duplication / WGC as fallbacks, `capture/windows/dxgi.rs`), GPU encode (NVENC
  `--features nvenc`; AMD/Intel `--features amf-qsv`), SendInput + the in-house UMDF gamepad drivers
  (`inject/windows/`), WASAPI loopback + virtual mic (`audio/windows/wasapi_*`). **Keyboard wire
  convention: US-positional VKs** (every first-party client sends the physical key's US-layout VK;
  the Windows client derives it from the scancode, NOT the layout-resolved `vkCode`) — the Windows
  injector resolves them via a fixed table mirroring the Linux `vk_to_evdev` (never through a
  keyboard layout: the SYSTEM service thread's layout re-reads positions as characters — the
  German y↔z / ö→ü scramble), while GameStream/Moonlight VKs are layout-semantic
  (`KEY_FLAG_SEMANTIC_VK`, resolved under the foreground app's layout, Sunshine's model). Linux
  renders positions under the session compositor's layout (libei) or the virtual keyboard's
  uploaded keymap (Sway/wlroots — honors `XKB_DEFAULT_LAYOUT` et al., default US); the Android
  client reads `KeyEvent.scanCode` first so a user-selected physical-keyboard layout can't
  re-map keycodes semantically. Ships as a **signed
  Inno Setup installer** that registers a `LocalSystem` SCM service launching into the interactive
  session for secure-desktop (UAC/lock-screen) capture (`windows/service.rs`), bundles the
  pf-vdisplay driver + the FFmpeg DLLs (+ VB-CABLE for the virtual mic), and is published by
  `windows-host.yml`. **Encoder is GPU-aware** (`encode.rs` `open_video` + `windows_resolved_backend`):
  `PUNKTFUNK_ENCODER=auto` (the host.env default) reads the **selected render adapter's** vendor →
  **NVENC** (NVIDIA, direct SDK, `encode/windows/nvenc.rs`), **AMF** (AMD) / **QSV** (Intel) via libavcodec
  (`encode/windows/ffmpeg_win.rs`, the Windows analogue of the Linux VAAPI backend — `WinVendor{Amf,Qsv}`,
  system-memory NV12/P010 readback default + opt-in zero-copy D3D11 behind `PUNKTFUNK_ZEROCOPY` with a
  system fallback), or software H.264 (`encode/sw.rs`, GPU-less). GameStream codec advertisement is
  probed per-GPU on AMF/QSV (`windows_codec_support` → `serverinfo`, AV1 gated; cached per selected
  GPU). **Multi-GPU is first-class** (`gpu.rs`): GPU inventory + a persisted auto/manual preference
  (`<config>/gpu-settings.json`, stored by stable PCI identity — LUIDs are per-boot) exposed over
  `GET /api/v1/gpus` + `PUT /api/v1/gpus/preference` and a web-console GPU card (Host page: list,
  Automatic/Prefer, "In use · backend" badge). One selection — precedence **console preference >
  `PUNKTFUNK_RENDER_ADAPTER` > max VRAM**, graceful fallback when the preferred GPU is absent —
  feeds `win_adapter::resolve_render_adapter_luid` (capture ring + IddCx render pin), the encoder
  vendor auto-detect (previously DXGI adapter 0 — wrong on hybrid boxes like NVIDIA dGPU + Intel
  Arc iGPU), and the NVENC 4:4:4 probe; a preference change applies to the next session. On Linux a
  matched manual preference picks the VAAPI render node / NVENC-vs-VAAPI auto choice (auto mode
  unchanged). *Implemented + unit-tested; not yet on-glass validated on the hybrid box.* **HDR (10-bit)**: WGC
  captures the HDR desktop as FP16/Rgb10a2 (DDA FP16 for the secure desktop), the encoder forces HEVC
  Main10 + BT.2020 PQ (NVENC ABGR10/P010; AMF/QSV P010 + a swscale Rgb10a2→P010 fallback), the client
  auto-detects PQ from the HEVC VUI — gated by `PUNKTFUNK_10BIT` + client `VIDEO_CAP_10BIT`; **Windows
  host only** (the Linux host stays 8-bit, blocked upstream). **Vulkan-game HDR over the virtual
  display**: NVIDIA/AMD Vulkan ICDs refuse to *advertise* an HDR color space for a surface on an IddCx
  indirect display (so Vulkan games — Doom: The Dark Ages, id Tech, etc. — say "device does not support
  HDR"), even though the ICD happily *accepts + presents* a forced HDR swapchain there. A tiny always-on
  Vulkan **implicit layer** (`packaging/windows/pf-vkhdr-layer/`, `VK_LAYER_PUNKTFUNK_hdr_inject`)
  injects the `HDR10_ST2084`/scRGB surface formats into `vkGetPhysicalDeviceSurfaceFormats[2]KHR`,
  self-gated on the display's actual advanced-color state (no-op on SDR / real monitors); bundled +
  HKLM-registered by the installer. **Live-validated: Doom: The Dark Ages enables HDR over the virtual
  display.** **AMF/QSV is CI-green but not yet on-glass validated** (no AMD/Intel Windows box in the
  lab); NVENC is live-validated. Newer/less battle-tested than the Linux host. Packaging: `packaging/windows/`.

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
   motion sign/scale derived, not yet live-verified. **Gamepad UI (iOS/iPadOS + macOS,
   2026-07-02 rework):** a connected pad swaps the home for a console-style launcher
   (`Home/Gamepad*` + `Settings/GamepadSettingsView`) — host carousel with a trailing Add
   Host tile (A connect · Y library · X settings · B back), a controller-navigable
   settings screen (vertical `GamepadMenuList`, left/right steps values), an add-host
   flow with an on-screen controller keyboard (`GamepadKeyboard` — no touch needed
   anywhere), and the coverflow library, all over an animated aurora backdrop
   (`GamepadScreenBackground`, TimelineView-driven drifting blobs — pure SwiftUI ON
   PURPOSE: a .metal lib only reliably bundles in one of the two build systems (SPM vs
   xcodeproj synced folders) these sources compile under). Input is the polled
   `GamepadMenuInput` (handlers don't fire outside a stream; on (re)start it SNAPSHOTS
   held buttons so a handoff press never double-fires), haptics dual-channel (device +
   `MenuHaptics` on the pad). macOS: same screens, settings/add-host as sheets (no
   fullScreenCover), NSScreen-based mode lists, scroll indicators `.never` (macOS
   "always show scroll bars" overrides `.hidden`); launcher/settings/add-host/keyboard
   render-verified live on this Mac via `PUNKTFUNK_FORCE_GAMEPAD_UI=1` (dev hook, forces
   the mode without a pad). Controller-in-hand on-glass validation still pending on all
   platforms. Tests: `swift test` in
   `clients/apple` (unit + real-codec round trip),
   `test-loopback.sh` (Swift client vs synthetic punktfunk1-hosts on loopback — runs on macOS;
   includes the pairing ceremony + `--require-pairing` gate),
   `RemoteFirstLightTests` (full pipeline over the LAN). See
   [`clients/apple/README.md`](clients/apple/README.md). **Stage 2 presenter is now the DEFAULT**
   (stage-1 is the Metal-unavailable / DEBUG fallback): explicit `VTDecompressionSession` decode →
   `CAMetalLayer`, presented from the hosting view's **main-runloop `CADisplayLink`** (`renderTick` pops
   the newest ready frame per vsync; macOS `displaySyncEnabled = false` is the real fullscreen-judder fix,
   ~11 ms p50). *(An off-main `CAMetalDisplayLink` and an off-main blocking-render present thread were
   both tried and reverted — both measured slower on macOS and iPad.)* **HDR fixed**
   (`design/apple-stage2-presenter.md`): the "too bright" bug was a missing reference-white anchor — the
   fix keeps the PQ-passthrough shader and adds `CAEDRMetadata.hdr10(…, opticalOutputScale: 203)` +
   `wantsExtendedDynamicRangeContent` on the layer (on all platforms; the old `#if os(macOS)` guard left
   iOS/tvOS EDR half-engaged), routing the 0xCE mastering metadata to the layer (via `setHdrMeta`) instead
   of a never-composited source buffer. **Mid-session SDR↔HDR** is handled: `render` reconciles the layer
   per-frame from the decoded `frame.isHDR` (per-mode pixel format `bgra8`/`rgba16Float`), so a game
   entering HDR mid-stream just reconfigures (last 0xCE grade cached + re-applied; pump drains 0xCE
   unconditionally). **4:4:4 added**: decode format is a 2×2 `(chroma, HDR)` matrix
   (`420v/x420/444v/x444`, all biplanar so the shaders are unchanged), advertised (`VIDEO_CAP_444`) only
   behind a **hardware-required `VTDecompressionSession` probe** (`Stage444Probe`, validated on M3) with a
   Settings opt-out + a bounded pump backstop for an undecodable 4:4:4 session. *HDR brightness + 4:4:4
   still need on-glass validation (Windows-HDR / `PUNKTFUNK_444` host).* Next: glass-to-glass numbers via
   `tools/latency-probe`.
   **Linux stage 1 done, first light 2026-06-12** (`clients/linux`, binary
   `punktfunk-client`): GTK4/libadwaita shell linking `punktfunk-core` directly (no C ABI;
   `NativeClient` is now `Sync` — mutexed plane receivers), mDNS host list, TOFU + SPAKE2
   PIN dialogs (identity shared with client-rs), FFmpeg software HEVC decode (LOW_DELAY,
   slice threads) → `GtkGraphicsOffload`-wrapped picture, PipeWire playback (mic-player
   jitter ring inverted), SDL3 gamepad capture + rumble/lightbar feedback, keyboard via
   exact inverse of the host VK table, absolute mouse + 120-unit scroll. Validated live
   against `serve` on this box: 1080p60, steady 60 fps, capture→decoded p50
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
   video is a **`SwapChainPanel`** bound to a **D3D11 composition swapchain**, presented from a
   **dedicated render thread** (`render.rs`, 2026-07-02 rewrite — presenting never touches or is
   stalled by the XAML thread): frame-latency-waitable swapchain + `SetMaximumFrameLatency(1)`
   (≤1 queued present, newest-wins drain after the wait, so a stream faster than the display drops
   backlog before any GPU work), **HiDPI-correct** (pixel-sized buffers + `SetMatrixTransform`
   96/DPI — DIP-sized buffers were blurry at 125/150 %), Contain-fit letterbox, WARP fallback.
   **FFmpeg decode with a D3D11VA hardware path on all vendors** (`gpu.rs` shares one D3D11 device
   between decoder + presenter, adapter picked by console pref `PUNKTFUNK_ADAPTER` > the window's
   monitor's adapter > default; `PUNKTFUNK_D3D_DEBUG=1` adds the debug layer): the decode pool is
   **decoder-only bind, sized/aligned by libavcodec itself** (get_format returns `AV_PIX_FMT_D3D11`
   and lets `hw_device_ctx` drive — three hand-built-frames-context strikes are why: NVIDIA rejects
   `DECODER|SHADER_RESOURCE` arrays, `BindFlags=0` fails texture creation, and Intel rejects
   non-128-aligned HEVC surfaces at the first `SubmitDecoderBuffers`), a DXVA **profile probe**
   before the hwdevice commits software-vs-hardware up front (no burned first IDR), and the
   presenter copies the decoded slice with ONE display-size-boxed `CopySubresourceRegion` (a planar
   slice is a single subresource in D3D11 — the old two-copy D3D12-style code silently no-opped =
   the black screen) into its sampleable NV12/P010 texture → per-plane SRVs + YUV→RGB shaders
   (NV12/BT.709, P010/BT.2020-PQ). **Software CPU decode is the fallback** (auto-selected,
   `DecoderPref` override, mid-session demotion + keyframe re-request) and now feeds the SAME
   shaders (swscale → NV12/P010 planes → two dynamic plane textures) so hw/sw colour math is
   identical. **HDR10**: the client advertises 10-bit/HDR (Settings toggle, gated on an HDR
   display), detects PQ in-band (`transfer == SMPTE2084`), and flips the swapchain to
   `R10G10B10A2` + ST.2084 with HDR10 metadata (0xCE mastering metadata plumbed). **WASAPI** render
   + mic capture, **SDL3** gamepads (rumble/lightbar/DualSense), `mdns-sd` discovery, and the full
   trust surface — all **in-app**: a polished WinUI shell (host tiles w/ monogram + status pills,
   `InfoBar` errors/hints, `ToggleSwitch` settings, status-chip stream HUD showing GPU/CPU decode +
   HDR), host list (live mDNS + saved + manual), settings (resolution/refresh/decoder/bitrate/HDR/
   mic), SPAKE2 PIN pairing screen, TOFU, pinned-fp-mismatch re-pair. **Live-validated 2026-07-02
   on the hybrid laptop (Intel Arc Pro iGPU + RTX 3500 Ada) against the local Windows host**:
   D3D11VA hardware decode 60 fps on BOTH vendors (headless, `PUNKTFUNK_ADAPTER`-forced; NVIDIA
   0.2 ms decode, Intel 0.2 ms), software path, and the GUI on glass (real decoded desktop pixels,
   GPU-decode HUD chip, ~18 ms capture→decoded p50 over loopback — dominated by the host's 60 Hz
   virtual-display capture cadence). HDR-on-glass still pending. **Stream input** is Win32 low-level hooks (`WH_KEYBOARD_LL`/`WH_MOUSE_LL`) — reactor
   exposes no raw key/pointer events; native Windows VK + absolute mouse (client-rect Contain-fit) +
   wheel, Ctrl+Alt+Shift+Q capture toggle. `--headless`/`--discover` keep CLI paths. Builds + clippy
   + fmt green on **`x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`** — the latter
   **cross-compiled off the one x64 runner** (no ARM64 runner; the x64 MSVC toolset's ARM64 cross
   compiler + a per-arch `FFMPEG_DIR` ARM64 tree, SDL3/libopus build-from-source cross-compile
   cleanly), and both ship as signed MSIX (`windows-msix.yml` matrix → `..._x64.msix`/`..._arm64.msix`,
   verified: ARM64 binaries + manifest arch). **windows-reactor is unpublished** (git
   dep pinned to commit `a4f7b2cb`, bumped 2026-07-02 from `b4129fcc` for `on_pointer_entered`/
   `on_pointer_exited` hover events — mechanical renames only: `SymbolGlyph`→`Symbol`,
   `placeholder`→`placeholder_text`, TextBox `on_changed`→`on_text_changed`, ToggleSwitch
   `on_changed`→`on_toggled`, `on_menu_item_clicked`→`on_item_clicked`, SwapChainPanel
   `on_ready`→`on_mounted`; `windows` pinned to the SAME commit so `IDXGISwapChain1` unifies with
   `set_swap_chain`). New-model runtime staging: reactor has NO build.rs anymore — the app's own
   `build.rs` calls `windows_reactor_setup::as_framework_dependent()` (same-rev build-dep, stages
   the bootstrap DLL + resources.pri that pack-msix expects) and `main` calls
   `windows_reactor::bootstrap()` before `App` (packaged MSIX: a no-op, the manifest's
   `Microsoft.WindowsAppRuntime.2` dependency resolves the runtime). `CARGO_WORKSPACE_DIR` is no
   longer required (harmless where still set). Gotcha: `CARGO_HOME` must be an ASCII path
   — the `ü` in the dev box's username breaks SDL3's MSVC precompiled-header build. **Parity/cleanup
   batch (2026-07-02)**: `app.rs` split into per-screen `app/` modules (mod=root/router · hosts ·
   connect · pair · speed · settings · licenses · stream · style; thread-driven state lives in ROOT
   `use_async_state` and flows down as props — a child's own async-state write does NOT re-render it);
   "Native display" now resolves the real monitor mode at connect (`MonitorFromWindow` →
   `EnumDisplaySettingsW`, was hardcoded 1080p60); per-host **speed test** (saved-host card button +
   `--headless --speed-test`, probe burst → recommended ≈70 % bitrate applied in one tap; bitrate
   setting is now a free-form NumberBox); **forget host** (ContentDialog confirm →
   `KnownHosts::remove_by_fp`); settings gained forwarded-controller picker + gamepad type + host
   compositor + capture-system-shortcuts — the previously-dead `Settings.compositor`/
   `inhibit_shortcuts` are now honored (off = Alt+Tab/Alt+Esc/Ctrl+Esc/Win act locally);
   **click-to-recapture** after a Ctrl+Alt+Shift+Q release with the HUD hint tracking capture state;
   input hook caches lock geometry (no per-move `GetClientRect`), audio jitter-ring trims via
   `drain`. Validated on the bare-metal RTX box: `--discover` (3 live LAN hosts), synthetic-host
   loopback E2E (TOFU connect → clock skew → HEVC negotiate → shared-D3D11 + D3D11VA init → WASAPI →
   session end; synthetic payload isn't decodable so decode output stays unvalidated), speed-test
   E2E. The WinUI window itself CANNOT be launched from SSH (session-0 → WinAppSDK 0x80070005,
   pre-existing; needs the console session, e.g. PsExec -i 1). **UX batch (2026-07-02 evening,
   UIA-smoke-tested on the hybrid laptop)**: host tiles get the WinUI pointer-over fill
   (`on_pointer_entered`/`exited` → root hover state → `ControlFillSecondary`), Settings is a stock
   **NavigationView** sidebar (Windows-Settings pattern: Display/Video/Input/Audio/About panes,
   built-in back arrow, section in root state; the section card is **keyed by section** — an
   in-place diff across sections re-sets a reused ComboBox's items, clearing WinUI's selection,
   but skips `selected_index` when the values compare equal → blank selection; the key forces a
   remount — and the content column rides its own section-switch slide-up tween), new
   **"Show the stats overlay (HUD)"** toggle
   (`Settings::show_hud`, applies mid-stream via the 400 ms HUD re-render), the Add-host modal
   slides up + fades in (root margin/opacity tween, same pattern as screen navigation), and a
   self-initiated disconnect (Ctrl+Alt+Shift+D → `Ended(None)`) returns to the host list silently
   instead of raising the error banner.
   Next: **HDR on-glass validation** (Windows host with `PUNKTFUNK_10BIT` → the HDR laptop
   display), then RAWINPUT relative-mouse pointer-lock.
   **Android stage 1 done** (`clients/android`, Kotlin app + `native/` Rust JNI core linking
   `punktfunk-core`; phone + Android TV): NDK `AMediaCodec` hardware HEVC decode → `SurfaceView` incl.
   **HDR10** (Main10/BT.2020 PQ) with low-latency tuning + a live stats HUD (`decode.rs`/`stats.rs`),
   Opus/AAudio audio + mic uplink (`audio.rs`/`mic.rs`), gamepad input with rumble/HID feedback
   (`feedback.rs`), **native `mdns-sd` mDNS discovery** (`discovery.rs`, polled over JNI — the same
   browse the Linux/Windows clients use, replacing the flaky per-OEM `NsdManager`; Kotlin keeps only
   the `MulticastLock` + permission UX), SPAKE2 PIN pairing + TOFU (Keystore identity +
   known-host store), Compose UI (Connect/Settings/Stream) with D-pad/controller focus nav. Built for
   `arm64-v8a` + `x86_64`; published to Google Play (Internal Testing) via `android.yml`
   (`ci/play-upload.py`). Next: real-device gamepad/HDR live-verify, presenter/latency polish.
2. **Sub-frame pipelining**: overlap encode and transmit within a frame. Requires a direct
   NVENC SDK wrapper (libavcodec only emits whole AUs) — the next big latency lever (~2–4 ms
   at high res).
3. **punktfunk/1 protocol growth.** **Done:** unified host (`serve --gamestream` runs GameStream + the
   punktfunk/1 QUIC host in one process; bare `serve` is the secure native-only default — GameStream is
   opt-in, trusted-LAN only, security-review #5/#9) with native pairing driven over the mgmt API /
   web console (`mod native_pairing`: arm-on-demand → display PIN, paired-device list).
   **Done:** PIN pairing is the default, host-gated — the host requires pairing and advertises
   `pair=required` unless opted out with `--allow-tofu`/`--open` (then `pair=optional`, accepts
   unpaired clients); clients render TOFU only for a `pair=optional` host and force re-pairing on a
   fingerprint change. **Done:** concurrent sessions — the accept loop spawns each session
   (`--max-concurrent`, default 4, an NVENC bound), each with its own virtual output + encoder, sharing
   the host-lifetime input/audio/mic services (shared-desktop multi-view on kwin/mutter/wlroots).
   **Done:** delegated pairing approval (§8b-1) — an unpaired device shows up as a pending request in
   the web console, one click approves + pins it. Next (see roadmap): gamescope multi-user isolation
   (per-session input/audio = independent desktops); §8b-2 peer-push approval from a paired device's
   own app.
4. **GameStream host polish**: HDR/10-bit (needs HDR capture + metadata plumbing; `av1_nvenc
   -highbitdepth 1` already encodes Main10 from 8-bit input on this box),
   reconnect-at-new-mode robustness. AV1 negotiation and surround audio are implemented
   and unit/live-capture tested — both still need a live Moonlight confirmation (select
   AV1 in a stock client; a real 5.1/7.1 listen incl. FEC under loss).

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
(cbindgen from `punktfunk-core/src/abi.rs`) and `api/openapi.json` (regenerate with
`cargo run -p punktfunk-host -- openapi > api/openapi.json`; spec lives in `mgmt.rs`).

CI is Gitea Actions (`.gitea/workflows/`, guide: docs-site `ci.md`): `ci.yml` runs the
workspace checks inside the `git.unom.io/unom/punktfunk-rust-ci` image plus web/docs-site
build+typecheck; `docker.yml` builds+pushes the web/docs/rust-ci images (host and native
clients are deliberately NOT containerized); `apple.yml` builds the xcframework and runs
`swift build`/`swift test` on the `macos-arm64` host-mode runner (home-mac-mini-1,
provisioned by `scripts/ci/setup-macos-runner.sh`). Per-client/host release workflows:
`deb.yml`/`rpm.yml`/`flatpak.yml` (Linux client), `android.yml` (Google Play), `windows-msix.yml`
(Windows client), `windows-host.yml` (Windows host installer), `release.yml` (Apple notarized DMG +
TestFlight), `decky.yml` (Steam Deck plugin); Windows builds run on a self-hosted Windows runner
(`home-windows-runner-1`, vmid 210, `windows-amd64:host` label). The runner is reproducible and
**owned by `unom/infra`**, not this repo, since it's shared across unom Windows projects going
forward: `unom/infra`'s `windows-runner/` Packer template bakes a generic Windows 11 template (OS
install + OpenSSH Server + VS Build Tools/NASM/CMake/LLVM + the act_runner/Node/rustup base, no
registration) on Proxmox once; `proxmox/windows-runner/` (Terraform, `bpg/proxmox`) full-clones it
(agent-based IP discovery, no pre-provisioned DHCP reservation needed) and registers the instance
over SSH remote-exec — the same bake-once/clone-fast split `proxmox/unom-1` uses for the Linux CI
host, just without a native Windows cloud-init (registration goes over `remote-exec`/SSH instead of
`initialization{}`; WinRM was tried first but is deprecated in OpenTofu, so this moved to SSH via
Windows' in-box OpenSSH Server). punktfunk layers its own extras on top of that generic runner:
`scripts/ci/provision-windows-wdk.ps1` (WDK + cargo-wdk for the UMDF drivers) and
`scripts/ci/provision-windows-punktfunk-extras.ps1` (FFmpeg x64/ARM64 trees, Inno Setup, the
`aarch64-pc-windows-msvc` rustup target) — both idempotent, and both run automatically at the start
of every Windows CI job via the shared `scripts/ci/ensure-windows-toolchain.ps1` step (a fast no-op
once already provisioned), rather than a separate manually-dispatched provisioning workflow — that
avoided a real footgun once there could be more than one `windows-amd64` runner: a manually
dispatched provisioning workflow has no way to target a *specific* runner instance, so it could
land on an already-provisioned box instead of the one that actually needed it.

## Layout

```
crates/punktfunk-core/        protocol · FEC · crypto · quic (punktfunk/1 control plane, feature-gated)
crates/punktfunk-host/
  gamestream/             Moonlight compat: nvhttp · pairing · rtsp · control · stream · gamepad · apps
  vdisplay/linux/{kwin,gamescope,mutter,wlroots}.rs   per-compositor client-sized virtual outputs
  vdisplay/windows/{pf_vdisplay,manager,identity}.rs  all-Rust IddCx virtual display (pf-vdisplay)
  linux/zerocopy/{egl,cuda,vulkan}.rs   dmabuf → CUDA → NVENC (tiled via EGL/GL, LINEAR via Vulkan)
  inject/linux/{libei,wlr,gamepad,dualsense,dualshock4,steam_*}.rs   Linux input (uinput xpad · UHID pads · virtual Deck)
  inject/windows/{sendinput,gamepad_windows,dualsense_windows,dualshock4_windows}.rs   Windows input (UMDF shared-mem pads)
  encode/linux/{mod,vaapi}.rs · encode/windows/{nvenc,ffmpeg_win}.rs · encode/sw.rs   per-GPU encoders (NVENC/CUDA · VAAPI · AMF/QSV) + GPU-less openh264
  capture/{linux/,windows/{dxgi,idd_push}}.rs · audio/{linux/,windows/wasapi_*}.rs
  windows/{service,install,interactive}.rs   SCM service + in-binary driver/web install
  capture.rs · encode.rs · audio.rs · gpu.rs · spike.rs · punktfunk1.rs · mgmt.rs · native_pairing.rs · stats_recorder.rs · library.rs
clients/probe/    punktfunk/1 reference/probe client (headless test/measurement tool)
clients/linux/    native Linux client (GTK4/libadwaita · FFmpeg · PipeWire · SDL3)
clients/windows/  native Windows client (WinUI 3 via windows-reactor · D3D11 · WASAPI · SDL3)
clients/apple/    native macOS/iOS/tvOS client (Swift · VideoToolbox · GameController)
clients/android/  native Android client (Kotlin app + native/ Rust JNI core over punktfunk-core)
clients/decky/    Steam Deck Decky plugin
packaging/windows/drivers/{pf-vdisplay,pf-dualsense,pf-xusb}/   in-house UMDF drivers (built from source in CI)
web/                          TanStack web console over the mgmt API (status · devices · pairing · GPU selection · performance graphs)
packaging/                    apt(deb) · RPM/COPR · Arch/sysext · Flatpak · Bazzite bootc · Windows host installer (per-dir READMEs)
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

# host (shell 2): bare `serve` is native-only (secure default); add --gamestream for Moonlight compat.
WAYLAND_DISPLAY=wayland-kde XDG_CURRENT_DESKTOP=KDE PUNKTFUNK_VIDEO_SOURCE=virtual \
PUNKTFUNK_ZEROCOPY=1 cargo run -rp punktfunk-host -- serve --gamestream

# punktfunk/1 native loopback test (no Moonlight needed; same env as serve, listener persists
# across sessions — bound it with --max-sessions):
cargo run -rp punktfunk-host -- punktfunk1-host --source virtual --seconds 10 --max-sessions 1
cargo run -rp punktfunk-probe -- --mode 1280x720x120 --out /tmp/a.h265 --input-test  # + --pin HEX
```

Pinned crate facts: `ashpd` 0.13 + `pipewire` 0.9 (must match ashpd's) + `ffmpeg-next` 8.x
(`ffmpeg-sys-next` auto-detects the system FFmpeg, so it builds against **FFmpeg 7.x/libavcodec 61
or 8.x/libavcodec 62** — validated live on Ubuntu 26.04 (8) and Bazzite F43 (7.1); the zero-copy
FFI also link-needs `libGL`/`libgbm`/`libcuda` at build time). Env knobs: `PUNKTFUNK_VIDEO_SOURCE=virtual|portal`,
`PUNKTFUNK_COMPOSITOR=kwin|gamescope|mutter`, `PUNKTFUNK_ZEROCOPY=1|0` (Linux default: ON for
VAAPI/AMD/Intel with a one-shot CPU downgrade if the dmabuf offer never negotiates, OFF/opt-in for
NVENC), `PUNKTFUNK_VAAPI_LOW_POWER=1|0` (pin the VAAPI entrypoint; auto = full-feature then VDEnc
fallback for modern Intel), `PUNKTFUNK_NV12=0` (opt OUT of the default GPU RGB→NV12 convert on the
NVIDIA tiled zero-copy path), `PUNKTFUNK_INTRA_REFRESH=1` (opt-in NVENC intra-refresh loss recovery),
`PUNKTFUNK_PIN_CLOCKS=1` (opt-in NVML GPU clock floor, root-gated), `PUNKTFUNK_GAMESCOPE_APP=...`,
`PUNKTFUNK_INPUT_BACKEND=...`, `PUNKTFUNK_PERF=1` (per-stage timing), `PUNKTFUNK_VIDEO_DROP=N` (FEC
test), `PUNKTFUNK_FEC_PCT=N`, `PUNKTFUNK_DSCP=1` (opt-in DSCP/SO_PRIORITY media QoS on the data +
GameStream video/audio sockets; no-op on the wire on Windows without a qWAVE policy),
`PUNKTFUNK_444=1` (full-chroma HEVC 4:4:4, see below).

**HEVC 4:4:4 (full chroma, Range Extensions)**: opt-in via `PUNKTFUNK_444`, negotiated like 10-bit —
the host emits 4:4:4 only when the client advertised `VIDEO_CAP_444` (wire bit `0x04` + ABI
`PUNKTFUNK_VIDEO_CAP_444`), the codec is HEVC, the session is single-process, **and** a GPU probe
(`encode::can_encode_444`, run before the Welcome) confirms support — else it resolves to 4:2:0 and
`Welcome::chroma_format` reflects the real value (honest downgrade; the client reads it via
`punktfunk_connection_chroma_format`). **punktfunk/1-native only** — GameStream/Moonlight stays 4:2:0
(stock clients can't decode 4:4:4). **NVENC is the implemented path**: Linux `hevc_nvenc` feeds a
swscale'd `yuv444p` (RGB-in is always 4:2:0 — verified on the RTX 5070 Ti — so the session forces CPU
RGB capture for 4:4:4); Windows NVENC keeps ARGB input + FREXT profile + `chromaFormatIDC=3` and the
DDA capturer delivers RGB. VAAPI / AMF / QSV **decline** (probe returns false — no validated 4:4:4
hardware in the lab; they'd produce 4:2:0). Software (openh264) is 4:2:0-only. Test with
`PUNKTFUNK_CLIENT_444=1 punktfunk-probe --out x.h265` then `ffprobe x.h265` (expect `pix_fmt yuv444p`).
*Linux NVENC mechanism validated on the RTX 5070 Ti (ffmpeg CLI); Windows NVENC + 10-bit-4:4:4 not yet
on-glass validated.*

## Conventions

- Rust 2021, `rustfmt` + `clippy -D warnings` clean before commit.
- Match the surrounding code's comment density and naming.
- Commit messages end with the Co-Authored-By trailer (see `git log`).
- `pkill` caution on this box: match exact comm names (`pkill -x gamescope-wl`,
  `pkill -x punktfunk-host`) — `pkill -f` self-matches the invoking shell.
