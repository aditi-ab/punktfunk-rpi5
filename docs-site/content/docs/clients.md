---
title: Clients
description: The ways to connect to a Punktfunk host — the Apple app, Moonlight, or the Linux client.
---

A Punktfunk host accepts clients over its own `punktfunk/1` protocol (the macOS, Linux, Windows, and
Android apps) and over GameStream (Moonlight). Pick whichever fits the device you're streaming *to*.
Ready to install?
**[Install a Client](/docs/install-client)** has the step-by-step for every device — plus how to
[update](/docs/install-client#keeping-a-client-up-to-date) and
[remove](/docs/install-client#removing-a-client) each one.

Two things apply to every app, whichever you pick:
[profiles and `punktfunk://` links](#profiles-and-links-every-app), and the keys and chords that
work [while you're streaming](#while-youre-streaming). What each one lets you change — resolution,
bitrate, codec, HDR, audio, controllers — is catalogued in
[Client settings](/docs/client-settings).

## Apple app (Mac, iPhone, iPad, Apple TV)

The native app for Apple devices speaks Punktfunk's own [`punktfunk/1`](/docs/how-it-works#two-protocols)
protocol — the lowest-latency, most resilient path, with the full feature set:

- **Automatic host discovery** — hosts on your network appear under *On this network*; no IP typing.
- **PIN pairing** built in, and pinned reconnects after that.
- **Controllers**, including DualSense — rumble, adaptive triggers, lightbar, motion, and touchpad.
- A **[game library](/docs/game-library)** — browse the host's installed games with cover art and
  launch one straight into the stream.
- A live **stats overlay** (resolution, fps, bitrate, latency) and a built-in **network speed test**
  to pick a bitrate for your link.
- **Widgets, Live Activities and Shortcuts** — a hosts widget for the home screen, a Live Activity
  while a session runs, and App Intents so Siri and the Shortcuts app can start a stream.

Open the app, pick your host, [pair](/docs/pairing) once, and stream. It builds from the
`clients/apple` directory in the repo (Swift / VideoToolbox / Metal).

## Moonlight (anything else)

Punktfunk also speaks the **GameStream** protocol, so any [Moonlight](https://moonlight-stream.org/)
client — a browser, a smart TV, an old phone, a games console — connects with no punktfunk-specific
software. (Most platforms also have a native Punktfunk app below — Moonlight is the catch-all.) See
[Connect with Moonlight](/docs/moonlight).

This is the broadest-compatibility option and great for couch gaming. It doesn't use the native
protocol's FEC/encryption extensions, but for a healthy LAN that rarely matters.

## Linux desktop client (GTK4)

`punktfunk-client` is the native graphical Linux client — a GTK4 / libadwaita app that speaks
`punktfunk/1` directly, with vendor-ordered hardware decode (**Vulkan Video first on NVIDIA and
AMD**, **VAAPI dmabuf first on Intel**; whichever isn't first is the fallback, and software decode
is last), PipeWire audio, and SDL3 controllers (rumble, lightbar, DualSense touchpad/motion). The
decoders are Punktfunk's own — the client links no FFmpeg at all, and talks to your GPU's Vulkan
and VAAPI drivers directly. To force one, pick it in *Preferences → Display → Video decoder* or
set `PUNKTFUNK_DECODER=native-vulkan|native-vaapi|software`. Like the Apple app it discovers hosts
on your network automatically, does PIN pairing, pins reconnects, and browses the host's
**game library** (with cover art) so you can launch a title straight into the stream.

It ships as a real package, not just a source build — full steps in
[Install a Client](/docs/install-client#linux-desktop-flatpak):

- **Any Flatpak distro (recommended)** — one command from the hosted `flatpak.unom.io` repo; the
  guide linked above has the exact command and how updates work. It's also the client the
  [Decky plugin](/docs/steam-deck) uses by default, though the plugin drives a native
  `punktfunk-client` just as well.
- **Ubuntu 26.04 or newer** — `apt install punktfunk-client` from the Punktfunk apt registry. The
  client package needs SDL3 and GTK4 ≥ 4.20, which Ubuntu 24.04 LTS doesn't ship — on 24.04 use the
  Flatpak above.
- **Fedora** — `sudo dnf install punktfunk-client` from the Gitea RPM registry (add the repo as in
  the [Fedora guide](/docs/fedora)).
- **Fedora Atomic / Bazzite** — use the Flatpak above. `rpm-ostree install punktfunk-client` works,
  but layering slows every OS update, so it's a last resort on an image-based system (see
  [Bazzite](/docs/bazzite)).
- **Arch** — `sudo pacman -Syu punktfunk-client` from the signed binary repo (see [Arch Linux](/docs/arch)).

Launch it, pick your host from the list, and stream. For scripting, use the
[`punktfunk` CLI](#scripting-the-punktfunk-cli) that ships in the same packages:

```sh
punktfunk launch <host-ref>              # start a session, waking the host first if it's asleep
punktfunk-client --connect <host>:9777   # the older flag — still supported for existing scripts
```

The client also updates itself (`punktfunk-client --check-update` / `--apply-update`) — see
[Keeping a client up to date](/docs/install-client#keeping-a-client-up-to-date).

## Android app (phone + Android TV)

The native Android app speaks `punktfunk/1` directly, on both phones and Android TV. It does hardware
HEVC decode (including [HDR10](/docs/hdr#per-client)), Opus audio with a mic uplink, game
controllers with rumble and DualSense feedback, automatic host discovery, PIN pairing with pinned
reconnects, the host's **game library** with cover art, and a live stats overlay — with D-pad and
game-controller focus navigation for the couch. It builds from the `clients/android` directory
(Kotlin + a shared Rust core).

**Controllers.** Plug a **DualSense**, **DualSense Edge** or **DualShock 4** into the phone or tablet
by USB and grant the USB permission Android asks for when it attaches — Punktfunk then drives the pad
itself instead of taking what Android's gamepad layer exposes, so the host gets rumble, adaptive
triggers, the lightbar and gyro. The app's **Controllers** screen lists attached pads and their
capture state, and the switch that turns this off is *DualSense / DualShock passthrough (USB)* in
Settings. Over **Bluetooth** the pad still works as an ordinary gamepad, but adaptive triggers and
the lightbar need the USB connection.

The app is on **[Google Play](https://play.google.com/store/apps/details?id=io.unom.punktfunk)** as a
public listing — no invite — or you can sideload the public APK instead (see
[Install a Client](/docs/install-client#android)); canary builds ride a separate, invite-only Play
Internal testing track. Then open the app, pick your host, [pair](/docs/pairing) once, and stream.

## Windows desktop client

`punktfunk-client` for Windows (`clients/windows`) is the native graphical client for Windows — pure
Rust, the same `punktfunk/1` core as the Apple, Linux, and Android apps, with a **WinUI 3** UI (host
list, settings, PIN pairing); the stream itself runs in Punktfunk's Vulkan presenter. Its decoder
order is per-vendor: **Vulkan Video, then D3D11VA, then software** on NVIDIA and AMD, and
**D3D11VA first** on Intel and other GPUs (Intel's driver advertises Vulkan Video, but DXVA is the
proven path there), with [10-bit/HDR present](/docs/hdr#per-client), WASAPI audio + mic,
SDL3 controllers (rumble, lightbar, DualSense), network discovery, the host's **game library** with
cover art, and the full PIN-pairing trust surface. It builds for both `x86_64` and `aarch64` and
ships as a **signed MSIX**. Launch it and pick a host from the list, just like the other native apps.

The package installs **two** Start-menu entries — **Punktfunk**, the desktop window, and
**Punktfunk Console**, a controller-driven fullscreen interface for a TV or HTPC (host list, pairing,
settings and game library, all navigable with a pad) — plus the headless
[`punktfunk` command](#scripting-the-punktfunk-cli) on your PATH.

> Hardware decode and HDR10 present are validated on glass on NVIDIA and Intel (including HDR
> pass-through on the Intel D3D11VA path). If anything misbehaves, **[Moonlight](/docs/moonlight)**
> is a proven alternative for Windows.

For scripting, prefer the [`punktfunk` CLI](#scripting-the-punktfunk-cli). The window binary's own
headless flags stay supported too:

```sh
punktfunk-client                                              # open the WinUI 3 window (host list / settings)
punktfunk-client --discover                                  # list hosts on the network
punktfunk-client --headless --speed-test --connect <host>:9777  # no window: probe the link, print measured/recommended bitrate
```

Prefer the broadest compatibility, or no install? **Moonlight** also streams to Windows (see below).

## webOS (LG TV) — community

[`pf-webos`](https://github.com/dyptan-io/pf-webos) is a native client for LG webOS TVs, built and
maintained by the community ([dyptan-io](https://github.com/dyptan-io)) on top of Punktfunk's
`punktfunk/1` protocol and core. It's not an official Punktfunk app, but it speaks the real protocol
directly (not Moonlight/GameStream) — LAN discovery or add-by-IP, PIN pairing with pinned reconnects,
hardware video decode via webOS's NDL DirectMedia API, and a browsable game library with cover art,
navigable with the Magic Remote.

It ships as a sideloadable `.ipk` (homebrew package) rather than through the LG Content Store — see
[Install a Client](/docs/install-client#lg-webos-tv-community) for the sideload steps.

## Scripting: the `punktfunk` CLI

`punktfunk` is the headless client — the same core the graphical apps use, with no window. It ships
in **every Linux client package** (apt, dnf, pacman and the Flatpak) and in the **Windows MSIX**, so
if you have a desktop client you already have it:

```sh
punktfunk hosts list --probe                  # saved hosts, each with a live reachability check
punktfunk pair <host>[:port] --pin 1234       # enrol this device with a host
punktfunk library <host-ref> --json           # the host's games, machine-readable
punktfunk launch <host-ref> --game <id>       # stream, waking the host first if it's asleep
punktfunk open 'punktfunk://connect/<host-ref>'
punktfunk speed-test <host-ref>               # measure the link, print the recommended bitrate
```

A `<host-ref>` is a saved host's id, its name, or an address — the same reference a `punktfunk://`
link takes. There is also `hosts add` / `hosts forget`,
[`wake`](/docs/wake-on-lan#from-the-command-line), `reachable`, `profiles list` and `reset`; run
`punktfunk help <command>` for a verb's flags.

Exit codes are stable, so a script can branch without parsing prose: **0** ok, **2** connect failed,
**3** trust rejected (re-pair), **4** the renderer couldn't start, **5** nothing matched what you
named, **6** it needs a person (pairing, or an unknown host).

Under the Flatpak, run it as `flatpak run --command=punktfunk io.unom.Punktfunk <args>`.

> The older headless flags stay supported for existing scripts — `punktfunk-client --connect`,
> `--discover` and `--headless --speed-test` on both Linux and Windows. `punktfunk` is the surface
> to build new things on: it wakes a sleeping host before connecting, which those never did.
>
> `punktfunk-probe` is a different thing again — an in-repo protocol test and latency-measurement
> tool for development. It isn't shipped in any package; you build it from source.

## Profiles and links (every app)

Two things work the same in the Apple, Linux, Windows and Android apps. **Settings profiles** are
named sets of stream overrides — bitrate, resolution, codec, HDR and the rest — that you bind to a
host or pick for a single connect, with every field you didn't touch still following your defaults.
And a **`punktfunk://` link** starts a stream from a browser, a desktop shortcut, a home-automation
rule or `punktfunk open`, carrying only *references* to things that already exist on your device —
never a setting, and never a trust decision.

[Profiles and links](/docs/profiles-and-links) has both in full: the link grammar, where each app
puts **Copy link** and **Create shortcut…**, and what a link is refused for. From a script,
`punktfunk profiles list` and `punktfunk launch --profile <ref>` reach the same profiles.

## While you're streaming

Click the stream and the desktop clients **capture** your keyboard and mouse — everything goes to
the host until you let go. **Ctrl+Alt+Shift+Q** (⌃⌥⇧Q or ⌘⎋ on a Mac) gives it back.

That chord, the three others a stream reserves, the controller chord that works with no keyboard in
reach, which app honours which of them, the two mouse modes, the three touch modes and stylus input
are all on [Mouse, touch and pen](/docs/input#getting-your-input-back).

Copying between the two machines is a separate opt-in: the host operator allows it in `host.env`
and you turn it on for that one host in your client. Content crosses today from the macOS, Windows
and Android apps — the Linux client has the switch but no bridge behind it yet, and iOS, iPadOS and
tvOS have neither. See [Shared clipboard](/docs/clipboard).

## Which should I use?

| You're streaming to… | Use |
|---|---|
| A Mac, iPhone, iPad, or Apple TV | The **[Apple app](#apple-app-mac-iphone-ipad-apple-tv)** |
| A Linux desktop or laptop | **[`punktfunk-client`](#linux-desktop-client-gtk4)** (GTK4) |
| A **Steam Deck** | The **[Decky plugin](/docs/steam-deck)** in Gaming Mode, or the [GTK4 client](#linux-desktop-client-gtk4) in Desktop Mode |
| An Android phone or TV | The **[Android app](#android-app-phone--android-tv)** |
| Windows | The native **[`punktfunk-client`](#windows-desktop-client)** (signed MSIX) or **[Moonlight](/docs/moonlight)** |
| An **LG webOS TV** | The community **[`pf-webos`](https://github.com/dyptan-io/pf-webos)** client, or **[Moonlight](/docs/moonlight)** |
| A browser, another smart TV, or any other device | **[Moonlight](/docs/moonlight)** |
| Scripts, plugins, home automation | The headless **[`punktfunk`](#scripting-the-punktfunk-cli)** CLI |
| Protocol development / latency measurement | **`punktfunk-probe`** (source build only) |

Whichever you choose, the first connection needs a one-time [pairing](/docs/pairing), and
[Install a Client](/docs/install-client) covers installing, updating and removing it.
