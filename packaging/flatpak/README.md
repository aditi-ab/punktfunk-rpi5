# punktfunk client — Flatpak (Steam Deck / SteamOS, and any flatpak distro)

The native Linux **client** (crate `punktfunk-client-linux`, binary `punktfunk-client`) is
published as a single-file **`.flatpak` bundle** to **Gitea's generic package registry** in
the public `unom` org. CI (`.gitea/workflows/flatpak.yml`) builds and publishes on every push
to `main` (a rolling `0.0.1-ciN.<sha>` build) and on `v*` tags (a clean `X.Y.Z`), and on tags
also attaches the bundle to the Gitea release.

> The **host** is NOT a flatpak (it needs unsandboxed `/dev/uinput` + zero-copy NVENC — see
> [`../README.md`](../README.md) "Why not Flatpak"). Only the **client** is sandbox-friendly.

## Why flatpak for the Steam Deck

SteamOS `/usr` is read-only and image-based, and the system is **missing `libadwaita` and
`libSDL3`** — so a bare `punktfunk-client` binary dropped into `~/.local/bin` won't run. Flatpak
is the Deck's native, update-survivable app path (the user already runs Moonlight and chiaki-ng
as flatpaks), and the bundle carries libadwaita (from `org.gnome.Platform//50`) + a bundled SDL3,
with HEVC-capable FFmpeg supplied automatically by the runtime's `codecs-extra` extension.

App id: **`io.unom.Punktfunk`** (matches the Apple bundle id family and the Decky plugin's
flatpak fallback).

## Install on the Deck (one-time)

The generic registry is a plain HTTP file store, so just download the bundle and install it
per-user (no root, survives SteamOS updates):

```sh
# Pick a version: a tag like 1.2.3, or the newest main build's 0.0.1-ciN.gSHA.
VER=1.2.3
URL="https://git.unom.io/api/packages/unom/generic/punktfunk-client-flatpak/$VER/punktfunk-client-$VER.flatpak"

# Flathub must be enabled (it is on the Deck) so the GNOME runtime + ffmpeg-full pull in:
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo

curl -fL -o /tmp/punktfunk-client.flatpak "$URL"
flatpak install --user --bundle /tmp/punktfunk-client.flatpak
```

Run it:

```sh
flatpak run io.unom.Punktfunk                 # GUI host list (mDNS)
flatpak run io.unom.Punktfunk --connect HOST:PORT
```

The **Decky plugin** launches exactly this (`flatpak run io.unom.Punktfunk --connect …`) once
installed — see [`../../clients/decky/README.md`](../../clients/decky/README.md).

## Updates

A bundle has no remote to track, so updates are "download the newer bundle and reinstall":

```sh
flatpak install --user --bundle /tmp/punktfunk-client.flatpak   # same command, newer file
```

(If you want `flatpak update` to track new builds automatically you'd need a hosted OSTree
repo, which Gitea cannot serve — see "Alternatives" below. The bundle is the simplest path.)

## Build locally / the CI fallback

CI builds this in a **`--privileged`** Fedora container, because `flatpak-builder` runs
`bubblewrap`, which needs user namespaces the default Docker executor denies. **If the Gitea
runner can't grant `--privileged`** (the job fails at `flatpak-builder` with
*"Creating new namespace failed: Operation not permitted"*), build it out-of-band and upload
by hand. The easiest place is **on the Deck itself** (it can run `org.flatpak.Builder`
user-scope, no root):

```sh
# On the Deck (or any flatpak box), one-time:
flatpak install --user -y flathub org.flatpak.Builder

# build-flatpak.sh auto-detects org.flatpak.Builder, generates cargo-sources.json (or reuses an
# existing one — see below), builds, and exports dist/punktfunk-client-<version>.flatpak:
bash packaging/flatpak/build-flatpak.sh

# Upload to the generic registry (PAT with write:package):
curl -fsS --user "enricobuehler:$REGISTRY_TOKEN" \
  --upload-file dist/punktfunk-client-*.flatpak \
  "https://git.unom.io/api/packages/unom/generic/punktfunk-client-flatpak/0.0.1-manual/punktfunk-client.flatpak"
```

> `cargo-sources.json` generation needs `python3` + `aiohttp` + `tomlkit`, which the Deck lacks.
> Generate it on a dev box (`build-flatpak.sh` does it, or run the upstream
> `flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json`), rsync it next
> to the manifest, and `build-flatpak.sh` reuses it (it only regenerates when the file is absent
> or `FORCE_GEN=1`).

> The Mac build host **cannot** build a Linux flatpak (no flatpak-builder for macOS), and
> home-worker-2 has no flatpak and no passwordless sudo to install it — so the Deck or the
> privileged CI container are the only two viable build sites.

## Manifest

[`io.unom.Punktfunk.yml`](io.unom.Punktfunk.yml). Runtime `org.gnome.Platform//50`
(GTK 4.20 + libadwaita 1.8 ≥ the crate floors of v4_16 / v1_5), built on freedesktop-sdk 25.08,
with two build-time SDK extensions: `org.freedesktop.Sdk.Extension.rust-stable` (→ //25.08,
**rustc 1.96** — the GTK4 dep chain, e.g. pango-sys 0.22, needs ≥ 1.92, which the EOL GNOME-48 /
24.08 rust-stable at 1.89 could not provide) and `org.freedesktop.Sdk.Extension.llvm20` (libclang,
needed by bindgen in ffmpeg-sys-next / sdl3-sys). HEVC-capable libavcodec (soname 61, accepted by
ffmpeg-next 8.x) is supplied **automatically at runtime** by the freedesktop `codecs-extra`
extension point (auto-downloaded with the runtime; no app-side codec declaration). A bundled
**SDL3 3.4.10** module (pinned to match `sdl3-sys 0.6.6+SDL-3.4.10`), and finish-args for Wayland +
`--device=all` (GPU/VAAPI render node + evdev + the hidraw char-devices SDL3 needs for DualSense)
+ `--socket=pulseaudio` (PipeWire-pulse: playback + mic) + `--share=network`. Alongside it:
`io.unom.Punktfunk.desktop`, `io.unom.Punktfunk.metainfo.xml`, `io.unom.Punktfunk.svg` (all
installed by the manifest). `cargo-sources.json` (the offline crate cache) is a pure function of
`Cargo.lock`; CI regenerates it each build and it is **gitignored** — generate it on any box with
network + `python3`/`aiohttp`/`tomlkit` (`build-flatpak.sh` does this automatically) and, for a
build host that lacks those (the Deck), rsync the generated file in alongside the manifest.

## Alternatives considered (and why the bundle wins)

- **Generic registry bundle (chosen):** one curl to publish, one `flatpak install --bundle` to
  consume; mirrors the existing deb/rpm curl-upload pattern exactly. No auto-update.
- **Release attachment:** also done on tags (the bundle is attached to the Gitea release), good
  for a human-facing download page; the generic registry gives the stable per-version URL the
  Decky fallback and scripts use.
- **Self-hosted OSTree repo (rejected):** would enable `flatpak update`, but Gitea has no
  flatpak/ostree registry, so it would mean serving a static OSTree tree over Gitea raw/Pages —
  more moving parts than the appliance needs today.
