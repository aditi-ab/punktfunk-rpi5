---
title: Quick Start
description: From nothing to streaming — set up a host and connect your first client.
---

This is the shortest path to a working stream. Each step links to the details.

## 1. Set up the host

On your Linux + NVIDIA machine, follow the guide for your system:

- [Ubuntu — GNOME](/docs/ubuntu-gnome)
- [Ubuntu — KDE Plasma](/docs/ubuntu-kde)
- [Fedora — KDE Plasma](/docs/fedora-kde)
- [Bazzite — gamescope / Steam](/docs/bazzite)

Each one covers the NVIDIA driver, the dependencies, and how to build and run the host. Check the
[Requirements](/docs/requirements) first if you're not sure your machine is a fit.

## 2. Start the host

From a terminal **inside your desktop session** (so the host can reach your compositor):

```sh
punktfunk-host serve --native
```

The host starts listening and prints its identity fingerprint. It advertises itself on your local
network, so clients can find it by name. Leave it running. (To start it automatically at boot, see
[Running as a Service](/docs/running-as-a-service).)

## 3. Connect and pair a client

On the device you want to stream to:

- **Apple (Mac, iPhone, iPad, Apple TV):** open the punktfunk app — your host appears under *On this
  network*. Tap it, and when prompted, **pair**.
- **Anything with Moonlight:** add the host (it should be discovered automatically), then pair.

To pair, the host needs to show a PIN. Arm pairing from the host's web console — the host displays a
4-digit PIN, you type it into the client, and they trust each other from then on. Pairing is required
by default. Full details: [Pairing & Trust](/docs/pairing).

## 4. Stream

Once paired, select the host and start streaming. The host creates a virtual display at your device's
resolution and refresh, and the picture comes up. Mouse, keyboard, and controllers flow back to the
host.

## Next steps

- Tune [resolution, refresh, and bitrate](/docs/configuration).
- Run the host [as a background service](/docs/running-as-a-service) so it's always available.
- Hit a snag? See [Troubleshooting](/docs/troubleshooting).
