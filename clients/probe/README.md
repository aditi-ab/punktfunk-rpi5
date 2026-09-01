# punktfunk — probe (reference client)

`punktfunk-probe` is the **headless reference client** for the `punktfunk/1` protocol — a
command-line tool for testing, latency measurement, and validating host behavior. It's not a
streaming app you'd watch on; it connects, exercises a plane, and reports numbers. If you want to
actually stream, use the [Linux](../linux/README.md), [Windows](../windows/README.md),
[Apple](../apple/README.md), or [Android](../android/README.md) clients.

Because it links the same **`punktfunk-core`** as every other client, it's also the canonical
example of driving the protocol end to end: QUIC control plane, UDP data plane, and the side planes
(input, audio, rumble) over QUIC datagrams.

## What it does

- **Receives a real stream**, writes a playable elementary stream (`.h265`/`.h264`/`.av1` — the
  extension tracks the **negotiated codec**; the probe advertises all three and the host picks), and
  reports per-frame **capture→received latency** percentiles (the host stamps each frame with
  its capture clock).
- **Verification mode** against a synthetic host — byte-checks deterministic test frames.
- **Exercises every plane** with scripted test traffic:
  `--input-test` (mouse/keyboard), `--mic-test` (a 440 Hz Opus tone up to the host mic),
  `--touch-test` (a synthetic finger), `--rich-input-test` (DualSense touchpad + motion, logging the
  HID-output feedback that comes back).
- **Trust** — `--pin <64-hex>` pins the host fingerprint; `--pair -` (PIN on stdin) runs the SPAKE2 pairing
  ceremony and prints the verified fingerprint to pin from then on. Without a pin it trusts on first
  use.
- **Discovery** — `--discover [secs]` browses the LAN for `_punktfunk._udp` hosts and prints each
  (name, addr:port, pairing requirement, cert fingerprint), then exits.
- **Negotiation knobs** — `--mode WxHxFPS`, `--remode` (mid-stream mode change), `--bitrate`,
  `--codec auto|h264|hevc|av1` (preference; the host resolves), `--audio-channels`
  (stereo / 5.1 / 7.1), `--compositor`, `--gamepad`, `--launch`, `--speed-test`.
  Env: `PUNKTFUNK_CLIENT_10BIT=1` / `PUNKTFUNK_CLIENT_444=1` advertise the 10-bit / 4:4:4 client
  caps (for testing a host's `PUNKTFUNK_10BIT`/`PUNKTFUNK_444`).

## Usage

```sh
# stream 720p120 from a host, save the video, and print latency percentiles:
cargo run -p punktfunk-probe -- --mode 1280x720x120 --connect HOST:PORT --out /tmp/a.h265

# list hosts on the LAN:
cargo run -p punktfunk-probe -- --discover

# pair with a host that requires it (read the PIN off the host), then stream:
cargo run -p punktfunk-probe -- --connect HOST:PORT --pair 1234
cargo run -p punktfunk-probe -- --connect HOST:PORT --pin <64-hex> --input-test
```

Full flag reference is in the module doc-comment at the top of [`src/main.rs`](src/main.rs).

## Related

- **[Project README](../../README.md)** — the host, the streaming clients, and the protocol
- **`punktfunk-host punktfunk1-host`** — the persistent native-protocol listener to probe against
  (see the "Running on this box" section of the repo README / `CLAUDE.md`)
