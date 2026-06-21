# punktfunk Windows client — MSIX packaging

The Windows client ships as **signed MSIX** packages so Windows boxes get a real package (Start
tile, clean install/uninstall) instead of a loose exe. CI builds + publishes them from
[`.gitea/workflows/windows-msix.yml`](../../../.gitea/workflows/windows-msix.yml) to Gitea's
**generic** package registry (`https://git.unom.io/unom/-/packages`), on every `main` push that
touches the client (canary) and on `vX.Y.Z` release tags (stable) — see
[Release Channels](https://punktfunk.unom.io/docs/channels).

**Two architectures, one x64 runner.** Both `x64` and `arm64` packages are produced off the single
x64 Windows runner — `x86_64-pc-windows-msvc` builds natively, `aarch64-pc-windows-msvc` is
cross-compiled (the x64 MSVC toolset ships the ARM64 cross compiler; the matrix points `FFMPEG_DIR`
at the runner's ARM64 FFmpeg tree, `C:\Users\Public\ffmpeg-arm64`). Artifacts are arch-suffixed
(`..._x64.msix` / `..._arm64.msix`, each with its matching `.cer`); `pack-msix.ps1 -Arch x64|arm64`
stamps the manifest `ProcessorArchitecture` and names the output. See
[`windows.yml`](../../../.gitea/workflows/windows.yml) for the cross-build rationale.

## What's in the package

`pack-msix.ps1` assembles a layout from a `cargo build --release` and runs `makeappx` + `signtool`:

| File | Source |
|---|---|
| `punktfunk-client.exe` | the release build |
| `Microsoft.WindowsAppRuntime.Bootstrap.dll`, `resources.pri` | auto-staged by windows-reactor's `build.rs` |
| `SDL3.dll` | auto-staged by the `sdl3` crate |
| `avcodec/avformat/avutil/swscale/swresample/...-*.dll` | `FFMPEG_DIR\bin` |
| `Assets\*.png` | checked-in tile/store logos (rasterized from `packaging/flatpak/io.unom.Punktfunk.svg`) |
| `AppxManifest.xml` | the template here, with `{VERSION}`/`{PUBLISHER}` substituted |

### Why an "unpackaged" WinUI app packages cleanly

windows-reactor calls `MddBootstrapInitialize2` with `OnPackageIdentity_NOOP`
(`crates/libs/reactor/src/app.rs`), so under MSIX **package identity** the App SDK bootstrapper is
a no-op and the runtime is resolved from the manifest's `<PackageDependency>` on
`Microsoft.WindowsAppRuntime.2` instead (reactor pins `WINDOWSAPPSDK_RELEASE_MAJORMINOR = 0x20000`
= 2.0). It's a full-trust Win32 app (`EntryPoint="Windows.FullTrustApplication"` + `runFullTrust`)
because it owns raw D3D11, Win32 low-level input hooks, WASAPI and SDL3.

## Versioning

MSIX requires a strictly 4-part numeric version. The workflow computes:
- `vX.Y.Z` tag → `X.Y.Z.0` (THE release; any `-rc`/`+meta` suffix is dropped for MSIX). Published to
  the stable `latest/` alias and attached to the unified Gitea Release.
- `main` push / `workflow_dispatch` → `0.3.<run_number>.0` (canary, climbs by run number; `canary/` alias).

## Signing & install

CI signs every build with a **stable self-signed code-signing cert** (`CN=unom`, SHA-1
`CD1EFDEEEC9743AFC38F56C5AF30C5A3009BE941`, valid to 2036). Its public half is checked in as
[`punktfunk-codesign.cer`](punktfunk-codesign.cer); the private `.pfx` + password live in the
`MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD` Actions secrets. Because it's the *same* cert every build,
trusting it is **one-time, per machine** — once imported, every future build and in-place upgrade is
trusted with no further prompt:

```powershell
# once per machine (elevated): trust the publisher
Import-Certificate -FilePath .\punktfunk-codesign.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
# then install the package for your CPU (and re-run for each upgrade — no re-trust needed)
Add-AppxPackage -Path .\punktfunk-client-windows_<ver>_x64.msix     # Intel/AMD
Add-AppxPackage -Path .\punktfunk-client-windows_<ver>_arm64.msix   # ARM64 (Snapdragon, etc.)
```

The matching `.cer` is also published next to each `.msix` in the registry, so it's always at hand.

The MSIX declares a dependency on the Windows App SDK 2.x runtime; install
[the App SDK runtime](https://aka.ms/windowsappsdk) if `Add-AppxPackage` reports a missing
`Microsoft.WindowsAppRuntime.2` framework.

`pack-msix.ps1` signing precedence: it uses the **`MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD`** secrets
when present (the stable cert above), else generates an *ephemeral* self-signed cert (forks / local
builds without the secrets). Either way it exports the signing cert's public `.cer` for the import.
**To move to a publicly-trusted (no-import) cert** — Azure Artifact Signing or a public OV cert —
replace the two secrets with the new `.pfx`; the cert's subject DN must equal the manifest
`Publisher`, so pass a matching `-Publisher` (it's stamped into the package `Identity`, and changing
it changes the package identity → a one-time reinstall).

## Building locally

On the Windows runner / dev VM (MSVC + Windows SDK present), after a release build:

```powershell
# x64
cargo build --release -p punktfunk-client-windows --target x86_64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix

# arm64 (cross-compiled; point FFMPEG_DIR at the ARM64 tree)
$env:FFMPEG_DIR = 'C:\Users\Public\ffmpeg-arm64'
cargo build --release -p punktfunk-client-windows --target aarch64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -Arch arm64 -TargetDir C:\t\aarch64-pc-windows-msvc\release -OutDir C:\t\msix
```

Validated end-to-end on the build VM (pack → sign → `Add-AppxPackage` → framework-dependency
resolution). The only step that needs a real display is *launching* the WinUI window (same
on-glass constraint as the rest of the client).
