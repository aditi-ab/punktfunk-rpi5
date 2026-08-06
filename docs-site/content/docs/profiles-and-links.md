---
title: Profiles and links
description: How settings profiles override your client defaults per host or per connect, and how punktfunk:// links start a stream from a shortcut, a script or a browser.
---

Two features that landed together in 0.22.0 and work with each other: **settings profiles**, named
bundles of stream settings you can attach to a host, and **`punktfunk://` links**, URLs that start a
stream you have already set up.

Both live in the client apps — the Apple app, the Linux GTK client, the Windows client and the
Android app. Neither exists in the host's [web console](/docs/web-console).

The controller-driven surfaces are a half-exception: Apple TV, the Android app's console mode and
the Steam Deck console the Decky plugin launches all *use* the profile a host is bound to and can
pin one as its own card, but none of them can create or edit one. Do that on a desktop or a phone
first. The Decky panel itself only *shows* those pins, nested under their host as one-tap cards.

## What a profile is

A profile is a *sparse* set of overrides on top of your normal client settings. Only the rows you
actually touch are stored. Everything else keeps following your defaults **live**, so changing a
default later also moves every profile that never overrode it.

Touching a row records the override even when you pick the value the default already has. That is a
deliberate *pin*: the profile keeps that value when the default later moves. The only way back to
inheriting is the row's explicit **Reset**.

Where the catalog is kept:

| Client | Stored in |
|---|---|
| Linux | `~/.config/punktfunk/client-profiles.json` |
| Windows | `%APPDATA%\punktfunk\client-profiles.json` |
| Apple | the app-group store, beside your saved hosts |
| Android | app-private storage |

The catalog is per device, and nothing syncs it, so a profile you make on your laptop doesn't appear
on your phone.

## Creating and editing one

Profiles are created and edited in the client's own **Settings** screen — there is no second editor,
so a profile can never drift from the surface it overrides.

1. Open Settings. At the top is a scope switcher listing **Default settings**, your profiles, and a
   **New profile** entry. Linux, Windows and iOS label the control **Editing**; macOS heads the
   preferences window with the layer's name; Android shows the choices as a row of chips.
2. Create a new profile. Linux, Apple and Android ask for a name (and a colour) first; Windows
   creates one called *Profile 1* and opens its edit sheet, where you rename it. Names must be
   unique, ignoring case.
3. Change the rows you want. Every row shows the *effective* value — the inherited default until you
   touch it.
4. A row you have overridden grows a marker and a **Reset** control. Reset drops that one override
   and puts the row back to following your defaults.

Each profile can carry a colour from a small preset palette (the exact swatches differ slightly
between apps). It tints the profile's chip on host cards, so a grid of hosts is readable at a
glance.

Renaming, duplicating (overrides and colour included) and deleting sit next to the switcher on
Linux and in the same menu on Apple. Windows puts them in the sheet the switcher's **Edit** entry
opens; Android puts them on the selected profile's own chip — tap it a second time.

While a stream runs with a profile, the profile's name closes the first line of the
[stats overlay](/docs/stats) — on the Apple client, from the Normal tier up.

## What a profile can't change

In profile scope, rows that aren't profileable simply don't render. They are facts about *this
device* — the video decoder and GPU it uses, its audio endpoints, which physical controller you
hold, whether it wakes hosts on connect and whether it shows a game library — not about how you
want a stream to look. **Share clipboard** is out for the neighbouring reason: it's a per-host trust
decision stored on the host record rather than a client setting at all — see
[Clipboard](/docs/clipboard).

The row-by-row list, and why each row stays global, is on
[Client settings](/docs/client-settings#settings-that-are-facts-about-your-device).

## Three ways to use a profile

**Bind it to a host.** Open a saved host's edit sheet and set **Profile**. Every plain click on that
host's card now uses it. This is the only sticky choice. (The Apple app also offers **Connect with ▸
Set Default Profile** on the card itself.)

**Use it once.** A card's menu has **Connect with** — pick a profile for this connect only. It never
rebinds the host. **Default settings** in that menu is a real choice: on a bound host it forces your
globals for one session. (Android lists the same choices flat, as *Connect with: …*.)

**Pin it as its own card.** A pinned profile gets its own card beside the host — one click, no menu.
Pin it in the host's edit sheet on Linux, Android and Apple, or from a card menu: **Pin as Card** on
Apple, **Pin as card: …** on Android, **Pin tiles** on Windows. A pinned card is a shortcut, not a
second host: unpinning changes neither the profile nor the host's binding.

## Deleting a profile

The confirmation tells you what breaks: how many hosts will fall back to **Default settings**, and
how many pinned cards will disappear. Bindings and pins are deliberately left pointing at the gone
profile rather than rewritten; everywhere they are read, a dangling reference resolves as "no
profile", which is exactly your defaults. Nothing errors and no connect is blocked.

## `punktfunk://` links

