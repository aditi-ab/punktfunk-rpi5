---
title: Ubuntu — KDE Plasma
description: Set up a punktfunk host on Ubuntu with KDE Plasma (KWin).
---

Set up a punktfunk host on **Ubuntu** running **KDE Plasma**. The host uses KDE's KWin compositor to
create a per-client virtual display. Needs **KWin 6.5.6 or newer**.

> New to this? Skim [Requirements](/docs/requirements) first.

## 1. NVIDIA driver

Identical to the GNOME guide — follow **step 1** of
[Ubuntu — GNOME](/docs/ubuntu-gnome#1-nvidia-driver): install the NVIDIA driver **and** the
`libnvidia-gl-<version>` userspace, enable `nvidia-drm modeset=1`, reboot, and verify with
`nvidia-smi`.

## 2. Install the host (apt)

The host is published as a `.deb` to the public Gitea apt registry — install and update with plain
`apt`. Trust the repo's signing key, add the repo, and install:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://git.unom.io/api/packages/unom/debian/repository.key \
  | sudo tee /etc/apt/keyrings/punktfunk.asc >/dev/null

echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] https://git.unom.io/api/packages/unom/debian stable main" \
  | sudo tee /etc/apt/sources.list.d/punktfunk.list

sudo apt update
sudo apt install punktfunk-host
```

This also pulls the web console (`punktfunk-web`) via `Recommends` (the pairing/status UI). The
desktop *client* — `punktfunk-client`, for the machine you stream *to* — is a separate package, not
needed on a host. The NVIDIA driver stays out of band (step 1). Updates later are just
`sudo apt update && sudo apt upgrade`.

## 3. Configure

The package ships the systemd **user** unit, the udev rule, and the sysctl tuning. As the desktop
user, grant gamepad access and write the KDE config:

```sh
sudo usermod -aG input "$USER"     # /dev/uinput for virtual gamepads (re-login to apply)
mkdir -p ~/.config/punktfunk
cat > ~/.config/punktfunk/host.env <<'ENV'
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=KDE
PUNKTFUNK_COMPOSITOR=kwin
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
ENV
```

> Make sure you're on a **KDE Wayland** session (not X11) — the picker on the login screen. The
> virtual-display path is Wayland-only. See the [Configuration reference](/docs/configuration) for
> every option.

## 4. Run

Start the host as a user service from **inside your Plasma session**:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f      # watch it come up + print its fingerprint
```

The host listens on UDP `9777` (native punktfunk/1) plus the GameStream ports and advertises over
mDNS. It requires **PIN pairing** by default — arm pairing from the web console and pair once from
your [client](/docs/clients).

### Web console

```sh
systemctl --user enable --now punktfunk-web
# read the auto-generated login password, then open http://<host-ip>:3000
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

To run it at boot — including fully **headless**, with KWin brought up automatically and no login —
see [Running as a Service](/docs/running-as-a-service); the headless appliance is built around KDE.

## Troubleshooting

- **KWin too old:** virtual outputs need KWin **≥ 6.5.6**. Check with `kwin_wayland --version`.
- **No picture / capture fails:** confirm you're on a Wayland session and the NVIDIA GL userspace is
  installed (`libnvidia-gl-<version>`). More in [Troubleshooting](/docs/troubleshooting).

## Appendix — build from source

If the apt registry has no build for your release, compile the host yourself (no clean updates / no
packaged units). Install the build toolchain and runtime libraries — the same `apt` line as the
[GNOME build-from-source appendix](/docs/ubuntu-gnome#appendix--build-from-source) — then:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release -p punktfunk-host
```

Write `~/.config/punktfunk/host.env` as in step 3, then run it inside your Plasma session:

```sh
cargo run --release -p punktfunk-host -- serve --native
```
