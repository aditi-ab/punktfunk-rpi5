---
title: Clients
description: The ways to connect to a punktfunk host — the Apple app, Moonlight, or the Linux client.
---

A punktfunk host accepts clients over its own `punktfunk/1` protocol (the macOS, Linux, Windows, and
Android apps) and over GameStream (Moonlight). Pick whichever fits the device you're streaming *to*.
Ready to install?
**[Install a Client](/docs/install-client)** has the step-by-step for every device.

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
client — a browser, a smart TV, an old phone, a games console — connects with no punktfunk-specific
software. (Most platforms also have a native punktfunk app below — Moonlight is the catch-all.) See
[Connect with Moonlight](/docs/moonlight).

This is the broadest-compatibility option and great for couch gaming. It doesn't use the native
protocol's FEC/encryption extensions, but for a healthy LAN that rarely matters.

## Linux desktop client (GTK4)

`punktfunk-client` is the native graphical Linux client — a GTK4 / libadwaita app that speaks
`punktfunk/1` directly, with hardware decode via Vulkan Video on every GPU vendor (including
NVIDIA), falling back to VAAPI dmabuf and then software, PipeWire audio, and SDL3 controllers
(rumble, lightbar, DualSense touchpad/motion). Like the Apple app it discovers hosts on your network
automatically, does PIN pairing, and pins reconnects.

It ships as a real package, not just a source build — full steps in
[Install a Client](/docs/install-client#linux-desktop-flatpak):

- **Any Flatpak distro (recommended)** — `flatpak install https://flatpak.unom.io/io.unom.Punktfunk.flatpakref`
  from the hosted [`flatpak.unom.io`](/docs/install-client#linux-desktop-flatpak) repo, then
  `flatpak update`; this is also what the [Decky plugin](/docs/steam-deck) launches.
- **Ubuntu / Debian** — `apt install punktfunk-client` from the punktfunk apt registry.
- **Fedora / Bazzite** — `rpm-ostree install punktfunk-client` from the Gitea RPM registry.
- **Arch** — `sudo pacman -Sy punktfunk-client` from the signed binary repo (see [Arch Linux](/docs/arch)).

Launch it, pick your host from the list, and stream. For scripting you can skip the host list and
connect straight away:

```sh
punktfunk-client --connect <host>:9777   # skip the picker, start a session immediately
```

## Android app (phone + Android TV)

The native Android app speaks `punktfunk/1` directly, on both phones and Android TV. It does hardware
HEVC decode (including HDR10), Opus audio with a mic uplink, game controllers with rumble and
DualSense feedback, automatic host discovery, PIN pairing with pinned reconnects, and a live stats
overlay — with D-pad and game-controller focus navigation for the couch. It builds from the
`clients/android` directory (Kotlin + a shared Rust core).

The app is in **Google Play Internal Testing** — request a tester invite on our
[**Discord**](https://discord.gg/kaPNvzMuGU) and we'll add you (see
[Install a Client](/docs/install-client#android)). Once added, open the app, pick your host,
[pair](/docs/pairing) once, and stream.

## Windows desktop client

`punktfunk-client` for Windows (`clients/windows`) is the native graphical client for Windows — pure
Rust, the same `punktfunk/1` core as the Apple, Linux, and Android apps, with a **WinUI 3** UI (host
list, settings, PIN pairing); the stream itself runs in punktfunk's Vulkan presenter. Its decoder
tries **Vulkan Video, then D3D11VA, then software**, with 10-bit/HDR present, WASAPI audio + mic,
SDL3 controllers (rumble, lightbar, DualSense), network discovery, and the full PIN-pairing trust
surface. It builds for both `x86_64` and `aarch64` and ships as a **signed MSIX**. Launch it and
pick a host from the list, just like the other native apps.

> Hardware decode is validated on NVIDIA and Intel GPUs; HDR10 is implemented with on-glass
> validation still pending. If anything misbehaves, **[Moonlight](/docs/moonlight)** is a proven
> alternative for Windows.

A headless CLI path exists for scripting/measurement:

```sh
punktfunk-client                                   # open the WinUI 3 window (host list / settings)
punktfunk-client --discover                        # list hosts on the network
punktfunk-client --headless --connect <host>:9777  # no window: connect, count frames, print stats
```

Prefer the broadest compatibility, or no install? **Moonlight** also streams to Windows (see below).

## Linux reference client (headless)

`punktfunk-probe` (in the repo) is a command-line client for the native protocol, used for
testing, development, and latency measurement — not an everyday client. It connects, streams to a
file, runs the speed test, and can discover hosts:

```sh
punktfunk-probe --discover                        # list hosts on the network
punktfunk-probe --connect <host>:9777 --pin <fp>  # connect to one
```

## Which should I use?

| You're streaming to… | Use |
|---|---|
| A Mac, iPhone, iPad, or Apple TV | The **Apple app** |
| A Linux desktop or laptop | **`punktfunk-client`** (GTK4) |
| A **Steam Deck** | The **[Decky plugin](/docs/steam-deck)** in Gaming Mode, or the GTK4 client in Desktop Mode |
| An Android phone or TV | The **Android app** |
| Windows | The native **`punktfunk-client`** (signed MSIX) or **Moonlight** |
| A browser, a smart TV, or any other device | **Moonlight** |
| Automated tests / latency measurement | **`punktfunk-probe`** (headless) |

Whichever you choose, the first connection needs a one-time [pairing](/docs/pairing).
