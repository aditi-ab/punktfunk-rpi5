# punktfunk-host — Debian/Ubuntu package (apt)

`punktfunk-host` is published as a `.deb` to **Gitea's Debian package registry** in the public
`unom` org, so the Ubuntu hosts update with plain `apt`. CI (`.gitea/workflows/deb.yml`) builds
and publishes on every push to `main` (a rolling `0.2.0~ciN.g<sha>` build) and on `host-v*` tags
(a clean `X.Y.Z`) — the rolling builds outrank the stray `0.1.1`, so plain `apt upgrade` always
gets the latest (no version pin needed).

The same workflow also publishes **`punktfunk-web`** (the browser management console — pairing +
status) and **`punktfunk-client`** (the GTK4 couch/Deck client). `punktfunk-host` **Recommends**
`punktfunk-web`, so a default `apt install punktfunk-host` pulls the console too (alongside the
udev/sysctl bits) unless you've disabled weak deps; `punktfunk-client` is independent — install it
on the box you stream *to*. (`punktfunk-probe` is the headless reference/test tool, not packaged
here.)

Package layout mirrors the Fedora RPM (`../rpm/punktfunk.spec`): the host binary, the `/dev/uinput`
udev rule, the systemd **user** unit, headless session helpers, the example config, and the OpenAPI
doc. Runtime `Depends` are computed by `dpkg-shlibdeps` from the binary itself (built in the Ubuntu
26.04 rust-ci image, so the lib soname package names match the target). The NVIDIA driver
(`libnvidia-encode` / `libEGL_nvidia` / `libcuda`) is **not** a dependency — it's installed out of
band, like on the RPM side.

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
# Web console — enable it and read the auto-generated login password (then open http://<host-ip>:3000):
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

## Firewall

Open the ports the host listens on. The **native `punktfunk/1`** plane:

- **QUIC control plane: UDP 9777** (`serve --native-port N` to change).
- **Data plane: an *ephemeral* UDP port** — negotiated per session, so there is no fixed port to
  open. For a restrictive firewall you'd need to allow a UDP range (the repo does not pin one).

And the **GameStream / Moonlight** ports (fixed) — only needed if you run the host with
`serve --gamestream` (opt-in, trusted LAN only); bare `serve` is native-only and doesn't open these:

| Port | Proto | Purpose |
|---|---|---|
| 47984 | TCP | HTTPS nvhttp (paired, mutual-TLS) |
| 47989 | TCP | HTTP nvhttp (`/serverinfo`, `/pair` PIN flow) |
| 48010 | TCP | RTSP handshake |
| 47998–48010 | UDP | Video RTP (+ FEC), ENet control (47999), audio (48000) |
| 5353 | UDP | mDNS auto-discovery |

The mgmt API (TCP 47990) binds to loopback by default — leave it closed unless you move it off
loopback with `--mgmt-bind IP:PORT` (which then requires `--mgmt-token`).

With `ufw`:

```sh
sudo ufw allow 9777/udp                                 # punktfunk/1 control plane
sudo ufw allow 47984/tcp && sudo ufw allow 47989/tcp && sudo ufw allow 48010/tcp
sudo ufw allow 47998:48010/udp
sudo ufw allow 5353/udp
# plus the ephemeral punktfunk/1 data port — open a UDP range you reserve for it.
```

With raw `nftables` (add to your `inet filter input` chain):

```
udp dport 9777 accept                  # punktfunk/1 control plane
tcp dport { 47984, 47989, 48010 } accept
udp dport { 47998-48010, 5353 } accept
# plus the ephemeral punktfunk/1 data port (a reserved UDP range).
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
Build it in the rust-ci image (or on an Ubuntu 26.04 box) so the resolved `Depends` match the
hosts; building on a GPU box is fine — the NVIDIA driver lib is filtered out either way.
