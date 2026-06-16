# punktfunk Windows client — MSIX packaging

The Windows client ships as a **signed MSIX** so Windows boxes get a real package (Start tile,
clean install/uninstall) instead of a loose exe. CI builds + publishes it from
[`.gitea/workflows/windows-msix.yml`](../../../.gitea/workflows/windows-msix.yml) to Gitea's
**generic** package registry (`https://git.unom.io/unom/-/packages`), on every `main` push that
touches the client and on `win-v*` release tags.

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
- `win-vX.Y.Z` tag → `X.Y.Z.0` (a real client release; `win-v*` is its own tag namespace, kept off
  the host's `host-v*` and Apple's `v*` to avoid the version-shadow bug).
- `main` push / `workflow_dispatch` → `0.2.<run_number>.0` (rolling, climbs by run number).

## Signing & install

Signing precedence in `pack-msix.ps1`:
1. **`MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD`** Actions secrets — a real (or shared) code-signing
   `.pfx` whose **subject DN must equal `-Publisher`** (default `CN=unom`). Drop these in later to
   move off self-signed with **no manifest change**; if the cert's subject differs, pass a matching
   `-Publisher` (it's stamped into the manifest `Identity`).
2. otherwise an **ephemeral self-signed** cert (subject = `-Publisher`) is generated and its public
   `.cer` is published next to the `.msix`.

To install a self-signed build, trust the cert once, then add the package:

```powershell
Import-Certificate -FilePath .\punktfunk-client-windows_<ver>_x64.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
Add-AppxPackage -Path .\punktfunk-client-windows_<ver>_x64.msix
```

The MSIX declares a dependency on the Windows App SDK 2.x runtime; install
[the App SDK runtime](https://aka.ms/windowsappsdk) if `Add-AppxPackage` reports a missing
`Microsoft.WindowsAppRuntime.2` framework.

## Building locally

On the Windows runner / dev VM (MSVC + Windows SDK present), after a release build:

```powershell
cargo build --release -p punktfunk-client-windows
pwsh -File crates/punktfunk-client-windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -TargetDir C:\t\release -OutDir C:\t\msix
```

Validated end-to-end on the build VM (pack → sign → `Add-AppxPackage` → framework-dependency
resolution). The only step that needs a real display is *launching* the WinUI window (same
on-glass constraint as the rest of the client).
