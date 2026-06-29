# Windows host packaging — signed Inno Setup installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

> Full picture (drivers-from-source, toolchain, CI, dev loop): **[`design/windows-build-and-packaging.md`](../../design/windows-build-and-packaging.md)**. This README is the `packaging/windows/` file index.

## x64 only (no ARM64)

Unlike the client (which ships x64 + ARM64 MSIX), the host is **x64-only by design**. It is coupled to
an NVIDIA GPU (NVENC, via `nvEncodeAPI64.dll` from the driver) and the **pf-vdisplay** virtual-display
driver — neither exists on Windows ARM64 (no ARM64 NVIDIA driver; the driver builds x64-only). An
ARM64 host would install but couldn't encode or create a virtual display, so we don't build one.
Revisit if NVIDIA-ARM Windows PCs ever ship.

## Why not MSIX (like the client)

The host installs a **`LocalSystem` SCM service** that `CreateProcessAsUserW`'s from Session 0 into the
interactive session for secure-desktop (UAC / lock screen) capture, adds firewall rules, and depends
on the **pf-vdisplay** UMDF/IDD virtual-display driver. MSIX's sandbox can install **neither** a SYSTEM
service of this kind **nor** a driver. So the host ships as a classic elevated installer.

The installer is deliberately thin: the real install logic lives in `punktfunk-host` subcommands, not
in PowerShell — `service install` (SCM registration, firewall rules, the default `host.env`, the
SYSTEM→interactive-session supervisor; `service.rs`), `driver install [--gamepad]` and `web setup`
(driver/console provisioning; `windows/install.rs`). The installer lays the exe into
`C:\Program Files\punktfunk\` and calls those subcommands elevated. Keeping the logic in the compiled
exe — not a `.ps1` *file* PowerShell reads in the machine codepage — is the fix for the ANSI-codepage
parse breakage that silently failed installs on non-English boxes.

## What the installer does

- Installs `punktfunk-host.exe` (+ `host.env.example`, this README) to `{app}` (`C:\Program Files\punktfunk`).
- **Optional task** *Install the pf-vdisplay virtual display driver* — `punktfunk-host.exe driver install`
  imports the driver's self-signed cert (machine `Root` + `TrustedPublisher`), creates the
  `root\pf_vdisplay` device node (only if absent, via nefconc — never devgen), and stages the driver with
  `pnputil /add-driver /install`.
  Best-effort: a driver failure warns but never aborts the install (the host degrades to a physical
  display without it).
- Runs `punktfunk-host service install` (idempotent; writes a default `host.env` only if absent, so
  user config survives upgrades) and, by the *Start service now* task, `service start`.
- **Web management console** (bundled when packed with `-WebDir`/`-BunExe`, which the CI always is):
  lays down the built **self-contained** `.output` server (Nitro `noExternals` — deps bundled +
  tree-shaken, ~75 files, no `node_modules`) + a portable **bun**, prompts for a console login
  password (pre-filled with a secure random default, shown again on the final page; kept on upgrade),
  then `punktfunk-host.exe web setup` writes the ACL'd `%ProgramData%\punktfunk\web-password`, registers the
  **`PunktfunkWeb`** scheduled task (boot, SYSTEM, restart-on-failure → `web-run.cmd` → `bun` on
  `:3000`), opens TCP 3000, and starts it. It proxies the host's loopback mgmt API with the host's
  own `%ProgramData%\punktfunk\mgmt-token`.
- **Upgrade:** stops a running `PunktfunkHost` service and waits for `STOPPED` before replacing files
  (otherwise the locked exe / respawning supervisor would block the copy), then re-points the service;
  the existing console password is kept (the wizard page is skipped).
- **Uninstall** (Add/Remove Programs): runs `service uninstall` (stop + delete service + remove
  firewall rules) and removes the `PunktfunkWeb` task + its firewall rule. The pf-vdisplay driver and the
  `%ProgramData%\punktfunk` config (incl. `web-password`) are intentionally left in place.

Silent install: `punktfunk-host-setup-<ver>.exe /VERYSILENT` (omit the driver with
`/MERGETASKS="!installdriver"`). A silent fresh install uses the generated random console password —
read it from `%ProgramData%\punktfunk\web-password`.

## Prerequisites on the target box

- A **GPU for hardware encode**: an NVIDIA GPU + driver (NVENC), or an AMD/Intel GPU (AMF/QSV) — the
  exe is built `--features nvenc,amf-qsv`. Software H.264 is the GPU-less fallback.
- **Virtual gamepads need no prerequisite.** The DualSense / DualShock 4 / Xbox 360 (XUSB) UMDF drivers
  are **bundled** in the installer (the *Install the virtual gamepad drivers* task) and
  `pnputil`-installed. **ViGEmBus is no longer used.**
- **The streaming microphone uses VB-CABLE**, bundled + silently installed by the installer (the *Install
  VB-CABLE virtual audio* task). The host writes the client's mic into VB-CABLE's input; its `CABLE
  Output` capture endpoint surfaces as a host mic. A Windows audio device can only be created by a
  **kernel-mode** driver (no UMDF path exists), so unlike our self-signed UMDF drivers we cannot ship our
  own — VB-CABLE is a vendor-signed cable that loads with no test-signing. It is **donationware** by
  VB-Audio, redistributed under VB-Audio's bundling grant (only the single base cable); see
  `licenses/VB-CABLE-NOTICE.txt`. The package binary is **not** in the repo — supply it to the packer via
  `-VbCableDir` / `$env:VBCABLE_DIR` (the extracted official package, containing `VBCABLE_Setup_x64.exe`).
  Absent → the installer is built without it and the host falls back to auto-installing the Steam
  Streaming pair. *(Endgame: attestation-sign our own MIT virtual-audio driver to drop this dependency.)*

## Files here

| File | Role |
|------|------|
| `punktfunk-host.iss` | Inno Setup script (the installer definition). |
| `pack-host-installer.ps1` | Orchestrator: cert + sign exe, **build + sign the drivers from source**, stage them + FFmpeg + the **web console** (`.output` + bun) + the HDR layer, run ISCC, sign setup.exe. |
| `build-pf-vdisplay.ps1` | Build pf-vdisplay from source (the `drivers/` workspace) + clear FORCE_INTEGRITY + sign `.dll`/`.cat` + export `.cer`. |
| `build-gamepad-drivers.ps1` | Sign + catalog the gamepad drivers (`pf-dualsense` + `pf-xusb`) from the same workspace build (`-SkipBuild`), one shared cert. |
| `install-vbcable.ps1` | On-target: seed VB-Audio's cert into `TrustedPublisher`, silently install the bundled VB-CABLE (`-i -h`). Run by the installer's *Install VB-CABLE virtual audio* task; idempotent + always exits 0 (non-fatal). |
| `clear-force-integrity.ps1` | Clear the `/INTEGRITYCHECK` PE bit so a self-signed driver loads (reused by every driver build). |
| `stage-pf-vdisplay.ps1` | Stage the just-built pf-vdisplay bundle + fetch/verify the **pinned** nefcon release. |
| `../../scripts/windows/web-run.cmd` | The `PunktfunkWeb` task action: loads the mgmt token + login password env, runs the bundled `bun` on the Nitro server (`:3000`). |
| `drivers/` | The all-Rust IddCx **driver source** workspace: the `pf-vdisplay` crate on `wdk-sys` / windows-drivers-rs + the owned `pf-driver-proto` ABI + `wdk-iddcx` / `wdk-probe`, plus `deploy-dev.ps1` (build/sign/install for dev). |
| `reset-pf-vdisplay.ps1` | **Dev:** recover a wedged driver — stop host → reap ghost monitor nodes → reload the adapter → start host (no reboot). See *Dev iteration* below. |
| `redeploy-pf-vdisplay.ps1` | **Dev:** one-shot redeploy — (optional) build → stop host → `deploy-dev.ps1 -Install` → reload adapter → start host. |
| `nvenc/nvenc.def`, `nvenc/gen-nvenc-importlib.ps1` | Synthesise `nvencodeapi.lib` for the `--features nvenc` link (llvm-dlltool / lib.exe). |
| `pf-vkhdr-layer/` | **HDR Vulkan layer** (standalone `cdylib`): lets Vulkan games (Doom: The Dark Ages, etc.) enable HDR over the virtual display by advertising the HDR surface formats the NVIDIA/AMD ICDs hide on an indirect display. Built by the packer, laid into `{app}\vklayer`, registered under `HKLM64\…\Khronos\Vulkan\ImplicitLayers` (opt-out *Install the HDR Vulkan layer* task). Self-gated on the display's HDR state. See its README. |

> **Drivers are built from source, not vendored.** All three (pf-vdisplay + the gamepad pf-dualsense /
> pf-xusb) are members of the all-Rust `drivers/` workspace (windows-drivers-rs / IddCx) and are
> **rebuilt + signed every release** by `build-pf-vdisplay.ps1` + `build-gamepad-drivers.ps1` - the
> checked-in prebuilt binaries were deleted (a stale `.cat` once stopped covering its `.inf` →
> `SPAPI_E_FILE_HASH_NOT_IN_CATALOG` on every box, and a frozen binary predated a driver IOCTL the host
> needed). Building from source keeps `.dll`/`.inf`/`.cat` in lockstep. nefcon (the device-node tool -
> the install creates the `root\pf_vdisplay` node with it, **never** `devgen`, which leaves persistent
> phantom devices) is fetched + SHA-256-verified from its pinned release in `stage-pf-vdisplay.ps1`. See
> [`design/windows-build-and-packaging.md`](../../design/windows-build-and-packaging.md) for the toolchain
> + signing details.

## Dev iteration on the test box (driver)

Two helpers wrap the painful manual steps of iterating on the pf-vdisplay driver against a live host
service. Run **elevated**; both default to the `PunktfunkHost` service.

```powershell
# Recover a WEDGED driver. Symptom: every session fails with
#   create virtual output: pf-vdisplay ADD ...: DeviceIoControl(0x222400): Element nicht gefunden (0x80070490)
# i.e. ERROR_NOT_FOUND — sustained ADD/REMOVE churn exhausted the IddCx monitor slots (ghost
# "Generic Monitor (punktfunk)" nodes pile up, target_ids climb). A host restart's CLEAR_ALL does NOT
# fix it; the driver instance must be reloaded. This clears the ghosts + cycles the adapter (no reboot —
# this box boots to Proxmox).
powershell -ExecutionPolicy Bypass -File reset-pf-vdisplay.ps1 -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe

