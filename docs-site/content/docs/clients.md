---
title: Clients
description: The ways to connect to a punktfunk host — the Apple app, Moonlight, or the Linux client.
---

A punktfunk host accepts two kinds of client. Pick whichever fits the device you're streaming *to*.

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

## Linux reference client

`punktfunk-client-rs` (in the repo) is a command-line client for the native protocol, mainly for
testing and development. It connects, streams to a file, runs the speed test, and can discover hosts:

```sh
punktfunk-client-rs --discover                       # list hosts on the network
punktfunk-client-rs --connect <host>:9777 --pin <fp> # connect to one
```

A full graphical Linux client (hardware decode + present) is on the [roadmap](/docs/roadmap).

## Which should I use?

| You're streaming to… | Use |
|---|---|
| A Mac, iPhone, iPad, or Apple TV | The **Apple app** |
| Windows, Android, Steam Deck, a browser, a TV | **Moonlight** |
| Another Linux box (testing) | **`punktfunk-client-rs`** |

Whichever you choose, the first connection needs a one-time [pairing](/docs/pairing).
