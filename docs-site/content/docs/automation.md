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

The canonical "decide, don't just observe" pattern — approve pairing from your phone: watch
`pairing.pending`, send yourself a notification, and call
`POST /api/v1/native/pending/{id}/approve` when you tap yes. The full API is documented at
[`/api/docs`](/api) on your host.
