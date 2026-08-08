---
title: "SteamOS (Host)"
description: "Run a Punktfunk host on SteamOS — stream its Game Mode (or desktop) to your other devices. One script, built on-device and ABI-matched to SteamOS."
---

This is for using a **SteamOS device as the host** — streaming *from* it to a laptop, TV, phone, or
another device. (For the usual case — streaming *to* a Steam Deck — see [Install a Client](/docs/install-client),
which uses the Flatpak + Decky plugin.)

We support SteamOS as a host mainly with an eye to the **upcoming Steam Machine** — a living-room,
desktop-class SteamOS box is a natural always-on streaming host. The **Steam Deck** is the SteamOS
device we can test on today, so it's what these instructions are validated against; the same
on-device build works on any SteamOS 3 system.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

SteamOS is an immutable, read-only Arch base, so the host isn't a system package. Instead a single
script builds the host **natively inside a Debian-trixie distrobox** (ABI-matched to SteamOS's
FFmpeg/glibc — the binary then runs natively on SteamOS) and wires it up as systemd user services.
Building on-device means a rebuild always matches the running OS, so a SteamOS update can't leave you
with a binary linked against the wrong libraries. Encode is auto-detected: **Vulkan Video** on the
Deck's AMD GPU (with libav **VAAPI** as the fallback), **NVENC** on NVIDIA. The installer writes
`RADV_PERFTEST=video_encode` into `~/.config/punktfunk/host.env` — Van Gogh's RADV driver hides
Vulkan encode behind that flag, and without it sessions quietly fall back to VAAPI. Set
`PUNKTFUNK_VULKAN_ENCODE=0` in `host.env` to force the libav VAAPI path.

> **Heads up:** in our testing the Steam Deck's WiFi *tx* topped out around ~250 Mbps of goodput
> regardless of band — enough for 1080p/1440p60, not 4K. This looked like a hardware/driver
> packet-rate limit rather than a bandwidth ceiling, but it's one device measured on one network:
> other SteamOS hardware, newer drivers, or a less congested band may do better. A wired dock
> sidesteps it entirely. See [Configuration](/docs/configuration) for bitrate guidance.

## Prerequisites

- A SteamOS device on **SteamOS 3** (e.g. a Steam Deck, LCD or OLED). Steady WiFi or, better, a wired dock.
- **A `sudo` password.** A stock SteamOS `deck` account has none at all, so `sudo` can't work
  until you set one. Do it once, before you start:
  ```sh
  passwd
  ```
  Without it the installer runs, but skips every root step: gamepad passthrough, `vhci-hcd`, the
  UDP buffer tuning, the atomic-update keep list and linger.
- **Run the installer on a real terminal** — Konsole in Desktop Mode, or `ssh -t`. A
  non-interactive ssh session has no TTY for the `sudo` prompt, and the same steps get skipped.
- **distrobox** installed (no root needed). If `distrobox` isn't found:
  ```sh
  curl -sfL https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local
  ```
  Make sure `~/.local/bin` is on your `PATH` (re-open the terminal).
- The first build downloads a container image + toolchain (~1 GB) and takes ~10–15 minutes, then
  another ~5–10 minutes for the HDR gamescope build (a second set of dependencies plus a fresh
  gamescope clone). Budget around 25 minutes for a first install. Later rebuilds are incremental,
  and the gamescope step is a no-op unless its sources changed.

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
2. builds `punktfunk-host`, the web console, and the **plugin/script runner** (so the console's
   [plugin store](/docs/plugins) works out of the box — the runner service itself stays opt-in),
3. writes config to `~/.config/punktfunk/` (a generated web-console login password),
4. raises the UDP socket buffers to 32 MB, installs the gamepad udev rule + the `vhci-hcd` autoload
   and adds you to the `input` group (virtual gamepads) **and the `punktfunk` group** (the usbip
   nodes **native Steam Deck controller passthrough** attaches through — creating that group if it
   does not exist yet), seeds the KDE RemoteDesktop grant for Desktop-mode input, and **registers all of it on SteamOS's
   atomic-update keep list** so OS updates carry it over — the installer asks for your `sudo`
   password **first, before the long build**, so you can authorise once and walk away,
