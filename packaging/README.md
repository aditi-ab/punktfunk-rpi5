# Packaging punktfunk for Fedora / Bazzite

The punktfunk host links system FFmpeg (NVENC on NVIDIA, VAAPI on AMD/Intel, with a GPU-less
software-H.264 fallback), PipeWire and Opus. This page covers packaging it for the
**Fedora Atomic / Bazzite** world (rpm-ostree + bootc), where most of those deps are already
present; the NVIDIA-specific notes below apply to the NVENC path.

> 👉 **Ubuntu/Debian hosts** install via `apt` from Gitea's package registry — see
> [`debian/README.md`](debian/README.md) (`apt update && apt upgrade` for new builds).

> 👉 **End-to-end Bazzite setup walkthrough** (install → udev/group → `host.env` → service →
> firewall → verify): [docs.punktfunk.unom.io/docs/bazzite](https://docs.punktfunk.unom.io/docs/bazzite).
> [`bazzite/README.md`](bazzite/README.md) keeps the install-path/packaging view (sysext vs bootc
> vs layering); this file is the higher-level packaging rationale.

```
packaging/
  rpm/punktfunk.spec      # the RPM (builds punktfunk-host from source with cargo)
  bazzite/host.env        # gamescope-default config for a Bazzite appliance
  bazzite/README.md       # step-by-step Bazzite setup guide
  bazzite/*sysext*.sh     # the no-layering path: build/install/publish the systemd-sysext
  bootc/Containerfile     # bake punktfunk into a Bazzite-based atomic image
  copr/                   # COPR build-from-SCM settings
```

The other packaging targets have their own READMEs: [`debian/`](debian/README.md) (apt),
[`arch/`](arch/README.md) (pacman binary repo + PKGBUILD + SteamOS sysext),
[`flatpak/`](flatpak/README.md) (the client), [`windows/`](windows/README.md) (host installer +
drivers), plus `kde/` and `linux/` helpers. **NixOS / Nix** users get a flake (`flake.nix` at the
repo root) with reproducible `punktfunk-host`, `-client`, `-web`, `-scripting` and `-gamescope`
packages plus a `services.punktfunk` NixOS module — see [`nix/README.md`](nix/README.md).

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

## Option A — systemd-sysext (recommended; no layering, no reboot)

On Bazzite / Fedora Atomic the recommended install is the **systemd-sysext** image — rpm-ostree
layering is a last resort per the Bazzite docs (it slows every OS update and can block upgrades),
while a sysext overlays `/usr` at runtime, survives OS updates, and updates in one command with
no reboot. CI wraps the same RPMs below into the image, so content and channels are identical.

```sh
curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
sudo bash punktfunk-sysext.sh install     # then: sudo punktfunk-sysext update | status | remove
```

Full walkthrough (incl. the F43→F44 rebase behavior and migration off layering):
[`bazzite/README.md`](bazzite/README.md).

## Option B — Gitea RPM registry (per-host, `rpm-ostree` layering)

The host's RPM is published to **unom's self-hosted Gitea RPM registry** (CI builds it on every
push), mirroring the [Debian/apt](debian/README.md) setup. Add one repo file, install, and track
updates with `rpm-ostree upgrade` — no COPR account needed. Full guide: [`rpm/README.md`](rpm/README.md).

```sh
# GPG-signed pkgs + Gitea-signed metadata → gpgcheck=1, repo_gpgcheck=1 (see rpm/README.md)
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[gitea-unom-bazzite]
name=punktfunk (unom, Bazzite)
baseurl=https://git.unom.io/api/packages/unom/rpm/bazzite
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
       https://git.unom.io/api/packages/unom/generic/punktfunk-keys/1/RPM-GPG-KEY-punktfunk
REPO
rpm-ostree install punktfunk && systemctl reboot
# updates:  rpm-ostree upgrade && systemctl reboot
```

## Option C — COPR (per-host, `rpm-ostree install`)

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

## Option D — bootc (image-based, atomic)

Layer punktfunk into a Bazzite image once, then rebase any number of hosts onto it — no
per-host drift. See `bootc/Containerfile`:
```sh
podman build -t ghcr.io/<you>/bazzite-punktfunk -f packaging/bootc/Containerfile .
podman push  ghcr.io/<you>/bazzite-punktfunk
# on the target:
sudo bootc switch ghcr.io/<you>/bazzite-punktfunk && systemctl reboot
```

## First-run setup (all options)

```sh
ujust add-user-to-input-group           # virtual gamepads need /dev/uinput (then re-login).
                                        # On Bazzite use ujust, NOT `usermod -aG input` (atomic OS — it won't stick).
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env   # edit (gamescope app, etc.)
systemctl --user enable --now punktfunk-host

# Management web console (pairing + status) — pulled in by default (the host RPM Recommends it;
# `--no-install-recommends` / headless-only boxes can skip it). Enable it and read the login password:
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'   # then open https://<host-ip>:47992
```

Pair a stock Moonlight client (mDNS-discovered), or connect the native punktfunk/1 client — via the
web console at `https://<host-ip>:47992` or directly.

> ⚠️ **COPR caveat:** COPR's mock chroot has no `bun`, so a COPR build produces only
> `punktfunk` + `punktfunk-client` — **not** `punktfunk-web`. For the console on a COPR/bootc host,
> install from the **Gitea RPM registry** (Option B — it carries `punktfunk-web`; the sysext image
> includes it too), which is also why `bootc/Containerfile` installs from there rather than COPR.

## Why not Flatpak (for the HOST)?

The host needs unsandboxed access the zero-copy NVENC path, `/dev/uinput`, the PipeWire
graph and the compositor's privileged protocols — a Flatpak sandbox fights all of these.
An RPM (or the bootc layer) installs into the host system where those just work.

> 👉 The **client** is a different story — it IS shipped as a Flatpak (the only viable
> Steam Deck install path: SteamOS `/usr` is read-only and lacks `libadwaita`/`libSDL3`). See
> [`flatpak/README.md`](flatpak/README.md). The client sandbox only needs the GPU render node,
> Wayland, PipeWire audio, the network and hidraw — all expressible as finish-args.

## Building the SRPM/RPM locally (Fedora only)

```sh
git archive --format=tar.gz --prefix=punktfunk-0.3.0/ -o ~/rpmbuild/SOURCES/punktfunk-0.3.0.tar.gz HEAD
rpmbuild -ba packaging/rpm/punktfunk.spec     # needs the BuildRequires from the spec
# (0.3.0 = the spec's default %{pf_version}; the prefix and tarball name must match it)
```
(Not buildable on Debian/Ubuntu — use a Fedora toolbox/container or COPR.)
