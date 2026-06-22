# Windows host build/deploy scripts

Helper scripts for the Windows host box (the RTX `.173` lab box, repo at
`C:\Users\Public\punktfunk-native`). Run them from the repo root in an **elevated** PowerShell.

## One-time: persist the build environment

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\setup-build-env.ps1
```

Persists (Machine scope) the three vars the NVENC build needs:

| var | value | why |
| --- | --- | --- |
| `PUNKTFUNK_NVENC_LIB_DIR` | `C:\Users\Public\nvenc` | NVENC import lib (`nvencodeapi.lib`) |
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
bun and runs `scripts\windows\web-setup.ps1`, which registers the **`PunktfunkWeb`** scheduled task
(at boot, as SYSTEM, restart-on-failure) running `{app}\web\web-run.cmd` →
`bun …\.output\server\index.mjs` on `:3000`, opens inbound TCP 3000, and writes the login password to
`%ProgramData%\punktfunk\web-password` (ACL'd to Administrators + SYSTEM). The mgmt bearer token it
proxies with is the host's own `%ProgramData%\punktfunk\mgmt-token`. Browse `http://<host-ip>:3000`
and log in with the password the installer shows on its final page. To change it, edit
`web-password` and re-run the task: `schtasks /run /tn PunktfunkWeb`.

### Rebuild + restart the console (dev box)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```

`bun install && bun run build`, installs the externalized server deps into `.output/server`
(with the `@unom` `.npmrc`), then restarts the `PunktfunkWeb` task and checks `:3000/login`. Use
this to iterate on the console against an installed host — `web-setup.ps1` (or a fresh install) is
what creates the task in the first place.

## Typical flow after pulling new code

```powershell
git pull
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```
