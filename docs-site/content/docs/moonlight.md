---
title: Connect with Moonlight
description: Stream from a Punktfunk host using any Moonlight client.
---

Punktfunk speaks the **GameStream** protocol, so [Moonlight](https://moonlight-stream.org/) connects
to it like it would to any GameStream host — no punktfunk-specific app needed. It's a great option for
a browser, a smart TV, or any device without a native client.

> Many platforms also have a **native Punktfunk client** with lower latency and built-in
> discovery/pairing — including **Windows** and **Android** (phone and Android TV). See
> [Clients](/docs/clients) before reaching for Moonlight.

## 1. Make sure the host is running with GameStream enabled

Moonlight needs the GameStream planes, which are **opt-in** on the host — but most installs already
enable them:

- **Linux packages, NixOS, SteamOS / Steam Deck** — already on: the shipped `punktfunk-host` user
  unit runs `serve --gamestream`. To turn it *off*, override `ExecStart` with a drop-in so a package
  upgrade doesn't undo it — see
  [What the unit starts](/docs/running-as-a-service#what-the-unit-starts) — or set
  `services.punktfunk.host.gamestream = false` on NixOS, or re-run the Deck installer with
  `--no-gamestream`.
- **Windows** — off unless you ticked the installer's *Enable GameStream (Moonlight) compatibility*
  checkbox. To turn it on afterwards, run this from an **elevated** prompt (`=off` puts it back) —
  see [Windows Host](/docs/windows-host):

  ```powershell
  punktfunk-host service install --gamestream=on
  punktfunk-host service restart
  ```

- **Running `serve` by hand** — add the flag yourself:

  ```sh
  punktfunk-host serve --gamestream
  ```

(Bare `serve` is the secure native-only default and stock Moonlight clients can't connect to it; the
native plane is always on, and `--gamestream` adds the Moonlight-compat surface.) GameStream pairs over
plain HTTP and its legacy control encryption is weaker than the native plane's, so only enable it on a
**trusted LAN**. See [Running as a Service](/docs/running-as-a-service) for the bundled unit. The host
advertises itself on the network, so Moonlight usually finds it on its own.

## 2. Add the host in Moonlight

Open Moonlight. Your host should appear automatically on the same network. If it doesn't, use **Add
Host manually** and enter the host machine's IP address.

Still nothing? Two causes account for almost all of it:

- **The GameStream ports aren't open on the host.** They are TCP **47984 / 47989 / 48010** and UDP
  **47998 / 47999 / 48000**, plus mDNS on UDP **5353**. On Linux the packages ship a ready-made rule
  for your distro's firewall but never enable it:

  ```sh
  sudo ufw allow punktfunk-gamestream                               # ufw
  sudo firewall-cmd --permanent --add-service=punktfunk-gamestream  # firewalld
  sudo firewall-cmd --reload
  ```

  On Windows the installer adds these rules itself — but only for **Private** and **Domain**
  networks, unless you ticked *Allow connections on Public networks* during setup.
- **Another GameStream host is running on the same machine.** Sunshine, Apollo and their forks bind
  the same fixed ports and advertise the same mDNS name, so the wrong host answers (or neither
  does). Run `punktfunk-host detect-conflicts` on the host machine — it lists any it finds.

Both are covered in more detail in [Troubleshooting](/docs/troubleshooting).

## 3. Pair

Moonlight's PIN is typed in on the **host** side, so you need the host's
[web console](/docs/web-console) running — it's the only UI where a Moonlight PIN can be entered.

1. Open the console at `https://<host-ip>:47992` and go to **Pairing**.
2. In Moonlight, select the host and choose **Pair** — it shows a 4-digit PIN.
3. The console's **Moonlight (GameStream) pairing** card now says a client is waiting. Type the PIN
   there and press **Submit PIN**.

The device then appears under **Paired devices**, and Moonlight remembers the host. See
[Pairing & Trust](/docs/pairing) for the full picture.

## 4. Stream

Moonlight lists **Desktop** plus the games the host found installed (Steam, Epic, GOG, Xbox), with
cover art — the same [library](/docs/game-library) the native clients show. Pick one and start
streaming. The host creates a virtual display at the resolution and frame rate Moonlight requests
(set these in Moonlight's settings), encodes it on the GPU, and streams it. Mouse, keyboard, and
controllers flow back to the host — and a Moonlight client that sends pen events, an iPad's Apple
Pencil included, drives the same host-side tablet a native client would, with pressure and tilt
intact (see [Pen and stylus](/docs/input#pen-and-stylus)).

That **Desktop** entry is the operator base list. An `apps.json` in the host's config directory
*replaces* it (it doesn't add to it), so put every entry you want in that file if you write one.

## Tips

- **Set your resolution and frame rate in Moonlight's settings** before connecting — the host matches
  whatever Moonlight asks for, creating the virtual display at that exact mode.
- **Codec:** HEVC (H.265) is a good default; AV1 is available if your client supports it.
- **HDR:** Moonlight only offers its HDR toggle when the host advertises a 10-bit codec, which it
  does only when [its capture path *and* its encoder](/docs/hdr#the-chain) can really deliver
  10-bit BT.2020 PQ. If the toggle is there, turn it on and pick HEVC or AV1 — H.264 stays SDR.
  Setting `PUNKTFUNK_10BIT=0` in [`host.env`](/docs/configuration) withdraws the offer entirely; it
  is on by default.
- **Bitrate:** start moderate and raise it. For very high bitrates, the [native
  clients](/docs/clients) have a built-in speed test; with Moonlight, set the bitrate manually.
- Moonlight uses the GameStream protocol, not Punktfunk's native FEC/encryption extensions. On a
  solid LAN this is fine; on a lossy link a [native client](/docs/clients) holds up better.
- Comparing Moonlight's performance overlay with a Punktfunk client's stats HUD? The numbers
  measure different slices of the pipeline — see [Understanding the Stats Overlay](/docs/stats)
  for a line-by-line comparison matrix before drawing conclusions.
