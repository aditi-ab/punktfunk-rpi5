---
title: Events & hooks
description: React to what the host does — lifecycle events over SSE, hook commands and webhooks, per-app prep/undo — for notifications, DND toggles, Home Assistant, and more.
---

The host emits a **lifecycle event** for the things you'd want to react to: a client connects or
disconnects, a stream starts or stops, a pairing request arrives, a virtual display is created,
the library changes, the host starts or shuts down. Two ways to consume them:

- **Hooks** — zero-code: entries in `~/.config/punktfunk/hooks.json` run a **command** or POST a
  **webhook** when a matching event fires. Covers the common automation: Do-Not-Disturb during a
  stream, a phone notification on a pairing request, pausing downloads while playing.
- **The event stream** — code: `GET /api/v1/events` on the management API is a standard
  [Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events)
  stream of the same events, for scripts and integrations that want to *decide* things (e.g.
  auto-approve pairing from a known subnet by calling the approve endpoint).

Hooks **observe** — they can never veto or delay a connection, a stream, or a pairing decision,
and nothing configured here runs anywhere near the streaming path.

## The events

| Kind | Fires when | Carries |
|---|---|---|
| `client.connected` / `client.disconnected` | a client session is admitted / goes away | device name, cert fingerprint, plane (`native`/`gamestream`); disconnect adds `reason`: `quit` (user stop), `timeout` (vanished), `error` |
| `session.started` / `session.ended` | an A/V session registers / ends | session id, client label, mode (`3840x2160@120`), HDR |
| `stream.started` / `stream.stopped` | video actually starts / stops | mode, HDR, client name, launched app id/title (when one was requested), plane |
| `game.running` | a launched game's own process is seen running (not merely its launcher) | app id, title, store, client, plane |
| `game.exited` | a launched game is gone | the same, plus `reason`: `exited` (the player quit it) or `terminated` (the host closed it, per your [session⇄game settings](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does)) |
| `pairing.pending` | an unpaired device knocks (once per device, not per retry) | device name, fingerprint, plane |
| `pairing.completed` / `pairing.denied` | a pairing is approved+stored / denied | device name, fingerprint, plane |
| `display.created` / `display.released` | a virtual display is minted / kept displays are released | backend + mode / count |
| `library.changed` | the game library is mutated | source: `manual`, or the provider id that reconciled (`PUT /api/v1/library/provider/{p}`) |
| `update.available` | a verified manifest announces a release newer than the running host — once per discovered version, not on every check | version, channel (`stable`/`canary`), and this host's install kind (`apt`, `windows-installer`, …) |
| `update.applied` | the new binary's first start after a successful update | `from`, `to` |
| `plugins.changed` | a plugin's registration changes (registered, restarted, deregistered, or its lease expired) | plugin id |
| `store.changed` | an install or uninstall finished, or a plugin catalog was refreshed | none — re-read `GET /api/v1/store/catalog` / `…/installed` |
| `host.started` / `host.stopping` | the serve planes come up / wind down | version, whether GameStream is enabled |

Every event is a small JSON document with a monotonic `seq`, a `ts_ms` timestamp, a `schema`
version (additive-only — fields get added, never renamed), and the fields above:

```json
{ "seq": 42, "ts_ms": 1784227449526, "schema": 1,
  "kind": "stream.started",
  "stream": { "mode": "2560x1440@120", "hdr": true,
              "client": "Living Room TV", "app": "steam:570", "plane": "native" } }
```

## Hooks: `hooks.json`

Create `~/.config/punktfunk/hooks.json` (Windows: `%ProgramData%\punktfunk\hooks.json`), or PUT
the same document to `/api/v1/hooks` from a script — changes apply immediately, no restart:

```json
{
  "hooks": [
    { "on": "stream.started",  "run": "/home/me/.config/punktfunk/scripts/on-stream.sh" },
    { "on": "stream.stopped",  "run": "/home/me/.config/punktfunk/scripts/off-stream.sh" },
    { "on": "client.connected", "filter": { "client": "Living Room TV" },
      "run": "kscreen-doctor output.HDMI-A-1.mode.3840x2160@60" },
    { "on": "pairing.pending",
      "webhook": "https://ha.local/api/webhook/punktfunk",
      "hmac_secret_file": "/home/me/.config/punktfunk/webhook-secret" }
  ]
}
```

