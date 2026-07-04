# Setting up punktfunk on Bazzite

A step-by-step setup guide for running the **punktfunk** low-latency streaming host on
**Bazzite** (the immutable, Fedora-Atomic gaming distro). Everything below is grounded in this
repo's packaging and ops files; where something is **not yet published or not in the repo**, it's
flagged explicitly. For the higher-level packaging rationale ("why not Flatpak", the build), see
[`../README.md`](../README.md).

> **What you get on Bazzite:** it already ships the three things punktfunk normally has to fight
> for — **gamescope**, **PipeWire/WirePlumber**, and (on the `-nvidia` images) the **NVIDIA driver
> with NVENC/EGL**. The only genuinely new runtime bits punktfunk adds are `ffmpeg-libs` (with
> NVENC, from RPM Fusion **nonfree**), `opus`, and `libei`.
> Source: `packaging/README.md`, `packaging/rpm/punktfunk.spec`.

> ⚠️ **COPR note (Path C only).** The legacy layering path's commands reference a COPR project
> named `enricobuehler/punktfunk` that is operator-run and may not be published (see
> `packaging/copr/README.md`); layer from the **Gitea RPM registry** instead (`../rpm/README.md`,
> the repo file `https://git.unom.io/api/packages/unom/rpm/bazzite.repo`) — it's what CI
> actually publishes to. Paths A (sysext) and B (bootc) don't involve the COPR at all.

---

## 1. Choose an install path

There are three paths on Bazzite, driven by different files in `packaging/`:

| Path | Driven by | What it does | Best for |
|---|---|---|---|
| **A — systemd-sysext** ✅ recommended | `packaging/bazzite/punktfunk-sysext.sh` + `build-sysext.sh` (published by `.gitea/workflows/rpm.yml`) | Overlays the host onto `/usr` as a system extension — no layering, no reboot, one-command updates | Everyone; the default |
| **B — bootc / OCI image** | `packaging/bootc/Containerfile` | Bakes punktfunk into a `FROM bazzite-nvidia` image once; you `bootc switch` any number of hosts onto it | Fleets, reproducible appliances, no per-host drift |
| **C — rpm-ostree layering** (legacy) | `packaging/rpm/` + the Gitea RPM registry | Layers the `punktfunk` RPM onto your deployment with `rpm-ostree install` | Only if you specifically want the RPM database to own the files |

