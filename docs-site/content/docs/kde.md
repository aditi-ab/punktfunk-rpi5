---
title: KDE Plasma (KWin)
description: Configure a punktfunk host for KDE — host.env, quirks, and a headless KWin session.
---

Configure a punktfunk host on **KDE Plasma**. The host uses KDE's KWin compositor to create a
per-client virtual display, captured zero-copy on NVIDIA. This page assumes the package is already
installed — see [Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora), [Arch](/docs/arch), or the
[Bazzite](/docs/bazzite) appliance.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

The host auto-detects your KWin session on every connect — including a box that switches between
the Plasma desktop and Steam Game Mode — so the starter `~/.config/punktfunk/host.env` is one line:

```ini
# ~/.config/punktfunk/host.env  (keys are case-sensitive)
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy (dmabuf → CUDA → NVENC) is ON by default; auto-falls back to CPU. Set =0 to force CPU.
```

> **Don't set `PUNKTFUNK_COMPOSITOR`, `WAYLAND_DISPLAY`, or `XDG_CURRENT_DESKTOP` here.** Pinning
> the compositor turns auto-detection **off** — per connect *and* mid-stream — so a switch to Game
> Mode then kills the stream instead of being followed, and stale session values point the host at
> dead sockets. Forcing a backend is for CI and dedicated appliances (the
> [headless session](#headless-session) below ships a `host.env.kde` that pins on purpose).

If the box switches between the desktop and Game Mode, also enable lingering — the host is a user
service, and without linger the logout moment of a session switch tears it (and PipeWire) down
mid-stream:

```sh
sudo loginctl enable-linger "$USER"
```

See the [Configuration reference](/docs/configuration) for every option.

## Use a Wayland session

KDE must run on **Wayland**, not X11 — pick the Wayland session from the picker on the login screen.
The virtual-display path is Wayland-only and will not come up under X11.

KWin must be **6.5.6 or newer** (virtual outputs land there). Check with:

```sh
kwin_wayland --version
```

## Streaming the interactive desktop

To stream a logged-in Plasma desktop (rather than a headless session, below), KWin has to hand the
host its restricted screencast protocol. The host package ships an `io.unom.Punktfunk.Host.desktop`
file whose `X-KDE-Wayland-Interfaces` grants exactly that on a normal interactive session
(least-privilege, the same mechanism krfb/krdp use). After a **fresh install, log out and back into
the Desktop session once** so KWin re-reads the grant.

A normal KDE login still lacks the RemoteDesktop grant that **input** injection needs — without it the
host pops an "Allow remote control?" dialog no headless box can answer. **Fedora and Bazzite** ship a
one-shot helper that seeds it (run once as the streaming user, no root):

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh    # Fedora / Bazzite
```

The `.deb` and Arch packages don't include that wrapper. Seed the grant by hand instead — copy the
shipped `kde-authorized` file into the portal store (the share dir is `/usr/share/punktfunk-host` on
Debian/Ubuntu, `/usr/share/punktfunk` on Arch), then log out and back in:

```sh
mkdir -p ~/.local/share/flatpak/db
cp /usr/share/punktfunk*/headless/kde-authorized ~/.local/share/flatpak/db/kde-authorized
```

A login-less appliance skips all of this — its headless session (below) needs none of these grants.

## Start the host

With `host.env` in place, start the host from **inside your Plasma session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f   # watch it come up and print its identity fingerprint
```

Then bring up [The Web Console](/docs/web-console) to arm pairing and connect a
[client](/docs/clients). To start at boot — including fully headless — see the
[headless session](#headless-session) below or [Running as a Service](/docs/running-as-a-service).

## Persistent per-client scaling

KWin round-trips per-client display scale: it names each session's virtual output per client, so a
scale you set for one client (150 %, 125 %, …) is reapplied on that client's next connect. See
[Virtual displays](/docs/virtual-displays).

## Headless session

For a login-less appliance — a box that streams at boot with no graphical login — the host brings up a
**dedicated headless KWin session** rather than relying on an interactive one. It runs its own
`kwin --virtual` session (shipped as the `punktfunk-kde-session.service` unit) with permission checks
relaxed, so it needs none of the interactive grants above.

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.kde ~/.config/punktfunk/host.env   # Debian/Ubuntu: /usr/share/punktfunk-host/host.env.kde

systemctl --user daemon-reload
systemctl --user enable --now punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"
```

The session unit brings up headless KWin; the host unit follows it and starts listening. See
[Running as a Service](/docs/running-as-a-service) for the full headless setup.

## Troubleshooting

- **KWin too old:** virtual outputs need KWin **≥ 6.5.6**. Check with `kwin_wayland --version`.
- **Black screen / no picture:** confirm you're on a Wayland session (not X11) and the NVIDIA GL
  userspace is installed. More in [Troubleshooting](/docs/troubleshooting).

To bring the console up and pair, see [The Web Console](/docs/web-console).