# Redeploy a driver build cleanly (stop host → install with a strictly-increasing DriverVer → reload
# adapter → start host). -Build runs `cargo build` first, but ONLY from an MSVC dev shell
# (LIBCLANG_PATH + Version_Number=10.0.26100.0); otherwise build separately and omit -Build.
powershell -ExecutionPolicy Bypass -File redeploy-pf-vdisplay.ps1 -Build -Verify -Probe C:\t-goal1\debug\punktfunk-probe.exe
```

The driver should reclaim monitor slots on REMOVE so churn can't wedge it; until it does, `reset` is
the recovery. From a Linux box drive either over SSH, e.g.
`ssh user@box 'powershell -ExecutionPolicy Bypass -File C:\...\reset-pf-vdisplay.ps1'`.

## Build locally (Windows, MSVC + Windows SDK + Inno Setup)

```powershell
# 1. import lib for the nvenc link
pwsh -File packaging\windows\nvenc\gen-nvenc-importlib.ps1 -OutDir C:\t\nvenc
$env:PUNKTFUNK_NVENC_LIB_DIR = 'C:\t\nvenc'

# 2. build the host
cargo build --release -p punktfunk-host --features nvenc

# 3. pack (self-signed unless MSIX_CERT_PFX_B64/MSIX_CERT_PASSWORD are set; -NoDriver to skip pf-vdisplay)
pwsh -File packaging\windows\pack-host-installer.ps1 -Version 0.0.0-dev -TargetDir C:\t\release -OutDir C:\t\out
```

## Release

Push a `vX.Y.Z` tag — one tag releases every platform (see
[Release Channels](https://punktfunk.unom.io/docs/channels)). The workflow builds, signs, and
publishes `punktfunk-host-setup-X.Y.Z.exe` + the public `.cer`, refreshes the stable `latest/`
alias, and attaches the installer to the unified Gitea Release. Main pushes publish rolling
`0.3.<run>` **canary** builds to the `canary/` alias.
