---
title: Quick Start
description: From nothing to streaming in five steps — install a host, open its console, pair a client, play.
---

Five steps, each linking to the detail only if you need it. Punktfunk is built for a **trusted home
network** — keep the host on your LAN or a VPN, never open it to the internet
([why](/docs/security)).

## 1. Install the host

On the PC you want to stream *from*, follow the one-page guide for its system — each one is the
install command, the group to join, and nothing else:

| Linux | Windows |
|---|---|
| [Ubuntu](/docs/ubuntu) · [Debian](/docs/debian) · [Fedora](/docs/fedora) · [Arch / CachyOS](/docs/arch) · [Bazzite](/docs/bazzite) · [SteamOS](/docs/steamos-host) · [NixOS](/docs/nixos) | [Windows 11](/docs/windows-host) |

Not sure your machine qualifies? [Requirements](/docs/requirements) is the checklist.

## 2. Start it

- **Windows and SteamOS:** nothing to do — the installer started the host and the web console, and
  they come back on every boot.
- **Linux packages:** from a terminal inside your desktop session, start the host and the console
  once; they restart at every login from then on:

  ```sh
  systemctl --user enable --now punktfunk-host punktfunk-web
  ```

  On Arch install `punktfunk-web` first (your install page says how). Running a firewall? Your
  install page has the one line that opens it.

The host announces itself on your network, so clients find it by name. It works out which desktop you
run by itself — there is nothing to configure for a first stream.

## 3. Open the web console

The console is where you admit new devices. Open **`https://<host-ip>:47992`** in a browser (the
certificate is the host's own, so your browser warns once — continue) and log in:

- **Linux:** the password was generated on first start — print it with
  `sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password`
  (SteamOS: the install script printed it; it's in `~/.config/punktfunk/web.env`).
- **Windows:** the installer showed it on its last page.

Lost it? [Forgot your password](/docs/forgot-password). Everything else about the console:
[The Web Console](/docs/web-console).

## 4. Install a client and pair it

On the device you want to stream *to*, install the app — [Install a Client](/docs/install-client)
has the link for every device (Mac, iPhone/iPad/Apple TV, Linux, Windows, Android, Steam Deck), and
any Moonlight client works too once you [turn GameStream on](/docs/moonlight).

Open the app: your host is already in the list. Select it and **connect**. Back in the console, the
device appears under **Pairing → Waiting for approval** — click **Approve** and it's in, no PIN to
type. (Prefer a PIN? Click **Pair a device** in the console and type the 4-digit code into the
client.) Pairing happens once; the device reconnects on its own from then on.
Details: [Pairing & Trust](/docs/pairing).

## 5. Stream

Select the host, start streaming. The host creates a display at your device's exact resolution and
refresh rate; mouse, keyboard and controllers flow back. On a desktop client the stream takes your
mouse and keyboard — **Ctrl+Alt+Shift+Q** (⌃⌥⇧Q on a Mac) hands them back.

## Now that it works

- Launch installed games straight into the stream — [Game library](/docs/game-library) (install the
  [plugin](/docs/plugins) for each launcher you use).
- Tune resolution, bitrate, codec and HDR per device — [Client settings](/docs/client-settings).
- Copy here, paste there — [Shared clipboard](/docs/clipboard). Wake a sleeping host —
  [Wake-on-LAN](/docs/wake-on-lan).
- Run the host headless, with nobody logged in — [Running as a service](/docs/running-as-a-service).
- Already running Sunshine or Apollo? [Switching from Sunshine](/docs/switching-from-sunshine).
- Stuck? [Troubleshooting](/docs/troubleshooting) starts from the symptom.
