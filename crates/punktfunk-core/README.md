# punktfunk-core

The shared protocol core: wire framing, FEC, crypto, and the data-plane session state, linked into
the [host](../punktfunk-host/) and every native client so there is exactly one implementation of the
wire format everywhere. `src/lib.rs`'s module docs are the map of what lives where.

Two things about this crate are contracts rather than choices:

- **No async on the per-frame path.** `tokio` and `quinn` are confined to the optional `quic`
  feature — the control plane — which is off by default so the core stays runtime-free.
- **The C ABI is versioned.** `abi.rs` generates [`include/punktfunk_core.h`](../../include/punktfunk_core.h)
  via cbindgen (`cargo run -p gen-headers`); `punktfunk_abi_version()` and `PunktfunkConfig::struct_size`
  are how an embedder detects a mismatch instead of corrupting a struct.

The crate builds `lib`, `cdylib` and `staticlib` at once: the rlib for the host and tools, the
cdylib for the Swift and Kotlin clients over the C ABI, the staticlib for C embedding and the test
harness.

## Test

```sh
cargo test -p punktfunk-core                 # unit + proptest + loopback
cargo run  -p loss-harness                   # FEC loss-resilience sweep (no network needed)
bash crates/punktfunk-core/tests/c/run.sh    # standalone C-ABI link + round-trip proof
```

The reassembler bounds attacker-controlled fields before allocating, AES-GCM keeps per-direction
nonce salts with sequence-as-AAD, and the ABI checks `struct_size`. Each has a regression test —
keep them green.
