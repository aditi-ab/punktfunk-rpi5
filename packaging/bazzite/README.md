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

> ⚠️ **Read this first — the COPR is operator-run, not yet published.**
> Both install paths below pull the punktfunk RPM from a COPR project named
> `enricobuehler/punktfunk`. That COPR is a configuration the maintainer has to **create and
> build** (see `packaging/copr/README.md` — it documents how to set it up, not a live repo URL you
> can assume exists). If `rpm-ostree install punktfunk` 404s, the COPR hasn't been published yet,
> and your only path is to **build the RPM yourself** (see the appendix). The guide flags every
> command that depends on the COPR being live.

---

## 1. Choose an install path

There are two supported paths on Bazzite, driven by different files in `packaging/`:

| Path | Driven by | What it does | Best for |
|---|---|---|---|
| **A — rpm-ostree layering** | `packaging/copr/README.md` + `packaging/rpm/punktfunk.spec` | Layers the `punktfunk` RPM onto your existing Bazzite deployment with `rpm-ostree install` | One host, quick iteration |
| **B — bootc / OCI image** | `packaging/bootc/Containerfile` | Bakes punktfunk into a `FROM bazzite-nvidia` image once; you `bootc switch` any number of hosts onto it | Fleets, reproducible appliances, no per-host drift |

**Trade-off:** Path A is a per-host package layer — simple, but each host accumulates its own
layered-package state. Path B builds one image (RPM Fusion + COPR + the package + udev rule
pre-installed) that you push to a registry and rebase hosts onto atomically — no per-host
`rpm-ostree install` drift, at the cost of running a `podman build`/`push` pipeline. Both
ultimately install the **same RPM** and require the **same first-run setup** (sections 3–6).

### Path A — rpm-ostree layering from the COPR

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

### Path B — bootc image (`FROM bazzite-nvidia`)

The image is built **off-host** (on any machine with `podman`) from
`packaging/bootc/Containerfile`, which bases on `ghcr.io/ublue-os/bazzite-nvidia:stable`
(override with `--build-arg BASE_IMAGE=…`), enables RPM Fusion free + nonfree, enables the COPR
(`--build-arg PUNKTFUNK_COPR=…`, default `enricobuehler/punktfunk`), and installs the package.

```sh
# Build + push (run from the repo root, on your builder machine):
podman build -t ghcr.io/<you>/bazzite-punktfunk -f packaging/bootc/Containerfile .
podman push  ghcr.io/<you>/bazzite-punktfunk

# On each target Bazzite host:
sudo bootc switch ghcr.io/<you>/bazzite-punktfunk && systemctl reboot
```

