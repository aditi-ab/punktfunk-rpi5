# punktfunk — Windows client

The native **Windows** app for streaming a punktfunk host to your PC. A modern WinUI 3 app that
discovers hosts on your network, pairs with a PIN, and streams at your display's own resolution and
refresh rate — with a hardware-accelerated D3D11 video path and HDR.

It's **pure Rust**: the UI is WinUI 3 driven through [windows-reactor](https://github.com/microsoft/windows-rs)
(a declarative, React-like framework), and it links the shared **`punktfunk-core`** directly to speak
the fast **`punktfunk/1`** protocol.

## Features

- **Hardware decode, GPU present** — FFmpeg HEVC with a **D3D11VA zero-copy path** (decoder and
  presenter share one D3D11 device; NV12/P010 textures sampled straight into a `SwapChainPanel`
  composition swapchain), with a robust software-decode fallback.
- **HDR10** — advertise 10-bit/HDR, detect PQ in-band, and flip the swapchain to `R10G10B10A2` +
  ST.2084 with HDR10 metadata.
- **Your display's native mode** — the host builds a virtual display at exactly your WxH@Hz.
- **Audio both ways** — WASAPI render + mic capture.
- **Full controller support** — SDL3 gamepads with rumble, lightbar, and DualSense feedback.
- **Your display's native mode, really** — "Native display" resolves the actual size + refresh of
  the monitor the window is on at connect time.
- **Find hosts automatically** — mDNS discovery lists hosts on your LAN, alongside saved and manual
  entries. First connect does a one-time **SPAKE2 PIN pairing** (or TOFU on trusted LANs), then
  reconnects on a pinned identity. Saved hosts carry per-host actions: a **network speed test**
  (probe burst over the real data plane → recommended bitrate, applied in one tap) and **forget**.
- **Polished shell** — host cards, settings (resolution / refresh / host compositor / decoder /
  codec / bitrate / HDR / forwarded controller / gamepad type / system shortcuts / audio channels /
  mic / stats-overlay level), the tiered stats overlay (Off / Compact / Normal / Detailed —
  Ctrl+Alt+Shift+S cycles it live in the session window), and the full trust surface. Stream input uses Win32 low-level
  hooks with Moonlight-style capture: Ctrl+Alt+Shift+Q releases the pointer, a click on the stream
  re-captures it, and system shortcuts (Alt+Tab, Win, …) can act locally or forward to the host.

Builds and ships for both **x64** and **ARM64** as a signed **MSIX**.

## Get it

Install the signed MSIX from the package registry — see
**[docs.punktfunk.unom.io/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.
A stock [Moonlight](https://moonlight-stream.org/) client also works over GameStream if you prefer.

## Build from source

Windows-only (the crate builds as a stub on other platforms so the workspace stays green). You need
the MSVC toolchain, an `FFMPEG_DIR` FFmpeg tree, and CMake (SDL3 builds from source). The Windows
App SDK runtime bootstrap is staged next to the exe by `windows-reactor-setup` from this crate's
own `build.rs` — no extra environment needed.

```sh
cargo build -p punktfunk-client-windows --target x86_64-pc-windows-msvc

# CLI paths for testing (no window):
punktfunk-client --discover                                   # list hosts on the LAN
punktfunk-client --headless --connect host[:port] [--pin HEX] # connect, count frames, print stats
punktfunk-client --headless --speed-test --connect host[:port]  # probe burst → recommended bitrate
```

> `CARGO_HOME` must be an ASCII path — non-ASCII characters break SDL3's MSVC precompiled-header
> build. Packaging (MSIX manifest, signing) lives in [`packaging/`](packaging/).

## Layout

Decode/present/input live in the spawned `punktfunk-session` binary (`clients/session`), not here —
this crate is the shell that discovers, pairs, and launches it.

```
src/
  main.rs                 entry point + CLI paths (--discover · --headless · --speed-test)
  bin/punktfunk-console.rs  the couch/HTPC Start-menu entry (re-execs with --console)
  app/                    WinUI 3 shell (windows-reactor), one module per screen:
                          mod (root/router) · hosts · connect · pair · speed · settings ·
                          library · help · licenses · stream · style (shared cards/pills)
  deeplink.rs             punktfunk:// activation, single-instance hand-off, shortcut writer
  spawn.rs                punktfunk-session child process + its stdout event contract
  shell_window.rs         hide/restore the shell HWND around a session
  gpu.rs                  DXGI adapter enumeration for the GPU picker
  trust.rs · discovery.rs persistent identity, TOFU/PIN pairing, mDNS browse
  probe.rs · wol.rs       speed probe · Wake-on-LAN
  logfile.rs              log tee to %LOCALAPPDATA%
packaging/                MSIX manifest, signing, pack script
```

## Manual smoke checklist

The windows-reactor pin is a moving target and WinUI regressions rarely show up in `cargo check` —
walk this after a reactor bump or a change to the render/state architecture:

- **Hosts** — discovery populates tiles; tile hover fill; "…" menu → Forget and Rename;
  add-host modal connects; WOL wait screen cancels.
- **Settings** — every section renders; combos still show their selection after a section
  switch AND a scope switch (the historic blank-combo reconciler bug); profile create /
  rename / delete (with confirm); colour swatches repaint; the Overridden marker appears on
  edit and clears on Reset; GPU combo lists adapters.
- **Pair** — PIN entry pairs (the typed PIN must reach the Connect click — `use_ref` mirror path).
- **Session** — connect → session spawns → HUD stats tick; Ctrl+Alt+Shift+Q releases the pointer;
  shell window restores on exit.
- **Shell** — speed test completes; library grid loads; `punktfunk://` deep link routes (second
  instance hands off and exits); window icon appears; screen-entrance animations play.

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
