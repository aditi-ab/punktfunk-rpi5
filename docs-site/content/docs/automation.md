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
triggers, USB rumble — instead of the emulated pad, hand the physical device from the couch to the
host over [VirtualHere](https://www.virtualhere.com/) (USB-over-IP), bound only while a client is
connected so the couch keeps its own controller the rest of the time.

**The two sides.** VirtualHere is a server/client pair, and you run both:

- **Server — on the couch** (where the pad is physically plugged in). Run the VirtualHere USB
  Server there; it shares the pad on the LAN. Leave it running.
- **Client — on the host** (where the game and this automation run). Install the VirtualHere
  Client (as a service, or the tray app). It auto-discovers the couch's shared pad on the same
  LAN; across subnets, add it once with `<VH_CLIENT> -t "MANUAL HUB ADD,<couch-ip>:7575"`. The
  client binary is `vhclientx86_64` on Linux, `vhui64.exe` on Windows, `vhclientosx` on macOS,
  `vhclientarm64` on ARM Linux.

The client's `-t` flag is a one-shot IPC to the already-running client: `-t LIST` prints every
visible device with its address (`server.port`, e.g. `couch-deck.11`); `-t "USE,<addr>"` mounts it
onto the host; `-t "STOP USING,<addr>"` hands it back. The automation just brackets
`USE` / `STOP USING` around a session.

### Zero-code: two hooks

Bracket it on the stream with two [hooks](#hooks-hooksjson) — mount when video starts, release
when it stops. This is all most setups need:

```json
{
  "hooks": [
    { "on": "stream.started", "run": "vhclientx86_64 -t \"USE,couch-deck.11\"" },
    { "on": "stream.stopped", "run": "vhclientx86_64 -t \"STOP USING,couch-deck.11\"" }
  ]
}
```

`couch-deck.11` is the device's VirtualHere address from `vhclientx86_64 -t LIST`.

### Scripted: resolve by name, release on shutdown

The hooks above hard-code the address, and can strand the pad on the host if it's stopped
mid-stream (the `stream.stopped` hook never fires). The
[`virtualhere-dualsense.ts`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/examples/virtualhere-dualsense.ts)
SDK example is the robust version: it resolves the device by **name substring** (survives the
address changing), can filter to **one couch** in a multi-client setup, and **releases the pad on
a clean shutdown** (`systemctl stop`, ^C) so the couch always gets its controller back.

It's a standalone [`@punktfunk/host` script](https://git.unom.io/unom/punktfunk/src/branch/main/sdk#running-a-single-script-as-a-service)
— run it in its own directory. The one edit is its import: the in-repo copy imports from
`../src/index.js`; outside the repo it's the published package:

```sh
mkdir ~/punktfunk-scripts && cd ~/punktfunk-scripts
bun init -y
bun add @punktfunk/host          # point the @punktfunk scope at the registry first — see the SDK README
# save the example as virtualhere-dualsense.ts, and change its first import to the package:
#   - import { connect } from "../src/index.js";
#   + import { connect } from "@punktfunk/host";
VH_DEVICE=DualSense bun virtualhere-dualsense.ts
```

`VH_DEVICE` is required — a VirtualHere address (`couch-deck.11`) or a device-name substring
(`DualSense`). Optional: `VH_CLIENT` overrides the client binary (default `vhclientx86_64`);
`VH_ONLY_CLIENT` binds only for one punktfunk client label. Running on the host box the SDK needs
no token or URL — it reads the host's loopback credentials itself (see
[Connection resolution](https://git.unom.io/unom/punktfunk/src/branch/main/sdk#connection-resolution)).

Keep it running as a
[systemd user unit](https://git.unom.io/unom/punktfunk/src/branch/main/sdk#running-a-single-script-as-a-service)
(its default `SIGTERM` triggers the script's own release step — so `systemctl stop` hands the pad
back), or drop it under the [scripting runner](/docs/plugins) with your other units.

> The example brackets on `client.connected` / `client.disconnected` — the pad returns to the
> couch the moment they disconnect. Switch to `stream.started` / `stream.stopped` if you'd rather
> pass it through only while video is actually flowing; both are noted in the file's header.