Each entry:

| Field | Meaning |
|---|---|
| `on` | Which events fire it: an exact kind (`stream.started`) or a `domain.*` prefix (`pairing.*`). |
| `run` | A shell command (`sh -c` on Linux). Gets the event JSON on **stdin** and flat **`PF_EVENT_*`** env vars. |
| `webhook` | A URL the event JSON is POSTed to. TLS-verified, redirects are never followed, no Punktfunk credentials attached. |
| `filter` | Optional exact-match constraints: `client` (device name), `fingerprint`, `plane` (`native`/`gamestream`), `app`. All present fields must match. |
| `timeout_s` | Command timeout (default 30, max 600) — on expiry the whole process group is killed. |
| `debounce_ms` | Minimum interval between firings of this hook (0 = every event). |
| `hmac_secret_file` | File with a secret; the webhook gains `X-Punktfunk-Signature: sha256=<hex HMAC-SHA256 of the body>` so your receiver can authenticate the host. |

### What the host refuses

The document is validated as a whole, and **one bad entry disables every hook** — the host logs
`hooks.json invalid — hooks disabled until fixed` and runs none until you correct it. The rules:

- An entry needs a non-empty `on`, plus `run` and/or `webhook`.
- `webhook` must be an `http(s)://` URL, and must **not** point at loopback, `localhost` or a
  link-local address (which also blocks the cloud metadata endpoint). A receiver on this same
  machine is what a `run` command is for. Ordinary LAN addresses — `192.168.x.x`, a ULA, a
  hostname — are fine, so Home Assistant on another box on your network works as written.
- `timeout_s` must be 1–600.
- If `hmac_secret_file` is set but unreadable, the host **skips** that POST rather than sending it
  unsigned. It also *warns* (and still signs) when that file isn't owned by you or is readable by
  anyone else — `chmod 600` it.

So check the log after editing the file: `journalctl --user -u punktfunk-host` on Linux, or the web
console's **Logs** page on either platform. Those lines name a hook by its webhook's
`scheme://host` or its command's program name, plus a short id — the URL path and the command's
arguments are left out, because that's where a Slack or ntfy token and an `Authorization:` header
live, and the **Logs** page is served over the API verbatim. The id is the same on every line about
one hook, so two hooks sharing a program or a webhook host stay apart.

A `run` command's shell one-liner vocabulary — the event flattened to env, values sanitized:

```sh
#!/bin/sh
# PF_EVENT_KIND=stream.started   PF_EVENT_SEQ=42
# PF_EVENT_STREAM_MODE=2560x1440@120   PF_EVENT_STREAM_HDR=true
# PF_EVENT_STREAM_CLIENT='Living Room TV'   PF_EVENT_STREAM_APP=steam:570
# PF_EVENT_STREAM_PLANE=native   PF_EVENT_JSON='{…the whole event…}'
[ "$PF_EVENT_KIND" = stream.started ] && makoctl mode -a do-not-disturb
```

Richer payloads (and the full document) are on stdin for `jq`. On Windows, a SYSTEM host runs the
command as the signed-in user of **that host's WTS session** (never as SYSTEM); that path can't
carry per-process env or stdin, so the event JSON's path is appended as the command's last argument
instead.

Verify a signed webhook (Python):

```python
import hmac, hashlib
expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
ok = hmac.compare_digest(request.headers["X-Punktfunk-Signature"], expected)
```

**Rules of the road:** hooks are fire-and-forget and bounded — at most 8 in flight (extra firings
are dropped with a log line, never queued), and a command that outlives its timeout is killed.
Hook commands run without elevation (as the host user on Linux and its WTS session user on
Windows), so `hooks.json` is operator-privileged config. On Linux, when a command names a script by
**absolute path**, the host checks that file *and every directory above
it* is owned by you (or root) and not group/world-writable, and refuses to run it — loudly, in the
log — if it isn't: whoever can rename an entry in a directory chooses what runs out of it. (A
world-writable directory with the sticky bit, like `/tmp`, passes — there only an entry's own owner
can replace it.) Quoting is understood, so a path with a space in it is checked like any other.
Write the full path (`/home/me/.config/punktfunk/scripts/on-stream.sh`, not `~/…`) if you want that
check: the shell expands `~` and looks up PATH names like `makoctl` only afterwards, so those are
never checked. On Windows there is no per-script check — the ACL on the config directory is the
boundary.

