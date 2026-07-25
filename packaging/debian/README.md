# punktfunk-host — Debian/Ubuntu package (apt)

`punktfunk-host` is published as a `.deb` to **Gitea's Debian package registry** in the public
`unom` org, so the Ubuntu hosts update with plain `apt`. CI (`.gitea/workflows/deb.yml`) builds
and publishes on every push to `main` (a rolling `<next-minor>~ciN.g<sha>` build — the base is
derived from the latest stable tag by `scripts/ci/pf-version.sh` — to the **`canary`** apt
distribution) and on `vX.Y.Z` tags (a clean `X.Y.Z` to the **`stable`** distribution, plus attached
to the unified Gitea Release). The two are separate apt distributions, so a stable box never jumps
to a canary build — see [Release Channels](https://punktfunk.unom.io/docs/channels). The repo line
below subscribes to `stable`; swap `stable` → `canary` for the latest main builds.

The same workflow also publishes **`punktfunk-web`** (the browser management console — pairing +
status) and **`punktfunk-client`** (the native GTK4/libadwaita Linux client). `punktfunk-host` **Recommends**
`punktfunk-web`, so a default `apt install punktfunk-host` pulls the console too (alongside the
udev/sysctl bits) unless you've disabled weak deps; `punktfunk-client` is independent — install it
on the box you stream *to*. (`punktfunk-probe` is the headless reference/test tool, not packaged
here.)

Package layout mirrors the Fedora RPM (`../rpm/punktfunk.spec`): the host binary, the `/dev/uinput`
udev rule, the systemd **user** unit, headless session helpers, the example config, and the OpenAPI
doc. Runtime `Depends` are computed by `dpkg-shlibdeps` from the binary itself. The NVIDIA driver
(`libnvidia-encode` / `libEGL_nvidia` / `libcuda`) is **not** a dependency — it's installed out of
band, like on the RPM side.

## Ubuntu 24.04 LTS (and why it needs a special build)

`punktfunk-host` needs **FFmpeg 8** (libavcodec62), but Ubuntu 24.04 LTS ships FFmpeg 6.1
(libavcodec60). So a host `.deb` built the obvious way — on the same Ubuntu 26.04 image as the
client (`ci/rust-ci.Dockerfile`) — declares `Depends: libavcodec62, …` and a glibc-2.41 floor that
24.04's apt can't satisfy ("the required packages are too recent"). To fix that, the host `.deb` is
instead built on an **Ubuntu 24.04 image** (`ci/rust-ci-noble.Dockerfile`) that carries a from-source
FFmpeg 8, and that FFmpeg is **bundled into the package** (`build-deb.sh BUNDLE_FFMPEG=1` → the
libav* land in `/usr/lib/punktfunk-host`, the binary's rpath points there, and the libav* sonames are
dropped from `Depends`). The result is **one** host `.deb` that installs on **Ubuntu 24.04 LTS through
26.04** (glibc floor 2.39; no distro-FFmpeg dependency). The client/web/scripting `.deb`s still build
on 26.04 (the native client needs SDL3 / GTK4 ≥ 4.20, absent on 24.04) — install the client on the box
you stream *to*, which is independent of the host's distro.

## Install on a host (one-time)

The registry is public, so no apt auth is needed — just trust the repo's signing key:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://git.unom.io/api/packages/unom/debian/repository.key \
  | sudo tee /etc/apt/keyrings/punktfunk.asc >/dev/null

echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] https://git.unom.io/api/packages/unom/debian stable main" \
  | sudo tee /etc/apt/sources.list.d/punktfunk.list

sudo apt update
sudo apt install punktfunk-host
```

Then, as the desktop user:

```sh
sudo usermod -aG input "$USER"          # virtual gamepads (re-login to take effect)
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk-host/host.env.example ~/.config/punktfunk/host.env   # then edit
systemctl --user enable --now punktfunk-host
# Web console — enable it and read the auto-generated login password (then open https://<host-ip>:47992):
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

## Firewall

**Debian ships no firewall and Ubuntu's `ufw` is installed-but-inactive by default**, so out of the
box there is nothing to open. If you turn one on, the `punktfunk-host` package ships a one-liner
opener for both **ufw** and **firewalld** (neither auto-enabled):

```sh
# ufw (Ubuntu) — profile at /etc/ufw/applications.d/punktfunk, read at once (no reload):
sudo ufw allow punktfunk-native        # the default native host
sudo ufw allow punktfunk-gamestream    # …add for Moonlight compat

# firewalld — service definitions at /usr/lib/firewalld/services/:
sudo firewall-cmd --reload                                          # load the installed definition
sudo firewall-cmd --permanent --add-service=punktfunk-native
#                              --add-service=punktfunk-gamestream    # …add for Moonlight compat
sudo firewall-cmd --reload
```

If you installed the **web console** (`punktfunk-web`) and want it reachable from another device,
open its port with the matching one-liner — `sudo ufw allow punktfunk-web` or `sudo firewall-cmd
--permanent --add-service=punktfunk-web && sudo firewall-cmd --reload` — which opens **TCP 47992**
(HTTPS, login-gated). The mgmt API (47990) is opened for paired clients by the `punktfunk-native`
profile (game-library browsing over mTLS); off-loopback it serves only read-only status/library and
keeps admin loopback-only.

Prefer explicit rules? Open the ports directly. The **native `punktfunk/1`** plane:

