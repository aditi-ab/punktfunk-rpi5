---
title: Fedora
description: Install the Punktfunk host on Fedora from the RPM registry.
---

Install a Punktfunk host on **Fedora** from the self-hosted RPM registry. The host installs as an
RPM-managed systemd **`--user`** service and updates with `dnf upgrade` like the rest of your
system — no building required. It works with either **KDE Plasma** or **GNOME**; the
desktop-specific setup (which compositor captures, headless sessions, quirks) lives on the
[desktop configure pages](#5-configure-your-desktop). Host encode is **NVENC on NVIDIA**; on
**AMD/Intel** HEVC and AV1 go through **Vulkan Video**, with **VAAPI** for H.264 and as the fallback
(`PUNKTFUNK_ENCODER=auto` picks per GPU).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

Install is two parts: **GPU driver** → **host RPM**. Then open the firewall and point the host at
your desktop from the [desktop configure pages](#5-configure-your-desktop).

## 1. NVIDIA driver (RPM Fusion akmod)

Enable RPM Fusion (free + nonfree), then install the akmod driver + CUDA. RPM Fusion's nonfree
NVIDIA repo is sometimes pre-enabled on some spins; the full free/nonfree repos below are still
needed (they carry the NVENC ffmpeg in the next step).

```sh
sudo dnf install \
  https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
  https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm
sudo dnf install akmod-nvidia xorg-x11-drv-nvidia-cuda
```

**NVENC ffmpeg.** Fedora ships `ffmpeg-free`, which is built **without** NVENC — the host can't
encode with it. Swap to RPM Fusion's ffmpeg:

```sh
sudo dnf install --allowerasing ffmpeg ffmpeg-libs
ffmpeg -hide_banner -encoders | grep nvenc   # expect hevc_nvenc / av1_nvenc / h264_nvenc
```

**Secure Boot.** If `mokutil --sb-state` says *enabled*, the akmod module is signed with a
locally-generated key that must be enrolled once:

```sh
sudo akmods --force                                              # build + sign the module
sudo mokutil --import /etc/pki/akmods/certs/public_key.der       # set a one-time password
sudo reboot
```

On the next boot a blue **MOK Manager** screen appears **on the machine's console** (not over
SSH): *Enroll MOK → Continue → Yes → (the password) → Reboot*. Then verify:

```sh
nvidia-smi                              # driver loads
ffmpeg -hide_banner -encoders | grep nvenc
```

(Or disable Secure Boot in firmware to skip the MOK step — fine for a dedicated test box.)

**AMD / Intel.** No akmod needed — the Mesa stack carries both encode paths. HEVC and AV1 go through
**Vulkan Video** by default (the Mesa Vulkan driver, present on any normal Fedora desktop), and
**VAAPI** is the H.264 path and the fallback. Install the freeworld VAAPI drivers for full codec
support (`mesa-va-drivers-freeworld` for AMD from RPM Fusion, `intel-media-driver` for Intel); on a
desktop these are usually already present.

## 2. Install the host (RPM)

The host is published to the self-hosted Gitea RPM registry, in a per-release group (an RPM is
soname-coupled to its base, so each Fedora release gets its own group). Pick the one matching your
release — `rpm -E %fedora` prints the number you're on:

- **Fedora 44** → `fedora-44`
- **Fedora 43** → `bazzite` — that group is a plain Fedora 43 build of the same `punktfunk` package,
  so it's the right one for a regular Fedora 43 box too

Put your group in the `baseurl` below, then add the repo and install:

```sh
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[punktfunk]
name=punktfunk
# The group for your release: fedora-44 on Fedora 44, bazzite on Fedora 43.
baseurl=https://git.unom.io/api/packages/unom/rpm/fedora-44
enabled=1
# Packages are GPG-signed (gpgcheck=1) AND the repo metadata is Gitea-signed (repo_gpgcheck=1).
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
       https://git.unom.io/api/packages/unom/generic/punktfunk-keys/1/RPM-GPG-KEY-punktfunk
REPO

sudo dnf install punktfunk
sudo usermod -aG input "$USER"     # /dev/uinput access for virtual gamepads (re-login to apply)
```

Also join `punktfunk` if **either** applies — you want the **virtual Steam Deck controller**
(paddles, trackpads, gyro — it reaches games as a real USB pad, which is why Steam Input adopts
it), or this box autologins into Steam **Gaming Mode** (Nobara and friends) and you want the host
to take that session over at the client's resolution:

```sh
sudo usermod -aG punktfunk "$USER"  # usbip/vhci + display-manager takeover (re-login to apply)
```

That is a second group on purpose: it grants write access to the usbip `attach` file, which
materialises an arbitrary emulated USB device, so it stays off the `input` group everyone is
routinely told to join. Join it only on a machine you trust. Skip it on a plain desktop host and
the pad simply arrives as an ordinary Xbox 360 controller; skip it on a Gaming Mode box and the
takeover silently degrades to mirroring the box's own screen — see
[gamescope](/docs/gamescope#nobara-and-other-autologin-display-managers).

Updates later are just `sudo dnf upgrade punktfunk`, followed by
`systemctl --user restart punktfunk-host` so the running host picks up the new binary. The package
ships the systemd user units, the udev rule, the UDP socket-buffer sysctl tuning, and example
configs.

The group you picked above is the **stable** channel. For the latest `main` build, point `baseurl` at
`fedora-44-canary` (or `bazzite-canary`) instead — see [Release Channels](/docs/channels). Updating
in general, including the opt-in one-click button in the web console, is covered in
[Updating the Host](/docs/updating).

> `fedora-44` and `bazzite` are the only stable groups published, so on Fedora 42 or older — or on a
> release newer than 44 — there's nothing matching yet. Build one with the same toolchain CI uses —
> `docker build --build-arg FEDORA_VERSION=NN -f ci/fedora-rpm.Dockerfile -t pf-rpm ci` then run
> `packaging/rpm/build-rpm.sh` inside it — or build from source (appendix below).

## 3. Check it installed

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

## 4. Open the firewall

Fedora runs **firewalld** by default and the package never edits your firewall, so the host stays
unreachable until you allow it. The RPM installs the service definitions — enable them once.

The packaged unit runs `serve --gamestream` — the RPM installs it as it ships and only rewrites the
binary path — so a host you enabled with `systemctl --user enable --now punktfunk-host` serves
**both** the native `punktfunk/1` plane and stock [Moonlight](/docs/moonlight) clients, and needs
**both** services:

```sh
sudo firewall-cmd --reload                                            # load the installed definitions
sudo firewall-cmd --permanent --add-service=punktfunk-native
sudo firewall-cmd --permanent --add-service=punktfunk-gamestream
sudo firewall-cmd --reload
```

`punktfunk-native` opens UDP 9777 (QUIC control), UDP 5353 (mDNS discovery) and TCP 47990 (the
mgmt/library API — HTTPS + mTLS, read-only off loopback). `punktfunk-gamestream` opens the fixed
Moonlight ports — TCP 47984, 47989 and 48010, UDP 47998–48000 — plus the same mDNS. The media
**data plane** uses an ephemeral UDP port the client opens with a hole-punch, so there is nothing
fixed to open for video.

Switched the host to **native-only** — dropped `--gamestream` with a
`systemctl --user edit punktfunk-host` drop-in, or you run `punktfunk-host serve` by hand? Then add
`punktfunk-native` alone and leave `punktfunk-gamestream` out. `systemctl --user cat punktfunk-host`
shows which one yours is.

And if you want the web console reachable from another device, open **TCP 47992**:

```sh
sudo firewall-cmd --permanent --add-service=punktfunk-web && sudo firewall-cmd --reload
```

## 5. Configure your desktop

How the host creates its virtual display and injects input depends on your desktop, not your distro.
Continue on the page for the desktop you run — it covers your `host.env`, any compositor quirks, and
starting the host:

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Hyprland](/docs/hyprland)
- [Sway / wlroots](/docs/sway)

Enable the browser management console (status, paired devices, arm pairing) — see
[Web Console](/docs/web-console).

For a headless KWin appliance that streams at boot with no graphical login, see
[KDE → Headless session](/docs/kde#headless-session).

Full config reference: [Configuration](/docs/configuration). Service model:
[Running as a Service](/docs/running-as-a-service).

## 6. Connect a client

From any [client](/docs/clients), `--discover` finds the host on the LAN. On first connect, complete
the **PIN pairing** — arm it from the host's [web console](/docs/web-console#arm-pairing), which
displays a 4-digit PIN to type into the client. See [Clients](/docs/clients) and
[Pairing](/docs/pairing).

## Next steps

- **Keep it current** — [Updating the Host](/docs/updating).
- **Remove it again** — [Uninstalling](/docs/uninstall).
- **Something not working?** — [Troubleshooting](/docs/troubleshooting).

## Appendix — build from source

If there's no RPM for your Fedora release and you don't want to build one, compile the host directly
(no clean updates / no packaged units — you wire those up by hand):

```sh
sudo dnf install gcc gcc-c++ make cmake clang clang-devel nasm git pkgconf-pkg-config \
  pipewire-devel wayland-devel wayland-protocols-devel libxkbcommon-devel opus-devel \
  libdrm-devel mesa-libgbm-devel mesa-libGL-devel mesa-libEGL-devel mesa-libGLES-devel libva-devel \
  ffmpeg-devel libei-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host
```

`mesa-libGL-devel` isn't optional — the zero-copy GPU path links `libGL`, and without it the build
fails at the link step with `cannot find -lGL`. The two `--features` are what the packaged builds
use: leave them off and the host has no direct NVENC (NVIDIA) and no Vulkan Video encode
(AMD/Intel), and quietly falls back to the slower libav backends.

Then write `~/.config/punktfunk/host.env` (as in `/usr/share/punktfunk/host.env.kde`, but the host
binary is `target/release/punktfunk-host`) and run it inside your desktop session — for a headless
KWin appliance see [KDE → Headless session](/docs/kde#headless-session).
