---
title: Host CLI
description: The punktfunk-host commands and the flags you'll actually use.
---

The host is one binary, `punktfunk-host`. Most of the time you'll run a single command; the rest reads
its settings from [`host.env`](/docs/configuration).

## `serve --native`

The normal way to run a host. Starts the unified host: the GameStream server (for Moonlight) **and**
the native `punktfunk/1` server, plus the management API/web console — all in one process.

```sh
punktfunk-host serve --native
```

| Flag | Meaning |
|---|---|
| `--native` | Also run the native `punktfunk/1` server (recommended; enables the Apple app and discovery). |
| `--native-port <PORT>` | Native QUIC port (default `9777`). |
| `--open` | Don't require pairing — serve any device on the network. Off by default; only for trusted single-user setups. |
| `--mgmt-bind <IP:PORT>` | Management API address (default loopback `127.0.0.1:47990`). |
| `--mgmt-token <TOKEN>` | Override the bearer token for the management API. |

These are the only flags `serve` accepts.

The management API is **always HTTPS with bearer-token auth**. If you don't pass `--mgmt-token`, a token
is auto-generated and persisted to `~/.config/punktfunk/mgmt-token`; `--mgmt-token` only overrides it. A
token is **required** when you bind the API off loopback with `--mgmt-bind`.

By default the host **requires pairing** — see [Pairing & Trust](/docs/pairing). On `serve --native` you
**arm pairing from the web console** (or mgmt API); the host then displays a 4-digit PIN. Pass `--open` to
turn off the mandatory-pairing default and serve any device on the network (trusted single-user setups
only). The pairing flags below are `punktfunk1-host`-only and do **not** apply to `serve`.

## `punktfunk1-host`

A standalone native-only host, mainly for testing the `punktfunk/1` path without the GameStream server
or web console.

```sh
punktfunk-host punktfunk1-host --source virtual
```

| Flag | Meaning |
|---|---|
| `--port <N>` | QUIC listen port (default `9777`). |
| `--source synthetic` · `virtual` | `virtual` uses a real virtual display + NVENC; `synthetic` emits test frames. |
| `--seconds <N>` / `--frames <N>` | Bound each session by wall-clock seconds or frame count. |
| `--max-concurrent <N>` | Stream at most N sessions at once (default 4); overflow waits in the queue. |
| `--max-sessions <N>` | Exit after N sessions (0 = serve forever). |
| `--allow-pairing` | Accept PIN pairing; the host prints a PIN when a client pairs. |
| `--require-pairing` | Only serve paired devices (implies `--allow-pairing`). |

`--max-concurrent`, `--allow-pairing`, and `--require-pairing` are **`punktfunk1-host`-only** — `serve` does not
accept them. On `serve --native` you arm pairing from the web console instead, and concurrency is not
yet capped from the command line.

Both `serve --native` and `punktfunk1-host` advertise the host on the network so clients can discover it. List
hosts from another machine with `punktfunk-probe --discover`.

## Environment

Most behaviour (compositor, video source, input backend, zero-copy) is set in
[`host.env`](/docs/configuration), not on the command line. When running as a
[service](/docs/running-as-a-service), the unit loads `host.env` for you.
