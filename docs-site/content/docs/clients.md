---
title: Clients
description: The ways to connect to a punktfunk host — the Apple app, Moonlight, or the Linux client.
---

A punktfunk host accepts clients over its own `punktfunk/1` protocol (the Apple and Linux apps) and
over GameStream (Moonlight). Pick whichever fits the device you're streaming *to*.

## Apple app (Mac, iPhone, iPad, Apple TV)

The native app for Apple devices speaks punktfunk's own [`punktfunk/1`](/docs/how-it-works#two-protocols)
protocol — the lowest-latency, most resilient path, with the full feature set:

- **Automatic host discovery** — hosts on your network appear under *On this network*; no IP typing.
- **PIN pairing** built in, and pinned reconnects after that.
- **Controllers**, including DualSense — rumble, adaptive triggers, lightbar, motion, and touchpad.
- A live **stats overlay** (resolution, fps, bitrate, latency) and a built-in **network speed test**
  to pick a bitrate for your link.

Open the app, pick your host, [pair](/docs/pairing) once, and stream. It builds from the
`clients/apple` directory in the repo (Swift / VideoToolbox / Metal).

## Moonlight (anything else)

punktfunk also speaks the **GameStream** protocol, so any [Moonlight](https://moonlight-stream.org/)
client — Windows, Android, Steam Deck, a browser, an old phone — connects with no punktfunk-specific
software. See [Connect with Moonlight](/docs/moonlight).

This is the broadest-compatibility option and great for couch gaming. It doesn't use the native
protocol's FEC/encryption extensions, but for a healthy LAN that rarely matters.

## Linux desktop client (GTK4)

`punktfunk-client` is the native graphical Linux client — a GTK4 / libadwaita app that speaks
`punktfunk/1` directly, with hardware decode (VAAPI → dmabuf on Intel/AMD, software fallback),
PipeWire audio, and SDL3 controllers (rumble, lightbar, DualSense touchpad/motion). Like the Apple
app it discovers hosts on your network automatically, does PIN pairing, and pins reconnects.

It ships as a real package, not just a source build:

- **Ubuntu / Debian** — `apt install punktfunk-client` from the punktfunk apt registry
  (see `packaging/debian/README.md`).
- **Fedora / Bazzite** — `rpm-ostree install punktfunk-client` from the Gitea RPM registry
  (see `packaging/rpm/README.md`).
- **Arch / SteamOS** — the `punktfunk-client` split package from the `PKGBUILD`
  (see `packaging/arch/README.md`).
- **Steam Deck / any Flatpak distro** — the `io.unom.Punktfunk` Flatpak bundle
  (see `packaging/flatpak/README.md`); this is what the Decky plugin launches.

Launch it, pick your host from the list, and stream. For scripting you can skip the host list and
connect straight away:

```sh
punktfunk-client --connect <host>:9777   # skip the picker, start a session immediately
```

## Windows desktop client (in development)

`punktfunk-client` for Windows (`crates/punktfunk-client-windows`) is the native graphical client
for Windows — pure Rust, the same `punktfunk/1` core as the Apple and Linux apps, with a winit +
Direct3D11 present surface, WASAPI audio, FFmpeg decode, SDL3 controllers, network discovery, and
PIN pairing. It builds on `x86_64-pc-windows-msvc` and runs the connect/decode/present/input path;
hardware (D3D11VA) decode, 10-bit/HDR present, and a native host-list/settings window are in
progress, so it is not yet packaged. For now it is driven from the command line:

```sh
punktfunk-client --discover                       # list hosts on the network
punktfunk-client --connect <host>:9777            # stream (trust-on-first-use)
punktfunk-client --connect <host>:9777 --pair 1234 # pair with the PIN the host shows
```

Until it ships, **Moonlight** remains the recommended way to stream to Windows (see below).

## Linux reference client (headless)

`punktfunk-client-rs` (in the repo) is a command-line client for the native protocol, used for
testing, development, and latency measurement — not an everyday client. It connects, streams to a
file, runs the speed test, and can discover hosts:

```sh
punktfunk-client-rs --discover                        # list hosts on the network
punktfunk-client-rs --connect <host>:9777 --pin <fp>  # connect to one
```

## Which should I use?

| You're streaming to… | Use |
|---|---|
| A Mac, iPhone, iPad, or Apple TV | The **Apple app** |
| A Linux desktop or laptop, or a Steam Deck | **`punktfunk-client`** (GTK4) |
| Windows, Android, a browser, a TV | **Moonlight** |
| Automated tests / latency measurement | **`punktfunk-client-rs`** (headless) |

Whichever you choose, the first connection needs a one-time [pairing](/docs/pairing).
