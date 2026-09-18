---
title: Shared clipboard
description: Copy on one machine and paste on the other — the two switches that have to be on, what actually crosses, and why the toggle does nothing when only one of them is flipped.
---

Punktfunk can share the clipboard between the machine you are sitting at and the host you are
streaming, in both directions — copy a URL on your laptop, paste it on the host, and back.

**Two separate switches have to be on:**

1. The **host** operator has to allow it, in the web console under **Host → Settings**. Off by
   default.
2. **You** have to turn it on for that one host, in that host's edit sheet on your client. Off by
   default on the macOS, Windows and Linux clients — **on by default on Android**.

Flipping one and not the other looks exactly like the feature not existing. Check both.

## 1. Allow it on the host

Open the web console, go to **Host → Settings**, and set **Shared clipboard** to **Text** or
**Text and files**. It applies to the next stream; no restart.

On a host nobody opens the console on, add a `PUNKTFUNK_CLIPBOARD` line to its `host.env` instead —
`~/.config/punktfunk/host.env` on Linux, `%ProgramData%\punktfunk\host.env` on Windows. A value set
there wins over the console, which then shows the setting as locked.

```ini
PUNKTFUNK_CLIPBOARD=files
```

The accepted values:

| Value | Effect |
|---|---|
| unset, empty, `off`, `0`, `false`, `no` | **Off (the default).** The host never advertises the clipboard capability and never accepts a clipboard transfer. |
| `text` (also `text-only`, `no-files`) | On for text, HTML, rich text and images. File transfer is refused. |
| `files` (also `on`, `1`, `true`, `yes`) | On, and file transfer is permitted by policy. |

Values are trimmed and compared case-insensitively. A value the host doesn't recognise is ignored
with a warning in the host log, and the console's setting applies instead.

The file is only read at startup, so restart the host. On Linux:

```bash
systemctl --user restart punktfunk-host
```

On Windows, from an Administrator prompt:

```powershell
punktfunk-host service restart
```

See [Configuration](/docs/configuration) for the rest of `host.env`.

> **About the file mode.** No client shipping today asks for file transfer, and no host clipboard
> backend offers file formats yet, so `files` and `text` behave the same in practice — `text` makes
> that explicit and keeps it that way.

## 2. Turn it on for that host, in your client