> ⚠️ The image build runs `dnf5 copr enable enricobuehler/punktfunk` — so **Path B also depends on
> the COPR being published** (or on you pointing `PUNKTFUNK_COPR` at a COPR you've built yourself).
> If the COPR doesn't exist, the `podman build` fails at the install step.

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
sudo usermod -aG input "$USER"     # then LOG OUT and back in (or reboot)
```

> 🔁 **The group change does not apply to your current login session** — you must re-login (or
> reboot). Until then, gamepad creation fails with a permission error on `/dev/uinput`. This is the
> single most common "why don't my gamepads work" gotcha.

If you installed from a tarball/source instead of the RPM (so the rule isn't in place), install it
manually — the exact commands from the rule file's header (`scripts/60-punktfunk.rules`):

```sh
sudo cp scripts/60-punktfunk.rules /etc/udev/rules.d/
sudo usermod -aG input "$USER"     # then re-login (or reboot)
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
```

Check health and logs:

```sh
systemctl --user status punktfunk-host
journalctl --user -u punktfunk-host -f
```

> **What `serve` actually starts.** The unit's `ExecStart` runs `punktfunk-host serve`, which is the
> **GameStream / Moonlight-compatible** host (mDNS discovery, pairing, RTSP, the fixed GameStream
> ports, **plus the management REST API on 47990**). The native `punktfunk/1` (QUIC) host is a
> *separate* subcommand — `punktfunk-host m3-host` — and is **not** what the bundled systemd unit
> launches. So out of the box on Bazzite you get the **Moonlight-compatible** host.
> (Source: `crates/punktfunk-host/src/main.rs` — `serve` → `gamestream::serve`; `m3-host` is its own
> path.)

> **Unit caveat:** `scripts/punktfunk-host.service` declares only `After=pipewire.service` and (in
> the upstream/dev layout) assumes the binary at `%h/punktfunk/target/release/punktfunk-host`. The
> **RPM-installed** binary lives at `/usr/bin/punktfunk-host`. If `systemctl --user cat
> punktfunk-host` shows `ExecStart` pointing at a missing path in your home dir, drop an override
> (`systemctl --user edit punktfunk-host`) setting `ExecStart=/usr/bin/punktfunk-host serve`.

---

## 6. Firewall

> ⚠️ **There is no firewall script or firewall doc in the repo.** The ports below are derived
> directly from the code constants (`crates/punktfunk-host/src/gamestream/mod.rs`, `mgmt.rs`) and
> the M2 port-map (`docs/m2-plan.md`). Treat the `firewall-cmd` lines as recommended-but-verified,
> not a checked-in script.

**GameStream / Moonlight ports** (fixed; Moonlight derives them from the HTTP base):

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

Open the GameStream ports with `firewalld` (Bazzite uses firewalld):

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

**If you also run the native `punktfunk/1` host** (`punktfunk-host m3-host`, not started by the
default unit):

- **QUIC control plane: UDP 9777** (default `--port`; change with `--port N`).
- **Data plane: an *ephemeral* UDP port** — `m3-host` binds `0.0.0.0:0` and tells the client which
  port it got, so there is **no fixed data port to open**. For a restrictive firewall you'd need to
  allow the ephemeral UDP range; the repo does not pin one.

```sh
# Only if you run `m3-host`:
sudo firewall-cmd --permanent --add-port=9777/udp && sudo firewall-cmd --reload
```

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

**3. (Optional) native punktfunk/1 client** — only if you're running the separate `m3-host`. The
repo's reference client is `punktfunk-client-rs`, e.g. `punktfunk-client-rs --mode 1280x720x120 --out
/tmp/a.h265` (add `--pin HEX` for PIN pairing). This is a headless/decode-to-file reference, not a
desktop viewer.

---

## 8. Troubleshooting (grounded in the repo's real gotchas)

- **Gamepads don't appear / permission denied on `/dev/uinput` or `/dev/uhid`.** You haven't
  re-logged-in after `usermod -aG input`. Log out and back in (or reboot) — group membership only
  takes effect on a new session. (`scripts/60-punktfunk.rules`, `packaging/README.md`.)

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
  `ExecStart=/usr/bin/punktfunk-host serve` if needed (section 5).

- **Moonlight can't see the host.** Ensure UDP 5353 (mDNS) and the GameStream ports are open
  (section 6) and client + host are on the same L2 LAN segment.

---

## Appendix — if the COPR isn't published yet

The COPR (`enricobuehler/punktfunk`) is **operator-run and may not be live**. If `rpm-ostree install
punktfunk` can't find the package, build the RPM yourself on a **Fedora** machine/toolbox (not
Debian/Ubuntu — the host links system FFmpeg/PipeWire and won't build there), per
`packaging/README.md`:

```sh
git archive --format=tar.gz --prefix=punktfunk-0.0.1/ \
  -o ~/rpmbuild/SOURCES/punktfunk-0.0.1.tar.gz HEAD
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
3. The bundled systemd unit runs the **GameStream/Moonlight** `serve` host, **not** the native
   `punktfunk/1` QUIC host (`m3-host` is separate and unmanaged by the unit).
4. The mgmt port (47990) is **loopback-only by default** — don't open it.
