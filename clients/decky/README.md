# Punktfunk — Steam Deck plugin (Decky)

Stream to your **Steam Deck** without ever leaving Gaming Mode. This
**[Decky Loader](https://decky.xyz/)** plugin adds a **Punktfunk** panel to the Quick Access Menu
(the `…` button): discover hosts on your network, pair with a PIN, tweak stream settings, and launch
a fullscreen, gamescope-focused stream — all from the couch, gamepad-navigable.

The video itself is the native GTK4 Linux client (the `io.unom.Punktfunk` flatpak); the plugin
discovers, pairs, configures, and *launches it the right way* so gamescope fullscreens it — the same
Steam-shortcut trick MoonDeck uses. Because it's built from real Steam UI primitives (`@decky/ui`),
the panel looks and feels native to Gaming Mode.

## What it does

1. **Discover** — browses the LAN over mDNS for Punktfunk hosts, in both the QAM panel and a
   fullscreen page; each host row opens a details view (address, pairing policy, certificate
   fingerprint to cross-check against the host's log).
2. **Pair** — for a host that requires it, a gamepad-navigable PIN keypad runs the SPAKE2 pairing
   ceremony headlessly, then remembers the host so future streams connect silently.
3. **Stream** — launches fullscreen via a branded "Punktfunk" Steam shortcut so gamescope focuses it.
4. **Games** — each host row has a games button that opens its **library picker**: pin titles as
   one-tap "Stream <Game>" rows in the QAM (jump straight into e.g. Playnite on the host), or
   **"Open library on screen"** to launch the client's controller-driven, console-style library
   browser (aurora backdrop + poster coverflow; A plays, B returns to Gaming Mode). Pins survive
   plugin reinstalls (stored next to the client's config) and follow a host across IP changes
   (matched by certificate fingerprint).
5. **Settings** — resolution / refresh / bitrate / gamepad type / host compositor / mic, written
   to the client's config.
6. **About** — plugin version, an explicit "Check for updates" button, the setup-guide link, and
   a force-stop for a wedged stream client.

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
https://unom.io/pf-decky
```

(short link for `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip`;
for a pinned version use `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/<version>/punktfunk.zip`
directly). The plugin then **self-updates** without
the Decky store — when a newer build exists, an **Update** button appears and drives Decky
Loader's own (SHA-256-verified) install. Installs and updates can take a couple of minutes on some
networks: Decky's installer also contacts its plugin store first, which may be slow or blackholed
before the actual download proceeds.

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
| `src/index.tsx` | Plugin entry: the QAM panel + route registration. |
| `src/page.tsx` | The `/punktfunk` fullscreen page — Hosts (with per-host details) / Settings / About tabs. |
| `src/settings.tsx` · `src/pair.tsx` | Stream-settings section; the gamepad-navigable PIN-pairing modal. |
| `src/library.tsx` | The per-host game picker (pin/unpin, "Open library on screen") + the pinned-game launch helper. |
| `src/hooks.ts` · `src/boundary.tsx` | Shared discovery/update/pins hooks + actions; the render error boundary. |
| `src/steam.ts` | Steam-shortcut launch (`AddShortcut` / `SetAppLaunchOptions` / `RunGame`) — the focus-correct stream start. The shortcut's exe is `/bin/sh` with the wrapper passed as an argument, so the script never needs an exec bit (Decky's zip extraction drops it and the root-owned plugins dir can't be chmodded by the unprivileged backend). Launch extras ride env-prefix tokens: `PF_LAUNCH=<id>` (pinned game) / `PF_BROWSE=1` + `PF_MGMT=<port>` (on-screen library); ids are validated space/quote-free at pin AND launch time. |
| `src/backend.ts` | Typed `callable` bridges to `main.py`. |
| `bin/punktfunkrun.sh` | The launch wrapper the Steam shortcut runs (so the window is focusable); maps `PF_LAUNCH`/`PF_BROWSE`/`PF_MGMT` to `--launch`/`--browse`/`--mgmt`. An older flatpak ignores the flags harmlessly (plain stream / hosts page). |
| `main.py` | Backend: `discover` (via `avahi-browse`) / `pair` / `library` (headless flatpak `--library`, TSV) / pins store (`decky-pinned.json`) / settings / `kill_stream` / `check_update` (with an explicit CA-bundle search — Decky's embedded Python has no usable default TLS roots on SteamOS). |
| `scripts/test-backend.py` | Stdlib-only checks for the backend's pure parsers (TSV, error classes, avahi TXT) + the pins round trip. |
| `plugin.json` · `update.json` | Decky manifest; CI-baked update channel. |

## Limitations / next steps

- No manual "add host by IP" entry yet (discovery is mDNS-only).
- No in-stream overlay inside the plugin — the client owns the session once launched.
- Pairing needs the operator to **arm pairing on the host** so it shows the PIN; the plugin can't arm
  it remotely.

## Related

- **[Documentation](https://docs.punktfunk.unom.io/docs/steam-deck)** — Steam Deck setup guide
- **[Linux client](../linux/README.md)** — the app this plugin launches
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
