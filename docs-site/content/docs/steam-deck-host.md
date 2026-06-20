---
title: "Steam Deck (Host)"
description: "Run a punktfunk host on a Steam Deck — stream its Game Mode (or desktop) to your other devices. One script, built on-device for SteamOS."
---

This is for using a **Steam Deck as the host** — streaming *from* it to a laptop, TV, phone, or
another Deck. (For the usual case — streaming *to* a Deck — see [Install a Client](/docs/install-client),
which uses the Flatpak + Decky plugin.)

SteamOS is an immutable, read-only Arch base, so the host isn't a system package. Instead a single
script builds the host **natively inside a Debian-trixie distrobox** (ABI-matched to SteamOS's
FFmpeg/glibc — the binary then runs natively on SteamOS) and wires it up as systemd user services.
Building on-device means a rebuild always matches the running OS, so a SteamOS update can't leave you
with a binary linked against the wrong libraries. Encode is **VAAPI** on the Deck's AMD GPU
(auto-detected; NVENC on NVIDIA).

> **Heads up:** the Deck's WiFi *tx* tops out around ~250 Mbps of goodput regardless of band (it's a
> hardware/driver packet-rate limit, not bandwidth) — plenty for 1080p/1440p60, not 4K. A wired dock
> lifts that. See [Configuration](/docs/configuration) for bitrate guidance.

## Prerequisites

- A Steam Deck on **SteamOS 3** (LCD or OLED). Steady WiFi or, better, a wired dock.
- **distrobox** installed (no root needed). If `distrobox` isn't found:
  ```sh
  curl -sfL https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
  ```
  Make sure `~/.local/bin` is on your `PATH` (re-open the terminal).
- The first build downloads a container image + toolchain (~1 GB) and takes ~10–15 minutes. Later
  rebuilds are incremental.

## 1. Get the source

In Desktop Mode open **Konsole** (or ssh in), then:

```sh
git clone https://git.unom.io/unom/punktfunk ~/punktfunk
```

## 2. Run the installer

```sh
bash ~/punktfunk/scripts/steamdeck/install.sh
```

It is idempotent — safe to re-run. In one pass it:

1. creates the `pf2` Debian-trixie distrobox and installs the build toolchain,
2. builds `punktfunk-host` (and the web console),
3. writes config to `~/.config/punktfunk/` (a generated web-console login password),
4. raises the UDP socket buffers to 32 MB and adds you to the `input` group (needs `sudo`; skipped
   with a warning if unavailable),
5. installs + starts the `punktfunk-host` and `punktfunk-web` **systemd user services** (with linger,
   so they run without a login session).

Useful flags:

| Flag | Effect |
|------|--------|
| `--open` | Accept **unpaired** clients (trust-on-first-use) — convenient on a fully trusted LAN. Default is PIN pairing required. |
| `--no-web` | Skip the management web console. |
| `--src=DIR` | Build from source at `DIR` instead of `~/punktfunk`. |

When it finishes it prints the web-console URL and how to pair.

## 3. Pair a device

By default the host **requires PIN pairing** (secure). Two ways to pair:

- **Web console** (printed at the end of step 2): open `http://<deck-ip>:3000`, log in with the
  generated password (in `~/.config/punktfunk/web.env`), go to **Devices → arm pairing**, and enter
  the PIN on your client.
- **From the client directly**: pick this Deck (it advertises over mDNS as `_punktfunk._udp`) and
  enter the PIN the host shows.

On a trusted home LAN you can instead install with `--open` and skip pairing entirely.

## 4. Verify

```sh
systemctl --user status punktfunk-host          # active (running)
journalctl --user -u punktfunk-host -f          # watch a client connect
```

Connect from any client ([Moonlight](/docs/moonlight) or a [native client](/docs/clients)). In Game
Mode the host attaches to the running gamescope session and streams it at your client's resolution; in
Desktop Mode it streams the KDE desktop. The host auto-detects which session is live per connection.

## Updating

After pulling new source, rebuild and restart in one step (config + pairings persist):

```sh
git -C ~/punktfunk pull          # or rsync new source in
bash ~/punktfunk/scripts/steamdeck/update.sh
```

## Notes & limits

- **Single session at a time** at custom resolutions — two clients requesting different modes will
  thrash the managed session. Pick one mode per session.
- **Keep the Deck awake.** Game Mode auto-suspends on idle, which drops the host off the network mid
  stream — disable auto-suspend (Settings → Power) for a headless host.
- **It survives OS updates**, but a major SteamOS bump can move library versions; if the host fails to
  start after an update, just re-run `update.sh` to rebuild against the new base.
- Deeper reference (services, container, manual steps): [`scripts/steamdeck/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/scripts/steamdeck/README.md).

Trouble? See [Troubleshooting](/docs/troubleshooting) and [Pairing](/docs/pairing).
