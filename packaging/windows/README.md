# Windows host packaging — signed Inno Setup installer

A one-file, signed `setup.exe` for the punktfunk streaming **host** on Windows, published to Gitea's
generic package registry (`punktfunk-host-windows`) by `.gitea/workflows/windows-host.yml`.

> Full picture (drivers-from-source, toolchain, CI, dev loop): **punktfunk-planning: `windows-build-and-packaging.md`** (internal planning repo). This README is the `packaging/windows/` file index.

## Windows 11 22H2+ only (no Windows 10)

The installer refuses anything below **Windows 11 22H2 (build 22621)** — `MinVersion=10.0.22621` in
`punktfunk-host.iss`, with a `[Messages]` override naming the requirement. The floor comes from the
**pf-vdisplay** driver: it is built against the **IddCx 1.10** class extension (the HDR `*2` DDIs +
the FP16 adapter cap, linked via the 1.10 `IddCxStub`, no runtime `IddCxGetVersion` downgrade), and
IddCx 1.10 first shipped in Windows 11 22H2. On older Windows — **all of Windows 10 including LTSC,
and Windows 11 21H2** — the driver *package* installs fine, but the device then fails to start with
**Code 10 `STATUS_DEVICE_POWER_FAILURE`** in Device Manager and every session dies with "pf-vdisplay
driver interface not found". Gating the installer turns that late, confusing failure into an upfront
message. (Down-level SDR-only support would need a runtime IddCx version check in the driver —
tracked as a possible future feature, not planned.)

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
  then `punktfunk-host.exe web setup` writes the ACL'd `%ProgramData%\punktfunk\web-password`, opens
  TCP 47992, and deletes any legacy `PunktfunkWeb` scheduled task. The console itself runs as a
  **supervised child of the `PunktfunkHost` service** (`bun` on `:47992`, restarted on any exit,
  stdout in `logs\web.log`), started the moment the host has written its mgmt token + identity cert.
  It proxies the host's loopback mgmt API with the host's own `%ProgramData%\punktfunk\mgmt-token`.
- **GameStream (Moonlight) compatibility is a wizard task** (**unchecked** by default — it pairs over
  plain HTTP, so it is opt-in like the Public-firewall task): the choice is passed to
  `service install --gamestream=on|off`, which writes `PUNKTFUNK_HOST_CMD=serve --gamestream` (or
  `serve`, the secure native-only host) into `host.env`. Unattended, add it with
  `/MERGETASKS=gamestream`. Upgrade-safe: a hand-customized `PUNKTFUNK_HOST_CMD` is never
  overwritten, and on an upgrade the task is inert entirely (the flag is omitted, so `host.env`
  keeps whatever it already says) — change an existing host with
  `punktfunk-host service install --gamestream=on|off` plus a service restart.
- **Branded, modern wizard**: `WizardStyle=modern dynamic windows11` (Inno ≥ 6.6 — Windows-11-style
  controls following the system light/dark theme; pre-6.6 compilers fall back to plain `modern`), with
  the punktfunk lens mark on the side panel / header tile and a multi-size `punktfunk.ico`
  (`SetupIconFile` + the Apps & features entry). Assets are generated **and committed** by
  `branding/gen-branding.ps1` from the canonical brand geometry (`web/src/components/brand-mark.tsx`);
  re-run it only when the brand changes.
- **Upgrade:** stops a running `PunktfunkHost` service and waits for `STOPPED` before replacing files
  (otherwise the locked exe / respawning supervisor would block the copy), then re-points the service;
  the existing console password is kept (the wizard page is skipped).
- **Uninstall** (Add/Remove Programs): runs `service uninstall` (stop + delete service + remove
  firewall rules), removes the `PunktfunkWeb` task + its firewall rule, then `driver uninstall` (+
  `--gamepad`) removes the punktfunk virtual-device drivers — the pf-vdisplay device node(s) and the
  pf-vdisplay / pf-gamepad / pf-xusb driver-store packages (the field report was that they survived
  uninstall). **A VB-CABLE from an older punktfunk install is intentionally NOT removed** (a
  third-party shared component the user may use elsewhere — its own uninstaller is
  `VBCABLE_Setup_x64.exe -u -h`); the `%ProgramData%\punktfunk`
  config (incl. `web-password`) is also left in place.

