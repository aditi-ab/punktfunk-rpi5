---
title: Ubuntu
description: Install the Punktfunk host on Ubuntu with apt.
---

Install a Punktfunk host on **Ubuntu** (Desktop or Server) from the apt registry. This
page covers the distro-level setup — GPU driver, package, gamepad access. It works with either GNOME
or KDE; how the host creates its virtual display and injects input is desktop-specific, so pick your
desktop on the [configure pages](#configure-your-desktop) afterward rather than here.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

> **Which releases.** There is one universal host package. It bundles FFmpeg 8 and needs **glibc 2.39
> or newer** — that's **Ubuntu 24.04 LTS through 26.04**, the range it's built and tested for. Check
> yours with `ldd --version`; below 2.39 the install fails and you build from source instead
> ([appendix](#appendix--build-from-source)). The desktop *client* package is built on Ubuntu 26.04
> and needs GTK4 ≥ 4.20 and SDL3, so it installs on **26.04 or newer** only — the host has no such
> limit.

> **Debian isn't a supported target.** The packages are built on Ubuntu images and their dependencies
> are resolved against Ubuntu's package names, and nothing in CI builds or tests on Debian. Debian 12
> (bookworm) is below the glibc floor and cannot install them at all. A newer Debian may work, but
> it's untested — build from source ([appendix](#appendix--build-from-source)) if you want to try.
> The `debian` in the repository URL below is the *package format*, not a supported distro.

## 1. GPU driver

On **NVIDIA**, install the recommended driver.

```sh
sudo ubuntu-drivers install      # or: sudo apt install nvidia-driver-<version>
```

Then make sure the **GL/EGL userspace** is present — Wayland compositors on NVIDIA need it, and the
base driver package doesn't always pull it in. Install the `libnvidia-gl` package matching your
driver version:

```sh
sudo apt install libnvidia-gl-<version>   # e.g. libnvidia-gl-550
```

Reboot, then confirm the driver and KMS modeset:

```sh
nvidia-smi
cat /sys/module/nvidia_drm/parameters/modeset   # should print Y
```

If modeset is not `Y`:

```sh
echo 'options nvidia-drm modeset=1' | sudo tee /etc/modprobe.d/nvidia-drm.conf
sudo update-initramfs -u && sudo reboot
```

> **Secure Boot:** on a machine with Secure Boot **enabled**, the NVIDIA kernel module won't load
> until you enrol its signing key. If `nvidia-smi` reports it can't talk to the driver, run
> `sudo mokutil --import /var/lib/shim-signed/mok/MOK.der` (set a one-time password), reboot, and
> choose **Enrol MOK** at the blue screen. Or disable Secure Boot in firmware.

On **AMD/Intel** none of the NVIDIA steps apply. Encode runs on the Mesa stack: **Vulkan Video** for
HEVC and AV1 (`mesa-vulkan-drivers`), with **VAAPI** for H.264 and as the fallback —
`mesa-va-drivers` on AMD, `intel-media-va-driver` on Intel, both of which the `punktfunk-host`
package recommends, so apt pulls the right one in with the host. Install `mesa-vulkan-drivers` too
if it isn't already on the box.

## 2. Install the host (apt)

`punktfunk-host` is published as a `.deb` to the public Gitea apt registry, so the box installs and
updates with plain `apt`. The registry is public — no auth needed, just trust its signing key:

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
The desktop *client* (`punktfunk-client`) is a separate package for the machine you stream *to* — not
installed on a host. The NVIDIA driver is **not** a dependency — you installed it out of band in
step 1. Later updates are just `sudo apt update && sudo apt upgrade`, or, to move only Punktfunk,
`sudo apt update && sudo apt install --only-upgrade punktfunk-host`. Either way, restart the running
host afterwards so it picks up the new binary:

```sh
systemctl --user restart punktfunk-host
```

A plain `apt upgrade` also moves `punktfunk-web` when a new console is out — restart that one too
(`systemctl --user restart punktfunk-web`) if you run it.

[Updating the Host](/docs/updating) covers the rest — the web console's **Updates** card, and the
opt-in one-click update button whose `punktfunk-update` group this package creates.

The `stable` component above is the stable channel. To track pre-release builds instead, see
[Release Channels](/docs/channels).

## 3. Grant gamepad access

Virtual gamepads inject through `/dev/uinput`, which is gated by the `input` group. Add yourself and
re-login so the new group membership takes effect:

```sh
sudo usermod -aG input "$USER"     # re-login to apply
```

Also join `punktfunk` if **either** applies — you want the **virtual Steam Deck controller**
(paddles, trackpads, gyro), or this box autologins into Steam **Gaming Mode** and you want the host
to take that session over at the client's resolution. That pad reaches games as a real USB device
over usbip — which is what makes Steam Input adopt it — and the same group authorizes the helper
that stops the display manager for a takeover. It is deliberately separate from `input`, because
writing the usbip `attach` file can materialise arbitrary emulated USB hardware:

```sh
sudo usermod -aG punktfunk "$USER"  # re-login to apply
```

Join it only on a machine you trust. On a plain desktop host, skipping it costs you nothing but
that one pad type; on a Gaming Mode box the takeover silently degrades to mirroring the box's own
screen — see [gamescope](/docs/gamescope#nobara-and-other-autologin-display-managers).

## 4. Check it installed

Before moving on, confirm the binary is there and nothing else is competing for the same job:

```sh
punktfunk-host --version           # the binary is on PATH
punktfunk-host detect-conflicts    # exits 1 if Sunshine/Apollo is also installed
```

If `detect-conflicts` reports another streaming host, remove it before going further — two hosts on
one machine is the most common reason a clean install never streams. See
[Troubleshooting → another streaming host is installed](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed).

Once you've enabled the service on your desktop page below, these are how you watch it:

```sh
systemctl --user status punktfunk-host      # active
journalctl --user -u punktfunk-host -f      # watch a client connect
```

## 5. Open the firewall (if you have one)

Ubuntu installs `ufw` but leaves it **inactive**, so out of the box
there is nothing to open. If you did enable one — common on Ubuntu Server — the package ships the
openers for both, because a package never edits your firewall itself.

The packaged unit runs the **secure native-only host** (`serve`, no GameStream), so a host you
enabled with `systemctl --user enable --now punktfunk-host` needs only the native opener:

```sh
# ufw:
sudo ufw allow punktfunk-native

# firewalld:
sudo firewall-cmd --reload                                        # load the installed definitions
sudo firewall-cmd --permanent --add-service=punktfunk-native
sudo firewall-cmd --reload
```

Enabled **GameStream/Moonlight compat** (`PUNKTFUNK_GAMESTREAM=1` in `host.env` — see
[What the unit starts](/docs/running-as-a-service#what-the-unit-starts)), or you pass
`--gamestream` by hand? Then also open its service:

```sh
sudo ufw allow punktfunk-gamestream                               # ufw
sudo firewall-cmd --permanent --add-service=punktfunk-gamestream && sudo firewall-cmd --reload
```

`punktfunk-native` opens UDP 9777 (QUIC control), UDP 5353 (mDNS discovery) and TCP 47990 (the
mgmt/library API — HTTPS + mTLS, read-only off loopback). `punktfunk-gamestream` opens the fixed
Moonlight ports — TCP 47984, 47989 and 48010, UDP 47998–48000 — plus the same mDNS. The media
**data plane** uses an ephemeral UDP port the client opens with a hole-punch, so there is nothing
fixed to open for video.

Running the web console (`punktfunk-web`) and want to reach it from another device? Open it too —
that's **TCP 47992**:

```sh
sudo ufw allow punktfunk-web                                                             # ufw
sudo firewall-cmd --permanent --add-service=punktfunk-web && sudo firewall-cmd --reload  # firewalld
```

Full port lists are in
[`packaging/debian/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md#firewall).

## Configure your desktop

How the host creates its virtual display and injects input depends on your desktop, not your distro.
Continue on the page for the desktop you run — it covers your `host.env`, any compositor quirks, and
starting the host:

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

## Appendix — build from source

If your release is older than the supported range above, or you want to track `main` directly,
compile the host yourself (no clean updates / no packaged units — you wire those up by hand).

Install the build toolchain and runtime libraries. The packaged host is built against **FFmpeg 8**:
Ubuntu 26.04's `libavcodec-dev` is FFmpeg 8, but 24.04's is FFmpeg 6.1 — on 24.04 build FFmpeg 8
yourself first (that's what `ci/rust-ci-noble.Dockerfile` does, and the shipped `.deb` bundles the
result) or stick with the packaged host.

```sh
sudo apt install build-essential pkg-config cmake clang libclang-dev nasm git curl \
  pipewire pipewire-pulse wireplumber libpipewire-0.3-dev libspa-0.2-dev \
  libwayland-dev wayland-protocols libxkbcommon-dev libopus-dev \
  libdrm-dev libgbm-dev libgl-dev libegl-dev libgles-dev mesa-common-dev libva-dev \
  ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libnvidia-egl-wayland1 libnvidia-egl-gbm1 libei-dev
```

Install Rust if you don't have it, then build:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host
```

Those two features are what the packaged build uses — without them the host has no direct NVENC
(NVIDIA) and no Vulkan Video encode (AMD/Intel), and falls back to the slower libav backends.

The host binary lands at `target/release/punktfunk-host`. Configure your desktop as above, then run
it from inside your session:

```sh
cargo run --release --locked \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host -- serve --gamestream
```

(The native plane is always on; `--gamestream` adds the Moonlight-compat surface — trusted LAN only.
Drop it for a secure native-only host.)