**Why A over C:** the Bazzite docs treat layering as a last resort — every layered package makes
every OS update slower and can **block upgrades entirely** until removed. A sysext never enters an
rpm-ostree transaction: it merges/unmerges at runtime, survives OS updates, and updating punktfunk
is one command with **no reboot** (layering needs one per update). It's the mechanism the Fedora
Atomic maintainers ship via [fedora-sysexts](https://fedora-sysexts.github.io/). All paths require
the **same first-run setup** (sections 3–6).

### Path A — systemd-sysext (recommended)

Run on the Bazzite host:

```sh
# One-time bootstrap; afterwards the tool is on PATH as `punktfunk-sysext` (it ships inside
# the image). `--channel canary` for rolling main-branch builds instead of releases.
curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
sudo bash punktfunk-sysext.sh install
```

This downloads the newest image for your Fedora base (host + tray + **web console**,
SHA-256-verified from the feed `…/packages/unom/generic/punktfunk-sysext/f<ver>[-canary]/`),
installs it as `/var/lib/extensions/punktfunk.raw`, merges it, and immediately applies what the
RPM scriptlets would have (udev reload, sysctl) plus the two `/etc` files a sysext can't carry
(the gamescope-session drop-in and the tray autostart entry, staged under
`/usr/share/punktfunk/etc/`). No reboot at any point. Day-2:

```sh
sudo punktfunk-sysext update    # fetch + merge the newest build (then restart the user service)
sudo punktfunk-sysext status    # merged?, installed vs latest, channel/feed
sudo punktfunk-sysext remove    # unmerge + delete; ~/.config/punktfunk is left alone
```

Details worth knowing:

- The image embeds `ID=fedora` + `VERSION_ID` (matched through Bazzite's `ID_LIKE`), so after a
  **major Bazzite rebase** (F43 → F44) the old image is **refused** instead of merging
  soname-broken binaries — `punktfunk-sysext update` then fetches the image built for the new
  base (feeds exist per Fedora major, from the same CI matrix as the RPM groups).
- SELinux labels are baked into the image at build time (squashfs pseudo-xattrs computed from
  the targeted policy) — without them udev couldn't read the gamepad rule under enforcing.
  Validated live on Bazzite 43.
- **Migrating from layering (path C):** install the sysext (it shadows the layered copy at
  once), then `sudo rpm-ostree uninstall punktfunk punktfunk-web && systemctl reboot`.

### Path B — bootc image (`FROM bazzite-nvidia`)

The image is built **off-host** (on any machine with `podman`) from
`packaging/bootc/Containerfile`, which bases on `ghcr.io/ublue-os/bazzite-nvidia:stable`
(override with `--build-arg BASE_IMAGE=…`), enables RPM Fusion free + nonfree, adds the Gitea RPM
repo (`--build-arg PUNKTFUNK_RPM_GROUP=…`, default `bazzite`), and installs the host **and the web
console** (`punktfunk punktfunk-web`). It uses the Gitea registry rather than the COPR specifically
because the registry carries `punktfunk-web` (COPR's mock chroot can't build it — no `bun`).

```sh
# Build + push (run from the repo root, on your builder machine):
podman build -t ghcr.io/<you>/bazzite-punktfunk -f packaging/bootc/Containerfile .
podman push  ghcr.io/<you>/bazzite-punktfunk

# On each target Bazzite host:
sudo bootc switch ghcr.io/<you>/bazzite-punktfunk && systemctl reboot
```

> ⚠️ The image installs from the **Gitea RPM registry** (group `bazzite`), so **Path B depends on
> that registry being populated** — CI (`.gitea/workflows/rpm.yml`) publishes `punktfunk` +
> `punktfunk-web` on every push to `main`. Packages are unsigned with GPG-signed metadata
> (`repo_gpgcheck=1`), matching `packaging/rpm/README.md`.

### Path C — rpm-ostree layering (legacy)

Run on the Bazzite host. (Commands verbatim from `packaging/README.md`.)

```sh
# 1. RPM Fusion (free + nonfree) — provides the NVENC-capable ffmpeg-libs.
#    Usually already enabled on Bazzite; harmless to re-run.
rpm-ostree install \
  https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm \
  https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm

# 2. Enable the punktfunk COPR repo  ⚠️ requires the COPR to be published (see callout above)
sudo wget -O /etc/yum.repos.d/_copr_punktfunk.repo \
  https://copr.fedorainfracloud.org/coprs/enricobuehler/punktfunk/repo/fedora-$(rpm -E %fedora)/

# 3. Layer punktfunk and reboot to activate the new deployment.
rpm-ostree install punktfunk
systemctl reboot
```

> The **reboot is mandatory** — `rpm-ostree install` stages a new deployment that only takes
> effect on the next boot. This is normal atomic-distro behavior, not a punktfunk quirk.

#### Updating a Path-C host — `rpm-ostree upgrade` is NOT enough

> ⚠️ **`rpm-ostree upgrade` will not update punktfunk on its own.** `upgrade` bumps the **base
> image** and only re-resolves *layered* packages **when the base changes**. A Bazzite base can
> sit frozen for months (a pinned `:stable` tag, a paused rebase), so `rpm-ostree upgrade` keeps
> reporting *"No updates available"* and your layered `punktfunk` stays put even after new RPMs
> land in the repo. (Diagnose: `rpm-ostree status` shows the base `Version:` unchanged, while
> `dnf -q repoquery --upgrades punktfunk` lists newer builds.)

To actually pull a newer host on a static base, force rpm-ostree to re-resolve just the punktfunk
layer — remove + re-add the same names in one transaction:

```sh
sudo rpm-ostree refresh-md --force
sudo rpm-ostree update \
  --uninstall punktfunk --uninstall punktfunk-web \
  --install   punktfunk --install   punktfunk-web
systemctl reboot
```

Or just run the helper, which detects what's layered and does the above:

```sh
sudo bash packaging/bazzite/update-punktfunk.sh          # stage; reboot when ready
sudo bash packaging/bazzite/update-punktfunk.sh --reboot # stage + reboot now
```

> **Channel gotcha:** the re-resolve picks the highest version across **every enabled**
> `/etc/yum.repos.d/punktfunk*.repo`. If `punktfunk-canary.repo` is enabled alongside the stable
> `punktfunk.repo`, canary's `<next-minor>.0-0.ciN` **outranks** the stable `X.Y.Z-1` and the box
> silently tracks canary. Enable exactly one channel — set `enabled=0` in the other repo file.

---

## 2. Prerequisites — what Bazzite gives you vs. what you must still do

**Already satisfied on Bazzite (`-nvidia` images):**

- NVIDIA driver: `libnvidia-encode` (NVENC) + `libEGL_nvidia` for the zero-copy path.
- `gamescope` — the default compositor backend punktfunk uses on Bazzite.
- PipeWire + WirePlumber — the capture/audio graph.

**You must still do (covered below):**

1. **Reboot** after layering / rebasing (section 1).
2. **Join the `input` group** and ensure the **udev rule** is installed (section 3) — required for
   virtual gamepads / DualSense.
3. **Place `host.env`** and **enable the systemd user service** (sections 4–5).
4. **Open firewall ports** (section 6).

RPM Fusion's `ffmpeg-libs` is a **weak dependency** (`Recommends:` in the spec) — the package
installs without it, but **NVENC encoding will fail at runtime** if it's missing. The RPM Fusion
step in section 1 covers this.

---

## 3. udev rule + the `input` group

punktfunk creates **virtual X-Box-360 gamepads** via `/dev/uinput` and **virtual DualSense** pads
via `/dev/uhid` (kernel `hid-playstation` driver — LEDs, adaptive triggers, touchpad, gyro). The
udev rule grants the `input` group access to both nodes.

The RPM **already installs** the rule to `/usr/lib/udev/rules.d/60-punktfunk.rules` and its `%post`
reloads udev. So on a packaged install (Path A or B) **you only need to join the `input` group**:

```sh
ujust add-user-to-input-group     # then LOG OUT and back in (or reboot)
```

> ⚠️ **On Bazzite use `ujust add-user-to-input-group`, NOT `sudo usermod -aG input $USER`.** Bazzite
> is an atomic (rpm-ostree) OS where `/etc/group` is managed declaratively — a plain `usermod` either
> doesn't stick or gets reverted on the next update. The `ujust` recipe edits the group the
> immutable-OS-correct way (and reloads udev). (`ujust` ships with Bazzite; `ujust --list` shows all
> recipes.)

> 🔁 **The group change does not apply to your current login session** — you must re-login (or
> reboot). Until then, gamepad creation fails with a permission error on `/dev/uinput`. This is the
> single most common "why don't my gamepads work" gotcha.

If you installed from a tarball/source instead of the RPM (so the rule isn't in place), install it
manually — the exact commands from the rule file's header (`scripts/60-punktfunk.rules`):

```sh
sudo cp scripts/60-punktfunk.rules /etc/udev/rules.d/
ujust add-user-to-input-group      # NOT `usermod` on Bazzite (see the note above); then re-login
sudo udevadm control --reload-rules && sudo udevadm trigger
```

The rule contents, for reference:

```
KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", GROUP="input", MODE="0660", TAG+="uaccess"
KERNEL=="uhid",   SUBSYSTEM=="misc", OPTIONS+="static_node=uhid",   GROUP="input", MODE="0660", TAG+="uaccess"
```

---

## 4. Configure `host.env`

The systemd user unit reads its environment from **`~/.config/punktfunk/host.env`**
(`EnvironmentFile=%h/.config/punktfunk/host.env` in `scripts/punktfunk-host.service`). The RPM
ships a Bazzite-tuned template at `/usr/share/punktfunk/host.env.bazzite`. Copy it into place:

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env
# then edit ~/.config/punktfunk/host.env
```

The Bazzite template (`packaging/bazzite/host.env`) contains:

```sh
XDG_RUNTIME_DIR=/run/user/1000
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus

# gamescope backend: spawned per session, no compositor login required.
PUNKTFUNK_COMPOSITOR=gamescope
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_GAMESCOPE_APP=steam -gamepadui

# gamescope hosts its own EIS input socket — input lands in the nested session.
PUNKTFUNK_INPUT_BACKEND=gamescope

# GPU zero-copy capture (dmabuf -> CUDA -> NVENC). Auto-falls back to CPU if unavailable.
PUNKTFUNK_ZEROCOPY=1

#RUST_LOG=info
```

**What each knob means and why these are the Bazzite defaults:**

| Knob | Value | Meaning |
|---|---|---|
| `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` | `…/user/1000` | Session bus / runtime dir. **`1000` assumes your user is UID 1000** — change both if `id -u` says otherwise. |
| `PUNKTFUNK_COMPOSITOR` | `gamescope` | **The Bazzite default.** The host spawns a **headless gamescope per session** at the client's exact resolution/refresh and captures its PipeWire node — so you need **no graphical desktop login** to stream. Bazzite ships gamescope, so this "just works." |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` | Create a per-client virtual output at the client's exact WxH@Hz (the flagship "native resolution, no scaling" mode), vs. `portal` which captures an existing monitor. |
| `PUNKTFUNK_GAMESCOPE_APP` | `steam -gamepadui` | The command launched **inside** the nested gamescope — here, a SteamOS-style couch UI. Set it to whatever you want the session to run. |
| `PUNKTFUNK_INPUT_BACKEND` | `gamescope` | Inject mouse/keyboard/gamepad into the nested gamescope via its own EIS socket. |
| `PUNKTFUNK_ZEROCOPY` | `1` | GPU zero-copy capture (dmabuf → CUDA → NVENC). Falls back to CPU automatically if unavailable. |
| `RUST_LOG` | (commented) | Uncomment `RUST_LOG=info` for verbose logs while debugging. |

**Optional — a real DualSense for clients holding one:** add `PUNKTFUNK_GAMEPAD=dualsense` to present
games a virtual Sony DualSense (lightbar, adaptive triggers, touchpad, motion) instead of the
default X-Box-360 pad. The feedback flows back to a real DualSense on the client.

**Alternative — drive the full Plasma/GNOME desktop** instead of a nested gamescope (per the
template's footer comment): switch to `PUNKTFUNK_COMPOSITOR=kwin` and
`PUNKTFUNK_INPUT_BACKEND=libei`, and run the host **inside** a KDE session with `WAYLAND_DISPLAY` /
`XDG_CURRENT_DESKTOP` set. The full knob list (FEC %, per-stage timing, etc.) is in
`scripts/host.env.example` / `/usr/share/punktfunk/host.env.example`.

> The gamescope default is what makes Bazzite the easy path: it's a **headless, per-session**
> compositor — no desktop login, no display manager, no `--drm` scanout. You don't need any of the
> headless-KDE bring-up scripts (`scripts/headless/run-headless-kde.sh`) on Bazzite unless you
> deliberately switch to the KWin backend.

---

## 5. Enable and start the service

punktfunk runs as a **systemd `--user` service** (not root) — it needs your graphical/user
session's PipeWire and D-Bus. The unit (`scripts/punktfunk-host.service`) is installed by the RPM
into the user unit directory.

```sh
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-host
# Management web console (pairing + status), if you installed punktfunk-web (it ships in the Gitea
# RPM registry / bootc image — COPR can't build it; see ../rpm/README.md). Read the login password:
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'   # then open https://<host-ip>:47992
```

Check health and logs:

```sh
systemctl --user status punktfunk-host
journalctl --user -u punktfunk-host -f
```

> **What `serve` actually starts.** The bundled unit's `ExecStart` runs `punktfunk-host serve
> --gamestream`, so out of the box you get the **unified host**: the native `punktfunk/1` (QUIC) plane
> — always on in `serve` — **plus** the GameStream/Moonlight-compat planes (mDNS discovery, pairing,
> RTSP, the fixed GameStream ports) and the management REST API on 47990. The `--gamestream` flag is
> what adds the Moonlight surface; GameStream pairs over plain HTTP and its legacy encryption is weaker
> than the native plane's (security-review #5/#9), so it's **opt-in and trusted-LAN only**. For a
> **secure native-only host**, drop `--gamestream` from the unit's `ExecStart` (bare `serve`) — native
> clients still work; only stock Moonlight stops.
> (Source: `crates/punktfunk-host/src/main.rs` — `serve` runs the native plane + mgmt; `--gamestream`
> adds `gamestream::serve`.)

> **Unit caveat:** `scripts/punktfunk-host.service` declares only `After=pipewire.service` and (in
> the upstream/dev layout) assumes the binary at `%h/punktfunk/target/release/punktfunk-host`. The
> **RPM-installed** binary lives at `/usr/bin/punktfunk-host`. If `systemctl --user cat
> punktfunk-host` shows `ExecStart` pointing at a missing path in your home dir, drop an override
> (`systemctl --user edit punktfunk-host`) setting `ExecStart=/usr/bin/punktfunk-host serve
> --gamestream` (or bare `serve` for a secure native-only host).

---

## 6. Firewall

Bazzite runs **firewalld**, so the ports must be opened. The `punktfunk-host` package installs
firewalld **service definitions** (`/usr/lib/firewalld/services/punktfunk-gamestream.xml` and
`punktfunk-native.xml`), so enabling is one command — reload first so firewalld picks up the
definition, add the service, reload to apply:

```sh
sudo firewall-cmd --reload
sudo firewall-cmd --permanent --add-service=punktfunk-gamestream   # Moonlight/GameStream host
#                              --add-service=punktfunk-native       # …or the native-only host
sudo firewall-cmd --reload
```

`punktfunk-gamestream` opens the fixed Moonlight ports + mDNS; `punktfunk-native` opens the QUIC
control port (UDP 9777) + mDNS. Enable both if the host runs `serve --gamestream` (both planes). The
per-port breakdown below is for reference (or for opening ports by hand); the ports are the code
constants (`crates/punktfunk-host/src/gamestream/mod.rs`, `mgmt.rs`) and the GameStream-host port-map
(`design/gamestream-host-plan.md`).

**GameStream / Moonlight ports** (fixed; Moonlight derives them from the HTTP base). These only apply
when the host runs `serve --gamestream` (the bundled unit's default); on a bare-`serve` native-only
host you don't open them:

| Port | Proto | Purpose |
|---|---|---|
| 47984 | TCP | HTTPS nvhttp (paired, mutual-TLS) |
| 47989 | TCP | HTTP nvhttp (`/serverinfo`, `/pair` PIN flow) |
| 48010 | TCP | RTSP handshake |
| 47998 | UDP | Video RTP (+ FEC) |
| 47999 | UDP | ENet control stream + remote input |
| 48000 | UDP | Audio (Opus) |
| 5353  | UDP | mDNS — so Moonlight auto-discovers the host (`_nvstream._tcp.local.`) |

**Management REST API:** **TCP 47990** — but `serve` **binds it to `127.0.0.1` (loopback) by
default**, so you do **not** open it in the firewall unless you deliberately move it off loopback
with `--mgmt-bind IP:PORT` (which also requires `--mgmt-token`). Leave it closed for a normal setup.

To open the GameStream ports by hand instead of the service (equivalent):

```sh
sudo firewall-cmd --permanent --add-port=47984/tcp \
                  --add-port=47989/tcp \
                  --add-port=48010/tcp
sudo firewall-cmd --permanent --add-port=47998/udp \
                  --add-port=47999/udp \
                  --add-port=48000/udp \
                  --add-port=5353/udp
sudo firewall-cmd --reload
```

**If you also run the native `punktfunk/1` host** (`punktfunk-host punktfunk1-host`, not started by the
default unit):

- **QUIC control plane: UDP 9777** (default `--port`; change with `--port N`).
- **Data plane: an *ephemeral* UDP port** — `punktfunk1-host` binds `0.0.0.0:0` and tells the client which
  port it got, so there is **no fixed data port to open**. For a restrictive firewall you'd need to
  allow the ephemeral UDP range; the repo does not pin one.

```sh
# Only if you run `punktfunk1-host`:
sudo firewall-cmd --permanent --add-port=9777/udp && sudo firewall-cmd --reload
```

---

## 6.5 Desktop (KDE) mode — stream the desktop at the client's resolution (optional)

The host **auto-detects** the live session per connect: in **Steam Gaming Mode** it attaches to the
running gamescope (no setup); switch the box to the **KDE Desktop** and it drives a KWin *virtual
output at the connecting client's exact resolution* (no TV-stretch, churn-free). The Desktop path
needs one one-shot setup the first time, because a normal KDE login withholds two things the
headless host needs — the privileged `zkde_screencast` virtual-output protocol, and an
auto-approved RemoteDesktop input grant:

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh
# then log out + back into the KDE Desktop session once (or reboot) so KWin restarts with the flag
```

That writes `~/.config/environment.d/10-punktfunk-kwin.conf`
(`KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`) and seeds the `kde-authorized` RemoteDesktop grant into
`~/.local/share/flatpak/db/`. Gaming Mode is unaffected. To connect from Desktop Mode, switch to it
(Steam → Power → Switch to Desktop), then connect the client; switching **mid-stream** requires a
reconnect (the host resolves the backend per connect).

---

## 7. Verify it's working

**1. Watch the startup log:**

```sh
journalctl --user -u punktfunk-host -f
```

A healthy `serve` startup logs the `punktfunk-host (punktfunk_core ABI v…)` banner, then `mDNS
advertising`, and an RTSP listening line on port 48010. No NVENC/EGL errors on the first connection.

**2. Pair a stock Moonlight client (recommended first test):**

- Open Moonlight on your phone/PC on the **same LAN** — the host should appear automatically (mDNS).
- Select it; Moonlight shows a 4-digit PIN. The host completes the GameStream pairing handshake (it
  persists across restarts).
- Launch the app — you should get video at your client's native resolution/refresh, with the nested
  `steam -gamepadui` (or whatever `PUNKTFUNK_GAMESCOPE_APP` you set) running inside gamescope.

**3. (Optional) native punktfunk/1 client** — only if you're running the separate `punktfunk1-host`. The
repo's reference client is `punktfunk-probe`, e.g. `punktfunk-probe --mode 1280x720x120 --out
/tmp/a.h265` (add `--pin HEX` for PIN pairing). This is a headless/decode-to-file reference, not a
desktop viewer.

---

## 8. Troubleshooting (grounded in the repo's real gotchas)

- **Gamepads don't appear / permission denied on `/dev/uinput` or `/dev/uhid`.** Either you haven't
  joined the `input` group, or you haven't re-logged-in since. On Bazzite join it with
  `ujust add-user-to-input-group` (a plain `sudo usermod -aG input` doesn't stick on an atomic OS —
  see section 3), then log out and back in (or reboot): group membership only takes effect on a new
  session. The host log makes this unambiguous — it prints `virtual gamepad created` /
  `virtual DualSense created` on success, or `… creation failed — controller input disabled` when
  the device node isn't writable. (`scripts/60-punktfunk.rules`, `packaging/README.md`.)

- **No video / NVENC fails to encode.** RPM Fusion's `ffmpeg-libs` (with NVENC) is missing — it's a
  weak dependency, so the package installed without it. Re-run the RPM Fusion step in section 1.
  (`packaging/rpm/punktfunk.spec`: `Recommends: ffmpeg-libs`.)

- **gamescope session won't come up / capture deadlocks.** punktfunk needs **gamescope ≥ 3.16.22** —
  older versions (e.g. the broken 3.16.20 some bases shipped) **deadlock on PipeWire ≥ 1.6**, and a
  wedged capture link can head-block the whole PipeWire daemon system-wide. Check with `gamescope
  --version`. Bazzite tracks recent gamescope, but verify if you hit hangs. (Project notes.)

- **NVENC/EGL silently stops working after a system update.** punktfunk's reference box uses the
  NVIDIA **open** kernel module, and a kernel update can silently drop it. On Bazzite the NVIDIA
  stack is image-managed (`bazzite-nvidia`), so this is **far less likely** — but if NVENC dies right
  after an `rpm-ostree`/`bootc` update, confirm the NVIDIA driver still loads (`nvidia-smi`) before
  blaming punktfunk.

- **`PUNKTFUNK_ZEROCOPY=1` but it falls back to CPU.** The zero-copy path needs working EGL/CUDA from
  the NVIDIA driver. The code falls back to CPU automatically; check the log for the fallback line and
  verify the `-nvidia` image / driver is healthy.

- **Wrong UID in `host.env`.** `XDG_RUNTIME_DIR=/run/user/1000` and the bus path assume UID 1000. Run
  `id -u`; if it's different, fix both lines or the host can't reach your session's PipeWire/D-Bus.

- **Service `ExecStart` points at a missing path in `$HOME`.** The dev unit references
  `%h/punktfunk/target/release/...`. The RPM binary is `/usr/bin/punktfunk-host`. Override
  `ExecStart=/usr/bin/punktfunk-host serve --gamestream` (or bare `serve` for native-only) if needed
  (section 5).

- **Moonlight can't see the host.** Ensure UDP 5353 (mDNS) and the GameStream ports are open
  (section 6) and client + host are on the same L2 LAN segment.

---

## Appendix — if the COPR isn't published yet

The COPR (`enricobuehler/punktfunk`) is **operator-run and may not be live**. If `rpm-ostree install
punktfunk` can't find the package, build the RPM yourself on a **Fedora** machine/toolbox (not
Debian/Ubuntu — the host links system FFmpeg/PipeWire and won't build there), per
`packaging/README.md`:

```sh
git archive --format=tar.gz --prefix=punktfunk-0.3.0/ \
  -o ~/rpmbuild/SOURCES/punktfunk-0.3.0.tar.gz HEAD    # 0.3.0 = the spec's default version
rpmbuild -ba packaging/rpm/punktfunk.spec    # needs the spec's BuildRequires + RPM Fusion
```

To publish the COPR for others (so `rpm-ostree install punktfunk` / the bootc image work), follow
`packaging/copr/README.md` — create the project, point build-from-SCM at the repo with spec path
`packaging/rpm/punktfunk.spec`, add RPM Fusion nonfree as an external repo, and select chroots
matching your Bazzite Fedora base (`rpm -E %fedora`).

---

### Accuracy flags

1. The COPR is **operator-run / not assumed published** — both install paths depend on it.
2. There is **no firewall script/doc in the repo** — the ports above are derived from the code.
3. The bundled systemd unit runs `serve --gamestream` — the native `punktfunk/1` QUIC plane (always
   on) **plus** the GameStream/Moonlight planes. Drop `--gamestream` for a secure native-only host;
   `punktfunk1-host` is a separate standalone native host, unmanaged by the unit.
4. The mgmt port (47990) is **loopback-only by default** — don't open it.
