# lumen Apple client (M5)

Swift + VideoToolbox (decode) + Metal (present) + SwiftUI, linking `lumen-core` through
the generated C ABI — **no glue layer**. Imports `include/lumen_core.h` via a module map.

## Wiring

1. Build the core as a static or dynamic library for Apple targets:
   ```sh
   rustup target add aarch64-apple-ios aarch64-apple-darwin
   cargo build -p lumen-core --release --target aarch64-apple-darwin   # liblumen_core.a / .dylib
   ```
2. Expose the C ABI to Swift with a module map (`module.modulemap` here) that points at
   the checked-in header `../../include/lumen_core.h`.
3. In Swift: create a client `LumenSession`, `lumen_client_poll_frame` on a display-link
   thread, feed the access unit to a `VTDecompressionSession`, present the `CVImageBuffer`
   with Metal aligned to the screen's refresh (frame pacing, plan §7).

## Status

Scaffold. The client half of `lumen_core` (`poll_frame`, FEC recovery, reassembly) is
complete and tested; this target adds the platform decode + present.