Silent install: `punktfunk-host-setup-<ver>.exe /VERYSILENT` (omit the driver with
`/MERGETASKS="!installdriver"`; disable Moonlight compat with `/MERGETASKS="!gamestream"`). A silent
fresh install uses the generated random console password — read it from
`%ProgramData%\punktfunk\web-password`.

## Prerequisites on the target box

- A **GPU for hardware encode**: an NVIDIA GPU + driver (NVENC), an AMD GPU (native AMF), or an
  Intel GPU (native QSV via the statically linked VPL dispatcher; the runtime ships in the Intel
  driver) — the CI exe is built `--features nvenc,amf-qsv,qsv`. Software H.264 is the GPU-less
  fallback.
- **Virtual gamepads need no prerequisite.** The DualSense / DualShock 4 / Xbox 360 (XUSB) UMDF drivers
  are **bundled** in the installer (the *Install the virtual gamepad drivers* task) and
  `pnputil`-installed. **ViGEmBus is no longer used.**
- **Audio uses Steam's streaming drivers — nothing is bundled.** A Windows audio device can only
  be created by a **kernel-mode** driver (no UMDF path exists), so unlike our self-signed UMDF
  drivers we cannot ship our own. The host instead mints its OWN devnode instances of Valve's
  vendor-signed streaming-audio drivers on the target box: **"Punktfunk Speakers"** (the
  client-only desktop-audio sink, from `SteamStreamingSpeakers.inf`) and **"Punktfunk
  Microphone"** (mic passthrough, from `SteamStreamingMicrophone.inf`). Audio therefore requires
  **Steam installed — never running**; the installer shows a suppressible notice when Steam is
  absent, and the host re-checks live, so installing Steam later just works. VB-CABLE was
  bundled for the mic until the audio-substrate change (2026-08) — a cable from an older install
  (or one the user installs) keeps working as a fallback mic target.

## Files here

| File | Role |
|------|------|
| `punktfunk-host.iss` | Inno Setup script (the installer definition). |
| `branding/` | Wizard branding: `gen-branding.ps1` renders the brand mark into the committed `wizard-image-*.bmp` / `wizard-small-*.bmp` (100–200% DPI) + `punktfunk.ico`. Re-run only on a brand change. |
| `pack-host-installer.ps1` | Orchestrator: cert + sign exe, **build + sign the drivers from source**, stage them + FFmpeg + the **web console** (`.output` + bun) + the HDR layer + branding, run ISCC, sign setup.exe. |
| `build-pf-vdisplay.ps1` | Build pf-vdisplay from source (the `drivers/` workspace) + clear FORCE_INTEGRITY + sign `.dll`/`.cat` + export `.cer`. |
| `build-gamepad-drivers.ps1` | Sign + catalog the gamepad drivers (`pf-gamepad` + `pf-xusb`) from the same workspace build (`-SkipBuild`), one shared cert. |
| `make-driver-cert.ps1` | Generate the stable `CN=punktfunk-driver` code-signing cert (the `DRIVER_CERT_PFX_B64` / `DRIVER_CERT_PASSWORD` secrets). No key container, so it works over SSH; self-tests with signtool where it can. See *Driver signing* above. |
| `clear-force-integrity.ps1` | Clear the `/INTEGRITYCHECK` PE bit so a self-signed driver loads (reused by every driver build). |
| `stage-pf-vdisplay.ps1` | Stage the just-built pf-vdisplay bundle + fetch/verify the **pinned** nefcon release. |
| `drivers/` | The all-Rust IddCx **driver source** workspace: the `pf-vdisplay` crate on `wdk-sys` / windows-drivers-rs + the owned `pf-driver-proto` ABI + `wdk-iddcx` / `wdk-probe`, plus `deploy-dev.ps1` (build/sign/install for dev). |
| `reset-pf-vdisplay.ps1` | **Dev:** recover a wedged driver — stop host → reap ghost monitor nodes → reload the adapter → start host (no reboot). See *Dev iteration* below. |
| `redeploy-pf-vdisplay.ps1` | **Dev:** one-shot redeploy — (optional) build → stop host → `deploy-dev.ps1 -Install` → reload adapter → start host. |
| `pf-vkhdr-layer/` | **HDR Vulkan layer** (standalone `cdylib`): lets Vulkan games (Doom: The Dark Ages, etc.) enable HDR over the virtual display by advertising the HDR surface formats the NVIDIA/AMD ICDs hide on an indirect display. Built by the packer, laid into `{app}\vklayer`, registered under `HKLM64\…\Khronos\Vulkan\ImplicitLayers` (opt-out *Install the HDR Vulkan layer* task). Self-gated on the display's HDR state. See its README. |

