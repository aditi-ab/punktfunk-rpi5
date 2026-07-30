---
title: Events & hooks
description: React to what the host does — lifecycle events over SSE, hook commands and webhooks, per-app prep/undo — for notifications, DND toggles, Home Assistant, and more.
---

The host emits a **lifecycle event** for the things you'd want to react to: a client connects or
disconnects, a stream starts or stops, a pairing request arrives, a virtual display is created,
the library changes, the host starts or shuts down. Two ways to consume them:

- **Hooks** — zero-code: entries in `~/.config/punktfunk/hooks.json` run a **command** or POST a
  **webhook** when a matching event fires. This covers the common automation: Do-Not-Disturb
  during a stream, a phone notification on a pairing request, pausing downloads while playing.
- **The event stream** — code: `GET /api/v1/events` on the management API is a standard
  [Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events)
  stream of the same events, for scripts and integrations that want to *decide* things (e.g.
  auto-approve pairing from a known subnet by calling the approve endpoint).

Hooks **observe** — they can never veto or delay a connection, a stream, or a pairing decision,
and nothing you configure here runs anywhere near the streaming path.

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
| `plugins.changed` | a plugin's registration changes (registered, restarted, deregistered, or its lease expired) | plugin id |
| `store.changed` | an install or uninstall finished, or a plugin catalog was refreshed | none — re-read `GET /api/v1/store/catalog` / `…/installed` |
| `host.started` / `host.stopping` | the serve planes come up / wind down | version, whether GameStream is enabled |

Every event is a small JSON document with a monotonic `seq`, a `ts_ms` timestamp, a `schema`
version (additive-only — fields get added, never renamed), and the fields above. Example:

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
    { "on": "stream.started",  "run": "~/.config/punktfunk/scripts/on-stream.sh" },
    { "on": "stream.stopped",  "run": "~/.config/punktfunk/scripts/off-stream.sh" },
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
| `webhook` | A URL the event JSON is POSTed to. TLS-verified, redirects are never followed, no punktfunk credentials attached. |
| `filter` | Optional exact-match constraints: `client` (device name), `fingerprint`, `plane` (`native`/`gamestream`), `app`. All present fields must match. |
| `timeout_s` | Command timeout (default 30, max 600) — on expiry the whole process group is killed. |
| `debounce_ms` | Minimum interval between firings of this hook (0 = every event). |
| `hmac_secret_file` | File with a secret; the webhook gains `X-Punktfunk-Signature: sha256=<hex HMAC-SHA256 of the body>` so your receiver can authenticate the host. |

A `run` command's shell one-liner vocabulary — the event flattened to env, values sanitized:

```sh
#!/bin/sh
# PF_EVENT_KIND=stream.started   PF_EVENT_SEQ=42
# PF_EVENT_STREAM_MODE=2560x1440@120   PF_EVENT_STREAM_HDR=true
# PF_EVENT_STREAM_CLIENT='Living Room TV'   PF_EVENT_STREAM_APP=steam:570
# PF_EVENT_STREAM_PLANE=native   PF_EVENT_JSON='{…the whole event…}'
[ "$PF_EVENT_KIND" = stream.started ] && makoctl mode -a do-not-disturb
```

Richer payloads (and the full document) are on stdin — `jq` away. On a Windows host running as
the service, the command runs **in your interactive session** (never as SYSTEM); that path can't
carry per-process env or stdin, so the event JSON's path is appended as the command's last
argument instead.

Verify a signed webhook (Python):

```python
import hmac, hashlib
expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
ok = hmac.compare_digest(request.headers["X-Punktfunk-Signature"], expected)
```

**Rules of the road:** hooks are fire-and-forget and bounded — at most 8 in flight (extra
firings are dropped with a log line, never queued), and a command that outlives its timeout is
killed. Because hook commands run as the host user, `hooks.json` is operator-privileged config;
a hook **script** must be owned by you (or root) and not group/world-writable, or the host
refuses to run it — loudly, in the log.

The two simplest cases also exist as plain [host.env](/docs/configuration) settings, no
`hooks.json` needed: `PUNKTFUNK_ON_CONNECT_CMD` and `PUNKTFUNK_ON_DISCONNECT_CMD`.

## Per-app prep/undo

For per-title setup (HDR toggle, MangoHud, a VRR tweak), attach `prep` steps to a GameStream
`apps.json` entry or a custom library entry — each `do` runs **before** the title launches
(synchronously — the launch waits), each `undo` runs at session end in **reverse order**,
best-effort, even if the session crashed:

```json
{ "id": 2, "title": "Steam", "compositor": "gamescope", "cmd": "steam -gamepadui",
  "prep": [
    { "do": "~/bin/hdr on",  "undo": "~/bin/hdr off" },
    { "do": "pactl set-default-sink game_sink", "undo": "pactl set-default-sink desk_sink" }
  ] }
```

