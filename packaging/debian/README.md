# punktfunk-host — Debian/Ubuntu package (apt)

`punktfunk-host` is published as a `.deb` to **Gitea's Debian package registry** in the public
`unom` org, so the Ubuntu hosts update with plain `apt`. CI (`.gitea/workflows/deb.yml`) builds
and publishes on every push to `main` (a rolling `0.0.1~ciN.<sha>` build) and on `v*` tags
(a clean `X.Y.Z`).

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
