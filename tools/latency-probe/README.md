# latency-probe

**Glass-to-glass latency**, measured photon to photon with `glass-timer.html` and a camera. There is
no binary here. For per-frame capture→…→reassembled percentiles over a real `punktfunk/1` session,
use the [`probe`](../../clients/probe/README.md) client.

Companion to [`loss-harness`](../loss-harness/README.md) (FEC loss sweep).

## Comparing against other clients (`glass-timer.html`)

Overlay-vs-overlay comparisons against other streaming clients are apples-to-oranges: their
stats end at decode / render-*submit*, while Punktfunk's display stage stamps the system's real
on-glass time — a ~2-refresh present tail every composited iOS/tvOS client pays but almost none
can measure. The only fair cross-client number is **photon-to-photon**, and `glass-timer.html`
is the host half of that measurement:

1. Open `glass-timer.html` fullscreen on the streamed host display (click for fullscreen).
2. Stream it, and film the host monitor and the client device **in the same shot** with a
   high-speed camera — an iPhone at 240 fps slo-mo gives ~4 ms resolution.
3. Scrub the footage: in any single video frame, the difference between the two on-screen
   counters IS the glass-to-glass latency. Read ~20 spread-out frames for a p50.
4. Repeat back-to-back per client (same host, game/scene, network) — Punktfunk, Moonlight, ….

Reading aids, designed to survive motion blur + re-encode: the binary strip encodes
centiseconds as fat cells (read it when the last digits smear); the 100 ms sweep bar compares
by eye for quick sub-10 ms estimates; the corner parity block flips every host frame — if it
ever reads GRAY on the client, the client blended two host frames (repeat/tear tell). A
button-to-photon variant needs no page at all: film controller + client screen and count video
frames from the physical press to the visible reaction.