> **Drivers are built from source, not vendored.** All three (pf-vdisplay + the gamepad pf-gamepad /
> pf-xusb) are members of the all-Rust `drivers/` workspace (windows-drivers-rs / IddCx) and are
> **rebuilt + signed every release** by `build-pf-vdisplay.ps1` + `build-gamepad-drivers.ps1` - the
> checked-in prebuilt binaries were deleted (a stale `.cat` once stopped covering its `.inf` →
> `SPAPI_E_FILE_HASH_NOT_IN_CATALOG` on every box, and a frozen binary predated a driver IOCTL the host
> needed). Building from source keeps `.dll`/`.inf`/`.cat` in lockstep. nefcon (the device-node tool -
> the install creates the `root\pf_vdisplay` node with it, **never** `devgen`, which leaves persistent
> phantom devices) is fetched + SHA-256-verified from its pinned release in `stage-pf-vdisplay.ps1`. See
> punktfunk-planning: `windows-build-and-packaging.md` (internal planning repo) for the toolchain
> + signing details.

## Driver signing (`DRIVER_CERT_PFX_B64`)

Our three UMDF drivers are signed with a **stable self-signed code-signing cert**, subject
`CN=punktfunk-driver`, supplied to `build-pf-vdisplay.ps1` / `build-gamepad-drivers.ps1` as the
`DRIVER_CERT_PFX_B64` + `DRIVER_CERT_PASSWORD` Actions secrets. On a `v*` tag build a missing cert
is a **hard failure** (`-RequireSignedCert`, default `auto` off `GITHUB_REF`); canary and local
builds still fall back to a per-build throwaway.

**Current fingerprint (SHA-1 thumbprint):** `4B8493E7CD565758D335F8F4F05C5A7261A13E02`

Verify a shipped driver against it:

```powershell
$dll = Get-ChildItem C:\Windows\System32\DriverStore\FileRepository\pf_vdisplay*\pf_vdisplay.dll |
         Select-Object -First 1 -Expand FullName
(Get-AuthenticodeSignature $dll).SignerCertificate.Thumbprint
```

Why stable matters here. The installer trusts the `.cer` that ships in the bundle
(`certutil -addstore -f Root` + `TrustedPublisher`, `crates/punktfunk-host/src/windows/install.rs`),
which is unavoidable for a self-signed cert — a self-signed leaf is its own root, so the chain only
validates if the root is present. That means the signature does **not** authenticate the download:
anyone who can alter the bundle can put their own cert next to their own driver. What a stable cert
buys is everything downstream of that: one anchor imported once instead of two more roots per
upgrade, a fingerprint we can publish out-of-band so a substituted driver is *detectable*, a
publisher an admin can allowlist, and continuity across releases. `driver install` purges stale
`CN=punktfunk-driver` certs before adding the current one, and `driver uninstall` removes them
entirely — including the pile left by the per-build-cert era.

