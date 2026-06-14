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
`/usr/local/bin` → `~/.local/bin` → a `flatpak run earth.buehler.punktfunk.Client`
fallback. The resolved argv and a clear `client-not-found` error surface to the UI. The
child PID is tracked so `disconnect()` (and plugin `_unload`) can terminate it.

> **TODO:** pin the canonical SteamOS install path once a Deck packaging story for
> `punktfunk-client` is settled (likely a flatpak, since SteamOS `/usr` is read-only).

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

Copy the built plugin directory to the Deck and restart Decky:

```sh
# the dir must contain: dist/, main.py, plugin.json, package.json
rsync -a --exclude node_modules clients/decky/ deck@<deck-ip>:~/homebrew/plugins/punktfunk/
# then, on the Deck, restart Decky Loader (Settings → Developer → "Restart" / reboot)
```

The **punktfunk** panel then appears in the Quick Access Menu.

## Limitations / next steps

- Launcher only — no in-stream overlay yet; the client owns the full session once launched.
- mDNS discovery depends on `avahi-browse`; no manual "add host by IP" entry yet.
- Pairing (PIN ceremony) is handled by the launched client, not the panel.
- Not yet tested on real Deck hardware.