The two simplest cases also exist as plain [host.env](/docs/configuration) settings, no
`hooks.json` needed: `PUNKTFUNK_ON_CONNECT_CMD` and `PUNKTFUNK_ON_DISCONNECT_CMD`.

## Per-app prep/undo

For per-title setup (HDR toggle, MangoHud, a VRR tweak), attach `prep` steps to a GameStream
`apps.json` entry or to a [custom library entry](/docs/game-library#adding-a-game-by-hand) — each
`do` runs **before** the title launches (synchronously — the launch waits), each `undo` runs at
session end in **reverse order**, best-effort, even if the session crashed:

```json
{ "id": 2, "title": "Steam", "compositor": "gamescope", "cmd": "steam -gamepadui",
  "prep": [
    { "do": "~/bin/hdr on",  "undo": "~/bin/hdr off" },
    { "do": "pactl set-default-sink game_sink", "undo": "pactl set-default-sink desk_sink" }
  ] }
```

A `do` that fails logs, keeps going, and its own `undo` is skipped (it never took effect).

Every prep command (and its `undo`) runs with the session's negotiated mode in its environment:
`PF_STREAM_WIDTH`, `PF_STREAM_HEIGHT`, `PF_STREAM_REFRESH` and `PF_STREAM_HDR` (`1`/`0`), plus the
app identity — `PF_APP_ID` for a native client's launch, `PF_APP_TITLE` for a Moonlight one. So a
per-mode frame cap is one step for every device —
`{ "do": "rtss-cli property:set Global FramerateLimit $PF_STREAM_REFRESH" }` — instead of one
hard-coded entry per client.

### One entry, every client

The point of those four variables is that the *entry* stops describing a device. Attach one script
to the title and let the session tell it what it got — 60 Hz on the phone, 4K120 HDR on the TV, from
the same two lines:

```json
{ "id": 2, "title": "Cyberpunk 2077", "cmd": "steam -applaunch 1091500",
  "prep": [
    { "do":   "/home/me/.config/punktfunk/scripts/mode.sh do",
      "undo": "/home/me/.config/punktfunk/scripts/mode.sh undo" }
  ] }
```

```sh
#!/bin/sh
# ~/.config/punktfunk/scripts/mode.sh — run as `mode.sh do` before the title, `mode.sh undo`
# at session end. Nothing here names a client: the negotiated mode arrives in the environment.
set -eu

CONF="${XDG_CONFIG_HOME:-$HOME/.config}/MangoHud/MangoHud.conf"

case "${1:-}" in
do)
  # Cap the game at the refresh this client actually negotiated.
  cp -f "$CONF" "$CONF.pf-bak"
  printf 'fps_limit=%s\n' "$PF_STREAM_REFRESH" >>"$CONF"

  # Light the panel's HDR only when the session really negotiated it.
  if [ "$PF_STREAM_HDR" = 1 ]; then
    kscreen-doctor output.HDMI-A-1.hdr.enable
  fi

  # The raster, for anything that wants pixels — a launcher's window size, a per-mode
  # config profile, or just a line in the journal naming what launched.
  logger -t punktfunk \
    "prep ${PF_APP_ID:-${PF_APP_TITLE:-desktop}}: ${PF_STREAM_WIDTH}x${PF_STREAM_HEIGHT}@${PF_STREAM_REFRESH}"
  ;;
undo)
  mv -f "$CONF.pf-bak" "$CONF"
  if [ "$PF_STREAM_HDR" = 1 ]; then
    kscreen-doctor output.HDMI-A-1.hdr.disable
  fi
  ;;
esac
```

Four things that example is quietly relying on:

- **`undo` sees exactly what its `do` saw.** The values are captured once, at launch, and held for
  the session — so teardown can branch on `PF_STREAM_HDR` and reach the same answer however the
  stream ended.
- **`PF_STREAM_HDR` is `1`/`0`**, the stream-marker file's spelling, not the `true`/`false` that
  `PF_EVENT_*` uses. One script can be written against either.
- **The app identity depends on the plane**: `PF_APP_ID` on a native client's launch,
  `PF_APP_TITLE` from a Moonlight one. `${PF_APP_ID:-${PF_APP_TITLE:-desktop}}` reads whichever one
  is set, and `:-` also catches the empty string a launch with no title of its own leaves behind.
- **`set -u` is doing work.** An older host doesn't set these, and the step then fails loudly (and
  disarms its own `undo`) instead of silently capping the game at `fps_limit=`.