A link starts a stream on a host this device already trusts. All four apps register the scheme with
the operating system, so a link works from a browser (behind the browser's own "open this app?"
prompt), a desktop shortcut, a home-automation rule or a script:

```text
punktfunk://connect/<host-ref>[?fp=<64-hex>][&host=<addr[:port]>][&launch=<id>][&profile=<ref>][&name=<label>]
```

`<host-ref>` is a saved host's stable record id, its name (unique, ignoring case), or `addr[:port]`.
Resolution goes in that order, and a name matching two saved hosts is refused rather than guessed.

| Parameter | Means |
|---|---|
| `fp` | the host certificate fingerprint the link expects — 64 hex characters |
| `host` | `addr[:port]` to fall back on when the reference no longer resolves; port defaults to `9777` |
| `launch` | a store-qualified [library](/docs/game-library) id such as `steam:570`, launched on arrival |
| `profile` | a settings profile, by id or unique name — for this connect only |
| `name` | a display label, shown as *claimed*, never trusted |

The scheme and the route word are case-insensitive, a trailing slash is fine, a `#fragment` is
dropped, unknown parameters are ignored, an empty value means "not given", and if a parameter
appears twice the first one wins. `pf://` parses as an input alias, but nothing emits it and no app
registers it with the operating system, so write `punktfunk://`.

`connect` is the only route any client acts on today; `wake` and `browse` parse, but every client
answers them with a notice. Values are capped (2048 for the whole URL, 128 for the host reference
and `launch`, 64 for `profile` and `name`), and `launch` must be printable ASCII with no spaces,
quotes, backslashes, `$` or backticks.

Worked examples:

```text
punktfunk://connect/Living%20Room%20PC
punktfunk://connect/Living%20Room%20PC?launch=steam:570
punktfunk://connect/Living%20Room%20PC?profile=Work
```

## What a link can and can't do

The rule the grammar exists to keep: **a link may only do what clicking a card you already have
could do, minus every trust decision.**

- It carries *references*, never values. There is no resolution, bitrate, codec or HDR parameter,
  so a web page cannot shape your session beyond choosing among your own configurations.
- There is no `pair` route and never will be. `punktfunk://pair/...` is refused outright;
  [pairing](/docs/pairing) stays something you do with the fingerprint on screen.
- A link naming a host you don't know is never connected. When it carries an address — as
  `<host-ref>` or as `host=` — Linux and Android open the app's normal trust prompt, pre-filled with
  that address and any `fp` the link carried, so the first connect is verified rather than blind.
  Windows and the Apple apps show a notice naming the host instead, and you pair from the host list
  yourself. A link with no address to fall back on — a bare name or a stale record id — is simply
  refused with a notice.
- If the link's `fp` contradicts the fingerprint already pinned for that host, it is a hard refusal
  with a notice. Nothing connects.
- A `profile=` that names nothing on this device, or two profiles at once, refuses **before**
  anything is dialled — a shortcut that can't honour its profile says so rather than streaming with
  the wrong settings. A link with no `profile=` honours the host's binding, exactly like a click.
- A link never preempts a running session. Linux and Windows say "A session is already running — end
  it first". Apple and Android do the same, except when the link points at the host you are already
  streaming, which just brings the app forward.

## Getting a link, and making a shortcut

On Linux and Windows a host card's menu has **Copy link** and **Create shortcut…**. On macOS and iOS
the card menu has **Copy Link**; tvOS has no clipboard, so it isn't offered there. Android has
**Copy link** in both of its homes — the touch grid's card menu, and the controller home's host
options (press Up on a host's tile).

On Linux, Apple and Android a pinned card has its own menu, and the link it hands out carries that
card's profile. Windows pinned tiles have no menu, and neither Windows action adds a `profile=`, so a
Windows link always uses the host's binding until you edit the URL yourself.

A copied link carries the host's stable record id, plus `host=` and `fp=` (the fingerprint only when
one is pinned). That is what keeps a shortcut written today working after the host changes address
or you reinstall the client.

**Create shortcut…** writes a launcher wrapped around that URL:

- **Linux** — a desktop entry in `~/.local/share/applications/`, which shows up in your app menu.
  Under Flatpak the sandbox can't write there, so the app offers you the URL to place yourself.
- **Windows** — a `.lnk` on your Desktop that runs `punktfunk-client.exe` with the URL as its
  argument, so it keeps working across updates.

Any other launcher works too, as long as it hands the URL to the client. From a script, or on a
headless box, use the `punktfunk` CLI:

```bash
punktfunk profiles list                          # ids, names, how many settings each overrides
punktfunk open 'punktfunk://connect/Desk?profile=Work'
```

It ships in the Linux client packages and in the Windows MSIX. The Flatpak build has it too, but
inside the sandbox — reach it with `flatpak run --command=punktfunk io.unom.Punktfunk`. See
[Clients](/docs/clients) for the rest of its verbs.
