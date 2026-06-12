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
| `--mgmt-token <TOKEN>` | Bearer token for the management API; required when `--mgmt-bind` isn't loopback. |

By default the host **requires pairing** — see [Pairing & Trust](/docs/pairing). Arm pairing from the
web console (or the `m3-host` flags below for a quick test).

## `m3-host`

A standalone native-only host, mainly for testing the `punktfunk/1` path without the GameStream server
or web console.

```sh
punktfunk-host m3-host --source virtual
```

| Flag | Meaning |
|---|---|
| `--port <N>` | QUIC listen port (default `9777`). |
| `--source virtual` | Use a real virtual display + NVENC (vs. `synthetic` test frames). |
| `--max-concurrent <N>` | Stream at most N sessions at once (default 4); overflow waits in the queue. |
| `--max-sessions <N>` | Exit after N sessions (0 = serve forever). |
| `--allow-pairing` | Accept PIN pairing; the host prints a PIN when a client pairs. |
| `--require-pairing` | Only serve paired devices (implies `--allow-pairing`). |

Both `serve --native` and `m3-host` advertise the host on the network so clients can discover it. List
hosts from another machine with `punktfunk-client-rs --discover`.

## Environment

Most behaviour (compositor, video source, input backend, zero-copy) is set in
[`host.env`](/docs/configuration), not on the command line. When running as a
[service](/docs/running-as-a-service), the unit loads `host.env` for you.
