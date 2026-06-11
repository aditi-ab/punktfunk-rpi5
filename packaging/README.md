# Packaging punktfunk for Fedora / Bazzite

The punktfunk host is Linux-only and links system FFmpeg (NVENC), PipeWire, Opus and
the NVIDIA driver. This directory packages it for the **Fedora Atomic / Bazzite** world
(rpm-ostree + bootc), where most of those deps are already present.

> 👉 **End-to-end Bazzite setup walkthrough** (install → udev/group → `host.env` → service →
> firewall → verify → troubleshooting): [`bazzite/README.md`](bazzite/README.md). This file is the
> higher-level packaging rationale.

```
packaging/
  rpm/punktfunk.spec      # the RPM (builds punktfunk-host from source with cargo)
  bazzite/host.env        # gamescope-default config for a Bazzite appliance
  bazzite/README.md       # step-by-step Bazzite setup guide
  bootc/Containerfile     # bake punktfunk into a Bazzite-based atomic image
  copr/                   # COPR build-from-SCM settings
```

## What's needed beyond base Fedora

| Dependency | Where it comes from |
|---|---|
| `ffmpeg-libs` with **NVENC** | **RPM Fusion nonfree** (`ffmpeg`, not `ffmpeg-free`) |
| NVIDIA driver (`libnvidia-encode`, `libEGL_nvidia`) | Bazzite **-nvidia** images ship it; plain Fedora: `akmod-nvidia` + `xorg-x11-drv-nvidia-cuda` |
| gamescope, PipeWire, wireplumber | **Bazzite ships these**; plain Fedora: `dnf install gamescope pipewire wireplumber` |
| `opus`, `libei` | Fedora base / updates |

On **Bazzite** the only genuinely new runtime bits are `ffmpeg-libs` (RPM Fusion) + `opus` +
`libei` — the rest of the stack is already there. The default backend is **gamescope**
(`packaging/bazzite/host.env`), which the host spawns headless per session — no desktop login.

## Option A — COPR (per-host, `rpm-ostree install`)

1. Create a COPR project, enable **build-from-SCM** pointing at this repo, spec path
   `packaging/rpm/punktfunk.spec` (see `copr/README.md`). Under *External Repositories* add
   RPM Fusion nonfree so `ffmpeg-devel` resolves at build time.
2. On the Bazzite host:
   ```sh
   # RPM Fusion (for the NVENC ffmpeg) — usually already enabled on Bazzite
   rpm-ostree install \
     https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
     https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm
   # enable the COPR + install punktfunk
   sudo wget -O /etc/yum.repos.d/_copr_punktfunk.repo \
     https://copr.fedorainfracloud.org/coprs/enricobuehler/punktfunk/repo/fedora-$(rpm -E %fedora)/
   rpm-ostree install punktfunk
   systemctl reboot
   ```

## Option B — bootc (image-based, atomic)

Layer punktfunk into a Bazzite image once, then rebase any number of hosts onto it — no
per-host drift. See `bootc/Containerfile`:
```sh
podman build -t ghcr.io/<you>/bazzite-punktfunk -f packaging/bootc/Containerfile .
podman push  ghcr.io/<you>/bazzite-punktfunk
# on the target:
sudo bootc switch ghcr.io/<you>/bazzite-punktfunk && systemctl reboot
```

## First-run setup (either option)

```sh
ujust add-user-to-input-group           # virtual gamepads need /dev/uinput (then re-login).
                                        # On Bazzite use ujust, NOT `usermod -aG input` (atomic OS — it won't stick).
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env   # edit (gamescope app, etc.)
systemctl --user enable --now punktfunk-host
```

Pair a stock Moonlight client (mDNS-discovered), or connect the native punktfunk/1 client.

## Why not Flatpak?

The host needs unsandboxed access the zero-copy NVENC path, `/dev/uinput`, the PipeWire
graph and the compositor's privileged protocols — a Flatpak sandbox fights all of these.
An RPM (or the bootc layer) installs into the host system where those just work.

## Building the SRPM/RPM locally (Fedora only)

```sh
git archive --format=tar.gz --prefix=punktfunk-0.0.1/ -o ~/rpmbuild/SOURCES/punktfunk-0.0.1.tar.gz HEAD
rpmbuild -ba packaging/rpm/punktfunk.spec     # needs the BuildRequires from the spec
```
(Not buildable on Debian/Ubuntu — use a Fedora toolbox/container or COPR.)
