# Windows host packaging — signed Inno Setup installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

## x64 only (no ARM64)

Unlike the client (which ships x64 + ARM64 MSIX), the host is **x64-only by design**. It is coupled to
an NVIDIA GPU (NVENC, via `nvEncodeAPI64.dll` from the driver) and the **SudoVDA** virtual-display
driver — neither exists on Windows ARM64 (no ARM64 NVIDIA driver; the vendored SudoVDA is x64-only). An
ARM64 host would install but couldn't encode or create a virtual display, so we don't build one.
Revisit if NVIDIA-ARM Windows PCs + an ARM64 SudoVDA ever ship.

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
| `pack-host-installer.ps1` | Orchestrator: cert + sign, stage the driver bundle, run ISCC, sign setup.exe, emit registry paths. |
| `stage-sudovda.ps1` | Stage the **vendored** SudoVDA driver + fetch/verify the **pinned** nefcon release into the bundle. |
| `install-sudovda.ps1` | Runs at install time (elevated): trust cert → gated device-node create → `pnputil` install. |
| `sudovda/` | **Vendored** prebuilt SudoVDA driver: `SudoVDA.inf` / `sudovda.cat` / `SudoVDA.dll` / `sudovda.cer`. |
| `nvenc/nvenc.def`, `nvenc/gen-nvenc-importlib.ps1` | Synthesise `nvencodeapi.lib` for the `--features nvenc` link (llvm-dlltool / lib.exe). |

> **Vendored driver:** SudoVDA has no upstream release (its repo is a source-only VS solution; Apollo
> embeds the driver in its own installer), so the prebuilt **signed** driver is checked in under
> `sudovda/` (MIT/CC0; v1.10.9.289, signer `CN=sudovda@su.mk`, Class=Display, HWID
> `Root\SudoMaker\SudoVDA`). To refresh it, copy the four files out of a box's driver store
> (`C:\Windows\System32\DriverStore\FileRepository\sudovda.inf_amd64_*`) and re-derive `sudovda.cer`
> from the `.cat` signer (`(Get-AuthenticodeSignature sudovda.cat).SignerCertificate | Export-Certificate`).
> nefcon (the device-node tool) **is** fetched + SHA-256-verified from its pinned release in
> `stage-sudovda.ps1`.

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

Push a `vX.Y.Z` tag — one tag releases every platform (see
[Release Channels](https://punktfunk.unom.io/docs/channels)). The workflow builds, signs, and
publishes `punktfunk-host-setup-X.Y.Z.exe` + the public `.cer`, refreshes the stable `latest/`
alias, and attaches the installer to the unified Gitea Release. Main pushes publish rolling
`0.3.<run>` **canary** builds to the `canary/` alias.
