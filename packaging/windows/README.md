# Windows host packaging — signed Inno Setup installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

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

The installer is deliberately thin: the real install logic — SCM registration, firewall rules, the
default `host.env`, and the SYSTEM→interactive-session supervisor — already lives in
`punktfunk-host service install` (`crates/punktfunk-host/src/service.rs`). The installer just lays the
exe into `C:\Program Files\punktfunk\` and calls that subcommand, elevated.

## What the installer does

- Installs `punktfunk-host.exe` (+ `host.env.example`, this README) to `{app}` (`C:\Program Files\punktfunk`).
- **Optional task** *Install the pf-vdisplay virtual display driver* — imports the driver's self-signed
  cert (machine `Root` + `TrustedPublisher`), creates the `root\pf_vdisplay` device node (only
  if absent, via nefconc — never devgen — `install-pf-vdisplay.ps1`), and stages the driver with
  `pnputil /add-driver /install`.
  Best-effort: a driver failure warns but never aborts the install (the host degrades to a physical
  display without it).
- Runs `punktfunk-host service install` (idempotent; writes a default `host.env` only if absent, so
  user config survives upgrades) and, by the *Start service now* task, `service start`.
- **Web management console** (bundled when packed with `-WebDir`/`-BunExe`, which the CI always is):
  lays down the built **self-contained** `.output` server (Nitro `noExternals` — deps bundled +
  tree-shaken, ~75 files, no `node_modules`) + a portable **bun**, prompts for a console login
  password (pre-filled with a secure random default, shown again on the final page; kept on upgrade),
  then `web-setup.ps1` writes the ACL'd `%ProgramData%\punktfunk\web-password`, registers the
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

- An **NVIDIA GPU + driver** — the installer's exe is built `--features nvenc` and load-depends on the
  driver's `nvEncodeAPI64.dll`.
- **ViGEmBus** (optional) for virtual gamepads — still a manual prerequisite (not bundled yet):
  <https://github.com/nefarius/ViGEmBus/releases>.

## Files here

| File | Role |
|------|------|
| `punktfunk-host.iss` | Inno Setup script (the installer definition). |
| `pack-host-installer.ps1` | Orchestrator: cert + sign, stage the driver + FFmpeg + **web console** (`.output` + bun) bundles, run ISCC, sign setup.exe, emit registry paths. |
| `stage-pf-vdisplay.ps1` | Stage the **vendored** pf-vdisplay driver + fetch/verify the **pinned** nefcon release into the bundle. |
| `install-pf-vdisplay.ps1` | Runs at install time (elevated): trust cert → gated device-node create (nefconc) → `pnputil` install. |
| `../../scripts/windows/web-run.cmd` | The `PunktfunkWeb` task action: loads the mgmt token + login password env, runs the bundled `bun` on the Nitro server (`:3000`). |
| `../../scripts/windows/web-setup.ps1` | Install-time (elevated): write the ACL'd console password, register the `PunktfunkWeb` task + firewall rule, start it. |
| `pf-vdisplay/` | **Vendored** signed pf-vdisplay driver: `pf_vdisplay.inf` / `pf_vdisplay.cat` / `pf_vdisplay.dll` / `punktfunk-driver.cer`. Built from `vdisplay-driver/`. |
| `vdisplay-driver/` | The all-Rust IddCx **driver source** (`pf-vdisplay` crate + vendored `wdf-umdf*` bindings) + `deploy-dev.ps1` (build/sign/install for dev). |
| `nvenc/nvenc.def`, `nvenc/gen-nvenc-importlib.ps1` | Synthesise `nvencodeapi.lib` for the `--features nvenc` link (llvm-dlltool / lib.exe). |

> **Vendored driver:** pf-vdisplay is our **all-Rust IddCx** virtual display (UMDF2), built from
> `packaging/windows/vdisplay-driver/`. It replaced the vendored SudoVDA C++ driver — full story in
> [`docs/windows-virtual-display-rust-port.md`](../../docs/windows-virtual-display-rust-port.md). The
> **signed** output (`pf_vdisplay.dll`/`.inf`/`.cat` + `punktfunk-driver.cer`; signer
> `punktfunk-ds-test` — the same cert the gamepad drivers ship, Class=Display, HWID `root\pf_vdisplay`)
> is checked in under `pf-vdisplay/`. To refresh it after a driver-source change, rebuild + re-sign with
> `vdisplay-driver/deploy-dev.ps1` and copy the staged `pf_vdisplay.{dll,inf,cat}` over the vendored
> copies. nefcon (the device-node tool — the install creates the node with it, **never** `devgen`, which
> leaves persistent phantom devices) **is** fetched + SHA-256-verified from its pinned release in
> `stage-pf-vdisplay.ps1`.

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
