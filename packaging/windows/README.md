# Windows host packaging — signed Inno Setup installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

## Why not MSIX (like the client)

The host installs a **`LocalSystem` SCM service** that `CreateProcessAsUserW`'s from Session 0 into the
interactive session for secure-desktop (UAC / lock screen) capture, adds firewall rules, and depends
on the **SudoVDA** kernel/IDD virtual-display driver. MSIX's sandbox can install **neither** a SYSTEM
service of this kind **nor** a driver. So the host ships as a classic elevated installer.

The installer is deliberately thin: the real install logic — SCM registration, firewall rules, the
default `host.env`, and the SYSTEM→interactive-session supervisor — already lives in
`punktfunk-host service install` (`crates/punktfunk-host/src/service.rs`). The installer just lays the
exe into `C:\Program Files\punktfunk\` and calls that subcommand, elevated.

## What the installer does

- Installs `punktfunk-host.exe` (+ `host.env.example`, this README) to `{app}` (`C:\Program Files\punktfunk`).
- **Optional task** *Install the SudoVDA virtual display driver* — imports the driver's self-signed
  cert (machine `Root` + `TrustedPublisher`), creates the `root\sudomaker\sudovda` device node (only
  if absent — `install-sudovda.ps1`), and stages the driver with `pnputil /add-driver /install`.
  Best-effort: a driver failure warns but never aborts the install (the host degrades to a physical
  display without it).
- Runs `punktfunk-host service install` (idempotent; writes a default `host.env` only if absent, so
  user config survives upgrades) and, by the *Start service now* task, `service start`.
- **Upgrade:** stops a running `PunktfunkHost` service and waits for `STOPPED` before replacing files
  (otherwise the locked exe / respawning supervisor would block the copy), then re-points the service.
- **Uninstall** (Add/Remove Programs): runs `service uninstall` (stop + delete service + remove
  firewall rules). The SudoVDA driver is intentionally left installed.

Silent install: `punktfunk-host-setup-<ver>.exe /VERYSILENT` (omit the driver with `/MERGETASKS="!installdriver"`).

## Prerequisites on the target box

- An **NVIDIA GPU + driver** — the installer's exe is built `--features nvenc` and load-depends on the
  driver's `nvEncodeAPI64.dll`.
- **ViGEmBus** (optional) for virtual gamepads — still a manual prerequisite (not bundled yet):
  <https://github.com/nefarius/ViGEmBus/releases>.

## Files here

| File | Role |
|------|------|
| `punktfunk-host.iss` | Inno Setup script (the installer definition). |
| `pack-host-installer.ps1` | Orchestrator: cert + sign, fetch/stage SudoVDA, run ISCC, sign setup.exe, emit registry paths. |
| `fetch-sudovda.ps1` | Download + SHA-256-verify the **pinned** SudoVDA + nefcon releases; stage the driver payload. |
| `install-sudovda.ps1` | Runs at install time (elevated): trust cert → gated device-node create → `pnputil` install. |
| `nvenc/nvenc.def`, `nvenc/gen-nvenc-importlib.ps1` | Synthesise `nvencodeapi.lib` for the `--features nvenc` link (llvm-dlltool / lib.exe). |

> **Pinning:** the SudoVDA / nefcon release URLs + SHA-256s in `fetch-sudovda.ps1` are the source of
> truth for what ships. Confirm the latest asset URLs and fill the SHA-256s to lock a release.

## Build locally (Windows, MSVC + Windows SDK + Inno Setup)

```powershell
# 1. import lib for the nvenc link
pwsh -File packaging\windows\nvenc\gen-nvenc-importlib.ps1 -OutDir C:\t\nvenc
$env:PUNKTFUNK_NVENC_LIB_DIR = 'C:\t\nvenc'

# 2. build the host
cargo build --release -p punktfunk-host --features nvenc

# 3. pack (self-signed unless MSIX_CERT_PFX_B64/MSIX_CERT_PASSWORD are set; -NoDriver to skip SudoVDA)
pwsh -File packaging\windows\pack-host-installer.ps1 -Version 0.0.0-dev -TargetDir C:\t\release -OutDir C:\t\out
```

## Release

Push a `host-win-vX.Y.Z` tag — the workflow builds, signs, and publishes
`punktfunk-host-setup-X.Y.Z.exe` + the public `.cer`, and refreshes the `latest/` alias. Main pushes
publish rolling `0.2.<run>` builds (no `latest/` update).