The client switch is **per saved host**, not global — handing a machine your clipboard is a decision
about *that* machine — so it lives in the host's edit sheet, deliberately not in a
[settings preset](/docs/presets-and-links#what-a-preset-cant-change).

| Client | Where the switch is | Label | Default |
|---|---|---|---|
| macOS | Host card menu → **Edit…** | **Share clipboard with this host** | Off |
| iOS, iPadOS | Host card menu → **Edit…** | **Share clipboard with this host** | Off |
| Windows | Host tile menu → **Edit…** | **Share clipboard with this host** | Off |
| Linux (GTK) | Host card menu → **Edit…** | **Share clipboard** | Off |
| Android (touch) | Host card menu → **Edit…** | **Shared clipboard** | **On** |

On Android the switch is only in the touch edit dialog. The controller/TV interface — Android TV,
and a phone with a controller attached — has its own **Edit Host** screen with no clipboard row, so
it stays on, the Android default.

The setting is read when a session starts, so if you change it while streaming, reconnect.

macOS can also flip it mid-session: **Stream ▸ Share Clipboard** (⌃⌥⇧C), which becomes **Stop
Sharing Clipboard** once the host has acknowledged it. On an iPad with a hardware keyboard the same
combo works, though there is no menu bar to show it in — and only while the pointer is released, as
a captured session sends the keys to the host.

tvOS and a Steam Deck in Gaming Mode have no clipboard switch — the Apple TV has no pasteboard to
share, and neither the Decky panel nor the client's console home has a host edit sheet — see
[what each client does](#which-hosts-and-clients-support-it) below.

## Nothing crosses until something pastes

A copy costs nothing. When you copy, your machine announces only the **list of formats** it now
holds — no bytes. The bytes are pulled across on a separate transfer, only when an application on
the other end actually pastes. Copying a large image and never pasting it transfers nothing.

That holds for everything you copy on your own machine, and for both directions on the host. It
does **not** hold for a host copy arriving at a Windows or Android client: those two fetch the
content straight away and put it on your local clipboard, whether or not you ever paste — on
Windows because the lazy path needs Windows delayed rendering, which the client doesn't implement
yet; on Android because there is no way to satisfy a paste from the network at all. The macOS and
iOS clients are lazy in both directions.

iOS has one deliberate exception. Backgrounding the app ends the session, so if the host copied
something you have not pasted yet, those bytes are pulled across as the session ends, up to 8 MiB. That is what makes "copy on the host,
switch to Safari, paste" work on an iPad. Nothing is fetched if you never leave the app, or if you
already pasted.

A single transfer is capped at 64 MiB. Nothing else limits size, so a very large host-side copy can
cross to a Windows or Android client for a paste that never happens.

What you copy on the **macOS, iOS or Windows client** is filtered for secrets: content marked
`org.nspasteboard.ConcealedType` or `org.nspasteboard.TransientType` on the Apple clients, or
`ExcludeClipboardContentFromMonitorProcessing` on Windows — what password managers set — is never
announced and never served. That check exists only in those clients. The Android client has no
equivalent, and neither does the host, so a password copied **on the host** is announced to your
client like anything else.

The clipboard rides the native Punktfunk protocol's control channel, so it only exists in sessions
from a Punktfunk client. A Moonlight client has no clipboard.

## Which hosts and clients support it

**Hosts.** The host runs on Linux and Windows, and both have a clipboard backend — but on Linux it
depends on the desktop session, which needs one of two mechanisms:

- `ext-data-control-v1` — KWin, wlroots/Sway and Hyprland. Tried first.
- GNOME's own `org.gnome.Mutter.RemoteDesktop.Session` clipboard, used directly. Tried second.

The older `zwlr-data-control-unstable-v1` is **not** implemented, so a compositor that offers only
that has no backend. Neither does a [gamescope](/docs/gamescope) session.

On Windows the host uses the Win32 clipboard, with delayed rendering so your content is only read
when a host application pastes.

**Clients**, and what each one actually moves:

| Client | What crosses |
|---|---|
| macOS | Plain text, rich text (RTF), HTML, and PNG, JPEG and GIF images |
| iOS, iPadOS | Plain text, rich text (RTF), HTML, and PNG, JPEG and GIF images |
| Windows | Plain text, and PNG images |
| Android, Android TV | **Plain text only** |
| Linux (GTK), Steam Deck | Nothing yet — see below |
| tvOS | Not implemented — tvOS has no pasteboard |

The **Linux client has the switch but no working clipboard bridge**: it enables the plane and then
has no code to read or write the desktop's own clipboard, so nothing is announced and nothing is
pasted. Turning it on there is harmless but has no effect today. On a Steam Deck in Gaming Mode
there is no switch at all — the Decky panel doesn't edit hosts — and since a Deck streams with that
same Linux client, a switch there would have nothing to move anyway.

When you copy **on the Windows client**, images cross only if the copying application publishes the
registered `PNG` clipboard format. Many Windows apps publish only a bitmap, and those copies aren't
announced yet. The other direction is fine: an image copied on the host reaches the Windows client
either way.

The host side is richer than any client: it can offer and accept text, HTML, RTF, PNG, JPEG and GIF.
What you get is whatever your client supports.

## Why the toggle does nothing (or is greyed out)

On macOS, **Stream ▸ Share Clipboard** is greyed out whenever you are not streaming, or the
connected host did not advertise a clipboard. On the other clients there is nothing to grey out —
the per-host switch always looks available, and a host that can't do it simply does nothing. Work
through these in order:

- **The host has it off.** The default. Nothing was added to `host.env`, or the value is `off`, `0`,
  `false` or empty. Fix it with step 1 above.
- **`host.env` was edited but the host wasn't restarted.** The file is read once, at startup.
- **The switch is off for this host in your client.** It is per saved host, and off by default
  everywhere except Android. Check the host's **Edit…** sheet — step 2 above.
- **The host's session has no supported backend.** The host allows the clipboard, so it still
  advertises the capability, but has nothing to read the desktop's clipboard with: a gamescope
  session, a compositor with only the old `zwlr-data-control-unstable-v1`, or a GNOME session whose
  Mutter doesn't expose the direct RemoteDesktop clipboard. Nothing on screen tells you this apart —
  the host log does.
- **The host is older than the feature.** A host from before clipboard sync never advertises it.
- **Your client doesn't implement it** — Linux (GTK), Steam Deck or tvOS. Nothing crosses
  regardless of what the host allows.
- **You changed the switch while connected.** Reconnect, or use ⌃⌥⇧C on macOS.
- **The copy was a secret, or a format nobody handles.** Concealed content is skipped on purpose on
  the macOS and Windows clients, and an image copied on Windows as a bare bitmap isn't announced.

Still stuck? The host log records what it decided on each session — a `clipboard control` line with
the resolved state, and a `clipboard backend unavailable` line when the session had nothing to bind
to. That is the fastest way to tell "off by policy" from "no backend" — see
[Troubleshooting](/docs/troubleshooting).
