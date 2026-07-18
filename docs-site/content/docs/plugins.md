---
title: Plugins
description: First-party plugins that sync your ROM collection or your Playnite library into the game library — and how to install them.
---

Plugins extend the host through the **scripting runner** (see [Events & hooks](/docs/automation)). A
plugin runs alongside the host, reconciles titles into your **game library** as a provider — so they
appear in the grid on every client — and can add its own page to the [web console](/docs/web-console).

Two first-party plugins today:

| Plugin | What it does |
|---|---|
| **ROM Manager** | Scans your ROM directories, matches each platform to an installed emulator, and syncs them into the library with box art. |
| **Playnite** | Mirrors your [Playnite](https://playnite.link) library — every store and emulator it manages — into the library, launched back through Playnite. |

## Installing a plugin

Plugins install as packages into the runner's plugins directory, and the runner
(`punktfunk-scripting`) supervises them. It's a one-time registry setup, then `bun add`:

```sh
# The runner's plugins directory:
#   Linux    ~/.config/punktfunk/plugins
#   Windows  %ProgramData%\punktfunk\plugins
cd ~/.config/punktfunk/plugins        # create it if it doesn't exist yet

# Point the @punktfunk scope at the registry (once):
cat > bunfig.toml <<'EOF'
[install.scopes]
"@punktfunk" = "https://git.unom.io/api/packages/unom/npm/"
EOF

bun add @punktfunk/plugin-rom-manager   # or @punktfunk/plugin-playnite
```

Then enable the runner — it's opt-in, off until you turn it on:

```sh
systemctl --user enable --now punktfunk-scripting     # Linux
Enable-ScheduledTask PunktfunkScripting               # Windows
```

Open the [web console](/docs/web-console) and the plugin's page appears in the nav automatically —
that's the whole install. Each plugin is also fully configurable from a `config.json` for headless
hosts (see its README).

> Plugins are operator-installed code that runs as the host user — they can launch games and run
> commands. Install only plugins you trust, from a registry you control.

## ROM Manager

`@punktfunk/plugin-rom-manager` — point it at your ROM directories and it scans them, matches each
platform to an installed emulator, fetches box art (SteamGridDB, or the keyless libretro thumbnails),
and reconciles the result into your library as the `rom-manager` provider. ~25 built-in platforms
(NES through Switch, PS1/2/PSP, Dreamcast, and more), per-game overrides, and a console page to
configure it all.

Install it as [above](#installing-a-plugin), then add a root or two — from the console's **ROM
Manager** page, or in `~/.config/punktfunk/rom-manager/config.json`:

```jsonc
{
  "roots": [
    { "dir": "/mnt/roms/snes", "platform": "snes" },
    { "dir": "/mnt/roms/ps1", "platform": "ps1", "excludes": ["*.sav"] }
  ],
  "art": { "provider": "auto", "steamGridDbKey": "" }
}
```

Full options and the platform/emulator list are in
[the plugin's repo](https://git.unom.io/unom/punktfunk-plugin-rom-manager).

## Playnite

`@punktfunk/plugin-playnite` — mirrors your **[Playnite](https://playnite.link)** library (Steam,
GOG, Epic, Xbox, itch, emulators, manually-added games — everything Playnite manages) into your
library. Launching a title hands it back to Playnite, which performs the real launch, so there are no
per-store launch commands to maintain. Covers are served by the host, so it scales to large libraries.

Because Playnite keeps its library locked while running, this plugin has **two parts**, both on the
Windows host:

1. **The plugin** (on the host) — install `@punktfunk/plugin-playnite` exactly as
   [above](#installing-a-plugin).
2. **The Punktfunk Sync extension** (in Playnite) — download `punktfunk-sync.pext` from the
   [plugin's builds](https://git.unom.io/unom/punktfunk-plugin-playnite/actions) and **double-click
   it** to install it in Playnite like any add-on, then restart Playnite once.

Open the console's **Playnite** page — it shows "Exporter connected", and your games sync within
seconds of any library change. Filters (installed-only, per-store, hidden) live on that page or in
`~/.config/punktfunk/playnite/config.json`. Details are in
[the plugin's repo](https://git.unom.io/unom/punktfunk-plugin-playnite).

## Writing your own

A plugin is a small TypeScript module built on `@punktfunk/host` (`definePlugin`), supervised by the
runner. Start from the [SDK README](https://git.unom.io/unom/punktfunk/src/branch/main/sdk) — or the
two plugins above, which are worked examples of the same shape.