- **QUIC control plane: UDP 9777** (`serve --native-port N` to change).
- **Data plane: a separate UDP port.** By default it's *random* — the host binds `0.0.0.0:0` and
  tells the client which port it got. Video flows host → client, but the **client sends the first
  packet** (a hole-punch), so the host learns the client's real source and streams back — this
  traverses NAT / inter-VLAN with no forwarded port. **You normally don't open it:** if a deny-inbound
  firewall drops the punch, the host waits ~2.5 s and falls back to the client-reported address, and a
  stateful firewall then admits the return (it just adds ~2.5 s to session start). To skip that delay,
  pin it with **`serve --data-port <PORT>`** (or `PUNKTFUNK_DATA_PORT`): the host binds that fixed
  port and streams direct (no punch-wait) — open exactly that one port. A fixed port serves one
  session at a time (concurrent ones fall back to random + hole-punch), and direct mode needs the
  client's reported address to be reachable (flat LAN / a non-remapping port-forward).

And the **GameStream / Moonlight** ports (fixed) — only needed if you run the host with
`serve --gamestream` (opt-in, trusted LAN only); bare `serve` is native-only and doesn't open these:

| Port | Proto | Purpose |
|---|---|---|
| 47984 | TCP | HTTPS nvhttp (paired, mutual-TLS) |
| 47989 | TCP | HTTP nvhttp (`/serverinfo`, `/pair` PIN flow) |
| 48010 | TCP | RTSP handshake |
| 47998–48010 | UDP | Video RTP (+ FEC), ENet control (47999), audio (48000) |
| 5353 | UDP | mDNS auto-discovery |

The mgmt API (TCP 47990, HTTPS + mTLS) binds all interfaces by default so paired clients can browse the
game library — the `punktfunk-native` profile opens it. Off-loopback it serves only read-only
status/library to a paired client cert; the admin surface stays loopback-only. Pass
`--mgmt-bind 127.0.0.1:47990` to keep it loopback-only (then leave 47990 closed).

With `ufw` (explicit ports, instead of the shipped profile):

```sh
sudo ufw allow 9777/udp                                 # punktfunk/1 control plane
sudo ufw allow 47990/tcp                                # mgmt/library API (HTTPS + mTLS; LAN = read-only, paired)
sudo ufw allow 47984/tcp && sudo ufw allow 47989/tcp && sudo ufw allow 48010/tcp
sudo ufw allow 47998,47999,48000/udp                    # GameStream video/control/audio
sudo ufw allow 5353/udp                                 # mDNS discovery
# The punktfunk/1 data plane uses a random UDP port; leave it closed on a LAN — the host hole-punches
# and falls back (~2.5s at session start if firewalled). To skip that, pin it: `serve --data-port
# 9778` and `ufw allow 9778/udp`.
```

With raw `nftables` (add to your `inet filter input` chain):

```
udp dport 9777 accept                  # punktfunk/1 control plane
tcp dport 47990 accept                 # mgmt/library API (HTTPS + mTLS; LAN = read-only, paired)
tcp dport { 47984, 47989, 48010 } accept
udp dport { 47998-48010, 5353 } accept
# The punktfunk/1 data plane is a random UDP port — normally left closed (hole-punch + ~2.5s
# fallback). Pin it with `serve --data-port <PORT>` to open exactly one instead.
```

## Updates

```sh
sudo apt update && sudo apt upgrade        # picks up the newest published build
systemctl --user restart punktfunk-host    # if the unit was already running
```

## Build a `.deb` locally

```sh
VERSION=0.0.1 bash packaging/debian/build-deb.sh   # -> dist/punktfunk-host_0.0.1_amd64.deb
```

Needs `dpkg-dev` (`dpkg-shlibdeps`, `dpkg-deb`). It builds the release binary first if missing.
Building on a GPU box is fine — the NVIDIA driver lib is filtered out either way.

That plain invocation hard-depends on the build box's system FFmpeg, so it only installs on a box
with the same libav* soname. For the **universal** package CI ships (installs on 24.04 LTS → 26.04),
build it in the noble image with FFmpeg bundled:

```sh
docker build -f ci/rust-ci-noble.Dockerfile -t pf-noble ci
docker run --rm -v "$PWD:/src" -w /src pf-noble \
  bash -lc 'VERSION=0.0.1 BUNDLE_FFMPEG=1 bash packaging/debian/build-deb.sh'
```

`BUNDLE_FFMPEG=1` needs `patchelf` and an FFmpeg install at `FFMPEG_PREFIX` (default `/opt/ffmpeg`,
which the noble image provides).

### The arm64 client `.deb`

The **client** also ships for arm64 (`punktfunk-client_<version>_arm64.deb`, published to the same
apt distribution — the registry keys pool entries by architecture, so an arm64 box needs no extra
configuration). There is no arm64 **host** package: the Linux host encodes with NVENC/QSV/AMF, all
x86.

It is cross-compiled on an ordinary amd64 machine in `ci/rust-ci-arm64cross.Dockerfile` — the
rust-ci toolchain plus an Ubuntu ports arm64 multiarch sysroot. No arm64 runner is involved:

```sh
docker build -f ci/rust-ci-arm64cross.Dockerfile -t pf-arm64cross .   # repo-root context
docker run --rm -v "$PWD:/w" -w /w pf-arm64cross \
  bash -lc 'VERSION=0.0.1 ARCH=arm64 TARGET=aarch64-unknown-linux-gnu \
              bash packaging/debian/build-client-deb.sh'
```

`TARGET` moves the binaries to `target/<triple>/release`; `ARCH` sets the package's
`Architecture:` field. Set both — one without the other builds an amd64 binary into a package
labelled arm64, or vice versa. `dpkg-shlibdeps` reads the arm64 sonames straight out of the
multiarch sysroot, so `Depends:` comes out right with no manual list.
