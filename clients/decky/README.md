# Punktfunk — Steam Deck plugin (Decky)

Stream to your **Steam Deck** without ever leaving Gaming Mode. This
**[Decky Loader](https://decky.xyz/)** plugin adds a **Punktfunk** panel to the Quick Access Menu
(the `…` button): the hosts you can stream, the pinned cards you set up, and one tap into each.

The plugin is a **launcher**, not a client. It doesn't decode video, browse your library, or hold
any settings of its own — the Rust client does all of that, and the plugin's job is to start it
*the right way* so gamescope fullscreens and focuses it (the same Steam-shortcut trick MoonDeck
uses). Everything the panel doesn't do is one tap away in the client's own gamepad UI.

## What it does

1. **Hosts** — the hosts on your network plus the ones you've saved, in one list. Discovery is
   mDNS; saved hosts are also probed directly, so a box reached over Tailscale or a VPN shows as
   online even though it never advertises. Rows sort online-first, then most recently used.
2. **Trust** — an unpaired host opens a small sheet with two ways in:
   - **Request access** (the default) — no PIN. The host's operator approves this Deck in its
     console or web UI and the stream starts by itself. See [Request access](#request-access).
   - **Use a PIN instead** — the gamepad-navigable keypad, running the same SPAKE2 ceremony.
3. **Stream** — launches fullscreen via a branded "Punktfunk" Steam shortcut so gamescope focuses
   it. A sleeping host is woken first (the client runs the real wake-and-wait loop, then dials).
4. **Pinned cards** — a *(host, profile)* pair renders nested under its host as `▸ <Profile name>`
   and streams with that settings profile applied. Cards are the **shared** pinning model every
   other client speaks, stored on the host's record — so one you make in the desktop client shows
   up here, and vice versa. The plugin renders them; it doesn't create or edit them.
5. **Open Punktfunk** — launches the client's **console home**: the host picker, add-host by
   address, PIN pairing, the game library browser, and the **full settings screen**. This is where
   everything the panel no longer does now lives.
6. **About** — plugin version, "Check for updates", "Recreate library shortcut", and a force-stop
   for a wedged stream.

To leave a stream: the in-client controller chord (**L1 + R1 + Start + Select**), or close the
"game" from the Steam overlay — either returns you to Gaming Mode.

### Request access

Request access is not a second pairing ceremony — it is a **launch**. The plugin saves the host
with the fingerprint it **advertised**, then starts an ordinary identified connect with the
handshake budget stretched to 185 s. The host *parks* that connection until its operator approves
the device, then admits the same connection; the stream starts on its own, and the record flips
to **paired** so every later stream is silent.

**No advertised fingerprint, no request access.** That pinned fingerprint is the only thing
standing between a 185-second wait and an impostor answering for the host, so a host you typed in
by address gets the PIN path only — and the sheet says why. The plugin never trusts-on-first-use
past a missing fingerprint.

## Install on the Deck

You need **[Decky Loader](https://decky.xyz/)** and a **Punktfunk client** on the Deck. On a normal
Deck that's the `io.unom.Punktfunk` flatpak ([`packaging/flatpak`](../../packaging/flatpak/README.md)) —
SteamOS `/usr` is read-only, so the flatpak (which bundles libadwaita/SDL3) is the canonical client.
A native install (sysext, distro package, nix profile, your own build) works too.

**The client must be v0.22.0 or newer** — that is when the headless `punktfunk` CLI shipped, and
the panel drives everything through it. An older client says so in the panel, with the update
button that fixes it right there. (Discovery no longer needs `avahi-browse` on the Deck; the
client's own mDNS does it.)

**Recommended — install from URL** (published by CI): in Decky → Settings → **Developer Mode** →
**Install Plugin from URL**, paste:

```
https://unom.io/pf-decky
```

(short link for `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip`;
for a pinned version use `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/<version>/punktfunk.zip`
directly). The plugin then **self-updates** without the Decky store — when a newer build exists, an
**Update** button appears and drives Decky Loader's own (SHA-256-verified) install. Installs and
updates can take a couple of minutes on some networks: Decky's installer also contacts its plugin
store first, which may be slow or blackholed before the actual download proceeds.

### Updating the client

The plugin also reports — and where it can, installs — updates for the **client** it launches.
What is possible depends on how that client was installed:

| Install | Update |
| --- | --- |
| **Flatpak** (the usual Deck client) | One tap. `flatpak update --user io.unom.Punktfunk` — a per-user install, which is why `sudo flatpak update` never touches it. |
| **.deb / .rpm** (and rpm-ostree, which stages for the next reboot) | One tap, *after* an explicit opt-in: `sudo usermod -aG punktfunk-update $USER`. The tap starts a fixed, parameterless root oneshot (`punktfunk-client-update.service`) through polkit — nothing about the request is attacker-influenceable, and the payload comes from your distro's own signed repositories. |
| **pacman** | Same, plus the root-owned `PACMAN_FULL_SYSUPGRADE=1` in `/etc/punktfunk/update.conf` — a partial upgrade is against Arch doctrine, so the only thing the helper will run is a full `pacman -Syu`. |
| **sysext, nix, a source build** | The plugin shows the command and stops. There is no feed behind those installs, and a button that can only fail is worse than one honest line. |

Whether a *newer* client exists is the client's own answer (`punktfunk-client --check-update`),
read from the Ed25519-signed per-channel manifest the host's update check already trusts —
`PUNKTFUNK_UPDATE_CHECK=0` disables the check, `PUNKTFUNK_UPDATE_APPLY=0` keeps the check but
never offers to install. A client too old to have that mode is reported as such rather than as
up to date.

## Build & sideload (development)

```sh
cd clients/decky
pnpm install
pnpm build                             # rollup → dist/index.js
pnpm run package                       # → out/punktfunk/ + out/punktfunk-v<ver>.zip
DECK=deck@<deck-ip> pnpm run deploy    # rsync → /tmp, sudo-install into the root-owned plugins dir, restart loader

python3.13 scripts/test-backend.py     # backend unit checks (needs Python ≥3.10)
```

`~/homebrew/plugins/` is root-owned (the loader runs as root), so `deploy.sh` stages to a temp dir
then `sudo`-installs and restarts the loader — set `DECKPASS=…` to run it non-interactively. A loader
restart is required for an out-of-band install to appear.

## Architecture

Everything below the panel is the CLI. `main.py` builds argv and maps exit codes; it parses none of
the client's data files and re-implements none of its rules.

| File | Role |
| --- | --- |
| `src/index.tsx` | Plugin entry + the QAM panel: update banner, hosts (with nested pinned cards), the console-home door, about. |
| `src/hooks.ts` | `useHosts` (one call merging discovery and the saved store), the update hooks, and the launch action. Also the trust-state model the rows render. |
| `src/trust.tsx` · `src/pair.tsx` | The trust sheet (Request access / Use a PIN instead / Cancel) and the gamepad-navigable PIN keypad. |
| `src/steam.ts` | Steam-shortcut launch (`AddShortcut` / `SetAppLaunchOptions` / `RunGame`) — the focus-correct stream start. The shortcut's exe is `/bin/sh` with the wrapper passed as an argument, so the script never needs an exec bit (Decky's zip extraction drops it and the root-owned plugins dir can't be chmodded by the unprivileged backend). |
| `src/backend.ts` · `src/boundary.tsx` · `src/os-icon.tsx` | Typed `callable` bridges to `main.py`; the render error boundary; the host row's OS mark. |
| `bin/punktfunkrun.sh` | The launch wrapper the Steam shortcut runs (so the window is focusable). Reads `PF_REF` / `PF_PROFILE` / `PF_REQUEST_ACCESS` / `PF_BROWSE` and runs `punktfunk launch` — or the session's `--browse` for console home. |
| `main.py` | Backend: four thin CLI shells (`discover` / `hosts` / `pair` / `trust_host`) plus the Steam-side work only a plugin can do — `runner_info`, `shortcut_art`, `apply_controller_config`, `kill_stream`, `check_update` / `update_client` (with an explicit CA-bundle search — Decky's embedded Python has no usable default TLS roots on SteamOS). |
| `scripts/test-backend.py` | Stdlib-only checks: argv shape, the CLI exit-code mapping, and the Steam configset editor. |
| `plugin.json` · `update.json` | Decky manifest; CI-baked update channel. |

### Why the launch goes through Steam

gamescope only gives focus and fullscreen to the window tree Steam launched via `reaper` (it
detects the "current app" by AppID — gamescope#484). A client spawned from the plugin's own
backend comes up invisible and unfocused. So the plugin registers non-Steam shortcuts whose exe is
`/bin/sh` running `bin/punktfunkrun.sh`, and starts them with `RunGame`.

There are **two** shortcuts, both named `Punktfunk` so Steam keys them to one Steam Input
configset (the key is the lowercase name): a hidden, stateful one that carries the stream, and the
visible, stateless library entry that opens console home.

## Limitations / next steps

- **Profiles and pinned cards can't be created here** — the panel renders them; making one needs
  the desktop client, or the client's own gamepad UI once that work lands. A Deck with no profiles
  simply sees host rows, and nothing is broken.
- **Per-game pins are on hold.** The shared model pins *host+profile*; nothing in the shared store
  persists a pinned *game* yet. The old `decky-pinned.json` is left on disk untouched so a later
  migration can read it.
- Pairing with a PIN needs the operator to **arm pairing on the host** so it shows the PIN; the
  plugin can't arm it remotely. Request access needs no arming — just an approval.
- **A parked connect looks like a hanging one.** The plugin toasts before launching a request-access
  stream to set expectations, which is a patch rather than a fix; teaching the session's connect
  screen the same "waiting for approval" copy the console shell already has would pay off for every
  shell.

## Related

- **[Documentation](https://docs.punktfunk.unom.io/docs/steam-deck)** — Steam Deck setup guide
- **[Linux client](../linux/README.md)** — the app this plugin launches
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
