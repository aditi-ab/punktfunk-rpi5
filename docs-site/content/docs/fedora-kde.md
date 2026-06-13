---
title: Fedora — KDE Plasma
description: Reproducible punktfunk host setup on Fedora KDE (KWin) via the RPM.
---

Set up a punktfunk host on **Fedora KDE** (the KDE Plasma spin). The host runs as an RPM-managed
systemd service and uses KWin to create per-client virtual displays, captured zero-copy
(dmabuf → CUDA → NVENC) on NVIDIA.

> Validated live on **Fedora 44 KDE Plasma** with an RTX 4090: KWin virtual output + full
> zero-copy capture. Everything below is the reproducible flow — paste it on a fresh box.

The setup has three parts: **NVIDIA driver** → **host RPM** → **KWin streaming session**.

## 1. NVIDIA driver (RPM Fusion akmod)

Enable RPM Fusion (free + nonfree), then install the akmod driver + CUDA. RPM Fusion's nonfree
NVIDIA repo is sometimes pre-enabled on the KDE spin; the full free/nonfree repos below are still
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
# Packages are unsigned; the repo METADATA is Gitea-signed — verify that, skip per-package sig.
gpgcheck=0
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
REPO

sudo dnf install punktfunk
sudo usermod -aG input "$USER"     # /dev/uinput access for virtual gamepads (re-login to apply)
```

Updates later are just `sudo dnf upgrade punktfunk`. The package ships the systemd user units, the
udev rule, the UDP socket-buffer sysctl tuning, and example configs.

> No matching `fedora-NN` group for your release yet? Build one with the same toolchain CI uses —
> `docker build --build-arg FEDORA_VERSION=NN -f ci/fedora-rpm.Dockerfile -t pf-rpm ci` then run
> `packaging/rpm/build-rpm.sh` inside it — or build from source (appendix below).

## 3. KWin streaming session

KWin's virtual-output capture uses its **privileged** `zkde_screencast` protocol, which an
*interactive* Plasma session will not hand to an external client. So the host streams from a
**dedicated headless KWin session** (`kwin --virtual` launched with
`KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`) — shipped as `punktfunk-kde-session.service`. This also
makes the box a self-contained appliance: it streams at boot with no graphical login.

```sh
# KWin appliance config (ships with the package):
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.kde ~/.config/punktfunk/host.env

# Start the headless KWin session + the host, and start user units at boot without a login:
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"
```

Check it came up:

```sh
systemctl --user status punktfunk-host          # active
journalctl --user -u punktfunk-host -f          # watch a client connect
```

The host now listens on `9777` (native punktfunk/1) + the GameStream ports, and advertises over
mDNS. It requires **PIN pairing** by default (secure on a LAN); pair once from your client.

## 4. Connect a client

From any [client](/docs/clients) — `punktfunk-client --discover` finds the host on the LAN. On
first connect, complete the PIN pairing (arm it from the host's web console / mgmt API, or run the
host with `--allow-pairing`). See [Clients](/docs/clients) and
[Running as a Service](/docs/running-as-a-service).

## Appendix — build from source

If there's no RPM for your Fedora release and you don't want to build one, compile the host
directly (no clean updates / no packaged units — you wire those up by hand):

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
binary is `target/release/punktfunk-host`) and run it inside the KWin session from step 3.
