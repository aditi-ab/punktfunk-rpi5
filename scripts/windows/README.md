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

## Rebuild + restart the web console

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```

`bun install && bun run build`, installs the externalized server deps into `.output/server`
(with the `@unom` `.npmrc`), then restarts the `PunktfunkWeb` task and checks `:3000/login`.

## Typical flow after pulling new code

```powershell
git pull
powershell -ExecutionPolicy Bypass -File scripts\windows\deploy-host.ps1
powershell -ExecutionPolicy Bypass -File scripts\windows\build-web.ps1
```
