# punktfunk — Steam Deck plugin (Decky)

Stream to your **Steam Deck** without ever leaving Gaming Mode. This
**[Decky Loader](https://decky.xyz/)** plugin adds a **punktfunk** panel to the Quick Access Menu
(the `…` button): discover hosts on your network, pair with a PIN, tweak stream settings, and launch
a fullscreen, gamescope-focused stream — all from the couch, gamepad-navigable.

The video itself is the native GTK4 Linux client (the `io.unom.Punktfunk` flatpak); the plugin
discovers, pairs, configures, and *launches it the right way* so gamescope fullscreens it — the same
Steam-shortcut trick MoonDeck uses. Because it's built from real Steam UI primitives (`@decky/ui`),
the panel looks and feels native to Gaming Mode.

## What it does

1. **Discover** — browses the LAN over mDNS for punktfunk hosts, in both the QAM panel and a
   fullscreen page.
2. **Pair** — for a host that requires it, a gamepad-navigable PIN keypad runs the SPAKE2 pairing
   ceremony headlessly, then remembers the host so future streams connect silently.
3. **Stream** — launches fullscreen via a hidden Steam shortcut so gamescope focuses it.
4. **Settings** — resolution / refresh / bitrate / gamepad / mic, written to the client's config.

To leave a stream: the in-client controller chord (**L1 + R1 + Start + Select**), or close the
"game" from the Steam overlay — either returns you to Gaming Mode.

## Install on the Deck

You need **[Decky Loader](https://decky.xyz/)** and the **`io.unom.Punktfunk` flatpak**
([`packaging/flatpak`](../../packaging/flatpak/README.md)) installed on the Deck — SteamOS `/usr` is
read-only, so the flatpak (which bundles libadwaita/SDL3) is the canonical client. Discovery uses
`avahi-browse`, which ships on SteamOS/Bazzite.

**Recommended — install from URL** (published by CI): in Decky → Settings → **Developer Mode** →
**Install Plugin from URL**, paste:

```
https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip
```

(or a pinned `.../punktfunk-decky/<version>/punktfunk.zip`). The plugin then **self-updates** without
the Decky store — when a newer build exists, an **Update to vX** button appears and drives Decky
Loader's own (SHA-256-verified) install.

## Build & sideload (development)

```sh
cd clients/decky
pnpm install
pnpm build                             # rollup → dist/index.js
pnpm run package                       # → out/punktfunk/ + out/punktfunk-v<ver>.zip
DECK=deck@<deck-ip> pnpm run deploy    # rsync → /tmp, sudo-install into the root-owned plugins dir, restart loader
```

`~/homebrew/plugins/` is root-owned (the loader runs as root), so `deploy.sh` stages to a temp dir
then `sudo`-installs and restarts the loader — set `DECKPASS=…` to run it non-interactively. A loader
restart is required for an out-of-band install to appear.

## Architecture

| File | Role |
| --- | --- |
| `src/index.tsx` | Frontend: QAM panel + the `/punktfunk` fullscreen page (host list, PIN keypad, settings). |
| `src/steam.ts` | Steam-shortcut launch (`AddShortcut` / `SetAppLaunchOptions` / `RunGame`) — the focus-correct stream start. |
| `src/backend.ts` | Typed `callable` bridges to `main.py`. |
| `bin/punktfunkrun.sh` | The launch wrapper the Steam shortcut targets (so the window is focusable). |
| `main.py` | Backend: `discover` (via `avahi-browse`) / `pair` / settings / `kill_stream` / `check_update`. |
| `plugin.json` · `update.json` | Decky manifest; CI-baked update channel. |

The client binary is resolved `PATH` → `/usr/bin` → `/usr/local/bin` → `~/.local/bin` → a
`flatpak run io.unom.Punktfunk` fallback, so the flatpak install always works.

## Limitations / next steps

- **Needs on-Deck validation in Gaming Mode** — the Steam-shortcut launch and headless pairing follow
  MoonDeck's proven pattern but are verified only at build time here.
- No manual "add host by IP" entry yet (discovery is mDNS-only).
- No in-stream overlay inside the plugin — the client owns the session once launched.
- Pairing needs the operator to **arm pairing on the host** so it shows the PIN; the plugin can't arm
  it remotely.

## Related

- **[Documentation](https://docs.punktfunk.unom.io/docs/steam-deck)** — Steam Deck setup guide
- **[Linux client](../linux/README.md)** — the app this plugin launches
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
