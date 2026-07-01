# latency-probe

A **glass-to-glass latency** measurement tool (design/implementation-plan §10): it renders a
timestamp/QR on the host, reads it back off the client's capture (or a photodiode, for true photons),
and tracks p50/p99 — so latency regressions are quantifiable rather than felt.

> **Status: scaffold.** The measurement harness is stubbed out and not yet wired to a live pipeline.
> For latency numbers today, use the per-frame **capture→…→reassembled** percentiles the
> [`probe`](../../clients/probe/README.md) client reports over a real `punktfunk/1` session.

```sh
cargo run -p latency-probe        # from the repo root
```

Companion to [`loss-harness`](../loss-harness/README.md) (FEC loss sweep).
