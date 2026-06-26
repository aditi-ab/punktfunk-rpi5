# pf-dualsense — virtual DualSense UMDF2 HID minidriver (M0 spike)

A self-authored **Rust UMDF2 HID minidriver** that presents a virtual Sony **DualSense**
(VID `054C` / PID `0CE6`) to Windows, so games drive adaptive triggers / lightbar / rumble —
capabilities ViGEm structurally cannot deliver. This is the M0 feasibility spike for rich
controller support in the punktfunk Windows host.

## Status (2026-06-21)

**Load + recognition: DONE.** A self-signed build **loads under Secure Boot ON** and enumerates as a
genuine DualSense HID game controller (`Status: OK`, VID `054C`, 273-byte DualSense report descriptor,
PID `0CE6` via `GET_DEVICE_ATTRIBUTES`). Validated live on the RTX box (`192.168.1.173`, Win11 25H2).

**Remaining:** the real-game `0x02` adaptive-trigger gate (Cyberpunk 2077 on the interactive desktop →
confirm `[pf-ds] *** OUTPUT ...` in the driver log), then wire into the host (M1+).

## This is a reference snapshot

The crate's `Cargo.toml` uses path-deps into `microsoft/windows-drivers-rs`
(`../../crates/wdk{,-sys,-build}`), so it builds **inside a `windows-drivers-rs` checkout's
`examples/` dir**, not standalone in this repo. On the dev box it lives at
`C:\Users\Public\m0\windows-drivers-rs\examples\pf-dualsense`. These files are checked in for
version control / portability of the spike.

## Build / sign / install recipe (the one that actually loads)

Prereqs on the Windows box: **WDK 26100**, **LLVM 21.1.2** (pinned — newer bindgen breaks),
`cargo-make`, Rust MSVC. A self-signed CodeSigning cert in `CurrentUser\My` + `LocalMachine\Root` +
`TrustedPublisher`.

Every build needs:

```powershell
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:Version_Number = '10.0.26100.0'   # else wdk-build picks 10.0.28000.0 (no km/crt) and bindgen fails
```

Then, in the example dir:

```powershell
cargo make                              # -> target\debug\pf_dualsense_package\ (.inf/.cat/.dll)

# *** CRITICAL: clear the PE FORCE_INTEGRITY bit ***
# windows-drivers-rs links the DLL with /INTEGRITYCHECK, which forces a CI-trusted page-hash
# signature a self-signed cert cannot satisfy (CodeIntegrity 3004 "hash not found" /
# 3089 VerificationError 7). SudoVDA.dll has this bit OFF. Clear bit 0x80 at PE-header offset +0x5e:
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

`devgen` is at `...\Windows Kits\10\Tools\10.0.26100.0\x64\devgen.exe`. SWD devgen devices clear on
reboot (recreate after each boot). TODO: drop the post-build PE patch by stopping wdk-build emitting
`/INTEGRITYCHECK`.

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

## Known limitations

- Uses **statics, not per-device WDF contexts** → only one device instance per WUDFHost works.
  Multi-instance needs proper device contexts.
- Port of the WDK `vhidmini2` UMDF2 sample; DualSense identity + 273-byte descriptor + feature blobs
  `0x05`/`0x09`/`0x20` from `crates/punktfunk-host/src/inject/dualsense.rs`.
