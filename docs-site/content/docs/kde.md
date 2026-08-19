---
title: KDE Plasma (KWin)
description: Configure a Punktfunk host for KDE — host.env, quirks, and a headless KWin session.
---

Configure a Punktfunk host on **KDE Plasma**. The host uses KDE's KWin compositor to create a
per-client virtual display, captured zero-copy on the GPU (NVIDIA, AMD and Intel alike). This page
assumes the package is already installed — see [Ubuntu](/docs/ubuntu), [Fedora](/docs/fedora),
[Arch](/docs/arch), or the
[Bazzite](/docs/bazzite) appliance.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## host.env

The host auto-detects your KWin session on every connect — including a box that switches between
the Plasma desktop and Steam Game Mode — so the starter `~/.config/punktfunk/host.env` is one line:

```ini
# ~/.config/punktfunk/host.env  (keys are case-sensitive)
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy capture→encode (dmabuf → CUDA/NVENC on NVIDIA, dmabuf → Vulkan Video or VAAPI on
# AMD/Intel) is ON by default; auto-falls back to CPU. Set PUNKTFUNK_ZEROCOPY=0 to force CPU.
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

On a normal Plasma login KWin drives your real hardware, and **any Plasma 6 release** can create the
virtual output — what it needs is the screencast grant below, not a particular version. The
[headless session](#headless-session) further down is the exception: it runs
`kwin_wayland --virtual`, and *that* backend only learned to create virtual outputs in
**KWin 6.5.6**, so an appliance box needs 6.5.6 or newer. Check with:

```sh
kwin_wayland --version
```

## Streaming the interactive desktop

To stream a logged-in Plasma desktop (rather than a headless session, below), KWin has to hand the
host its restricted screencast protocol. The host package ships an `io.unom.Punktfunk.Host.desktop`
file whose `X-KDE-Wayland-Interfaces` grants exactly that on a normal interactive session
(least-privilege, the same mechanism krfb/krdp use). After a **fresh install, log out and back into
the Desktop session once** so KWin re-reads the grant.

**Input needs no extra setup.** That same `.desktop` file also grants `org_kde_kwin_fake_input`, so
the host injects keyboard, mouse and touch straight into KWin — no portal, and no "Allow remote
control?" dialog for a headless box to answer, on every distro. All a fresh install needs is the log
out and back in above, so KWin re-reads the grant.

The old RemoteDesktop-portal grant only matters now if you force the libei input backend
(`PUNKTFUNK_INPUT_BACKEND=libei`) — that one injects through the RemoteDesktop portal, and on KDE it
is also what a portal capture (`PUNKTFUNK_VIDEO_SOURCE=portal`) is anchored to, so the capture
inherits the same grant. To seed it, Fedora and Bazzite ship a one-shot helper (run once as the
streaming user, no root); the `.deb` and Arch packages don't include that wrapper, so copy the
shipped `kde-authorized` file into the portal store by hand instead (the share dir is
`/usr/share/punktfunk-host` on Ubuntu, `/usr/share/punktfunk` on Arch) and log out and back
in:

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh    # Fedora / Bazzite

mkdir -p ~/.local/share/flatpak/db                        # Ubuntu / Arch
cp /usr/share/punktfunk*/headless/kde-authorized ~/.local/share/flatpak/db/kde-authorized
```

A login-less appliance skips all of this — its headless session (below) needs none of these grants.

## Start the host

With `host.env` in place, start the host from **inside your Plasma session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f   # watch it come up and print its identity fingerprint
```

This unit runs `serve --gamestream`, so it serves stock [Moonlight](/docs/moonlight) clients as well
as the native ones. For a native-only host, see
[What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

A desktop-login host should also follow your session's lifetime, or restarting Plasma leaves the host
wired to a compositor that is gone — it keeps answering, and every session after that fails at
capture. Add the drop-in from
[Restart the host with your desktop](/docs/running-as-a-service#restart-the-host-with-your-desktop).
Skip it on the headless appliance route below.

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
cp /usr/share/punktfunk/host.env.kde ~/.config/punktfunk/host.env   # Ubuntu: /usr/share/punktfunk-host/host.env.kde

systemctl --user daemon-reload
systemctl --user enable --now punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"
```

The session unit brings up headless KWin; the host unit follows it and starts listening. See
[Running as a Service](/docs/running-as-a-service) for the full headless setup.

## Troubleshooting

- **KWin isn't handing over the screencast protocol:** run `punktfunk-host probe-compositor` from
  inside the session. It exits 0 only when KWin is up *and* advertising the grant to the host, and
  prints the reason when it isn't — after a fresh install that is usually just the missing log out
  and back in. On the headless appliance session, also check `kwin_wayland --version` is ≥ 6.5.6.
  The other cause has nothing to do with the `.desktop` file: `getcap /usr/bin/punktfunk-host` must
  print **nothing at all**. A host binary carrying a Linux capability cannot be identified by KWin
  and is never offered the protocol, however correctly its grant is installed — see
  [GPU scheduling priority](/docs/running-as-a-service#gpu-scheduling-priority).
- **"KWin virtual output failed: Could not find output"** (the message arrives translated — a
  Brazilian session reads *"Não foi possível encontrar saída"*): KWin got the request and refused
  it, so the grant above is fine and only the output creation is not. Two shapes. Its **backend**
  can't create one — a nested KWin, or `kwin_wayland --virtual` below 6.5.6; a normal Plasma
  session on the DRM backend always can. Or, on **KWin 6.6+**, KWin created the output and then
  left it **disabled**. That reports identically, and it is new: 6.6 put a
  `workspace()->findOutput()` hop in front of the stream, so from 6.6 on the output must also be
  *enabled and workspace-managed*, where 6.5 and earlier streamed it either way. Check
  `punktfunk-host list-monitors` for a
  `Virtual-punktfunk-*` marked *disabled*, a stale entry in `~/.config/kwinoutputconfig.json`, and
  `journalctl --user -b -t kwin_wayland` for *"Applying output configuration failed!"* — KWin logs
  that when it declines to enable one more output next to your current monitors, and keeps the old
  configuration instead. Streaming a real monitor
  ([`PUNKTFUNK_CAPTURE_MONITOR`](/docs/configuration), or **Streamed screen** in the console) skips
  virtual-output creation entirely and is the workaround while you sort the above out.
- **Black screen / no picture:** confirm you're on a Wayland session (not X11) and, on NVIDIA, that
  the GL userspace is installed. More in [Troubleshooting](/docs/troubleshooting).

To bring the console up and pair, see [The Web Console](/docs/web-console).