> ⚠️ **The private key is now worth stealing.** It is trusted as a machine **root** on every
> punktfunk box, with code-signing EKU and no practical revocation path (nobody removes a stale
> root, and self-signed roots aren't in any CRL users honour). Keep it in the CI secret and nowhere
> else — not on a dev laptop. This is the trade for stability, and the reason attestation signing
> (which chains to Microsoft and needs no root import at all) remains the real fix.

Generating it — **run `make-driver-cert.ps1` yourself** on a Windows box; it prints the thumbprint
and writes the two secret values to files, and the private key never touches a certificate store:

```powershell
pwsh -File packaging\windows\make-driver-cert.ps1 -TestOnly   # dry run: generates, self-tests, keeps nothing
pwsh -File packaging\windows\make-driver-cert.ps1             # the real thing
```

Then add both values as **repo**-level Gitea Actions secrets on `unom/punktfunk` — same scope as
the `MSIX_CERT_PFX_B64` cert next door, and only this repo builds drivers (`RPM_GPG_PRIVATE_KEY` is
org-level because other repos publish RPMs; nothing else needs this one). Back up the `.pfx` and its
password somewhere you'd keep a signing key, then delete the output folder.

Two details the script exists to get right, both learned the hard way:

- It builds the cert with the .NET `CertificateRequest` API instead of `New-SelfSignedCertificate`,
  so **no key container is involved** and generation works over SSH. `New-SelfSignedCertificate`
  fails there with `NTE_PERM 0x80090010` — a network logon has no key container. Note that
  *consuming* a `.pfx` (signtool, or loading it in .NET) still needs one, which is why the script's
  signtool self-test reports SKIPPED over SSH rather than failing. The key is valid either way; run
  it at a console/RDP session to exercise the self-test, or let a canary build be the proof.
- The extension set is explicit and matches what the drivers have always been signed with —
  `KeyUsage=DigitalSignature` (critical), `EKU=codeSigning` (non-critical), SubjectKeyIdentifier,
  and deliberately **no** basicConstraints. This is not the place to improvise: a chain-building
  difference would surface as a failed driver install on a user's machine, not as a build error.

It also avoids `Get-Random` for the .pfx passphrase (that's `System.Random`, not a cryptographic
RNG) and uses .NET's own PKCS#12 writer rather than OpenSSL, whose 3.x default AES-256/PBKDF2
encryption produces a `.pfx` Windows CryptoAPI often cannot read.

Keep an offline backup of the .pfx + password somewhere you'd keep a signing key. Losing it means
the next release ships a cert nobody has trusted before, and every user's installer adds a second
root — recoverable, but only by re-running the install.

## Dev iteration on the test box (driver)

Two helpers wrap the painful manual steps of iterating on the pf-vdisplay driver against a live host
service. Run **elevated**; both default to the `PunktfunkHost` service. (The `C:\t-goal1\...` probe
path below is the maintainer's test box — substitute your own `punktfunk-probe.exe` build.)

```powershell
# Recover a WEDGED driver. Symptom: every session fails with
#   create virtual output: pf-vdisplay ADD ...: DeviceIoControl(0x222400): Element nicht gefunden (0x80070490)
# i.e. ERROR_NOT_FOUND — sustained ADD/REMOVE churn exhausted the IddCx monitor slots (ghost
# "Generic Monitor (Punktfunk)" nodes pile up, target_ids climb). A host restart's CLEAR_ALL does NOT
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
# 1. build the host (NVENC needs no import lib — its entry points are runtime-loaded; `qsv`
#    statically links the vendored VPL dispatcher — needs cmake + a libclang, no FFmpeg)
cargo build --release -p punktfunk-host --features nvenc,qsv

# 2. pack (self-signed unless MSIX_CERT_PFX_B64/MSIX_CERT_PASSWORD are set; -NoDriver to skip pf-vdisplay)
pwsh -File packaging\windows\pack-host-installer.ps1 -Version 0.0.0-dev -TargetDir C:\t\release -OutDir C:\t\out
```

## Release

Push a `vX.Y.Z` tag — one tag releases every platform (see
[Release Channels](https://punktfunk.unom.io/docs/channels)). The workflow builds, signs, and
publishes `punktfunk-host-setup-X.Y.Z.exe` + the public `.cer`, refreshes the stable `latest/`
alias, and attaches the installer to the unified Gitea Release. Main pushes publish rolling
`<next-minor>.<run>` **canary** builds (base derived from the latest stable tag by
`scripts/ci/pf-version.ps1`) to the `canary/` alias.
