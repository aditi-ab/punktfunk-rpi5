---
title: Host CLI
description: The punktfunk-host commands and the flags you'll actually use.
---

The host is one binary, `punktfunk-host`. Most of the time you'll run a single command; the rest reads
its settings from [`host.env`](/docs/configuration).

## `serve`

The normal way to run a host. By default `serve` starts the **secure native host**: the native
`punktfunk/1` server (QUIC, SPAKE2 PIN pairing, per-direction AEAD) plus the management API/web
console — all in one process. The native plane is **always on**; there is no flag to turn it off.

```sh
punktfunk-host serve
```

Add `--gamestream` (alias `--moonlight`) to **also** run the GameStream/Moonlight-compatible planes
(nvhttp pairing, RTSP, ENet control, `_nvstream` mDNS) — required for stock [Moonlight](/docs/moonlight)
clients. This is **opt-in** because GameStream carries inherent on-path weaknesses (pairing over plain
HTTP; its legacy control encryption can reuse GCM nonces), so enable it **only on a trusted LAN**. The
native plane is immune to those issues.

```sh
punktfunk-host serve --gamestream
```

| Flag | Meaning |
|---|---|
| `--gamestream` / `--moonlight` | Also run the GameStream/Moonlight-compat planes (for stock Moonlight clients). Opt-in, trusted-LAN only — see above. |
| `--native` | No-op. The native `punktfunk/1` server always runs in `serve`; kept only for backward compatibility. |
| `--native-port <PORT>` | Native QUIC port (default `9777`). |
| `--open` | Don't require pairing — serve any device on the network. Off by default; only for trusted single-user setups. |
| `--mgmt-bind <IP:PORT>` | Management API address (default `0.0.0.0:47990` — all interfaces, so paired clients can browse the game library over mTLS; pass `127.0.0.1:47990` to keep it loopback-only). |
| `--mgmt-token <TOKEN>` | Override the bearer token for the management API. |

These are the only flags `serve` accepts.

The management API is **always HTTPS**. It binds all interfaces by default so a **paired client** can
fetch the game library over its mTLS certificate — but off loopback that certificate reaches only the
read-only status + library endpoints. The **admin surface** (arming pairing, removing devices, session
control, library edits) authenticates with a **bearer token** and is honored **from loopback only**, so
it is never LAN-exposed even under the default wide bind. If you don't pass `--mgmt-token`, a token is
auto-generated and persisted to `~/.config/punktfunk/mgmt-token` (the bundled web console reads the same
file); `--mgmt-token` only overrides it. Pass `--mgmt-bind 127.0.0.1:47990` to keep 47990 loopback-only.
Every endpoint is documented in the interactive [**API Reference**](/api).

By default the host **requires pairing** — see [Pairing & Trust](/docs/pairing). On `serve` you
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
accept them. On `serve` you arm pairing from the web console instead, and concurrency is fixed at
the built-in default (4 sessions) rather than settable from the command line.

Both `serve` and `punktfunk1-host` advertise the host on the network so clients can discover it. List
hosts from another machine with `punktfunk-probe --discover`.

## Environment

Most behaviour (compositor, video source, input backend, zero-copy) is set in
[`host.env`](/docs/configuration), not on the command line. When running as a
[service](/docs/running-as-a-service), the unit loads `host.env` for you.
