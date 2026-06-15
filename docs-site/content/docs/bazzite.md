---
title: Bazzite — gamescope
description: Set up a punktfunk host on Bazzite, streaming a Steam/gamescope session at your client's mode.
---

[Bazzite](https://bazzite.gg/) already ships everything a punktfunk host needs — the NVIDIA driver,
NVENC, PipeWire, and **gamescope**. So a Bazzite host is the most "appliance-like" setup: the host
launches its own gamescope session at the **client's** resolution and refresh, so your games run at
the mode of the device you're streaming to, not the TV the box is plugged into.

> This is ideal for a dedicated game-streaming box. For a general desktop, prefer
> [Ubuntu/Fedora KDE](/docs/ubuntu-kde) or [GNOME](/docs/ubuntu-gnome).

## Install

The host ships as an RPM in punktfunk's **Gitea RPM registry** (public), so a Bazzite / Fedora
Atomic box layers and updates it with `rpm-ostree`. Add the repo, then layer the host plus the web
console and reboot:

```sh
# Add the repo. Our RPMs are unsigned, but Gitea GPG-signs the repo METADATA — verify that
# (repo_gpgcheck=1) and skip the per-package signature check (gpgcheck=0). The signed metadata
# carries each package's SHA256, so authenticity still holds. (Don't curl Gitea's served
# bazzite.repo verbatim — it sets gpgcheck=1, which fails on unsigned packages.)
sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'
[gitea-unom-bazzite]
name=punktfunk (unom, Bazzite)
baseurl=https://git.unom.io/api/packages/unom/rpm/bazzite
enabled=1
gpgcheck=0
repo_gpgcheck=1
gpgkey=https://git.unom.io/api/packages/unom/rpm/repository.key
REPO

# Layer the host + the web console, then reboot into the new deployment.
# (punktfunk Recommends punktfunk-web; list it explicitly so it's pulled regardless of weak-dep
# settings — the Gitea registry carries punktfunk-web, which COPR can't build.)
rpm-ostree install punktfunk punktfunk-web
systemctl reboot
```

`rpm-ostree upgrade` then tracks new builds automatically (Bazzite's auto-update timer does this
for you). For a fully baked appliance image there's also a **bootc** Containerfile that installs
the same RPMs from this registry — see `packaging/bootc/` and `packaging/rpm/README.md` in the repo.
Building from source works too (Bazzite is Fedora Atomic underneath, and its FFmpeg builds the host
fine — same steps as [Fedora KDE](/docs/fedora-kde)), but the registry is the supported path.

## Allow controller input

Gamepad and DualSense input needs your user in the `input` group. On Bazzite, don't use
`usermod` — the base is immutable and the group is managed by a recipe. Use:

```sh
ujust add-user-to-input-group
```

Then **log out and back in**. (A controller that's "detected but does nothing" is almost always this
permission, not a client problem.)

## Configure

The RPM ships a gamescope-ready config you can copy as your starting point:

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env
```

The key settings in `~/.config/punktfunk/host.env` point the host at the gamescope backend:

```sh
PUNKTFUNK_COMPOSITOR=gamescope
PUNKTFUNK_GAMESCOPE_SESSION=steam   # the host owns a Steam session at the client's mode
PUNKTFUNK_INPUT_BACKEND=gamescope
PUNKTFUNK_ZEROCOPY=1
```

With this, when a client connects the host starts a `gamescope-session-plus` (Steam) session at the
client's exact resolution and refresh, and relaunches it if the client changes mode. There should be
**no physical gaming session already running** on the box.

## Run as an always-on host

Bazzite hosts are typically headless. Enable the host service and linger so it starts at boot — see
[Running as a Service](/docs/running-as-a-service). Because the host launches its own gamescope
session per client, you don't need a separate desktop-session unit.

```sh
systemctl --user enable --now punktfunk-host
# Web console (pairing + status) — enable it and read the auto-generated login password,
# then open http://<host-ip>:3000:
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

## Good to know

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  current gamescope is fine; this only bites if you've pinned an old one.
- **The mouse cursor isn't included in the captured image** — a gamescope limitation for now.
- **HDR isn't supported yet** on the gamescope path — gamescope's capture output is 8-bit. SDR streams
  normally.

Then [connect a client](/docs/clients) — Moonlight works great for couch gaming, and the Apple app for
Apple TV / iPad.
