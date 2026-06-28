# punktfunk Decky plugin (SteamOS / Steam Deck)

A **[Decky Loader](https://decky.xyz/)** plugin that adds a **punktfunk** panel to the Steam
Deck's Quick Access Menu (the QAM, opened with the `…` button), so you can launch the
punktfunk streaming client from **Gaming Mode** without dropping to the desktop.

Because Decky plugins run inside Steam's CEF, the panel is built from real Steam UI
primitives (`@decky/ui`: `PanelSection`, `PanelSectionRow`, `ButtonItem`, `Field`,
`Spinner`) — so it looks and feels native to Gaming Mode.

> **Full Gaming-Mode client.** Discovery, a fullscreen page, in-UI SPAKE2 PIN pairing,
> stream settings, and a stream that actually launches fullscreen under gamescope (via a
> Steam shortcut, MoonDeck-style). The video itself is the existing GTK4 flatpak client
> (`io.unom.Punktfunk`) — the plugin discovers, pairs, configures, and *launches it the
> right way* so gamescope focuses it. The Steam-shortcut launch + pairing need a real Deck
> in Gaming Mode to fully confirm.

## What it does

1. **Discover** — browses the LAN over mDNS for punktfunk/1 hosts (`_punktfunk._udp`,
   backend `discover()` via `avahi-browse`). Shown in both the QAM panel and a **fullscreen
   page** (Decky route `/punktfunk`, via `routerHook.addRoute`).
2. **Pair** — for a `pair=required` host: a gamepad-navigable PIN keypad. The operator arms
   pairing on the host (it shows a 4-digit PIN), the user enters it on the Deck, and the
   backend runs the SPAKE2 ceremony headlessly via the flatpak client's `--pair` mode
   (`pair()`), persisting the host as paired so the stream then connects silently.
3. **Stream** — launches fullscreen in Gaming Mode. The plugin registers ONE hidden
   non-Steam shortcut pointing at `bin/punktfunkrun.sh`, passes `PF_HOST` as the shortcut's
   Steam launch options, and starts it with `SteamClient.Apps.RunGame` — so gamescope
   focuses + fullscreens it. (A flatpak launched directly from the backend is invisible:
   gamescope only focuses the process tree Steam launched via `reaper` — gamescope#484.)
   The wrapper then execs `flatpak run io.unom.Punktfunk --connect <host>`.
4. **Settings** — resolution / refresh / bitrate / gamepad / mic, written to the client's
   `client-gtk-settings.json` (`get_settings`/`set_settings`), which the launched client reads.

To leave the stream: the in-client controller chord (**L1+R1+Start+Select**) or close the
"game" from the Steam overlay — exiting the client ends the Steam game and returns to
Gaming Mode automatically.

## Architecture

| File | Role |
| --- | --- |
| `src/index.tsx` | Frontend: QAM panel + the `/punktfunk` fullscreen page (host list, PIN keypad modal, settings). |
| `src/steam.ts` | Steam-shortcut launch (`AddShortcut` / `SetAppLaunchOptions` / `RunGame`) — the focus-correct stream start. |
| `src/backend.ts` | Typed `callable` bridges to `main.py`. |
| `bin/punktfunkrun.sh` | The launch wrapper the Steam shortcut targets (so the window is focusable). |
| `main.py` | Backend: `discover` / `pair` / `runner_info` / `get_settings` / `set_settings` / `kill_stream` / `check_update`. |
| `plugin.json` | Decky plugin manifest. |
| `update.json` | CI-baked `{channel, manifest}` — where `check_update()` polls (absent on dev builds). |
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
- A punktfunk/1 host on the LAN (`punktfunk-host serve` or `punktfunk1-host`).

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

## Updating (self-update, no store)

The plugin updates itself without the official Decky store. CI (`decky.yml`) publishes a tiny
per-channel `manifest.json` next to the zip in the Gitea registry:

```json
{"version":"0.3.123","artifact":".../punktfunk-decky/0.3.123/punktfunk.zip","sha256":"…"}
```

and bakes an `update.json` (`{channel, manifest}`) into the plugin so it knows which channel it was
installed from. The backend `check_update()` reads the **installed** version from `package.json` —
the value Decky itself reports (it does **not** read `plugin.json`) — fetches the channel manifest,
and compares. When a newer build exists the frontend shows an **Update to vX** button that drives
Decky Loader's own install RPC:

```ts
window.DeckyBackend.callable("utilities/install_plugin")(artifact, "punktfunk", version, hash, /*UPDATE=*/2)
```

The loader (root) downloads the immutable per-version zip, **SHA-256-verifies** it against `hash`,
replaces `~/homebrew/plugins/punktfunk`, and hot-reloads — the unprivileged backend never writes the
root-owned plugins dir itself. `window.DeckyBackend` / `utilities/install_plugin` are loader
internals (not `@decky/api`), so every access is guarded; missing them, the button falls back to a
toast pointing at **Install Plugin from URL**.

> CI stamps a **plain numeric** semver per channel (`0.3.<run>` canary, `X.Y.Z` stable) into
> `package.json`. Decky's `compare-versions` orders pre-release identifiers lexically (so `ci10 < ci9`)
> — a `-ciN` suffix would mis-detect updates.

**Optional — native Updates tab:** Decky's store is single-source (a custom store URL *replaces* the
official catalog), so punktfunk doesn't ship one by default. A user who wants the native update badge
can point Decky → Settings → **Custom store** at a punktfunk-only store JSON — not recommended if you
use other plugins, since it hides the official catalog.

## Limitations / next steps

- **Needs on-Deck validation in Gaming Mode**: the Steam-shortcut launch (`AddShortcut` /
  `RunGame` / the `gameId` encoding) and the headless pairing env are coded to MoonDeck's
  proven pattern but verified only at build time here.
- mDNS discovery depends on `avahi-browse`; no manual "add host by IP" entry yet.
- No in-stream overlay (latency/bitrate HUD) inside the plugin — the client owns the session
  once launched; leave it with the L1+R1+Start+Select chord.
- Pairing requires the operator to **arm pairing on the host** (so it shows the PIN); the
  plugin can't arm it remotely (no host mgmt token on the Deck).
- Settings are written to the flatpak's sandbox config path; if the client ever moves its
  config location, that path mapping must follow.
