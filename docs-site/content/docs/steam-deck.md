---
title: Steam Deck (Decky)
description: Install the Punktfunk Decky plugin to discover, pair, and stream from the Steam Deck's Gaming Mode — no drop to Desktop.
---

The **Decky plugin** adds a **Punktfunk** panel to the Steam Deck's Quick Access Menu (the `…`
button), so you can find a host, pair, and start streaming **without leaving Gaming Mode**. It's the
couch-friendly front end for the Steam Deck — built from real Steam UI, gamepad-navigable end to end.

The plugin is a **launcher**, not a second client. It doesn't decode video, browse your library or
hold settings of its own — it starts the regular
[Linux client](/docs/clients#linux-desktop-client-gtk4) (usually the `io.unom.Punktfunk` Flatpak)
the way gamescope needs so it fullscreens correctly. Everything the panel doesn't do is one tap
away in that client's own gamepad UI. So the Deck has two ways to stream, and they share one
client + one paired identity:

- **Gaming Mode** → the **Decky plugin** (this page).
- **Desktop Mode** → run the [Flatpak](/docs/install-client#steam-deck) directly, like any Linux app.

## Before you start

You need three things on the Deck:

1. **Decky Loader** — the plugin loader. Install it from [decky.xyz](https://decky.xyz/) if you
   haven't already.
2. **A Punktfunk client on the Deck** — the plugin doesn't decode video itself, it launches a
   client. On a normal Deck that's the Flatpak, installed once in **Desktop Mode**:

   ```sh
   flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
   ```

   (Full options: [Install a Client → Steam Deck](/docs/install-client#steam-deck).) If you have
   no Flatpak but a native `punktfunk-client` — a sysext, a distro package, a nix profile, your own
   build — the plugin uses that instead; with both installed the Flatpak wins, unless
   `PF_DECKY_CLIENT=native` (or `flatpak`) is set in the plugin backend's environment. Both kinds
   share `~/.config/punktfunk`, so your identity, known hosts and settings are the same either way.

   **The client must be v0.22.0 or newer.** The panel drives everything through the client's
   headless `punktfunk` command, which shipped in that release. An older client says so in the
   panel, with the update button that fixes it right there.
3. **A Punktfunk host** running on your LAN — see [Install the Host](/docs/install). The Deck finds
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

The **Punktfunk** panel appears in the Quick Access Menu right away — no Deck restart needed.

> **Channels.** `https://unom.io/pf-decky` is a short link to the **stable** channel (moves on
> `vX.Y.Z` releases), currently
> `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/latest/punktfunk.zip`. For the
> latest `main` build use the **canary** zip —
> `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/canary/punktfunk.zip` — or pin an
> exact version with `https://git.unom.io/api/packages/unom/generic/punktfunk-decky/<version>/punktfunk.zip`.
> See [Release Channels](/docs/channels).

## Use it

Open the **Punktfunk** panel from the Quick Access Menu. It has one list — the hosts you can
stream — plus a door into the client's own gamepad UI for everything else.

- **Hosts** — hosts on your network appear automatically (mDNS), alongside the ones you've already
  saved. A saved host is also probed directly, so a box reached over a VPN or Tailscale shows as
  online even though it never advertises. Tap **Refresh** to rescan. The list sorts online hosts
  first, then whichever you streamed most recently. A lock icon means the host still has to let
  this Deck in.
- **Let a host in** — tapping a locked host opens a small sheet with two ways through:
  - **Request access** — no PIN at all. See [Request access](#request-access) below.
  - **Use a PIN instead** — [arm pairing on the host](/docs/pairing) (its console or web console
    shows a 4-digit PIN), then enter it on the Deck's keypad.

  Either way the host is remembered, so the next connection is silent.
- **Stream** — tap a host and the stream launches fullscreen in Gaming Mode. The plugin drives a
  hidden Steam shortcut behind the scenes so gamescope focuses and fullscreens it.
- **Sleeping host?** Streaming sends a [Wake-on-LAN](/docs/wake-on-lan) packet and waits for the
  host to actually come back before dialling, so a stream survives a resume from sleep. Nothing to
  enable — it's a no-op until the client has learned that host's MAC address, and the packet only
  lands if the host machine is armed to wake in its BIOS and its network card.
- **Pinned cards** — a host with pinned [settings profiles](/docs/client-settings) shows them
  nested underneath it as `▸ <Profile name>`. Tapping one streams that host with that profile
  applied — your "4K on the TV" and "battery saver" presets, one tap each. Pins are made in the
  Punktfunk app (or any other client) and shared across all of them; the panel shows them, it
  doesn't create them.
- **Open Punktfunk** — opens the client's console home: the host picker, adding a host by address,
  pairing, browsing a host's [game library](/docs/game-library), and the **full settings screen**.
  This is where resolution, bitrate, codec, audio, controllers and the stats overlay live.
- **Library entry** — a visible, branded **Punktfunk** app also appears in your Steam library, and
  launching it opens that same console home — it does not resume a stream. If it ever disappears,
  the Quick Access Menu panel has a button to put it back.

> **Where did the plugin's settings tab go?** Into the app, at **Open Punktfunk → Settings** — the
> same rows over the same settings, gamepad-navigable, and one tap from the same panel. The plugin
> used to carry its own copy of that screen, which meant two places to change one setting and a
> copy that fell behind. There is now one.

With **Controller type** on *Automatic* the Deck's built-in controller is forwarded as a **Steam
Deck** pad (paddles, both trackpads, gyro) — that needs Steam Input set to **Off** for Punktfunk
(game page → ⚙ → Controller Settings), else Steam keeps those controls and only sticks + buttons
reach the host.

### Request access

**Request access lets you in without typing a PIN**: instead of the host showing you a code, you
ask, and whoever is at the host approves the Deck in its [web console](/docs/web-console) or on
screen.

Tap the host → **Request access**. The Deck says *"Approve this Deck in <host>'s console — the
stream starts by itself"*, and the stream opens and waits. The moment somebody approves it, the
picture comes up — no going back to the panel, nothing else to tap. If nobody approves within
about three minutes, it gives up like any failed connection and you can try again or use a PIN.

It's the better option when you're not the person sitting at the host, or when reading a PIN off
another screen is awkward. Two things to know:

- The host must be **advertising on your network** for this to be offered. A host you added by
  address (a VPN box, another subnet) has no advertised identity for the Deck to pin, so the sheet
  offers the PIN path only and says so. That's a safety rule, not a limitation to work around:
  pinning the advertised identity is what stops something else answering in the host's place while
  the Deck waits.
- Once approved, the host shows as **paired** and every later stream connects silently.

> **Steam Input off is a trade-off, not a free win.** The plugin installs a Steam Input layout
> called **Punktfunk** and points its shortcuts at it, and that layout's whole job is making the
> Deck's touchscreen arrive at the stream as *real touch*. Leaving Steam Input **On** with that
> layout gives you native touch plus a standard gamepad; setting it **Off** gives you the full Steam
> Deck pad — paddles, both trackpads, gyro — but the touchscreen stops working as touch. Pick per
> game, on the game page → ⚙ → **Controller Settings**.

To **leave a stream**: **hold [L1 + R1 + Start + Select](/docs/input#leaving-with-a-controller)**
for about a second and a half, or close the "game" from the Steam overlay. Either ends the session
and drops you straight back to Gaming Mode. A quick press of the same four only releases captured
input, so it is safe to hit by accident.

**The Steam and `…` buttons stay with the Deck while streaming.** SteamOS opens its own menus for
them no matter what, so forwarding the raw press as well opened *both* menus at once — the Deck's
covering the stream. To reach the **host's** menus instead: **hold Select** for the host's Steam
menu ([how it works](/docs/input#the-guide-button-xbox--ps--steam-and-quick-access)), or open the
Punktfunk panel — while a stream runs it grows a **Host menus** section whose two buttons,
**Steam menu on host** and **Quick access on host**, press the button on the host and close the
Deck's own menu so the host's shows through. Want the raw forwarding back? **Open Punktfunk →
Settings → Steam / guide button** → *Send to host*.

## Updating

The plugin **checks for updates itself** — no Decky store needed. It covers **both** the plugin *and*
the streaming client (they version independently), so when either has a newer build the panel shows an
**Update** button at the top of the panel. Tap it: the client updates in place, and if the plugin
itself changed it downloads, verifies, replaces itself, and reloads — all without leaving Gaming
Mode.

One exception: if your client isn't one the plugin can install for you (a sysext, a nix profile, a
source build), the panel shows you the update **command** instead of a button — tap-to-install would
only fail. A pending plugin update still gets its button.

The plugin check follows the [channel](/docs/channels) you installed from: a plugin installed from the
**stable** link tracks stable releases; one installed from the **canary** link tracks `main` builds.

> **Updating the client from the terminal?** The Flatpak client is installed **per-user**, so run
> `flatpak update --user io.unom.Punktfunk` — **without `sudo`**. `sudo flatpak update` only touches
> the *system* installation and silently skips the client. (Un-sudo'd `flatpak update` updates both
> scopes, so it's the safe default.)

> If the plugin **Update** button never appears (an older Decky Loader, or no network), update the
> plugin manually: Decky → **Developer** → **Install Plugin from URL**, and paste the same channel
> link again. Decky replaces the installed copy in place.

## Troubleshooting

| Symptom | Fix |
|---|---|
| The panel says **"Update the Punktfunk client"** | The installed client predates v0.22.0 and has no `punktfunk` command to drive. Tap the update button in the same panel, or update it in Desktop Mode. |
| The stream never starts, or the panel can't reach the client | Install the client Flatpak in Desktop Mode (see [Before you start](#before-you-start)). |
| No hosts listed | Make sure the host is running and on the **same LAN**. Tap **Refresh**. For a host mDNS can't reach, add it by address in **Open Punktfunk → Add host**. |
| Pairing fails / "not armed" | The PIN is shown only after you **arm pairing on the host**. Arm it, then enter the PIN within the window — or use **Request access** instead, which needs no PIN. |
| **Request access** isn't offered | The host isn't advertising on this network, so there's no identity to pin. Use the PIN path. |
| A request-access stream sits there | That's it waiting — somebody has to approve the Deck on the host. It gives up after about three minutes. |
| Stream launches but doesn't focus | Start it from the panel (not by launching the client by hand) so Steam/gamescope focuses it. |
| The stream wedges — black, or won't close | Panel → **About** → **Force-stop**, then start it again. |
| The **Punktfunk** library entry disappeared | Panel → **Recreate library shortcut**; it puts the entry back in place. |
| You want a clean slate | **Open Punktfunk → Settings** for stream settings, or `punktfunk reset` in Desktop Mode to forget every saved host. Your paired identity is kept either way. |

Nothing here matching? The problem is probably on the host side — start at
[Troubleshooting](/docs/troubleshooting), which is organised by symptom (host not found, pairing
rejected, black picture).

## Uninstalling

Removing the plugin through Decky removes the plugin and nothing else, so do these in order:

1. **Remove the plugin.** Quick Access Menu (`…`) → the **plug** icon (Decky) → the **gear**
   (Settings) → **Plugins** → **Punktfunk** → **Uninstall**.
2. **Remove the Steam shortcuts it created.** The plugin adds two non-Steam entries, both named
   **Punktfunk** — the one you see in your library, and a second one it keeps hidden to carry the
   stream. Decky removes neither. In your library, right-click a **Punktfunk** entry →
   **Manage → Remove non-Steam game from your library**, and repeat for the hidden one once you've
   let the library show hidden games.
3. **Remove the client**, if you're done streaming on this Deck. In Desktop Mode:

   ```sh
   flatpak uninstall --user --delete-data io.unom.Punktfunk
   ```

   Your identity and saved hosts live in `~/.config/punktfunk` and survive that — delete the
   directory too for a clean slate. See
   [Removing a client](/docs/install-client#removing-a-client).
4. **Revoke the pairing on the host.** The host still trusts this Deck until you remove it from its
   [web console](/docs/web-console) — see
   [Managing paired devices](/docs/pairing#managing-paired-devices).

The Steam Input layout the plugin installed also stays behind as a selectable template named
*Punktfunk* (`~/.local/share/Steam/controller_base/templates/punktfunk.vdf`), along with the
per-account configset entry pointing at it. Leave them — with the shortcuts gone they apply to
nothing — or delete the file if you'd rather not see it offered as a template.

The plugin source lives in
[`clients/decky`](https://git.unom.io/unom/punktfunk/src/branch/main/clients/decky/README.md).
