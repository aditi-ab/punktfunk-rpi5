# pf-dualsense — virtual DualSense UMDF2 HID minidriver

A self-authored **Rust UMDF2 HID minidriver** that presents a virtual Sony **DualSense**
(VID `054C` / PID `0CE6`) to Windows, so games drive adaptive triggers / lightbar / rumble —
capabilities ViGEm structurally cannot deliver. It's how the punktfunk Windows host gives a client's
DualSense a near-native feel with **no external gamepad dependencies** (no ViGEmBus).

Shipping: the driver is one member of the in-tree driver workspace
([`packaging/windows/drivers/`](../../README.md)), built from source in CI, and bundled +
`pnputil`-installed by the Windows host [installer](../../README.md). The host feeds it over a shared
memory channel from `crates/punktfunk-host/src/inject/windows/dualsense_windows.rs`. The same UMDF driver also
serves the **DualShock 4** identity per a `device_type` byte the host stamps.

This README captures the driver-authoring lore — the bugs and the signing recipe that make a
self-signed UMDF HID driver actually load. The authoritative build/sign/package flow (CI + Inno Setup)
lives in the [Windows host packaging README](../../README.md).

## Build workspace

This crate builds as a member of the [`packaging/windows/drivers/`](../../drivers) workspace, which
uses the published **crates.io `wdk`/`wdk-sys`/`wdk-build`** (0.4/0.5) — not the old dev-box
`windows-drivers-rs` path-deps. It's a separate cargo workspace from the main tree because driver
crates are cdylibs built with the WDK toolchain on Windows only; it path-deps the shared ABI crate
[`crates/pf-driver-proto`](../../../../crates/pf-driver-proto/README.md).

## Build / sign / install recipe (the one that actually loads)

Prereqs on the Windows box: **WDK 26100**, **LLVM** (the current default; bindgen 0.72 builds on clang
22), Rust MSVC. Built as a member of the `packaging/windows/drivers/` workspace (plain `cargo build`, no
cargo-make). A self-signed CodeSigning cert in `CurrentUser\My` + `LocalMachine\Root` +
`TrustedPublisher`.

Every build needs:

```powershell
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:Version_Number = '10.0.26100.0'   # else wdk-build picks 10.0.28000.0 (no km/crt) and bindgen fails
```

The shipping flow is `build-gamepad-drivers.ps1` (one level up): workspace `cargo build --release`
plus the sign steps below, staged for the installer. The original manual dev-box recipe, kept as
lore (paths reflect that era's cargo-make layout):

```powershell
cargo make                              # -> target\debug\pf_dualsense_package\ (.inf/.cat/.dll)

# *** CRITICAL: clear the PE FORCE_INTEGRITY bit ***
# windows-drivers-rs links the DLL with /INTEGRITYCHECK, which forces a CI-trusted page-hash
# signature a self-signed cert cannot satisfy (CodeIntegrity 3004 "hash not found" /
# 3089 VerificationError 7). SudoVDA.dll (third-party VDD prior art, not used by punktfunk) has
# this bit OFF. Clear bit 0x80 at PE-header offset +0x5e:
$f = 'target\debug\pf_dualsense_package\pf_dualsense.dll'
$b = [IO.File]::ReadAllBytes($f); $pe = [BitConverter]::ToInt32($b,0x3c); $off = $pe + 0x5e
$dc = [BitConverter]::ToUInt16($b,$off); $bb = [BitConverter]::GetBytes([uint16]($dc -band 0xFF7F))
$b[$off]=$bb[0]; $b[$off+1]=$bb[1]; [IO.File]::WriteAllBytes($f,$b)

signtool sign /fd SHA256 /sha1 <cert-thumbprint> $f
Remove-Item target\debug\pf_dualsense_package\pf_dualsense.cat
Inf2Cat /driver:target\debug\pf_dualsense_package /os:10_x64
signtool sign /fd SHA256 /sha1 <cert-thumbprint> target\debug\pf_dualsense_package\pf_dualsense.cat

pnputil /add-driver target\debug\pf_dualsense_package\pf_dualsense.inf /install
devgen /add /hardwareid "root\pf_dualsense"     # creates the (transient, SWD) device node
```

`devgen` (under `Windows Kits\10\Tools\<ver>\x64\`) is only for manual testing — the shipping
install is `punktfunk-host.exe driver install --gamepad`, and the host SwDeviceCreate's the device
per session (no persistent devnode). SWD devgen devices clear on reboot. TODO: drop the post-build
PE patch by stopping wdk-build emitting `/INTEGRITYCHECK`.

## The three bugs that made it work (porting a WDK C sample to Rust)

`WDF_*_CONFIG_INIT` / `WDF_OBJECT_ATTRIBUTES_INIT` macros set **non-zero** defaults — `mem::zeroed()`
silently breaks them:

1. **FORCE_INTEGRITY** (above) — the load wall.
2. **Timer `ExecutionLevel`** — zeroed = Invalid → `WdfTimerCreate` 0xC0200209. Set
   `ExecutionLevel/SynchronizationScope = InheritFromParent` + `AutomaticSerialization = TRUE`
   (the working vhidmini2 shape).
3. **Queue `Settings.Parallel.NumberOfPresentedRequests`** — zeroed = 0 → a parallel queue presents
   zero requests → `EvtIoDeviceControl` never fires → no HID handshake → ~5 s timeout →
   `CM_PROB_FAILED_START`. Set to `u32::MAX`.

## Notes

- **Multi-pad** works via `UmdfHostProcessSharing=ProcessSharingDisabled` — each pad gets its own
  WUDFHost (so the per-instance statics don't collide), and the driver reads its pad index from the
  device Location (`WdfDeviceAllocAndQueryProperty`) to poll its own `*-boot-<index>` bootstrap
  mailbox (the DATA section itself is unnamed — the sealed pad channel,
  punktfunk-planning: `gamepad-channel-sealing.md` — and its `pad_index` is validated against this
  index on attach).
- Port of the WDK `vhidmini2` UMDF2 sample; the DualSense identity + 273-byte descriptor + feature
  blobs `0x05`/`0x09`/`0x20` come from `crates/punktfunk-host/src/inject/proto/dualsense_proto.rs`.
