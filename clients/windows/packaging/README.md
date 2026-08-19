# punktfunk Windows client — packaging

The Windows client ships **three ways, packed from one assembled layout** by CI
([`.gitea/workflows/windows-client.yml`](../../../.gitea/workflows/windows-client.yml)) to Gitea's
**generic** package registry (`https://git.unom.io/unom/-/packages`), on every `main` push that
touches the client (canary) and on `vX.Y.Z` release tags (stable) — see
[Release Channels](https://punktfunk.unom.io/docs/channels):

1. **Inno Setup installer** (`punktfunk-client-setup_<arch>.exe`) — the **default download**. A
   per-user, no-UAC install to `%LOCALAPPDATA%\Programs\Punktfunk`. It exists because the MSIX
   install shape breaks the top user-reported flows: the exe lands under the ACL'd
   `C:\Program Files\WindowsApps`, which Steam's *Add a Non-Steam Game* picker can't browse, and
   the alias/`shell:AppsFolder` activation defeats the Steam overlay's injection and Big Picture
   launch — Steam must spawn the exe itself from a normal path. `punktfunk-client.iss` +
   `pack-client-installer.ps1`; it re-creates the manifest's declarative grants per-user
   (`punktfunk://` in HKCU Classes, Start shortcuts, `{app}` on the user PATH for the
   `punktfunk` CLI) and fetches the Windows App Runtime when missing.
2. **Portable zip** (`punktfunk-client-windows_<arch>-portable.zip`) — the same signed file set,
   nothing registered.
3. **Signed MSIX** (`punktfunk-client-windows_<arch>.msix`) — kept for **Microsoft Store**
   compatibility. Everything below the fold documents this path.

`pack-msix.ps1` assembles the layout and packs the MSIX; `pack-client-installer.ps1` then consumes
that same `layout/` for the installer + zip (and signs the four exes individually — the MSIX only
signs its container).

# MSIX packaging

**Two architectures, one x64 runner.** Both `x64` and `arm64` packages are produced off the single
x64 Windows runner — `x86_64-pc-windows-msvc` builds natively, `aarch64-pc-windows-msvc` is
cross-compiled (the x64 MSVC toolset ships the ARM64 cross compiler; since M10 nothing in the
package links FFmpeg, so neither arch needs a per-arch `FFMPEG_DIR` tree staged on the runner —
one less thing the ARM64 leg can be missing). Artifacts are arch-suffixed
(`..._x64.msix` / `..._arm64.msix`, plus a matching `.cer` only in the fallback signing modes 2 and 3
— Azure signing emits none); `pack-msix.ps1 -Arch x64|arm64`
stamps the manifest `ProcessorArchitecture` and names the output. See
[`windows-client.yml`](../../../.gitea/workflows/windows-client.yml) for the cross-build rationale.

## What's in the package

`pack-msix.ps1` assembles a layout from a `cargo build --release` and runs `makeappx` + `signtool`:

| File | Source |
|---|---|
| `punktfunk-client.exe` | the release build (the WinUI shell) |
| `punktfunk-session.exe` | the release build — the Vulkan session client the shell spawns for every stream (sibling resolution, `src/spawn.rs`). Skia links statically; `vulkan-1.dll` is a GPU-driver component, never bundled. ARM64 builds it `--no-default-features` (no Skia console UI) until rust-skia ships aarch64-pc-windows-msvc prebuilts |
| `Microsoft.WindowsAppRuntime.Bootstrap.dll`, `resources.pri` | staged by the client's `build.rs` via `windows-reactor-setup::as_framework_dependent()` |
| `SDL3.dll` | auto-staged by the `sdl3` crate |
| `licenses\*` | the project's MIT/Apache texts + the generated `THIRD-PARTY-NOTICES.txt` (MSIX has no installer EULA page, so attribution ships as files) |
| `Assets\*.png` | checked-in tile/store logos (rasterized from `packaging/flatpak/io.unom.Punktfunk.svg`) |
| `AppxManifest.xml` | the template here, with `{VERSION}`/`{PUBLISHER}` substituted |

**No FFmpeg DLLs.** The client decodes natively since M10 (`pf-vkdecode` / `pf-dxvadec` /
OpenH264+rav1d — punktfunk-planning `design/client-native-decode.md` §6), so nothing here
link-imports `libav*` and the wildcard `avcodec/avformat/avutil/swscale/swresample-*.dll` copy is
gone, along with the FFmpeg LGPL notice that accompanied it — shipping that notice now would claim
a dependency the package doesn't have. The **host** installer is unchanged:
`packaging/windows/pack-host-installer.ps1` still ships those DLLs for its AMF/QSV encode path.

### Why an "unpackaged" WinUI app packages cleanly

`main` calls `windows_reactor::bootstrap()`, which runs `MddBootstrapInitialize2` with
`OnPackageIdentity_NOOP` (`crates/libs/reactor/src/bootstrap.rs`), so under MSIX **package
identity** the App SDK bootstrapper is a no-op and the runtime is resolved from the manifest's
`<PackageDependency>` on `Microsoft.WindowsAppRuntime.2` instead (reactor pins
`WINDOWSAPPSDK_RELEASE_MAJORMINOR = 0x20000` = 2.0). It's a full-trust Win32 app
(`EntryPoint="Windows.FullTrustApplication"` + `runFullTrust`) because it owns raw D3D11, Win32
low-level input hooks, WASAPI and SDL3.

## Versioning

MSIX requires a strictly 4-part numeric version. The workflow computes:
- `vX.Y.Z` tag → `X.Y.Z.0` (THE release; any `-rc`/`+meta` suffix is dropped for MSIX). Published to
  the stable `latest/` alias and attached to the unified Gitea Release.
- `main` push / `workflow_dispatch` → `X.<Y+1>.<run_number>.0` (canary — the minor *after* the latest
  `v*` tag, per `scripts/ci/pf-version.ps1`, climbing by run number; `canary/` alias).

## Signing & install

CI signs every build with **Azure Artifact Signing** (formerly Trusted Signing) — account
`unomsigning`, certificate profile `unom-io`, endpoint `https://neu.codesigning.azure.net/`. That
chain is publicly trusted, so **there is nothing to import**:

```powershell
# install the package for your CPU (and re-run for each upgrade)
Add-AppxPackage -Path .\punktfunk-client-windows_<ver>_x64.msix     # Intel/AMD
Add-AppxPackage -Path .\punktfunk-client-windows_<ver>_arm64.msix   # ARM64 (Snapdragon, etc.)
```

The MSIX declares a dependency on the Windows App SDK 2.x runtime; install
[the App SDK runtime](https://aka.ms/windowsappsdk) if `Add-AppxPackage` reports a missing
`Microsoft.WindowsAppRuntime.2` framework.

### How signing resolves

`pack-msix.ps1` picks a backend in this order:

1. **Azure Artifact Signing** when `AZURE_CODESIGNING_ENDPOINT` / `_ACCOUNT` / `_PROFILE` are all
   set (the workflow sets them; they aren't secret). Credentials come from `AZURE_TENANT_ID` /
   `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` — the `punktfunk-ci-signing` service principal, which
   holds only the *Artifact Signing Certificate Profile Signer* role scoped to the `unom-io` profile.
   Keys are HSM-backed and never leave Azure, so there is no `.pfx` and no `.cer` is emitted.
2. **`MSIX_CERT_PFX_B64` / `MSIX_CERT_PASSWORD`** — the older stable self-signed cert (`CN=unom`,
   public half checked in as [`punktfunk-codesign.cer`](punktfunk-codesign.cer)), kept as a fallback.
3. An **ephemeral** self-signed cert (forks / local builds with no secrets at all).

Modes 2 and 3 still export a `.cer` to import into `Cert:\LocalMachine\TrustedPeople` first. On a
`v*` tag, a build with no real signing backend **fails closed** rather than shipping a throwaway.

Two things about Azure mode that are easy to get wrong:

- **Timestamping is mandatory, not best-effort.** Azure mints a leaf cert per request that expires in
  about three days. An untimestamped signature therefore stops verifying within days of release, so
  the script refuses to retry without one (modes 2 and 3 keep the old best-effort retry).
- **The manifest `Publisher` must equal the signer's subject exactly**, because MSIX package identity
  is Name + Publisher. The default `-Publisher` is the `unom-io` profile's verified subject; after
  signing, the script reads the signature back off the `.msix` and fails the build on any drift.
  Changing it makes a *different* package — existing installs must be uninstalled, not upgraded.

## Building locally

On the Windows runner / dev VM (MSVC + Windows SDK present), after a release build:

```powershell
# x64
cargo build --release -p punktfunk-client-windows --target x86_64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -TargetDir C:\t\x86_64-pc-windows-msvc\release -OutDir C:\t\msix

# arm64 (cross-compiled; no extra environment — the client links no FFmpeg)
cargo build --release -p punktfunk-client-windows --target aarch64-pc-windows-msvc
pwsh -File clients/windows/packaging/pack-msix.ps1 `
  -Version 0.2.0.0 -Arch arm64 -TargetDir C:\t\aarch64-pc-windows-msvc\release -OutDir C:\t\msix
```

Validated end-to-end on the build VM (pack → sign → `Add-AppxPackage` → framework-dependency
resolution). The only step that needs a real display is *launching* the WinUI window (same
on-glass constraint as the rest of the client).
