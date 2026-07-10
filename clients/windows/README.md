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
the MSVC toolchain, an `FFMPEG_DIR` FFmpeg tree, and CMake (SDL3 builds from source). windows-reactor's
`build.rs` downloads the Windows App SDK NuGets and needs `CARGO_WORKSPACE_DIR` set.

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

```
src/
  main.rs                 entry point + CLI paths (--discover · --headless · --speed-test)
  app/                    WinUI 3 shell (windows-reactor), one module per screen:
                          mod (root/router) · hosts · connect · pair · speed · settings ·
                          licenses · stream · style (shared cards/pills/monograms)
  present.rs · gpu.rs      SwapChainPanel D3D11 composition swapchain; shared D3D11 device
  video.rs                FFmpeg HEVC decode (D3D11VA zero-copy + software fallback)
  audio.rs                WASAPI render + mic capture
  gamepad.rs              SDL3 controllers + rumble/lightbar/DualSense feedback
  input.rs                Win32 low-level hooks → host input (pointer lock · click-to-capture)
  session.rs              session lifecycle over the NativeClient connector (+ speed probe)
  trust.rs · discovery.rs persistent identity, TOFU/PIN pairing, mDNS browse
packaging/                MSIX manifest, signing, pack script
```

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