The same `prep` array works on a custom `library.json` entry, where the identity arrives as
`PF_APP_ID`. The console's Library form has no input for prep steps and **clears** them on save, so
edit that file directly.

## Reacting to a game, not a stream

`stream.stopped` tells you the *stream* ended; `game.exited` tells you the *game* did. Often the
same moment, but not always — a desktop stream has no game at all, and a stream can outlive its
game if you turned off "end the session when the game exits". No polling needed:

```json
{ "hooks": [
    { "on": "game.running", "run": "/home/me/.config/punktfunk/scripts/game-up.sh" },
    { "on": "game.exited",  "run": "/home/me/.config/punktfunk/scripts/game-down.sh" }
] }
```

Both carry the title in `PF_EVENT_GAME_TITLE` / `PF_EVENT_GAME_APP`, and `game.exited` adds
`PF_EVENT_REASON` so a script can tell "the player quit" (`exited`) from "the host closed it"
(`terminated`) — worth checking before you, say, power the TV off.

Ending the session when a game exits needs no script: it is the default, on the console's
**Virtual displays** page under
[When a game or a session ends](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does).

## The event stream (`GET /api/v1/events`)

For a shell script or a status widget, the easy way is
[`punktfunk-host ctl watch`](/docs/host-cli#ctl) — it does the SSE, the `Last-Event-ID` resume and
the reconnect for you, and prints **one JSON object per line**, so the credentials never leave the
host binary:

```sh
punktfunk-host ctl watch --kinds pairing.pending,stream.'*'
```

It also emits a synthetic `{"kind":"ctl.resync"}` line when the stream fell off the host's catch-up
ring, which is the signal to re-snapshot rather than trust what you have.

For code that wants the raw stream, subscribe to SSE on the management API directly (loopback +
bearer token — the same credentials as the rest of the admin surface):

```sh
. ~/.config/punktfunk/mgmt-token   # sets PUNKTFUNK_MGMT_TOKEN
curl -Nk -H "Authorization: Bearer $PUNKTFUNK_MGMT_TOKEN" \
  "https://127.0.0.1:47990/api/v1/events?kinds=pairing.*,stream.*"
```

The token file holds `PUNKTFUNK_MGMT_TOKEN=<token>`, not a bare token, so it can be sourced (or
handed to a systemd unit as an `EnvironmentFile`) — `cat` it straight into the header and every
request comes back 401. The runner's `plugin-token` file has the same shape.

- Frames carry `id:` (the event's `seq`), `event:` (the kind), `data:` (the event JSON).
- Reconnect with the standard `Last-Event-ID` header (or `?since=<seq>`) and the host replays
  what you missed from its in-memory ring (~1024 events); if you fell off the ring you get one
  `event: dropped` frame first — resync from the REST snapshots (`/status`, `/clients`, …).
- No cursor replays the whole ring. One `event: live` frame closes the replay: what follows it
  happened after you connected, so a notifier should stay quiet until it arrives.
- `?kinds=` filters server-side: exact kinds or `domain.*` prefixes, comma-separated.

## Scripts, plugins, and the runner

For anything beyond a `curl` one-liner there is **`@punktfunk/host`** — the TypeScript SDK
(`sdk/` in the repo): typed events with automatic reconnect/resume, the REST surface, and a
plugin convention (`punktfunk-plugin-*`). Its **runner** (`punktfunk-scripting`) supervises a
directory of scripts and installed plugins as one service: crash-restarts with backoff, and a
`systemctl stop` that interrupts plugins structurally so their cleanup runs. See the SDK README
for the five-line quickstart and unit templates.

For ready-made plugins — sync your ROM collection or your Playnite library into the game library, or
hand a USB device on the couch to the host — see [Plugins](/docs/plugins). Install one from the web
console's **Plugins** page (Browse → pick → confirm; the host installs it and restarts the runner),
or from a terminal with `punktfunk-host plugins add <name>` followed by
`punktfunk-host plugins enable`.

The canonical "decide, don't just observe" pattern — approve pairing from your phone: watch
`pairing.pending`, send yourself a notification, and call
`POST /api/v1/native/pending/{id}/approve` when you tap yes. The full API is documented at
[`/api/docs`](/api) on your host.

> A unit under the runner auto-connects with the host's **scoped plugin token**, which covers
> the everyday surface (status, library, sessions, events) but deliberately not **hook
> registration**, **pairing administration**, the **plugin store** (`/api/v1/store…`, reads
> included), the **update endpoints** (`/api/v1/update…`), or another plugin's UI credential — so a
> plugin defect can't admit new devices, install code, or trigger an update. Those routes answer
> 403 on the plugin token. A script that should administer pairing (like the approval pattern above)
> opts into the full-admin credential explicitly: set `PUNKTFUNK_MGMT_TOKEN` on the unit (e.g.
> a `systemctl --user edit punktfunk-scripting` drop-in) or pass `{ token }` to `connect()`.

## Recipe: full controller passthrough (VirtualHere)

To get a controller's *native* features on the host — DualSense gyro, touchpad, adaptive
triggers, USB rumble — or to use a device no emulation can stand in for (a racing wheel, a HOTAS),
hand the physical device from the couch to the host over
[VirtualHere](https://www.virtualhere.com/) (USB-over-IP) while you play.

**Use the plugin.** [VirtualHere passthrough](/docs/plugins#virtualhere-usb-passthrough) finds the
device by name (so it survives the couch rebooting), brackets it around the session, gives it back
if anything crashes, and tells you which half of the setup is broken. That is the supported route;
the rest of this section is for people who would rather not install a plugin.

**Turn off controller forwarding on the couch.** Whatever route you take, the client that hands the
device over should stop *also* forwarding it: Settings → **Forward controllers**, off
([Client settings](/docs/client-settings#input)). Otherwise the host gets two controllers for one
pair of hands and games read both. On Linux and Windows it matters twice over — while the client
has the pad open it has *claimed* the device node, and VirtualHere cannot bind a device somebody
else is holding.

**The two sides.** VirtualHere is a server/client pair, and you run both: the **server on the couch**
(where the device is plugged in) shares it, and the **client on the host** mounts it. The client's
`-t` flag is a one-shot IPC to the already-running client — `-t LIST` prints every visible device
with its address (`server.port`, e.g. `couch-deck.11`), `-t "USE,<addr>"` mounts it, and
`-t "STOP USING,<addr>"` hands it back.

### Zero-code: two hooks

Bracket it on the stream with two [hooks](#hooks-hooksjson):

```json
{
  "hooks": [
    { "on": "stream.started", "run": "vhclientx86_64 -t \"USE,couch-deck.11\"" },
    { "on": "stream.stopped", "run": "vhclientx86_64 -t \"STOP USING,couch-deck.11\"" }
  ]
}
```

`couch-deck.11` is the device's address from `vhclientx86_64 -t LIST`.

The trade-offs the plugin exists to fix: the address is hard-coded, so it breaks when the couch
reboots or the device moves port; and if the stream ends abnormally the `stream.stopped` hook never
fires, leaving the device stranded on the host until somebody notices. There is also a
[`virtualhere-dualsense.ts`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/examples/virtualhere-dualsense.ts)
SDK example to build your own script on.

> VirtualHere is a commercial product, sold separately by VirtualHere Pty. Ltd. — free for one
> shared device, licensed beyond that. Punktfunk is not affiliated with it.
