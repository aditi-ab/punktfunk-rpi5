# punktfunk-core

The **shared protocol core** — the one place where punktfunk's transport, forward error correction,
and crypto live. It's linked into the [host](../punktfunk-host/README.md) and every native client, so
there's exactly one implementation of the wire format everywhere.

Written in Rust with **no async on the per-frame path** (native threads only). It exposes both a
normal Rust API and a **stable, versioned C ABI**, so the Swift and Kotlin clients — and any C
embedder — link the same code as the Rust ones.

## What's in here

- **Transport & session** (`session.rs`, `transport/`, `packet.rs`) — the `punktfunk/1` data plane
  over raw UDP: packetization, reassembly (with attacker-bounded limits), pacing, and socket tuning.
- **FEC** (`fec/`) — the wall-breaker. Two codes:
  - **GF(2⁸)** classic Reed–Solomon with the *Cauchy* generator matrix — byte-identical to the
    `nanors` library Moonlight uses, so our parity is decodable by a stock Moonlight client.
  - **GF(2¹⁶) Leopard-RS** (SIMD, O(n log n)) — up to 65535 shards/block, which removes the ~1 Gbps
    FEC ceiling. `punktfunk/1` negotiates this one.
- **Crypto** (`crypto.rs`) — AES-128-GCM session encryption with per-direction nonce salts and
  sequence-as-AAD; SPAKE2 PIN pairing lives behind the `quic` feature.
- **QUIC control plane** (`quic.rs`, `client.rs`, feature `quic`) — the Hello/Welcome/Start handshake,
  cert pinning/TOFU, reverse audio, and the embeddable `NativeClient` connector. This is the **only**
  place `tokio`/`quinn` are allowed; the feature is **off by default** so the core stays runtime-free.
- **C ABI** (`abi.rs`) — the versioned surface (`punktfunk_abi_version()`, `PunktfunkConfig` carrying
  its own `struct_size`) that generates [`include/punktfunk_core.h`](../../include/punktfunk_core.h)
  via cbindgen at build time.

## Build outputs

The crate builds three ways at once (`crate-type = ["lib", "cdylib", "staticlib"]`):

| Output | Used by |
|--------|---------|
| `lib` (rlib) | the host, probe, and tools link it as a normal Rust crate |
| `cdylib` (`.so`/`.dylib`) | the Swift / Kotlin clients via the C ABI |
| `staticlib` (`.a`) | the C test harness and static embedding |

## Test

```sh
cargo test -p punktfunk-core                 # unit + proptest + loopback
cargo run  -p loss-harness                   # FEC loss-resilience sweep (no network needed)
bash crates/punktfunk-core/tests/c/run.sh    # standalone C-ABI link + round-trip proof
```

## Design invariants (do not regress)

- **One core, linked everywhere** — protocol/FEC/crypto live only here, behind the stable C ABI.
- **No async on the hot path** — the per-frame pipeline is native threads only; `quic` (tokio/quinn)
  is control-plane only, feature-gated, off by default.
- **Security hardening stays intact** — the reassembler bounds attacker-controlled fields before
  allocating; AES-GCM keeps per-direction nonce salts + seq-as-AAD; the ABI checks `struct_size`.
  Regression tests exist — keep them green.

## Related

- **[`punktfunk-host`](../punktfunk-host/README.md)** — the streaming host built on this core
- **[Clients](../../clients/)** — the apps that link this core over the C ABI (or directly, in Rust)
- **punktfunk-planning: `implementation-plan.md`** (internal planning repo) — why GF(2¹⁶) FEC, the
  latency budget, and the architecture thesis
