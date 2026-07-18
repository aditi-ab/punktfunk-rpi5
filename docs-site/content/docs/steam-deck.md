---
title: Steam Deck (Decky)
description: Install the punktfunk Decky plugin to discover, pair, and stream from the Steam Deck's Gaming Mode — no drop to Desktop.
---

The **Decky plugin** adds a **punktfunk** panel to the Steam Deck's Quick Access Menu (the `…`
button), so you can find a host, pair, and start streaming **without leaving Gaming Mode**. It's the
couch-friendly front end for the Steam Deck — built from real Steam UI, gamepad-navigable end to end.

Under the hood the plugin doesn't decode video itself: it discovers hosts, runs the PIN pairing, and
**launches the regular [Linux client](/docs/clients#linux-desktop-client-gtk4)** (the
`io.unom.Punktfunk` Flatpak) the way gamescope needs so it fullscreens correctly. So the Deck has two
ways to stream, and they share one client + one paired identity:

- **Gaming Mode** → the **Decky plugin** (this page).
- **Desktop Mode** → run the [Flatpak](/docs/install-client#steam-deck) directly, like any Linux app.

## Before you start

You need three things on the Deck:

1. **Decky Loader** — the plugin loader. Install it from [decky.xyz](https://decky.xyz/) if you
   haven't already.
2. **The punktfunk client Flatpak** — the plugin launches it, so install it once in **Desktop Mode**:

   ```sh
   flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
   ```

   (Full options: [Install a Client → Steam Deck](/docs/install-client#steam-deck). Without it, the
   panel's **Stream** button reports `flatpak-not-found`.)
3. **A punktfunk host** running on your LAN — see [Install the Host](/docs/install). The Deck finds
   it automatically over mDNS, so nothing to configure here.

## Install the plugin

The plugin is published as a ready-to-install zip on every build. You don't need the Decky CLI or a
developer toolchain — just paste a URL into Decky:

1. On the Deck, open the **Quick Access Menu** (`…`) → the **plug** icon (Decky) → the **gear**
   (Settings) → enable **Developer Mode**.
2. Open the new **Developer** tab and choose **Install Plugin from URL**.
3. Paste the **stable** link and confirm:

   ```
   https://unom.io/pf-decky
   ```

The **punktfunk** panel appears in the Quick Access Menu right away — no Deck restart needed.

> **Channels.** `https://unom.io/pf-decky` is a short link to the **stable** channel (moves on
> `vX.Y.Z` releases), currently
> `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip`. For the
> latest `main` build use the **canary** zip —
> `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/canary/punktfunk.zip` — or pin an
> exact version with `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/<version>/punktfunk.zip`.
> See [Release Channels](/docs/channels).

## Use it

Open the **punktfunk** panel from the Quick Access Menu, or **Open punktfunk** for the full-screen
page (host list + stream settings).

- **Discover** — hosts on your network appear automatically (mDNS). Tap **Refresh** to rescan. A
  lock icon means the host requires [pairing](/docs/pairing).
- **Pair** — for a locked host, [arm pairing on the host](/docs/pairing) (its console or web
  console shows a 4-digit PIN), then enter that PIN on the Deck's keypad. Pairing persists, so the
  next connection is silent.
- **Stream** — pick a host and the stream launches fullscreen in Gaming Mode (as a "Punktfunk"
  Steam shortcut, so gamescope focuses it — it shows up in your library with its own artwork, and
  relaunching it from there streams to the last host).
- **Settings** — resolution, refresh, bitrate, gamepad type, and mic, written to the client the
  plugin launches. Leave **Resolution** / **Refresh** on *Native* to get the Deck's own mode. With
  **Gamepad type** on *Automatic* the Deck's built-in controller is forwarded as a **Steam Deck**
  pad (paddles, both trackpads, gyro) — that needs Steam Input set to **Off** for Punktfunk (game
  page → ⚙ → Controller Settings), else Steam keeps those controls and only sticks + buttons reach
  the host.

To **leave a stream**: **hold L1 + R1 + Start + Select** for about two seconds, or close the
"game" from the Steam overlay. Either ends the session and drops you straight back to Gaming Mode.

## Updating

The plugin **checks for updates itself** — no Decky store needed. It covers **both** the plugin *and*
the streaming client (they version independently), so when either has a newer build the panel shows an
**Update** button (in the Quick Access Menu and on the full page). Tap it: the client updates in
place, and if the plugin itself changed it downloads, verifies, replaces itself, and reloads — all
without leaving Gaming Mode.

The plugin check follows the [channel](/docs/channels) you installed from: a plugin installed from the
**stable** link tracks stable releases; one installed from the **canary** link tracks `main` builds.

> **Updating the client from the terminal?** The client is a **per-user** Flatpak, so run
> `flatpak update --user io.unom.Punktfunk` — **without `sudo`**. `sudo flatpak update` only touches
> the *system* installation and silently skips the client. (Un-sudo'd `flatpak update` updates both
> scopes, so it's the safe default.)

> If the plugin **Update** button never appears (an older Decky Loader, or no network), update the
> plugin manually: Decky → **Developer** → **Install Plugin from URL**, and paste the same channel
> link again. Decky replaces the installed copy in place.

## Troubleshooting

| Symptom | Fix |
|---|---|
| **Stream** shows `flatpak-not-found` | Install the client Flatpak in Desktop Mode (see [Before you start](#before-you-start)). |
| No hosts listed | Make sure the host is running and on the **same LAN**; the Deck needs `avahi` (shipped on SteamOS). Tap **Refresh**. |
| Pairing fails / "not armed" | The PIN is shown only after you **arm pairing on the host**. Arm it, then enter the PIN within the window. |
| Stream launches but doesn't focus | Start it from the panel (not by launching the Flatpak by hand) so Steam/gamescope focuses it. |

The plugin source lives in
[`clients/decky`](https://git.unom.io/unom/punktfunk/src/branch/main/clients/decky/README.md).