5. installs + starts the `punktfunk-host` and `punktfunk-web` **systemd user services** (with linger,
   so they run without a login session) plus a boot-time **rebuild check** that repairs the host
   automatically if a SteamOS update ever breaks its library links.

Useful flags:

| Flag | Effect |
|------|--------|
| `--open` | Accept **unpaired** clients (trust-on-first-use) — convenient on a fully trusted LAN. Default is PIN pairing required. |
| `--no-gamestream` | Run a **secure native-only** host — skip the GameStream/Moonlight-compat planes (see below). Default keeps them on so stock Moonlight works. |
| `--no-web` | Skip the management web console. |
| `--src=DIR` | Build from source at `DIR` instead of `~/punktfunk`. **Only if you must** — the post-OS-update rebuild check and the web console's one-click update both look for `~/punktfunk/scripts/steamdeck/update.sh`, so with a relocated source you have to update by hand: `PUNKTFUNK_SRC=DIR bash DIR/scripts/steamdeck/update.sh --pull` (without `PUNKTFUNK_SRC` the script still looks for `~/punktfunk`). A symlink (`ln -s DIR ~/punktfunk`) restores both. |

When it finishes it prints the web-console URL and how to pair.

> **GameStream/Moonlight compat is on by default.** The native `punktfunk/1` plane (used by
> Punktfunk's own clients — SPAKE2 PIN pairing, per-direction AEAD) is **always on** and is the secure
> path. The installer also enables the **GameStream/Moonlight-compat planes** so stock
> [Moonlight](/docs/moonlight) works — but those carry inherent on-path weaknesses (pairing over plain
> HTTP; legacy control encryption that can reuse GCM nonces), so enable them only on a **trusted LAN**.
> If you only ever use native clients, install with `--no-gamestream` for a host with no GameStream
> surface at all.

> **First install — reboot once before streaming.** KWin only authorizes Desktop-mode screen capture
> on a fresh session, and the new `input` and `punktfunk` groups (native Steam Deck controller
> passthrough) only take
> effect on a new login — so after the **first** install, **reboot the Deck** (a re-run that changes
> nothing doesn't need it). Streaming **Game Mode** with a generic Xbox pad works right away; **Desktop
> capture and the native Steam Deck controller need the reboot.** If a client connects and every
> session ends with `KWin does not expose zkde_screencast_unstable_v1` or the pad shows up as an Xbox
> 360 controller, you haven't rebooted yet.

## 3. Pair a device

By default the host **requires PIN pairing** (secure). Two ways to pair:

- **Web console** (printed at the end of step 2): open `https://<device-ip>:47992` (self-signed host
  cert — your browser warns once; trust it and continue),
  [arm pairing](/docs/web-console#arm-pairing), and enter the PIN on your client.
- **From the client directly**: pick this host (it advertises over mDNS as `_punktfunk._udp`) and
  enter the PIN the host shows.

On a trusted home LAN you can instead install with `--open` and skip pairing entirely.

### Console login password

The installer generates a random console login password (printed at the end of step 2) and writes it
to `~/.config/punktfunk/web.env`. To read it back or set your own, see
[The Web Console](/docs/web-console#login-password).

> **Installed before 0.25.0? Rotate that password once.** Older versions of the script created
> `web.env` at the account's default umask, so the console password and session secret sat on disk
> world-readable — any local account could read them. Re-running `install.sh` or `update.sh` now
> tightens the file to `0600` and tells you it did, but a chmod cannot un-leak a secret that was
> already readable. Change `PUNKTFUNK_UI_PASSWORD` in `~/.config/punktfunk/web.env`, then
> `systemctl --user restart punktfunk-web`.

## 4. Verify

```sh
systemctl --user status punktfunk-host          # active (running)
journalctl --user -u punktfunk-host -f          # watch a client connect
```

Connect from a [native client](/docs/clients), or from [Moonlight](/docs/moonlight) (unless you
installed with `--no-gamestream`). In Game Mode the host attaches to the running gamescope session and
streams it at your client's resolution; in Desktop Mode it streams the KDE desktop. The host
auto-detects which session is live per connection. See [Steam / gamescope](/docs/gamescope) for the
attach-vs-managed detail and known limits.

## HDR (10-bit BT.2020 PQ)

The installer also builds `punktfunk-gamescope` — gamescope plus the small patch that adds the
10-bit PQ formats to its capture node (see [HDR on gamescope](/docs/gamescope#hdr-on-gamescope)) —
and points the host at it. Nothing to configure: with it in place, a 10-bit-capable client with
HDR enabled streams true HDR10 from Game Mode; anything missing and the session streams 8-bit SDR
instead (never a mislabelled picture). Check what the host resolved with:

```sh
( set -a; . ~/.config/punktfunk/host.env; set +a
  ~/punktfunk/target-steamos/release/punktfunk-host hdr-probe )
```

Run it exactly like this — the probe reads its answers from the environment, and `host.env` is what
the service runs with (including which gamescope the host uses). A bare shell doesn't have it, so
the probe would describe an environment your host never runs in.

The build is best-effort: if it fails, the installer says so loudly and everything else keeps
working in SDR — re-run `update.sh` to retry. `PUNKTFUNK_GAMESCOPE_HDR=0` in
`~/.config/punktfunk/host.env` forces SDR deliberately.

## Updating

Two ways, and both keep your config and pairings.

**From the web console (easiest).** The Updates card shows an **Update now** button — no opt-in and
no root, because this install is owned by your own user. It asks for the console password, then
rebuilds on the device, so expect several minutes; the card shows progress and the full log lands in
`~/.config/punktfunk/logs/update-steamos.log`. See [Updating](/docs/updating).

**From a terminal.** One command — `--pull` fetches the new source first:

```sh
bash ~/punktfunk/scripts/steamdeck/update.sh --pull
```

Drop `--pull` if you rsync source in yourself. `update.sh` also retrofits anything a newer installer
adds — the plugin runner, the HDR gamescope, the atomic-update keep list, the rebuild check, the
`punktfunk` group, and the `0600`/`0700` permissions on `~/.config/punktfunk` — onto an older
install. You do **not** need to run the group or firewall commands from the release notes by hand on
a Deck: the group is the script's job, and stock SteamOS runs no firewall for the port note to apply
to. Rotating the console password after a pre-0.25.0 install is the one thing still on you (above).

> **This install follows the canary channel.** An on-device source build tracks `main`, not stable
> `vX.Y.Z` releases, so the console offers you the newest `main` build. See
> [Release Channels](/docs/channels).

### Going back to an earlier version

Because this is a source build, "roll back" means "check out an older commit and rebuild":

```sh
git -C ~/punktfunk checkout v0.22.3      # any release tag
bash ~/punktfunk/scripts/steamdeck/update.sh
```

That leaves the checkout on a detached tag, where `git pull` has no branch to fast-forward — so a
later `update.sh --pull`, **and the console's Update now button** (which runs exactly that), stops
with *"You are not currently on a branch."* Run `git -C ~/punktfunk checkout main` first when you
want to follow `main` again.

## Uninstalling

There is no uninstall script — the install is spread across your user session and a few root-owned
files, so remove it in that order. Stop and forget the services first:

```sh
systemctl --user disable --now punktfunk-host punktfunk-web \
  punktfunk-scripting punktfunk-rebuild-check
rm -f ~/.config/systemd/user/punktfunk-*.service
systemctl --user daemon-reload
sudo loginctl disable-linger "$USER"      # only if nothing else on this device needs linger
```

Then the build container and everything the installer put under your home:

```sh
distrobox rm -f pf2                       # the build container (~1 GB)
rm -f  ~/.local/bin/punktfunk-scripting ~/.local/bin/punktfunk-gamescope
rm -rf ~/.local/lib/punktfunk-scripting ~/.local/share/punktfunk-scripting
rm -f  ~/.local/share/punktfunk/gamescope.stamp
rm -f  ~/.local/share/applications/io.unom.Punktfunk.Host.desktop
rm -rf ~/punktfunk                        # the source checkout and its target-steamos build dir
```

The build container shares your real home directory, so the toolchains it installed are in there
too: `~/.cargo`, `~/.rustup` and `~/.bun` (plus the cargo download cache). Delete those only if
nothing else of yours uses Rust or bun.

Then the root-owned tuning. Don't skip this: the keep-list entry is what carries the other three
files through every SteamOS update, so left alone they stay on the device indefinitely.

```sh
sudo rm -f /etc/atomic-update.conf.d/punktfunk.conf \
           /etc/udev/rules.d/60-punktfunk.rules \
           /etc/modules-load.d/punktfunk.conf \
           /etc/sysctl.d/99-punktfunk-net.conf
sudo udevadm control --reload-rules
```

Two things are deliberately left alone. `~/.config/punktfunk/` holds your identity certificate,
paired devices and the console password — delete it only if you're not coming back:

```sh
rm -rf ~/.config/punktfunk
```

And the installer may have seeded a KDE RemoteDesktop portal grant at
`~/.local/share/flatpak/db/kde-authorized` (only if you had none); remove that file if nothing
else on the device relies on it. Your `input` group membership is harmless to keep — drop it with
`sudo gpasswd -d "$USER" input` if you'd rather not. The `punktfunk` group is worth actually
dropping once the host is gone, because it can present emulated USB hardware and nothing else uses
it: `sudo gpasswd -d "$USER" punktfunk`.

See [Uninstalling](/docs/uninstall) for the other install methods and what each one leaves behind.

## Notes & limits

- **Single session at a time** at custom resolutions — two clients requesting different modes will
  thrash the managed session. Pick one mode per session.
- **Keep the device awake.** On handhelds, Game Mode auto-suspends on idle, which drops the host off
  the network mid stream — disable auto-suspend (Settings → Power) for a headless host.
- **Native Steam Deck controller passthrough** presents the client's pad as a real Steam Deck
  controller (paddles, trackpads, gyro) via a virtual USB device — that needs the `input` **and
  `punktfunk`** groups and the
  `vhci-hcd` module live, so it only works **after the first-install reboot** above; until then the pad
  degrades to a generic Xbox 360 controller (still fully playable). The second group is separate on
  purpose: it can present arbitrary emulated USB hardware, which is why it is not folded into the
  `input` group every gamepad guide tells you to join. Check both with `id -nG`, and check the nodes
  themselves with `ls -l /sys/devices/platform/vhci_hcd.0/attach` — group `punktfunk`, mode `0660`.
  If you're streaming *to* another
  Steam Deck, also set Steam Input to **Off** for Punktfunk on that Deck — see
  [Stream to a Steam Deck](/docs/steam-deck).
- **It survives OS updates — automatically.** SteamOS A/B updates rebuild `/etc` and can move
  library versions; the installer defends both sides. The system tuning (gamepad udev rule,
  `vhci-hcd`, UDP buffers) is registered on SteamOS's own atomic-update keep list
  (`/etc/atomic-update.conf.d/`), so updates carry it over; and a boot-time check
  (`punktfunk-rebuild-check`) probes the host binary and re-runs the build only if the new OS
  actually broke its library links — you should never need to intervene. (Re-running `update.sh`
  by hand still works and is harmless.)
- Deeper reference (services, container, manual steps): [`scripts/steamdeck/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/scripts/steamdeck/README.md).

Trouble? See [Troubleshooting](/docs/troubleshooting) and [Pairing](/docs/pairing).
