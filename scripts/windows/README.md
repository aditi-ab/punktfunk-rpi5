# Windows host build/deploy scripts

Helper scripts for the Windows host box (the RTX `.173` lab box, repo at
`C:\Users\Public\punktfunk-native`). Run them from the repo root in an **elevated** PowerShell.

## One-time: persist the build environment

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\setup-build-env.ps1
```

Persists (Machine scope) the vars the host build needs (NVENC itself needs none — its entry
points are runtime-loaded from the driver's `nvEncodeAPI64.dll`):

| var | value | why |
| --- | --- | --- |
| `LIBCLANG_PATH` | `C:\Program Files\LLVM\bin` | bindgen (`libclang.dll`) |
| `CMAKE_POLICY_VERSION_MINIMUM` | `3.5` | `audiopus_sys` / cmake crates |

`FFMPEG_DIR` is **not** set — the `--features nvenc` build the RTX box uses does not link
libavcodec (that is only the `amf-qsv` feature). The VS C++ toolchain is loaded per-build via
`vcvars64.bat` (auto-discovered with `vswhere`).

## Rebuild + redeploy the host service

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1
```

Stops `PunktfunkHost`, backs up the current binary (`punktfunk-host.exe.bak`), builds
`--release -p punktfunk-host --features nvenc` from the current source, then restarts the
service on the new binary — **with automatic rollback** if the build fails or the new binary
won't start. The service is down only for the build duration.

## Web management console

On an **installed** host (the `setup.exe`) the console is set up automatically — no manual steps.
The installer bundles the built (self-contained, no-`node_modules`) `.output` server + a portable
bun and runs `punktfunk-host.exe web setup`, which registers the **`PunktfunkWeb`** scheduled task
(at boot, as SYSTEM, restart-on-failure) running `{app}\web\web-run.cmd` →
`bun …\.output\server\index.mjs` on `:47992`, opens inbound TCP 47992, and writes the login password to
`%ProgramData%\punktfunk\web-password` (ACL'd to Administrators + SYSTEM). The mgmt bearer token it
proxies with is the host's own `%ProgramData%\punktfunk\mgmt-token`. Browse `http://<host-ip>:47992`
and log in with the password the installer shows on its final page. To change it, edit
`web-password` and re-run the task: `schtasks /run /tn PunktfunkWeb`.

### Rebuild + restart the console (dev box)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```

`bun install && bun run build` (Nitro `noExternals` -> a self-contained `.output`, no
`node_modules`/`.npmrc`), then restarts the `PunktfunkWeb` task and checks `:3000/login`. Use
this to iterate on the console against an installed host - `punktfunk-host.exe web setup` (or a
fresh install) is what creates the task in the first place.

## Typical flow after pulling new code

```powershell
git pull
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```
