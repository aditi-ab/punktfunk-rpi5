# punktfunk Decky plugin (SteamOS / Steam Deck)

A **[Decky Loader](https://decky.xyz/)** plugin that adds a **punktfunk** panel to the Steam
Deck's Quick Access Menu (the QAM, opened with the `…` button), so you can launch the
punktfunk streaming client from **Gaming Mode** without dropping to the desktop.

Because Decky plugins run inside Steam's CEF, the panel is built from real Steam UI
primitives (`@decky/ui`: `PanelSection`, `PanelSectionRow`, `ButtonItem`, `Field`,
`Spinner`) — so it looks and feels native to Gaming Mode.

> **Spike / launcher only.** This is a minimal but functional first cut: discover hosts,
> connect, disconnect. It launches the existing native GTK4 client
> (`punktfunk-client`) over the top of Gaming Mode. An in-stream overlay (latency / bitrate
> HUD, mid-session controls) and a fuller real-Steam-components UI are the next steps.
> Runtime behavior on a real Deck is **untested** — only the build is verified here.

## What it does

1. **Refresh** — browses the LAN over mDNS for punktfunk/1 hosts (the `_punktfunk._udp`
   service) via the backend `discover()`.
2. **Lists discovered hosts** — name, `ip:port`, and a lock icon for whether pairing is
   required (`pair=required` in the host's TXT record).
3. **Connect** — selecting a host calls `connect(host, port)`, which launches
   `punktfunk-client --connect host:port`; a toast and the status line reflect the result.
4. **Disconnect** — `disconnect()` terminates the launched client.

## Architecture

| File | Role |
| --- | --- |
| `src/index.tsx` | Frontend QAM panel (`@decky/ui` + `@decky/api`). |
| `main.py` | Backend `Plugin` class: `discover` / `connect` / `disconnect` / `status` exposed over the Decky bridge. |
| `plugin.json` | Decky plugin manifest. |
| `decky.pyi` | Type stub for the injected `decky` module (vendored from the template). |

### Discovery (`discover()`)

Shells out to **`avahi-browse -rpt _punktfunk._udp`** (SteamOS and Bazzite ship
`avahi-daemon`; this avoids bundling python-zeroconf):

- `-r` resolve services, `-p` parseable output, `-t` terminate after the cache dump.
- Resolved records start with `=` and are semicolon-separated:
  `=;iface;protocol;name;type;domain;hostname;address;port;txt`.
- The `txt` column is space-separated, quoted `"key=value"` tokens. We read the keys the
  host advertises (`crates/punktfunk-host/src/discovery.rs`): `proto`, `fp`, `pair`, `id`.
- Records are deduped on the `id` TXT key (a host re-advertises per interface and across
  IPv4/IPv6), preferring the IPv4 address for the user-facing host string.

### Client launch (`connect()`)

The client binary `punktfunk-client` is resolved in order: `PATH` → `/usr/bin` →
`/usr/local/bin` → `~/.local/bin` → a `flatpak run io.unom.Punktfunk` fallback. The resolved
argv and a clear `client-not-found` error surface to the UI. The child PID is tracked so
`disconnect()` (and plugin `_unload`) can terminate it.

> On the **Steam Deck** the client install is the flatpak `io.unom.Punktfunk`
> (`packaging/flatpak/`) — SteamOS `/usr` is read-only and lacks `libadwaita`/`libSDL3`, so
> the flatpak (which bundles them) is the canonical path; the resolver's flatpak fallback
> launches exactly that.

## Prerequisites

- **Decky Loader** installed on the Deck (https://decky.xyz/).
- **`punktfunk-client`** (the GTK4/libadwaita Linux client, crate `punktfunk-client-linux`)
  installed and runnable on the Deck — via `.deb`/RPM/flatpak, or symlinked into
  `~/.local/bin`.
- **avahi** (`avahi-daemon` + `avahi-browse`) for discovery — present on SteamOS/Bazzite.
- A punktfunk/1 host on the LAN (`punktfunk-host serve --native` or `m3-host`).

## Build

```sh
pnpm install
pnpm build      # rollup → dist/index.js
```

(`npm install && npm run build` also works.)

## Install on the Deck

### Option A — Decky "install from URL" (recommended; published by CI)

CI (`.gitea/workflows/decky.yml`) builds the plugin into a store-layout zip and publishes it to
Gitea's **generic package registry** on every push to `main` and on `v*` tags, exposing a stable
URL. In Decky's settings → **Developer Mode** → **Install Plugin from URL**, paste:

```
https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip
```

(or a pinned version: `.../punktfunk-decky/<version>/punktfunk.zip`). On tags the same zip is
also attached to the Gitea release. The zip's layout is the store-required one — a single
top-level `punktfunk/` dir holding `plugin.json`, `package.json`, `main.py`, `dist/index.js`,
`README.md`, and `LICENSE`.

### Option B — manual dev copy (sideload)

Decky's `~/homebrew/plugins/` is **root-owned** (PluginLoader runs as root and manages it), so a
plain `rsync` into it fails — stage to a writable temp dir, then `sudo`-install and restart the
loader. The two helper scripts do exactly this:

```sh
cd clients/decky
pnpm install
pnpm run package                       # → out/punktfunk/ + out/punktfunk-v<ver>.zip
DECK=deck@<deck-ip> pnpm run deploy    # rsync → /tmp, sudo cp into plugins/, chown root, restart
```

`deploy.sh` prompts for the Deck's sudo password interactively (via `ssh -t`); set `DECKPASS=…`
to run it non-interactively. Equivalent by hand:

```sh
cd clients/decky && pnpm build && bash scripts/package.sh
rsync -azp --delete out/punktfunk/ deck@<deck-ip>:/tmp/punktfunk/
ssh -t deck@<deck-ip> 'sudo sh -c "rm -rf ~deck/homebrew/plugins/punktfunk && \
  cp -r /tmp/punktfunk ~deck/homebrew/plugins/punktfunk && \
  chown -R root:root ~deck/homebrew/plugins/punktfunk && systemctl restart plugin_loader"'
```

A loader restart is required for an out-of-band install to appear. The **punktfunk** panel then
shows up in the Quick Access Menu.

> The plugin launches the client via the flatpak `io.unom.Punktfunk` (see
> [`../../packaging/flatpak/README.md`](../../packaging/flatpak/README.md)) — install that on
> the Deck too, or the panel's Connect surfaces a `client-not-found` error.

## Limitations / next steps

- Launcher only — no in-stream overlay yet; the client owns the full session once launched.
- mDNS discovery depends on `avahi-browse`; no manual "add host by IP" entry yet.
- Pairing (PIN ceremony) is handled by the launched client, not the panel.
- Not yet tested on real Deck hardware.
