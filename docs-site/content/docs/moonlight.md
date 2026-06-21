---
title: Connect with Moonlight
description: Stream from a punktfunk host using any Moonlight client.
---

punktfunk speaks the **GameStream** protocol, so [Moonlight](https://moonlight-stream.org/) connects
to it like it would to any GameStream host — no punktfunk-specific app needed. It's a great option for
a browser, a smart TV, or any device without a native client.

> Many platforms also have a **native punktfunk client** with lower latency and built-in
> discovery/pairing — including **Windows** and **Android** (phone and Android TV). See
> [Clients](/docs/clients) before reaching for Moonlight.

## 1. Make sure the host is running with GameStream enabled

Moonlight needs the GameStream planes, which are **opt-in**. Run the host with `--gamestream`:

```sh
punktfunk-host serve --gamestream
```

(Bare `serve` is the secure native-only default and stock Moonlight clients can't connect to it; the
native plane is always on, and `--gamestream` adds the Moonlight-compat surface.) GameStream pairs over
plain HTTP and its legacy control encryption is weaker than the native plane's, so only enable it on a
**trusted LAN**. If you run the host as a [service](/docs/running-as-a-service), make sure its
`ExecStart` includes `--gamestream`. The host advertises itself on the network, so Moonlight usually
finds it on its own.

## 2. Add the host in Moonlight

Open Moonlight. Your host should appear automatically on the same network. If it doesn't, use **Add
Host manually** and enter the host machine's IP address.

## 3. Pair

Select the host and choose **Pair**. Moonlight shows a 4-digit PIN. On the host, you confirm pairing
(from the web console, or it accepts the ceremony when armed) — see [Pairing & Trust](/docs/pairing).
Once paired, Moonlight remembers the host.

## 4. Stream

Pick an app/desktop and start streaming. The host creates a virtual display at the resolution and
frame rate Moonlight requests (set these in Moonlight's settings), encodes it on the GPU, and streams
it. Mouse, keyboard, and controllers flow back to the host.

## Tips

- **Set your resolution and frame rate in Moonlight's settings** before connecting — the host matches
  whatever Moonlight asks for, creating the virtual display at that exact mode.
- **Codec:** HEVC (H.265) is a good default; AV1 is available if your client supports it.
- **Bitrate:** start moderate and raise it. For very high bitrates, the [native
  clients](/docs/clients) have a built-in speed test; with Moonlight, set the bitrate manually.
- Moonlight uses the GameStream protocol, not punktfunk's native FEC/encryption extensions. On a
  solid LAN this is fine; on a lossy link a [native client](/docs/clients) holds up better.
