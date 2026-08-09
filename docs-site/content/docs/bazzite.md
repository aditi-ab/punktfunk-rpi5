---
title: Bazzite
description: Set up a Punktfunk host on Bazzite — it follows the box between Steam Gaming Mode (gamescope) and the KDE Plasma desktop automatically.
---

[Bazzite](https://bazzite.gg/) already ships everything a Punktfunk host needs — the NVIDIA driver,
NVENC, PipeWire, **gamescope**, and the **KDE Plasma desktop**. So a Bazzite host is the most
"appliance-like" setup, and it streams **both** of Bazzite's faces:

- **Steam Gaming Mode** (gamescope) — the couch/handheld game UI.
- **The KDE Plasma desktop** — the full desktop you get from "Switch to Desktop".

The host **auto-detects which one is live and follows the box across the switch** — including
mid-stream. You flip between Gaming Mode and Desktop with Bazzite's normal Steam UI /
"Switch to Desktop"; the host just re-targets whatever's running and keeps streaming. Nothing in
`host.env` forces a mode.

> Ideal for a dedicated game-streaming box that you also occasionally want as a remote desktop. For a
> pure desktop machine, install on [Ubuntu](/docs/ubuntu) or [Fedora](/docs/fedora) and configure the
> [KDE](/docs/kde) or [GNOME](/docs/gnome) desktop directly — simpler.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## Install

The host installs as a **systemd system extension (sysext)** — no `rpm-ostree` layering. The
Bazzite docs treat layering as a last resort (layered packages slow every OS update and can block
upgrades until removed); a sysext never enters an rpm-ostree transaction: it overlays `/usr`
read-only from `/var/lib/extensions/`, survives OS updates, installs and updates **without a
reboot**, and is removable in one command. This is the same mechanism the Fedora Atomic
maintainers ship via the [fedora-sysexts](https://fedora-sysexts.github.io/) project.

```sh
# One-time bootstrap (afterwards the updater is on PATH as `punktfunk-sysext`):
curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
sudo bash punktfunk-sysext.sh install          # add `--channel canary` for rolling builds
```

That downloads the newest image — host + tray + web console + the plugin runner
(`punktfunk-scripting`), plus the HDR `punktfunk-gamescope` build — merges it, and applies the
udev/sysctl setup on the spot; the host is usable immediately, no reboot. The feed's checksum
manifest is OpenPGP-signed by packages@unom.io (key `AF245C506F4E4763`, the same one that signs our
RPMs), and `punktfunk-sysext` checks that signature against a key baked into the script before it
trusts a single checksum — so it needs `gpg` on the box, and it refuses a feed it can't verify.

The plugin runner rides along in the image but isn't started: run
`systemctl --user enable --now punktfunk-scripting` when you want [plugins](/docs/plugins).

From then on:

```sh
sudo punktfunk-sysext update     # fetch + merge the newest build
sudo punktfunk-sysext status     # channel, installed vs latest version
sudo punktfunk-sysext remove     # unmerge and delete the image (~/.config/punktfunk is kept)
```

After an update, restart the host so it runs the new binary (the updater prints this reminder too).
The image carries the console as well, so restart that first if you enabled it:

```sh
systemctl --user restart punktfunk-web     # only if you run the console
systemctl --user restart punktfunk-host
```

To **switch channel** later, re-run the install: `sudo punktfunk-sysext install --channel canary`
(or `--channel stable`). `update` takes no channel flag — it follows whatever the last install wrote
to `/etc/punktfunk-sysext.conf`. To be able to **go back** to a build that worked, keep a copy of the
image before you update, and re-install that file afterwards:

```sh
sudo cp /var/lib/extensions/punktfunk.raw ~/punktfunk-known-good.raw   # before updating
sudo punktfunk-sysext install --from-file ~/punktfunk-known-good.raw   # to go back to it
```

The web console can also run the update for you — see [Updating the Host](/docs/updating), which
needs the one-time `sudo usermod -aG punktfunk-update $USER`.

`remove` deletes the image and the `/etc` files it seeded (the tray autostart entry, and the
gamescope session drop-in unless you've edited it), but three things it created outside `/usr` stay
behind. To clear those too — services first, because once the image unmerges their binaries are
gone and the units just keep failing:

```sh
systemctl --user disable --now punktfunk-host punktfunk-web
sudo punktfunk-sysext remove
sudo rm -f /etc/modules-load.d/punktfunk.conf /etc/udev/rules.d/60-punktfunk.rules
sudo groupdel punktfunk-update     # the (empty) group for web-console updates
```

[Uninstalling](/docs/uninstall) has the same walkthrough for the other install methods, and for the
clients.

Three things to know:

- **After a Bazzite major rebase** (Fedora 43 → 44) the old image **refuses to load** rather than
  run against mismatched system libraries — run `sudo punktfunk-sysext update` once and it fetches
  the image built for the new base.
- **Already layering Punktfunk?** Install the sysext (it shadows the layered copy immediately),
  then drop the layer so it stops slowing your updates:
  `sudo rpm-ostree uninstall punktfunk punktfunk-web && systemctl reboot`.
- **If it refuses the feed.** `refusing to install from an unsigned feed` means that Fedora major's
  feed predates signing; it gets sealed on the next publish. To install from it anyway, accepting
  that the images are unauthenticated, run
  `sudo env PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED=1 bash punktfunk-sysext.sh install`. The other message,
  `the feed's SHA256SUMS is NOT signed by packages@unom.io`, is not the same thing — don't install;
  re-download the script and try again.

For a fully baked appliance image there's also a **bootc** Containerfile that installs the RPMs
from the registry at image-build time — see `packaging/bootc/` in the repo. Plain `rpm-ostree`
layering from the [RPM registry](https://git.unom.io/unom/-/packages) keeps working too: add the
repo exactly as on [Fedora](/docs/fedora), with the `baseurl` group matching your Fedora base, then
`sudo rpm-ostree install punktfunk punktfunk-web` and reboot. The sysext is still the supported
default. Building from source also works (Bazzite is Fedora Atomic underneath — same steps as
[Fedora](/docs/fedora)).

## Allow controller input

Gamepad and DualSense input needs your user in the `input` group. On Bazzite, don't use
`usermod` — the base is immutable and the group is managed by a recipe. Use:

```sh
ujust add-user-to-input-group
```

Then **log out and back in**. (A controller that's "detected but does nothing" is almost always this
permission, not a client problem.)

Then join `punktfunk` — `usermod` is fine here, because unlike `input` this group is ours and the
sysext creates it on merge:

```sh
sudo usermod -aG punktfunk "$USER"   # then log out and back in
```

This box **is** a Gaming Mode box, so that group is not optional in practice: it authorizes the
helper the host uses to stop the display manager when it takes the Gaming Mode session over at your
client's resolution, and it gates the usbip `attach` file the **virtual Steam Deck controller**
(paddles, trackpads, gyro) attaches through. It is a separate group on purpose — writing that file
can materialise arbitrary emulated USB hardware, so it is not folded into the group everyone is
told to join for gamepads. Without it the pad arrives as an ordinary Xbox 360 controller, and the
takeover degrades to mirroring the box's own screen — see
[gamescope](/docs/gamescope#nobara-and-other-autologin-display-managers).

## Configure

The RPM ships a Bazzite-tuned config you can copy as your starting point:

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env
```

The template is deliberately minimal — it does **not** force a compositor, because the host
auto-detects Gaming Mode (gamescope) vs Desktop (KWin) on every connect and follows the switch
mid-stream. No session anchors are needed either (a user service inherits the right runtime dir).
The only settings that matter (GPU zero-copy is on by default):

```sh
PUNKTFUNK_VIDEO_SOURCE=virtual
# GPU zero-copy (dmabuf → CUDA → NVENC) is ON by default; auto-falls back to CPU. Set =0 to force CPU.
PUNKTFUNK_GAMESCOPE_ATTACH=1    # Gaming Mode = attach to the box's own session — SDR, and no cursor (see below)
```

### Gaming Mode: attach vs managed

For Gaming Mode there are two models (pick one; the shipped default is **attach**):

- **Attach** (`PUNKTFUNK_GAMESCOPE_ATTACH=1`, the template's default) — the **box** owns its
  gamescope session on its own display, and the host attaches to whatever's live without ever
  tearing it down (on a headless box, a box-owned autologin session is restarted at the client's
  resolution on a mismatch; with a display connected it streams at the box's own mode). Switching
  Desktop ↔ Game is rock-solid.
- **Managed** (`PUNKTFUNK_GAMESCOPE_MANAGED=1`, and remove the attach line) — the host takes the
  box's gamescope over and relaunches it **headless** at the *client's* exact resolution and
  refresh — Game Mode on the virtual screen — restoring the box on idle.

Full treatment: [Steam / gamescope → How the host gets a
gamescope](/docs/gamescope#how-the-host-gets-a-gamescope).

Mid-stream Gaming ↔ Desktop following (`PUNKTFUNK_SESSION_WATCH`) is **on by default** on
Bazzite/SteamOS. See [Configuration](/docs/configuration) for the full list of knobs.

### Streaming the KDE Plasma desktop

The **virtual output** (video) for the Desktop session needs no config — the host package ships an
`io.unom.Punktfunk.Host.desktop` file whose `X-KDE-Wayland-Interfaces` grants the host KWin's
restricted screencast protocol on a normal interactive Plasma session (background:
[KDE Plasma](/docs/kde)). After a **fresh host install, log out and back into the Desktop session
once** so KWin re-reads that grant.

The one thing a normal KDE login lacks is the RemoteDesktop grant for headless **input** injection.
Seed it once (as the streaming user, no root) so the host auto-approves instead of popping an
un-answerable dialog:

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh
```

Gaming Mode needs none of this — it auto-attaches.

## Run as an always-on host

Bazzite hosts are typically headless. Enable the host service and linger so it starts at boot — see
[Running as a Service](/docs/running-as-a-service). One host service covers both Gaming Mode and the
Desktop; it follows whichever the box is in.

```sh
systemctl --user enable --now punktfunk-host
systemctl --user enable --now punktfunk-web     # web console: pairing + status
sudo loginctl enable-linger "$USER"             # start at boot with nobody logged in
```

Without that last line the `--user` units don't start until someone logs in — which on a headless box
never happens.

Then open [The Web Console](/docs/web-console) for the login password and to
[arm pairing](/docs/web-console#arm-pairing).

## Good to know

These apply to the **Gaming Mode (gamescope)** path; the KDE Desktop path is unaffected:

- **gamescope 3.16.22 or newer is required; 3.16.23 or newer for the Steam overlay.** Below 3.16.22
  headless capture can deadlock; between the two, capture works but the Steam overlay (Shift+Tab /
  the Quick Access Menu) is never painted into the captured node. Bazzite's current gamescope is
  past both; this only bites if you've pinned an old one.
- **The template pins attach, and that costs you both the cursor and HDR.** The sysext ships the
  `punktfunk-gamescope` build, but it only reaches a session the host starts itself — and the
  `host.env` template above sets `PUNKTFUNK_GAMESCOPE_ATTACH=1`, where the live session is
  Bazzite's own stock gamescope. Comment that line out and let the managed default take over: you
  get the compositor-drawn pointer and real HDR. To stay on attach instead, set
  `PUNKTFUNK_GAMESCOPE_HDR=0` and `PUNKTFUNK_GAMESCOPE_BIN=/usr/bin/gamescope`. Why each half
  breaks: [gamescope → Known limits](/docs/gamescope#known-limits) for the cursor,
  [HDR → Linux + gamescope](/docs/hdr#linux--gamescope) for the failed connect.

Those are the two that bite on Bazzite. The full set — touch, mouse modes, the clipboard — is on
[gamescope → Known limits](/docs/gamescope#known-limits).

Then [connect a client](/docs/clients) — Moonlight works great for couch gaming, and the Apple app for
Apple TV / iPad. Trouble? See [Troubleshooting](/docs/troubleshooting).