A `do` that fails logs, keeps going, and its own `undo` is skipped (it never took effect).

## Reacting to a game, not a stream

`stream.stopped` tells you the *stream* ended; `game.exited` tells you the *game* did. They are
often the same moment, but not always — a desktop stream has no game at all, and a stream can
outlive its game if you turned off "end the session when the game exits".

If you have been polling the host to work out when a game finished, you don't need to any more:

```json
{ "hooks": [
    { "on": "game.running", "run": "~/.config/punktfunk/scripts/game-up.sh" },
    { "on": "game.exited",  "run": "~/.config/punktfunk/scripts/game-down.sh" }
] }
```

Both carry the title in `PF_EVENT_GAME_TITLE` / `PF_EVENT_GAME_APP`, and `game.exited` adds
`PF_EVENT_REASON` so a script can tell "the player quit" (`exited`) from "the host closed it"
(`terminated`) — worth checking before you, say, power the TV off.

Ending the session yourself when a game exits needs no script at all: it is the default behavior,
under Host → *Virtual displays* →
[When a game or a session ends](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does).

## The event stream (`GET /api/v1/events`)

For code, subscribe to the SSE stream on the management API (loopback + bearer token — the
same credentials as the rest of the admin surface):

```sh
curl -Nk -H "Authorization: Bearer $(cat ~/.config/punktfunk/mgmt-token)" \
  "https://127.0.0.1:47990/api/v1/events?kinds=pairing.*,stream.*"
```

- Frames carry `id:` (the event's `seq`), `event:` (the kind), `data:` (the event JSON).
- Reconnect with the standard `Last-Event-ID` header (or `?since=<seq>`) and the host replays
  what you missed from its in-memory ring (~1024 events); if you fell off the ring you get one
  `event: dropped` frame first — resync from the REST snapshots (`/status`, `/clients`, …).
- `?kinds=` filters server-side: exact kinds or `domain.*` prefixes, comma-separated.

## Scripts, plugins, and the runner

For anything beyond a `curl` one-liner there is **`@punktfunk/host`** — the TypeScript SDK
(`sdk/` in the repo): typed events with automatic reconnect/resume, the REST surface, and a
plugin convention (`punktfunk-plugin-*`). Its **runner** (`punktfunk-scripting`) supervises a
directory of scripts and installed plugins as one service: crash-restarts with backoff, and a
`systemctl stop` that interrupts plugins structurally so their cleanup runs. See the SDK README
for the five-line quickstart and unit templates.

For ready-made plugins — sync your ROM collection or your Playnite library into the game library,
with a console page to manage them — see [Plugins](/docs/plugins). Installing one is two commands:
`punktfunk-host plugins add <name>`, then `punktfunk-host plugins enable`.

The canonical "decide, don't just observe" pattern — approve pairing from your phone: watch
`pairing.pending`, send yourself a notification, and call
`POST /api/v1/native/pending/{id}/approve` when you tap yes. The full API is documented at
[`/api/docs`](/api) on your host.

> A unit under the runner auto-connects with the host's **scoped plugin token**, which covers
> the everyday surface (status, library, sessions, events) but deliberately not **hook
> registration** or **pairing administration** — so a plugin defect can't admit new devices or
> install commands. A script that should administer pairing (like the approval pattern above)
> opts into the full-admin credential explicitly: set `PUNKTFUNK_MGMT_TOKEN` on the unit (e.g.
> a `systemctl --user edit punktfunk-scripting` drop-in) or pass `{ token }` to `connect()`.

## Recipe: full controller passthrough (VirtualHere)

To get a controller's *native* features on the host — DualSense gyro, touchpad, adaptive
triggers, USB rumble — or to use a device no emulation can stand in for, like a racing wheel or a
HOTAS, hand the physical device from the couch to the host over
[VirtualHere](https://www.virtualhere.com/) (USB-over-IP) while you play.

**Use the plugin.** [VirtualHere passthrough](/docs/plugins#virtualhere-usb-passthrough) does all of
this for you: it finds the device by name (so it survives the couch rebooting), brackets it around
the session, gives it back if anything crashes, and tells you which half of the setup is broken when
it isn't working. That is the supported route, and the rest of this section is only for people who
would rather not install a plugin.

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

Know what this trades away, because the plugin exists to fix exactly these: the address is
hard-coded, so it breaks when the couch reboots or the device moves port; and if the stream ends
abnormally the `stream.stopped` hook never fires, leaving the device stranded on the host until
somebody notices. There is also a
[`virtualhere-dualsense.ts`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/examples/virtualhere-dualsense.ts)
SDK example if you want a worked script to build your own on.

> VirtualHere is a commercial product, sold separately by VirtualHere Pty. Ltd. — free for one
> shared device, licensed beyond that. Punktfunk is not affiliated with it.
