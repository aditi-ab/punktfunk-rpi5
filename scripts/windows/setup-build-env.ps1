<#
  Persist the punktfunk Windows-host build environment (Machine scope, run once, elevated).

  After this, deploy-host.ps1 needs only the VS toolchain (it loads vcvars64.bat itself).
  These are BUILD-time vars; runtime config lives in C:\ProgramData\punktfunk\host.env.

    powershell -ExecutionPolicy Bypass -File scripts\windows\setup-build-env.ps1
#>
$ErrorActionPreference = 'Stop'

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
if (-not $admin) { throw "Run elevated (Machine-scope env requires Administrator)." }

# NVENC import lib (nvencodeapi.lib); libclang for bindgen; cmake policy floor for audiopus_sys.
$vars = [ordered]@{
  'PUNKTFUNK_NVENC_LIB_DIR'      = 'C:\Users\Public\nvenc'
  'LIBCLANG_PATH'               = 'C:\Program Files\LLVM\bin'
  'CMAKE_POLICY_VERSION_MINIMUM' = '3.5'
  # FFMPEG_DIR is only needed for the `amf-qsv` feature (libavcodec). The RTX box builds
  # `--features nvenc`, which does NOT link FFmpeg, so it is intentionally left unset.
}
foreach ($k in $vars.Keys) {
  [Environment]::SetEnvironmentVariable($k, $vars[$k], 'Machine')
  Write-Host ("set (Machine)  {0} = {1}" -f $k, $vars[$k])
}
Write-Host "Done. Open a fresh shell (or reboot) for these to be inherited by new processes."
