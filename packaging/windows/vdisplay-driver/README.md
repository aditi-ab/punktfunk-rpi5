# pf-vdisplay — punktfunk Windows virtual display (Rust IddCx)

P1 of replacing the vendored **SudoVDA** C++ driver with one we own — a pure-Rust UMDF2 **IddCx**
(Indirect Display Driver) virtual display, drop-in compatible with the host's existing
`vdisplay/sudovda.rs` IOCTL control plane. Full rationale + roadmap:
[`docs/windows-virtual-display-rust-port.md`](../../../docs/windows-virtual-display-rust-port.md).

## Layout

```
vdisplay-driver/
  wdf-umdf-sys/     VENDORED bindgen FFI to WDF + IddCx (links WdfDriverStubUm + IddCxStub)
  wdf-umdf/         VENDORED safe wrappers (IddCx*/Wdf*)
  pf-vdisplay/      OUR driver crate (cdylib) + pf_vdisplay.inx
```

`wdf-umdf-sys` / `wdf-umdf` are vendored from [MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)
(MIT — see `LICENSE.virtual-display-rs`). They're a self-contained bindgen over the WDK, **not**
`windows-drivers-rs` (which the gamepad drivers use): IddCx functions are direct `IddCxStub` exports the
WDF function-table macro can't reach, so a unified bindgen is the cleaner base. Local fix carried in
`wdf-umdf-sys/build.rs`: resolve the `IddCxStub` lib path by the SDK version that actually contains
`um\x64\iddcx\<ver>` (a newer base SDK alongside the WDK has `um\x64` but no `iddcx`).

## Status

- **Scaffold builds** ✅ — workspace + vendored bindings + `pf-vdisplay` compile in-tree to
  `pf_vdisplay.dll`. The reference (virtual-display-rs) was separately built + installed + loaded
  (Status OK) on the RTX box, proving the IddCx-in-Rust chain end to end.
- **Next (P1 driver logic):** port the IddCx driver — `DriverEntry` → `IDD_CX_CLIENT_CONFIG`
  (adapter-init / parse-monitor-description / query-target-modes / assign-swapchain) → device + monitor
  context, generic EDID, no-op swap-chain drain (DDA still captures for P1). Logging via
  `OutputDebugString` (no `log`/`driver-logger`/`tokio`).
- **Then (control plane):** the SudoVDA-compatible IOCTL surface on the control device
  (`ADD`/`REMOVE`/`PING`/`GET_WATCHDOG`/`GET_VERSION`/`SET_RENDER_ADAPTER`, byte-identical structs +
  the `{e5bcc234-…}` interface GUID) so `vdisplay/sudovda.rs` drives it **unchanged**; a default mode
  table + the per-`ADD` client mode injected as preferred; the watchdog.
- **Later (P2):** push swap-chain frames straight to the host (skip DDA); HDR via the IddCx 1.11 D3D12
  acquire path.

## Build

Needs the WDK (UMDF 2.31 + IddCx stubs), LLVM/clang (`LIBCLANG_PATH`), and the pinned
`nightly-2024-07-26` (auto-selected via `rust-toolchain.toml`). From `pf-vdisplay/`, inside an MSVC dev
shell:

```
set LIBCLANG_PATH=C:\Program Files\LLVM\bin
cargo build            # -> ../target/x86_64-pc-windows-msvc/debug/pf_vdisplay.dll
```

## Sign + install (same recipe as the gamepad drivers)

1. (no FORCE_INTEGRITY bit to clear — this crate doesn't set `/INTEGRITYCHECK`)
2. `signtool sign /fd SHA256 /sha1 <punktfunk-ds-test thumbprint>` the renamed `pf_vdisplay.dll`
3. `stampinf -f pf_vdisplay.inf -d * -a amd64 -u 2.15.0 -v <ver>` ; `Inf2Cat /driver:<dir> /os:10_X64` ;
   sign the `.cat`
4. `pnputil /add-driver pf_vdisplay.inf` ; create the root devnode (`nefconc --create-device-node
   --hardware-id root\pf_vdisplay --class-name Display --class-guid {4d36e968-…}`, mirroring
   `install-sudovda.ps1`)

Bundles into the Inno Setup installer the same way as `gamepad-drivers/` once the driver is functional.
