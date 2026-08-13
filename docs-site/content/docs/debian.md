---
title: Debian
description: Install the Punktfunk host on Debian 13 with apt — including LMDE and Linux Mint.
---

Install a Punktfunk host on **Debian 13 ("trixie") or newer** from the apt registry. This page
covers the distro-level setup — GPU driver, package, gamepad access. How the host creates its
virtual display and injects input is desktop-specific, so pick your desktop on the
[configure pages](#configure-your-desktop) afterward rather than here.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

> **Which releases.** The host package needs **glibc 2.39 or newer**; Debian 13 has 2.41, so it
> installs and runs there. **Debian 12 (bookworm) has glibc 2.36 and cannot install it** — build
> from source ([Ubuntu appendix](/docs/ubuntu#appendix--build-from-source), which applies here too)
> or upgrade. Check yours with `ldd --version`.

> **The desktop client is not packaged for Debian yet.** `punktfunk-client` is built on Ubuntu 26.04
> and floors at `libc6 >= 2.43` (Debian 13 has 2.41), on top of needing GTK4 ≥ 4.20. On a Debian
> box, stream *to* it with a [different client](/docs/install-client) — the Flatpak, or a build from
> source. The **host**, the **web console** and the **plugin runner** all install normally.

## What works on Debian 13

| Package | Debian 13 | What it is |
|---|---|---|
| `punktfunk-host` | ✅ | The streaming host |
| `punktfunk-web` | ✅ | The browser management console |
| `punktfunk-scripting` | ✅ | The plugin/script runner |
| `punktfunk-gamescope` | ✅ | The patched gamescope (HDR + cursor + real refresh) |
| `punktfunk-client` | ❌ | Desktop client — `libc6 >= 2.43`, see above |

## 1. GPU driver

On **NVIDIA**, the driver lives in Debian's `contrib` / `non-free` / `non-free-firmware`
components, which a default install does not enable. Debian 13 keeps its sources in the deb822
format, so add them there and refresh:

```sh
sudo sed -i 's/^Components: .*/Components: main contrib non-free non-free-firmware/' \
  /etc/apt/sources.list.d/debian.sources
sudo apt update
sudo apt install nvidia-driver firmware-misc-nonfree
```

Debian 13 ships driver 550, comfortably above the [535 floor](/docs/requirements).

Reboot, then confirm the driver and KMS modeset — Wayland on NVIDIA needs `modeset=1`:

```sh
nvidia-smi
cat /sys/module/nvidia_drm/parameters/modeset   # should print Y
```

If modeset is not `Y`:

```sh
echo 'options nvidia-drm modeset=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf
sudo update-initramfs -u && sudo reboot
```

> **Secure Boot:** with Secure Boot enabled, Debian's DKMS-built NVIDIA module must be signed and
> its key enrolled before it will load. If `nvidia-smi` can't talk to the driver, enrol the MOK
> (`sudo mokutil --import /var/lib/dkms/mok.pub`, reboot, choose **Enrol MOK**) or disable Secure
> Boot in firmware.

On **AMD/Intel** none of the NVIDIA steps apply. Encode runs on the Mesa stack: **Vulkan Video** for
HEVC and AV1 (`mesa-vulkan-drivers`), with **VAAPI** for H.264 and as the fallback —
`mesa-va-drivers` on AMD, `intel-media-va-driver` on Intel (the latter is in `non-free`).

## 2. Install the host (apt)

The registry is public — no auth needed, just trust its signing key:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://git.unom.io/api/packages/unom/debian/repository.key \
  | sudo tee /etc/apt/keyrings/punktfunk.asc >/dev/null

echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] https://git.unom.io/api/packages/unom/debian stable main" \
  | sudo tee /etc/apt/sources.list.d/punktfunk.list

sudo apt update
sudo apt install punktfunk-host
```

`punktfunk-host` `Recommends` the browser console (`punktfunk-web`), so apt pulls it in by default.
The NVIDIA driver is **not** a dependency — you installed it out of band in step 1. Later updates
are `sudo apt update && sudo apt upgrade`; restart the running host afterwards so it picks up the
new binary:

```sh
systemctl --user restart punktfunk-host
```

The `stable` component above is the stable channel. To track pre-release builds instead, see
[Release Channels](/docs/channels).

## 3. Grant gamepad access

Virtual gamepads inject through `/dev/uinput`, gated by the `input` group. Add yourself and re-login:

```sh
sudo usermod -aG input "$USER"     # re-login to apply
```

Also join `punktfunk` if you want the **virtual Steam Deck controller** (paddles, trackpads, gyro) —
it reaches games as a real USB device over usbip, which is what makes Steam Input adopt it. Join it
only on a machine you trust: writing the usbip `attach` file can materialise arbitrary emulated USB
hardware.

```sh
sudo usermod -aG punktfunk "$USER"  # re-login to apply
```

## 4. Check it installed

```sh
punktfunk-host --version           # the binary is on PATH
punktfunk-host detect-conflicts    # exits 1 if Sunshine/Apollo is also installed
```

Two hosts on one machine is the most common reason a clean install never streams — see
[Troubleshooting](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed).

## 5. Open the firewall (if you have one)

**Debian ships no firewall enabled by default**, so out of the box there is nothing to open. If you
run one, the package installs the openers:

```sh
# ufw:
sudo ufw allow punktfunk-native

# firewalld:
sudo firewall-cmd --reload                                        # load the installed definitions
sudo firewall-cmd --permanent --add-service=punktfunk-native
sudo firewall-cmd --reload
```

Add `punktfunk-gamestream` for Moonlight compat and `punktfunk-web` (TCP 47992) to reach the console
from another device. Full port lists are in
[`packaging/debian/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md#firewall).

## Cinnamon, Linux Mint and LMDE

**A Cinnamon desktop cannot host a virtual display, and no setting changes that.** Punktfunk gives
each client its own screen at that device's exact resolution by asking the compositor to create a
virtual output. Cinnamon's compositor, **Muffin**, has no such API: it forked from Mutter 3.36, and
its `org.cinnamon.Muffin.ScreenCast` interface offers only `RecordMonitor` and `RecordWindow` —
never the `RecordVirtual` that Mutter gained in 42. Its portal backend
(`xdg-desktop-portal-xapp`) implements no ScreenCast either, so the route that serves Sway and
Hyprland is closed too. This is upstream's to fix, not a Punktfunk setting.

What **does** work on a Mint or LMDE box is **gamescope**: the host starts its own headless
gamescope for each connecting client and runs the game inside it, so it needs no desktop compositor
at all. Your Cinnamon session keeps running untouched; the stream is the game, not the desktop.

```sh
sudo apt install punktfunk-gamescope
echo 'PUNKTFUNK_COMPOSITOR=gamescope' >> ~/.config/punktfunk/host.env
systemctl --user restart punktfunk-host
```

The pin is required: auto-detection reads the live session, finds Cinnamon, and stops with an error
rather than guessing. Set a game to launch with
[`PUNKTFUNK_GAMESCOPE_APP`](/docs/gamescope) or per-session launch commands, then see
[Steam / gamescope](/docs/gamescope) for the rest.

> **Install `punktfunk-gamescope`, not Debian's.** Debian ships **no** `gamescope` package at all,
> and the patched build is what gives the stream HDR, a visible cursor, and the client's real
> refresh rate instead of a hardcoded 60 Hz.

If you want to stream the **desktop** from a Mint box, the answer today is to run a KDE, GNOME, Sway
or Hyprland session instead — all four expose a virtual-output API.

## Configure your desktop

How the host creates its virtual display and injects input depends on your desktop, not your distro:

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Hyprland](/docs/hyprland)
- [Sway / wlroots](/docs/sway)

Then bring up [The Web Console](/docs/web-console) to arm pairing and connect your first
[client](/docs/clients). To run the host at boot — including fully **headless** — see
[Running as a Service](/docs/running-as-a-service).

## Next steps

- **Keep it current** — [Updating the Host](/docs/updating).
- **Remove it again** — [Uninstalling](/docs/uninstall).
- **Something not working?** — [Troubleshooting](/docs/troubleshooting).
- **Build from source** (Debian 12, or tracking `main`) — the
  [Ubuntu appendix](/docs/ubuntu#appendix--build-from-source) applies unchanged; Debian 13's
  `libavcodec-dev` is new enough to build against.
