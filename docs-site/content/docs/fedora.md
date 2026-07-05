---
title: Fedora
description: Install the punktfunk host on Fedora from the RPM registry.
---

Install a punktfunk host on **Fedora** from the self-hosted RPM registry. The host installs as an
RPM-managed systemd **`--user`** service and updates with `dnf upgrade` like the rest of your
system — no building required. It works with either **KDE Plasma** or **GNOME**; the
desktop-specific setup (which compositor captures, headless sessions, quirks) lives on the
[desktop configure pages](#3-configure-your-desktop). Host encode is **NVENC on NVIDIA** and **VAAPI on
AMD/Intel** (`PUNKTFUNK_ENCODER=auto` picks per GPU).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

Install is two parts: **GPU driver** → **host RPM**. Then point the host at your desktop from the
[desktop configure pages](#3-configure-your-desktop).

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

**AMD / Intel (VAAPI).** No akmod needed — the Mesa stack provides the VAAPI encoder. Install the
freeworld VAAPI drivers for full codec support (`mesa-va-drivers-freeworld` for AMD from RPM Fusion,
`intel-media-driver` for Intel); on a desktop these are usually already present. The host auto-picks
VAAPI on these GPUs.

## 2. Install the host (RPM)

The host is published to the self-hosted Gitea RPM registry, in a per-Fedora-release group (an RPM
is soname-coupled to its base, so Fedora 44 has its own `fedora-44` group). Add the repo and
install:

```sh
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[punktfunk]
name=punktfunk
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

Updates later are just `sudo dnf upgrade punktfunk`. The package ships the systemd user units, the
udev rule, the UDP socket-buffer sysctl tuning, and example configs.

> No matching `fedora-NN` group for your release yet? Build one with the same toolchain CI uses —
> `docker build --build-arg FEDORA_VERSION=NN -f ci/fedora-rpm.Dockerfile -t pf-rpm ci` then run
> `packaging/rpm/build-rpm.sh` inside it — or build from source (appendix below).

## 3. Configure your desktop

How the host creates its virtual display and injects input depends on your desktop, not your distro.
Continue on the page for the desktop you run — it covers your `host.env`, any compositor quirks, and
starting the host:

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Sway / wlroots](/docs/sway)

Enable the browser management console (status, paired devices, arm pairing) — see
[Web Console](/docs/web-console).

For a headless KWin appliance that streams at boot with no graphical login, see
[KDE → Headless session](/docs/kde#headless-session).

Full config reference: [Configuration](/docs/configuration). Service model:
[Running as a Service](/docs/running-as-a-service).

## 4. Connect a client

From any [client](/docs/clients), `--discover` finds the host on the LAN. On first connect, complete
the **PIN pairing** — arm it from the host's [web console](/docs/web-console#arm-pairing), which
displays a 4-digit PIN to type into the client. See [Clients](/docs/clients) and
[Pairing](/docs/pairing).

## Appendix — build from source

If there's no RPM for your Fedora release and you don't want to build one, compile the host directly
(no clean updates / no packaged units — you wire those up by hand):

```sh
sudo dnf install gcc gcc-c++ make cmake clang clang-devel nasm git \
  pipewire-devel wayland-devel wayland-protocols-devel libxkbcommon-devel opus-devel \
  libdrm-devel mesa-libgbm-devel mesa-libEGL-devel mesa-libGLES-devel libva-devel \
  ffmpeg-devel libei-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release -p punktfunk-host
```

Then write `~/.config/punktfunk/host.env` (as in `/usr/share/punktfunk/host.env.kde`, but the host
binary is `target/release/punktfunk-host`) and run it inside your desktop session — for a headless
KWin appliance see [KDE → Headless session](/docs/kde#headless-session).
