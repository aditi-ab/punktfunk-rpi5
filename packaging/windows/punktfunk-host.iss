; punktfunk host installer (Inno Setup 6).
;
; Produces a signed setup.exe that lays the host into Program Files, optionally installs the bundled
; pf-vdisplay virtual-display driver, and DELEGATES service registration to `punktfunk-host service
; install`. The real, idempotent install logic (SCM registration, firewall rules, default host.env,
; the SYSTEM->interactive-session CreateProcessAsUserW supervisor for secure-desktop capture) lives in
; crates/punktfunk-host/src/service.rs - this script does NOT duplicate it. That SYSTEM service model
; is exactly why MSIX is unusable here and we ship a classic elevated installer instead.
;
; Built by pack-host-installer.ps1, e.g.:
;   ISCC.exe /DMyAppVersion=0.2.123 /DBinDir=C:\t\release /DStageDir=C:\t\out\stage \
;            /DOutputDir=C:\t\out packaging\windows\punktfunk-host.iss
; Omit /DStageDir to build an installer WITHOUT the bundled driver (driver becomes a prerequisite).

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "."
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
; Absolute paths to the two extra payload files, passed by pack-host-installer.ps1 (validated there).
#ifndef HostEnv
  #define HostEnv "..\..\scripts\windows\host.env.example"
#endif
#ifndef Readme
  #define Readme "README.md"
#endif
; Branding assets (wizard side panel + header tile BMPs, setup/app icon), generated + committed by
; branding/gen-branding.ps1 from the canonical brand-mark geometry. Relative to this script's dir:
; works from the repo checkout AND from the staged copy (pack-host-installer.ps1 stages branding\
; next to the staged .iss).
#ifndef BrandingDir
  #define BrandingDir "branding"
#endif
; The plugin/script runner launcher (the action the opt-in PunktfunkScripting task runs) - staged
; next to the .iss by pack-host-installer.ps1 (absolute path passed in).
#ifndef ScriptingRunCmd
  #define ScriptingRunCmd "..\..\scripts\windows\scripting-run.cmd"
#endif
; StageDir (the staged pf-vdisplay payload + nefconc.exe + install-pf-vdisplay.ps1) is optional.
#ifdef StageDir
  #define WithDriver
#endif
; GamepadStageDir (the built-from-source UMDF gamepad drivers + install-gamepad-drivers.ps1) is optional.
#ifdef GamepadStageDir
  #define WithGamepad
#endif
; AudioCableStageDir (the official base VB-CABLE package + install-vbcable.ps1) is optional - present
; when the VB-CABLE package was supplied to the packer. It is the streaming virtual microphone; on a
; headless host (no real audio output) a virtual cable is required for mic + desktop-audio passthrough.
#ifdef AudioCableStageDir
  #define WithAudioCable
#endif
; FfmpegBin (a dir of FFmpeg shared DLLs) is optional - present when the host is built with
; --features amf-qsv (the AMD/Intel AMF/QSV encode backend link-imports the FFmpeg libs).
#ifdef FfmpegBin
  #define WithFfmpeg
#endif
; WebDir (the built web .output tree) + BunExe (a portable bun.exe) are passed together by
; pack-host-installer.ps1 to bundle the management console. Both required -> WithWeb.
#ifdef WebDir
  #ifdef BunExe
    #define WithWeb
  #endif
#endif
; ScriptingBundle (the built runner-cli.js) + BunExe are passed together by pack-host-installer.ps1
; to bundle the plugin/script runner. Both required -> WithScripting.
#ifdef ScriptingBundle
  #ifdef BunExe
    #define WithScripting
  #endif
#endif
; VkLayerDir (the staged pf-vkhdr-layer: pf_vkhdr_layer.dll + .json) is optional - present when the
; HDR Vulkan layer was built. It lets Vulkan games (Doom: The Dark Ages, etc.) enable HDR over the
; virtual display (the ICD won't advertise HDR there; the layer injects the surface formats, self-
; gated on the display's actual HDR state).
#ifdef VkLayerDir
  #define WithVkLayer
#endif

[Setup]
AppId={{7C9E6A52-1F4B-4E8D-A3C7-2B5D8F1E0A93}
AppName=Punktfunk Host
AppVersion={#MyAppVersion}
AppPublisher=unom
AppPublisherURL=https://git.unom.io/unom/punktfunk
DefaultDirName={autopf}\punktfunk
DefaultGroupName=Punktfunk
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
PrivilegesRequired=admin
; HARD floor: Windows 11 22H2 (build 22621). The pf-vdisplay driver is built against IddCx 1.10
; (HDR *2 DDIs + FP16 caps, no runtime downgrade) — on anything older (all of Windows 10 incl.
; LTSC, Windows 11 21H2) the driver package installs but the device fails to start with Code 10
; STATUS_DEVICE_POWER_FAILURE, and the host can't stream. Gate the install instead; the message
; is customized in [Messages] below.
MinVersion=10.0.22621
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
OutputDir={#OutputDir}
OutputBaseFilename=punktfunk-host-setup-{#MyAppVersion}
Compression=lzma2/max
SolidCompression=yes
; Modern branded wizard: Windows-11-style controls that follow the system light/dark theme
; (Inno Setup >= 6.6; CI provisions current 6.x via choco). An older local compiler falls back
; to the plain modern style so a dev pack still builds.
#if VER >= EncodeVer(6,6,0)
WizardStyle=modern dynamic windows11
#else
WizardStyle=modern
#endif
; Brand assets (branding/gen-branding.ps1): the violet lens mark on a dark panel/tile - self-
; contained dark art, so it reads correctly in both the light and dark wizard appearance. The
; wildcard names carry 100..200% DPI variants; Setup picks the closest.
SetupIconFile={#BrandingDir}\punktfunk.ico
WizardImageFile={#BrandingDir}\wizard-image-*.bmp
WizardSmallImageFile={#BrandingDir}\wizard-small-*.bmp
UninstallDisplayName=Punktfunk Host {#MyAppVersion}
; The branded multi-size .ico (installed below). The host exe now embeds the same icon + a
; "Punktfunk Host" FileDescription (build.rs winresource) for Task Manager/Explorer; the file
; copy stays as the uninstall-entry icon.
UninstallDisplayIcon={app}\punktfunk.ico
; {app} goes on the machine PATH (see [Registry] + PathNeedsAdd/RemoveAppFromPath below) so the
; documented one-liners — `punktfunk-host plugins add playnite` — work by name from an elevated
; prompt. Broadcasts WM_SETTINGCHANGE so already-open shells pick it up after a restart.
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Messages]
; Shown when MinVersion rejects the OS — name the actual requirement instead of Inno's generic
; "requires Windows version 10.0.22621" (users on Windows 10 LTSC hit this; see the pf-vdisplay
; IddCx 1.10 note at MinVersion above).
WinVersionTooLowError=Punktfunk Host requires Windows 11 22H2 (build 22621) or newer.%n%nIts virtual display driver needs the IddCx 1.10 framework, which is not available on older Windows — including all editions of Windows 10 (LTSC too) and Windows 11 21H2.

[Tasks]
#ifdef WithDriver
Name: "installdriver"; Description: "Install the pf-vdisplay virtual display driver (required for native-resolution streaming)"
#endif
#ifdef WithGamepad
Name: "installgamepad"; Description: "Install the virtual gamepad drivers (DualSense / DualShock 4 / Xbox 360 - no ViGEmBus needed)"
#endif
#ifdef WithAudioCable
; VB-Audio's bundling grant requires the end user to see VB-CABLE's origin + donationware status
; at install time - keep the vendor, URL, and donationware wording in this visible task text (the
; full notice ships in {app}\licenses\VB-CABLE-NOTICE.txt).
Name: "installaudiocable"; Description: "Install VB-CABLE virtual audio for microphone passthrough (VB-CABLE by VB-Audio, www.vb-cable.com - donationware, all participations welcome)"
#endif
#ifdef WithVkLayer
Name: "installhdrlayer"; Description: "Install the HDR Vulkan layer (lets Vulkan games like Doom use HDR on the virtual display)"
#endif
; Host-config choice, applied via `service install --gamestream=on|off` (writes PUNKTFUNK_HOST_CMD
; in host.env; a hand-customized value is left alone). Checked = the Moonlight-compatible unified
; host; unchecked (DEFAULT) = the secure native-only host (Punktfunk clients only).
;
; OPT-IN, like allowpublicfw below and for the same reason: the host itself WARNs on every start
; that this plane pairs over plain HTTP and its legacy control encryption can reuse GCM nonces
; (security-review #5/#9), so an on-path LAN attacker could MITM pairing or recover input. A
; default-on security downgrade cannot be squared with that warning - least of all on the silent
; path, where the wizard never appears and 1839d756 makes an unattended install take these very
; defaults. Reported by a user who found the warning in their log and had never been shown a
; choice, because they installed through winget.
;
; Turning it on unattended is `/MERGETASKS="gamestream"`; on an UPGRADE this task is inert either
; way (GamestreamParam omits the flag unless FreshHostInstall), so an existing host keeps whatever
; host.env already says - changing it afterwards is `service install --gamestream=on|off`.
Name: "gamestream"; Description: "Enable GameStream (Moonlight) compatibility - lets stock Moonlight clients connect (uses legacy plain-HTTP pairing; for trusted LANs)"; Flags: unchecked
; Firewall scope, forwarded as `--allow-public-network` to `service install` / `web setup`. Unchecked
; (default) = accept connections on Private + Domain networks only (the trusted-network profiles
; punktfunk is meant for). Check ONLY for a network you trust that Windows classifies as Public (e.g.
; some headless / no-gateway LAN setups) - it opens the streaming + console ports on Public too.
Name: "allowpublicfw"; Description: "Allow connections on Public networks (only for a trusted network Windows marks as Public)"; Flags: unchecked
Name: "startservice"; Description: "Start the Punktfunk Host service now (also starts on every boot)"
; The per-user status tray (punktfunk-tray.exe): shows running/stopped/failed at a glance and
; offers open-console / start / stop / restart without a terminal. HKLM Run = every user who signs
; in to this host box gets one (each session keeps exactly one via a Local\ mutex).
Name: "trayicon"; Description: "Show the Punktfunk status icon in the notification area at sign-in"

[Files]
Source: "{#BinDir}\punktfunk-host.exe"; DestDir: "{app}"; Flags: ignoreversion
; The status tray companion (windows-subsystem, embeds its own icons). Installed unconditionally
; (small); only STARTED/registered when the trayicon task is selected.
Source: "{#BinDir}\punktfunk-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#HostEnv}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Readme}"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion
; The branded icon, referenced by UninstallDisplayIcon (Apps & features shows it for the entry).
Source: "{#BrandingDir}\punktfunk.ico"; DestDir: "{app}"; Flags: ignoreversion
#ifdef LicensesDir
; License/attribution payload -> {app}\licenses: the project's MIT/Apache texts, the generated
; THIRD-PARTY-NOTICES (permissive crate attributions), and (on an amf-qsv build) the FFmpeg LGPL
; notice + license text. Staged by pack-host-installer.ps1.
Source: "{#LicensesDir}\*"; DestDir: "{app}\licenses"; Flags: ignoreversion
#endif
#ifdef WithFfmpeg
; FFmpeg shared DLLs (avcodec/avutil/swscale/...) laid down next to the exe - the AMD/Intel
; (AMF/QSV) encode backend link-imports them, so the exe won't start without them. NVENC/software-
; only builds simply omit this block. These are unmodified BtbN *lgpl-shared* builds, linked
; dynamically (replaceable DLLs) - FFmpeg is used under the LGPL v2.1+; see {app}\licenses.
Source: "{#FfmpegBin}\*.dll"; DestDir: "{app}"; Flags: ignoreversion
#endif
; The portable bun runtime -> {app}\bun\bun.exe. Shared by the web console AND the plugin/script
; runner (both run on bun), so stage it once when EITHER is bundled.
; restartreplace is the safety net under StopBunRuntimes: Windows refuses to delete a RUNNING image,
; so if any bun survives the pre-copy stop, DeleteFile fails with code 5 and - with no reboot-time
; MoveFileEx fallback - Inno can only show the user a dead-end Retry/Skip/Cancel box. With it, the
; install completes and the new runtime lands on the next restart instead.
#if defined(WithWeb) || defined(WithScripting)
Source: "{#BunExe}"; DestDir: "{app}\bun"; DestName: "bun.exe"; Flags: ignoreversion restartreplace uninsrestartdelete
#endif
#ifdef WithWeb
; The web management console: the self-contained Nitro SSR bundle (.output = server + public; deps
; bundled in, no node_modules) -> {app}\web\.output. No launcher script anymore: the PunktfunkHost
; service supervises bun directly (`service.rs` "web console child"); `punktfunk-host.exe web setup`
; provisions the password/firewall at install time.
Source: "{#WebDir}\*"; DestDir: "{app}\web\.output"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif
#ifdef WithScripting
; The plugin/script runner: one self-contained bundle (effect + the SDK inlined) -> {app}\scripting\
; runner-cli.js, and the launcher the (opt-in) PunktfunkScripting task runs -> {app}\scripting\
; scripting-run.cmd. Runs on the shared bun above.
Source: "{#ScriptingBundle}"; DestDir: "{app}\scripting"; DestName: "runner-cli.js"; Flags: ignoreversion
Source: "{#ScriptingRunCmd}"; DestDir: "{app}\scripting"; DestName: "scripting-run.cmd"; Flags: ignoreversion
#endif
#ifdef WithDriver
; The driver payload + nefconc.exe + install-pf-vdisplay.ps1, extracted to {tmp} and removed after install.
Source: "{#StageDir}\*"; DestDir: "{tmp}\pfvdisplay"; Flags: deleteafterinstall recursesubdirs createallsubdirs; Tasks: installdriver
#endif
#ifdef WithGamepad
; The built-from-source UMDF gamepad drivers + install-gamepad-drivers.ps1, extracted to {tmp}, removed after.
Source: "{#GamepadStageDir}\*"; DestDir: "{tmp}\gamepad"; Flags: deleteafterinstall recursesubdirs createallsubdirs; Tasks: installgamepad
#endif
#ifdef WithAudioCable
; The official base VB-CABLE package + install-vbcable.ps1, extracted to {tmp}, removed after install.
Source: "{#AudioCableStageDir}\*"; DestDir: "{tmp}\vbcable"; Flags: deleteafterinstall recursesubdirs createallsubdirs; Tasks: installaudiocable
#endif
#ifdef WithVkLayer
; The HDR Vulkan implicit layer (cdylib + its JSON manifest) laid into {app}\vklayer and registered
; below. The manifest's library_path is ".\pf_vkhdr_layer.dll" (relative to the JSON), so the two
; must live in the same directory.
Source: "{#VkLayerDir}\pf_vkhdr_layer.dll"; DestDir: "{app}\vklayer"; Flags: ignoreversion; Tasks: installhdrlayer
Source: "{#VkLayerDir}\pf_vkhdr_layer.json"; DestDir: "{app}\vklayer"; Flags: ignoreversion; Tasks: installhdrlayer
#endif

[InstallDelete]
; The retired web-console launcher: the console runs as a supervised child of the host service now,
; and Inno never deletes a file it merely stopped shipping — without this a pre-supervision
; install's copy would linger in {app}\web forever. Unconditional: harmless when absent.
Type: files; Name: "{app}\web\web-run.cmd"

[Registry]
; Auto-start the status tray at sign-in (all users of this host box; uninsdeletevalue removes it
; with the app). Operators who moved --mgmt-bind can append --mgmt-addr/--mgmt-port here.
Root: HKLM64; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
  ValueName: "PunktfunkTray"; ValueData: """{app}\punktfunk-tray.exe"""; Flags: uninsdeletevalue; Tasks: trayicon
; Toast identity for the tray's notifications ("client connected"). The tray process tags itself
; with this AppUserModelID (win.rs TRAY_AUMID — keep in sync), and this registration is what makes
; Windows 11 attribute its toasts as "Punktfunk" with the brand icon instead of a generic entry —
; the same Classes\AppUserModelId mechanism the Windows App SDK uses for unpackaged apps. No Start
; menu shortcut needed. Installed unconditionally (like the tray exe itself): the keys are inert
; without the tray running.
Root: HKLM64; Subkey: "SOFTWARE\Classes\AppUserModelId\unom.punktfunk.tray"; ValueType: string; \
  ValueName: "DisplayName"; ValueData: "Punktfunk"; Flags: uninsdeletekey
Root: HKLM64; Subkey: "SOFTWARE\Classes\AppUserModelId\unom.punktfunk.tray"; ValueType: string; \
  ValueName: "IconUri"; ValueData: "{app}\punktfunk.ico"
; Put {app} on the MACHINE PATH so `punktfunk-host plugins add …` / `punktfunk-host service …` are
; runnable by name. Appended to the existing value ({olddata}) and guarded by PathNeedsAdd so a
; repair/upgrade never appends a duplicate. Deliberately NOT `uninsdeletevalue` — that would delete
; the whole Path value; the uninstaller surgically removes just our entry (RemoveAppFromPath).
; expandsz preserves the %SystemRoot%-style entries other software puts here.
Root: HKLM64; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
  ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
  Check: PathNeedsAdd(ExpandConstant('{app}'))
#ifdef WithVkLayer
; Register the HDR Vulkan implicit layer system-wide. The 64-bit Vulkan loader reads
; HKLM64\SOFTWARE\Khronos\Vulkan\ImplicitLayers; the value NAME is the manifest path and the DWORD
; DATA is 0 (= enabled). uninsdeletevalue removes just this value on uninstall. The layer is inert
; unless the target display has HDR enabled, and honors DISABLE_PF_VKHDR=1 as a global off-switch.
Root: HKLM64; Subkey: "SOFTWARE\Khronos\Vulkan\ImplicitLayers"; ValueType: dword; ValueName: "{app}\vklayer\pf_vkhdr_layer.json"; ValueData: 0; Flags: uninsdeletevalue; Tasks: installhdrlayer
#endif

[Run]
#ifdef WithDriver
Filename: "{app}\punktfunk-host.exe"; Parameters: "driver install --dir ""{tmp}\pfvdisplay"""; WorkingDir: "{app}"; \
  StatusMsg: "Installing the pf-vdisplay virtual display driver..."; \
  Flags: runhidden waituntilterminated; Tasks: installdriver
#endif
#ifdef WithGamepad
Filename: "{app}\punktfunk-host.exe"; Parameters: "driver install --gamepad --dir ""{tmp}\gamepad"""; WorkingDir: "{app}"; \
  StatusMsg: "Installing the virtual gamepad drivers..."; \
  Flags: runhidden waituntilterminated; Tasks: installgamepad
#endif
#ifdef WithAudioCable
; Silently install the bundled VB-CABLE (the streaming virtual microphone). Best-effort: install-vbcable.ps1
; always exits 0 (a missing cable just disables mic passthrough; the host falls back + retries), so a
; cable hiccup never fails the whole install.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\vbcable\install-vbcable.ps1"" -Dir ""{tmp}\vbcable"""; \
  StatusMsg: "Installing VB-CABLE virtual audio (microphone passthrough)..."; \
  Flags: runhidden waituntilterminated; Tasks: installaudiocable
#endif
; Register (or re-point, on upgrade - idempotent) the SYSTEM service from its FINAL {app} location:
; service install records current_exe() as the SCM binPath, so it must run from {app}, not {tmp}.
; --gamestream=on|off carries the wizard's GameStream task choice into host.env's PUNKTFUNK_HOST_CMD.
Filename: "{app}\punktfunk-host.exe"; Parameters: "service install {code:GamestreamParam}{code:PublicFwParam}"; WorkingDir: "{app}"; \
  StatusMsg: "Registering the Punktfunk Host service..."; Flags: runhidden waituntilterminated
Filename: "{app}\punktfunk-host.exe"; Parameters: "service start"; WorkingDir: "{app}"; \
  StatusMsg: "Starting the Punktfunk Host service..."; Flags: runhidden waituntilterminated; Tasks: startservice
#ifdef WithWeb
; Provision the console: write the ACL'd login password, open TCP 47992, and delete the legacy
; PunktfunkWeb scheduled task (the console runs as a supervised child of the host service now — the
; service just started above will bring it up once the host has written the mgmt token + identity
; cert; nothing here starts or registers anything). {code:WebSetupParams} appends -PasswordFile only
; on a fresh install. Order note: StopBunRuntimes DISABLED any legacy task before the copy, so it
; cannot respawn between the service start above and this delete.
Filename: "{app}\punktfunk-host.exe"; Parameters: "web setup {code:WebSetupParams}{code:PublicFwParam}"; WorkingDir: "{app}"; \
  StatusMsg: "Setting up the Punktfunk web console..."; Flags: runhidden waituntilterminated
#endif
#ifdef WithScripting
; Register the plugin/script runner's scheduled task (boot, restart-on-failure) but leave it
; DISABLED - the runner is OPT-IN (inert until you add scripts/plugins). Enable it when ready:
;   punktfunk-host plugins enable
; Principal: NT AUTHORITY\LocalService, NOT SYSTEM - plugins are operator-installed code; a plugin
; defect must cost a throwaway service account, not the box's highest privilege. `plugins enable`
; grants LocalService read on the two secrets the runner needs (plugin-token, cert.pem) and
; converges tasks an older installer registered as SYSTEM.
; Best-effort (-ErrorAction SilentlyContinue): a task hiccup never fails the whole install. No braces
; in the command, so no Inno {{ }} escaping needed.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""$a=New-ScheduledTaskAction -Execute '{app}\scripting\scripting-run.cmd'; $t=New-ScheduledTaskTrigger -AtStartup; $p=New-ScheduledTaskPrincipal -UserId 'LocalService' -LogonType ServiceAccount; $s=New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; Register-ScheduledTask -TaskName PunktfunkScripting -Action $a -Trigger $t -Principal $p -Settings $s -Force -ErrorAction SilentlyContinue | Out-Null; Disable-ScheduledTask -TaskName PunktfunkScripting -ErrorAction SilentlyContinue | Out-Null"""; \
  StatusMsg: "Registering the Punktfunk script runner (disabled; opt-in)..."; Flags: runhidden waituntilterminated
#endif
#if defined(WithWeb) || defined(WithScripting)
; Put back what StopBunRuntimes disabled to unlock bun.exe. Deliberately the LAST [Run] entry that
; touches the tasks: it has to follow `web setup`'s re-register AND the scripting entry above, whose
; unconditional Disable-ScheduledTask is correct for a fresh install but would otherwise silently
; switch an operator's plugin runner off on every upgrade. Skipped entirely when neither task was
; enabled beforehand (a fresh install), so it can't enable anything the user never asked for.
Filename: "powershell.exe"; Parameters: "{code:RestoreTasksParams}"; \
  StatusMsg: "Restoring the console + script runner tasks..."; \
  Flags: runhidden waituntilterminated; Check: NeedsTaskRestore
#endif
; Launch the status tray as the SIGNED-IN user (not the elevated install user) right away, so the
; icon appears without waiting for the next sign-in.
Filename: "{app}\punktfunk-tray.exe"; Flags: runasoriginaluser nowait skipifsilent; Tasks: trayicon

[UninstallRun]
; Quit the tray FIRST - it is this exe being deleted, so it must not be running. --quit closes the
; current session's instance (an elevated caller may message a medium-IL window; UIPI only blocks
; low->high); the taskkill then reaps instances in OTHER signed-in sessions. [UninstallRun] runs
; before file deletion, so a raced survivor only means a delete-on-reboot leftover, nothing worse.
; (runasoriginaluser is not valid in [UninstallRun] - both entries run elevated, which is fine.)
Filename: "{app}\punktfunk-tray.exe"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkTrayQuit"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM punktfunk-tray.exe"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkTrayKill"
Filename: "{app}\punktfunk-host.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkHostServiceUninstall"
; Remove the punktfunk drivers we installed (pf-vdisplay devnode + driver package, then the gamepad
; driver packages). AFTER service uninstall so the host no longer holds the devices. Unconditional
; (not #ifdef'd on this build's bundled payload - an upgrade may have dropped a payload the original
; install laid down); `driver uninstall` is best-effort and no-ops when nothing is installed.
; VB-CABLE is deliberately NOT removed: it is a third-party shared component the user may use
; elsewhere - see licenses\VB-CABLE-NOTICE.txt for its own uninstall.
Filename: "{app}\punktfunk-host.exe"; Parameters: "driver uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkVdisplayDriverUninstall"
Filename: "{app}\punktfunk-host.exe"; Parameters: "driver uninstall --gamepad"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkGamepadDriverUninstall"
#ifdef WithWeb
; Remove the console's firewall rule + any LEGACY PunktfunkWeb task and stray listener (the
; service-supervised console itself died with `service uninstall` above, via its kill-on-close job;
; this sweep only covers pre-supervision leftovers). Leaves %ProgramData%\punktfunk config, like the
; host uninstall does.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName PunktfunkWeb -ErrorAction SilentlyContinue; Get-NetTCPConnection -LocalPort 47992,3000 -State Listen -ErrorAction SilentlyContinue | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }; Unregister-ScheduledTask -TaskName PunktfunkWeb -Confirm:$false -ErrorAction SilentlyContinue; Get-NetFirewallRule -DisplayName 'Punktfunk web console (*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkWebCleanup"
#endif
#ifdef WithScripting
; Stop + remove the PunktfunkScripting task (leaves %ProgramData%\punktfunk config + the operator's
; scripts/plugins, like the rest of the uninstall does). Unconditional cleanup of the task name.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName PunktfunkScripting -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName PunktfunkScripting -Confirm:$false -ErrorAction SilentlyContinue"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkScriptingCleanup"
#endif

[Code]
{ Captured in InitializeSetup - BEFORE [Run] calls `service install`, which creates host.env. True on
  a first-ever install, False on an upgrade. Gates the install-time-only CONFIG choices (see
  GamestreamParam): a silent run has no wizard on screen, so every task checkbox falls back to its
  script default. Re-applying those defaults on an upgrade would overwrite a setting the user chose
  on an earlier run, with nothing shown. `winget upgrade` runs us exactly that way. }
var
  FreshHostInstall: Boolean;

function HostEnvPath: String;
begin
  Result := ExpandConstant('{commonappdata}\punktfunk\host.env');
end;

{ True if another Moonlight-compatible streaming host is not merely present but will actually RUN.
  Sunshine and its forks register a "<Name>Service"; `Start` is that service's start type
  (REG_DWORD): 0 boot, 1 system, 2 automatic, 3 manual, 4 disabled. Only 0-2 come up on their own,
  and only a host that comes up can take the GameStream ports or load a second virtual-display
  driver - which is the entire content of the warning below.

  DELIBERATELY NARROWER than it was. The old probe also counted the service key existing at ANY
  start type, plus a bare Program Files\<Name> directory - so a disabled service, or a leftover folder
  from an uninstall, read as a live conflict. Combined with the silent-install default of IDNO that
  aborted setup, and a field report followed within hours of 0.20.0: `winget install
  unom.PunktfunkHost` failed with exit code 1 (0x8A150006) on a box whose Sunshine was not running.
  Nothing about a dormant install can clash, and the tray reached the same conclusion in this same
  release (3e782852 dropped its always-on warning over a merely-INSTALLED Sunshine as a false
  alarm) - the two surfaces now agree.

  A host running as a plain user process rather than a service is not detected here, by choice:
  that cannot be seen from pure Pascal, it is a runtime condition rather than an install-time one,
  and the host already reports it where it belongs (the `punktfunk::detect` startup warning, the
  `detect-conflicts` subcommand, and /api/v1/local/summary). }
function StreamHostEnabled(SvcKey: String): Boolean;
var
  StartType: Cardinal;
begin
  Result := RegQueryDWordValue(HKLM, 'SYSTEM\CurrentControlSet\Services\' + SvcKey, 'Start', StartType)
    and (StartType <= 2);
end;

{ Runs before any wizard page - the earliest point we can warn. Detect a conflicting host and let
  the user abort (default) or continue. Returning False cancels setup. }
function InitializeSetup(): Boolean;
var
  Found: String;
begin
  Result := True;
  { Record the fresh-vs-upgrade verdict while host.env still reflects the PREVIOUS run. }
  FreshHostInstall := not FileExists(HostEnvPath);
  Found := '';
  if StreamHostEnabled('SunshineService') then Found := Found + '    - Sunshine' + #13#10;
  if StreamHostEnabled('ApolloService') then Found := Found + '    - Apollo' + #13#10;
  if StreamHostEnabled('VibeshineService') then Found := Found + '    - Vibeshine' + #13#10;
  if StreamHostEnabled('VibepolloService') then Found := Found + '    - Vibepollo' + #13#10;
  if StreamHostEnabled('LuminalShineService') then Found := Found + '    - LuminalShine' + #13#10;
  if Found <> '' then
    { SuppressibleMsgBox, NOT MsgBox: a plain MsgBox ignores /SUPPRESSMSGBOXES and displays even
      under /VERYSILENT - i.e. an unattended install (winget) would block on a modal dialog with no
      wizard on screen and nobody to click it. Suppressed, this returns Default = IDNO, so a silent
      install onto a box that already runs Sunshine/Apollo ABORTS (Setup exits non-zero) instead of
      proceeding into the unsupported dual-host state the message describes. }
    Result := SuppressibleMsgBox(
      { NB: keep #13#10 off the START of a line - ISPP reads a leading '#' as a preprocessor
        directive and aborts the compile with "Unknown preprocessor directive". }
      'Another game-streaming host is installed on this PC and set to start automatically:' + #13#10#13#10 + Found + #13#10 +
      'Running Punktfunk alongside Sunshine / Apollo / other Moonlight-compatible hosts is NOT ' +
      'supported. They bind the same GameStream network ports (47984, 47989, 47998-48010) and ' +
      'install a conflicting virtual-display driver, which causes pairing failures, "address ' +
      'already in use" errors and capture glitches.' + #13#10#13#10 +
      'Stop and disable its service (or uninstall it) before using Punktfunk. A host that is ' +
      'installed but disabled does not clash and is not reported here.' + #13#10#13#10 +
      'Continue with the installation anyway?',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES;
end;

{ The GameStream task choice, forwarded to `service install` (which writes host.env's
  PUNKTFUNK_HOST_CMD - only if it is unset or still one of the two canonical values, so a
  hand-customized command line survives upgrades).

  FRESH INSTALL ONLY. On an upgrade the flag is omitted entirely, which `service install` reads as
  "keep host.env as-is" (windows/service.rs: "None = flag absent, keep host.env as-is"). Passing an
  explicit on/off would rewrite PUNKTFUNK_HOST_CMD whenever it still holds either canonical value -
  so a user who chose GameStream ON, then upgraded with the task unchecked (which is what EVERY
  silent run does, since there is no wizard to carry the old choice forward), would have it turned
  OFF with nothing on screen. Only a hand-edited command line survived that. }
function GamestreamParam(Param: String): String;
begin
  if not FreshHostInstall then
    Result := ''
  else if WizardIsTaskSelected('gamestream') then
    Result := '--gamestream=on'
  else
    Result := '--gamestream=off';
end;

{ Firewall scope: the "allowpublicfw" task opens the streaming + console ports on Public networks too
  (default = Private/Domain only). Forwarded to both `service install` and `web setup`. Returns a
  LEADING SPACE so it concatenates after the preceding code-substitution param without a gap.
  (Do NOT write a literal code-constant token in this comment: Inno's brace comments do not nest,
  so its closing brace would end the comment early and break the [Code] parse.)

  FRESH INSTALL ONLY, for the same reason as GamestreamParam: on an upgrade the flag is omitted and
  both `service install` and `web setup` resolve the scope from the marker the previous install
  recorded. Re-applying the task default would re-scope the firewall on every upgrade - and since
  this task is default-UNCHECKED, a silent upgrade would silently REVOKE a Public opt-in the user
  made once. The explicit =on/=off form is used so a fresh install still states its choice. }
function PublicFwParam(Param: String): String;
begin
  if not FreshHostInstall then
    Result := ''
  else if WizardIsTaskSelected('allowpublicfw') then
    Result := ' --allow-public-network=on'
  else
    Result := ' --allow-public-network=off';
end;

#ifdef WithWeb
var
  WebPwPage: TInputQueryWizardPage;
  FreshWebInstall: Boolean;   { captured at start - web-setup creates the file mid-run }

function WebPasswordPath: String;
begin
  Result := ExpandConstant('{commonappdata}\punktfunk\web-password');
end;

{ Pre-fill the console password field with a crypto-strong default (Inno has no RNG): a one-shot
  PowerShell writes 12 random bytes as dashed hex; strip the dashes -> a 24-char hex password. }
procedure GenerateRandomWebPassword(var Pw: String);
var
  ResultCode: Integer;
  TmpOut: String;
  Lines: TArrayOfString;
begin
  Pw := '';
  TmpOut := ExpandConstant('{tmp}\webpwgen.txt');
  if Exec('powershell.exe',
      '-NoProfile -ExecutionPolicy Bypass -Command "' +
      '$b=New-Object byte[] 12;' +
      '([System.Security.Cryptography.RandomNumberGenerator]::Create()).GetBytes($b);' +
      '[IO.File]::WriteAllText(' + '''' + TmpOut + '''' + ',[System.BitConverter]::ToString($b))"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    if (ResultCode = 0) and LoadStringsFromFile(TmpOut, Lines) and (GetArrayLength(Lines) > 0) then
    begin
      Pw := Trim(Lines[0]);
      StringChangeEx(Pw, '-', '', True);
    end;
    DeleteFile(TmpOut);
  end;
end;

procedure InitializeWizard;
var
  DefaultPw: String;
begin
  FreshWebInstall := not FileExists(WebPasswordPath);
  WebPwPage := CreateInputQueryPage(wpSelectTasks,
    'Web console', 'Set the Punktfunk web console login password',
    'The management console is served on https://this-computer:47992 and is login-gated. Keep the ' +
    'secure password generated below (it is shown again on the final page) or enter your own - you ' +
    'can change it later in %ProgramData%\punktfunk\web-password.');
  WebPwPage.Add('Console password:', False);   { visible, so the admin can read the generated default }
  DefaultPw := '';
  GenerateRandomWebPassword(DefaultPw);
  WebPwPage.Values[0] := DefaultPw;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  { On upgrade the password already exists - keep it, don't re-prompt. }
  Result := (PageID = WebPwPage.ID) and (not FreshWebInstall);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = WebPwPage.ID) and (Trim(WebPwPage.Values[0]) = '') then
  begin
    MsgBox('Please enter a web console password (it cannot be empty).', mbError, MB_OK);
    Result := False;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and FreshWebInstall then
    WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 +
      'Web console:  https://<this-PC-IP>:47992' + #13#10 +
      'Login password:  ' + Trim(WebPwPage.Values[0]);
end;

function WebSetupParams(Param: String): String;
begin
  { Pass the password to `punktfunk-host.exe web setup` via a temp file, not the cmdline (which
    lands in the install log). Only on a fresh install - on upgrade web setup keeps the existing
    file. }
  Result := '--app-dir "' + ExpandConstant('{app}') + '"';
  if FreshWebInstall then
    Result := Result + ' --password-file "' + ExpandConstant('{tmp}\webpw.txt') + '"';
end;
#endif

{ On upgrade a running tray locks punktfunk-tray.exe - kill every session's instance so the copy
  can overwrite it (the [Run] entry / next sign-in relaunches the new build). Best-effort; a fresh
  install is a no-op. }
procedure StopTrays;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM punktfunk-tray.exe', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

{ On upgrade the running service locks punktfunk-host.exe (and the supervisor would respawn it from
  the OLD binary), so stop it and WAIT for STOPPED before files are copied. Best-effort; a fresh
  install is a no-op (the service doesn't exist yet). }
procedure StopHostServiceAndWait;
var
  ResultCode: Integer;
begin
  Exec('powershell.exe',
    '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''SilentlyContinue''; ' +
    '$s=Get-Service -Name ''PunktfunkHost''; ' +
    'if($s -and $s.Status -ne ''Stopped''){Stop-Service -Name ''PunktfunkHost'' -Force; ' +
    'try{$s.WaitForStatus(''Stopped'',[TimeSpan]::FromSeconds(30))}catch{}}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

#if defined(WithWeb) || defined(WithScripting)
{ Each bun task's enabled state as it was BEFORE StopBunRuntimes disabled it, so the [Run] restore
  entry can put it back. PunktfunkScripting is OPT-IN, which makes this load-bearing rather than
  tidy: re-enabling it unconditionally would switch the plugin runner on for everyone, and leaving
  it disabled would switch it off for everyone who had it on. }
var
  WebTaskWasEnabled, ScriptingTaskWasEnabled: Boolean;

{ Escape a value for embedding in a single-quoted PowerShell literal ('' is PS's escaped quote).
  The install dir is user-chosen, so it can legitimately contain an apostrophe. }
function PsLiteral(S: String): String;
begin
  Result := S;
  StringChangeEx(Result, '''', '''''', True);
end;

{ Is the task registered AND not Disabled? Answered through the exit code, so there is no temp file
  to write, read back, and clean up. A missing task reports False. }
function TaskEnabled(TaskName: String): Boolean;
var
  ResultCode: Integer;
begin
  Result := False;
  if Exec('powershell.exe',
    '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$t=Get-ScheduledTask -TaskName ''' + PsLiteral(TaskName) + ''' -ErrorAction SilentlyContinue; ' +
    'if($t -and $t.State -ne ''Disabled''){exit 1}; exit 0"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := ResultCode = 1;
end;

{ Free the bundled bun.exe (and the console's own files) BEFORE the copy. Windows will not delete a
  running image, so a surviving bun means "DeleteFile failed; code 5" on bun\bun.exe - the modal a
  user hit updating to 0.22.1.
  The CONSOLE's bun is a child of the host service now (its kill-on-close job), so for it
  StopHostServiceAndWait (which precedes this) is the real stop. This routine remains for everything
  the service does NOT own: the scripting runner (a LocalService Task Scheduler task that listens on
  NO port, so a port sweep cannot see it), the LEGACY PunktfunkWeb task on the one upgrade that
  crosses the supervision migration, and strays from even older installs. That is why it stops tasks
  by name and processes by image path, not just by socket.
  DISABLE before stopping: both tasks carry aggressive restart-on-failure (and the legacy web task a
  logon trigger), so a force-kill on its own invites a respawn into the middle of a copy that takes
  well over a minute at lzma2/max. Then WAIT for the processes to actually go - Stop-ScheduledTask
  returns when termination is merely requested, and Stop-Process is TerminateProcess, also
  asynchronous.
  Best-effort throughout; a fresh install is a no-op. }
procedure StopBunRuntimes;
var
  ResultCode: Integer;
begin
  WebTaskWasEnabled := TaskEnabled('PunktfunkWeb');
  ScriptingTaskWasEnabled := TaskEnabled('PunktfunkScripting');
  Exec('powershell.exe',
    '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''SilentlyContinue''; ' +
    '$app=''' + PsLiteral(ExpandConstant('{app}')) + '''.ToLower(); ' +
    'foreach($t in ''PunktfunkWeb'',''PunktfunkScripting''){ Disable-ScheduledTask -TaskName $t; Stop-ScheduledTask -TaskName $t }; ' +
    { Scoped to OUR bun by image path - a blanket kill would take out an unrelated bun (a dev box
      runs its own). The port half stays runtime-agnostic: a pre-bun install ran node on :3000. }
    'for($i=0; $i -lt 40; $i++){ ' +
      '$b=@(Get-Process -Name bun | Where-Object { $_.Path -and $_.Path.ToLower().StartsWith($app) }); ' +
      '$l=@(Get-NetTCPConnection -LocalPort 47992,3000 -State Listen); ' +
      'if($b.Count -eq 0 -and $l.Count -eq 0){ break }; ' +
      '$b | ForEach-Object { Stop-Process -Id $_.Id -Force }; ' +
      '$l | ForEach-Object { Stop-Process -Id $_.OwningProcess -Force }; ' +
      'Start-Sleep -Milliseconds 250 }"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

{ The [Run] restore, built here so it names only the tasks that were actually enabled before the
  copy. Runs LAST, after the scripting entry has re-registered-then-disabled PunktfunkScripting -
  that entry is right for a fresh install and wrong for an upgrade, and this is what puts an
  operator's enabled runner back.
  The PunktfunkWeb half is the CANCEL-path safety net only: on a completed install `web setup` has
  DELETED the legacy task (the console runs under the host service now), so Enable-ScheduledTask
  hits nothing and no-ops under SilentlyContinue. If the user cancels mid-install, though,
  DeinitializeSetup runs this same restore and puts the old (task-owned) world back intact. }
function RestoreTasksParams(Param: String): String;
begin
  Result := '-NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference=''SilentlyContinue''; ';
  if WebTaskWasEnabled then
    Result := Result + 'Enable-ScheduledTask -TaskName PunktfunkWeb | Out-Null; ';
  if ScriptingTaskWasEnabled then
    Result := Result +
      'Enable-ScheduledTask -TaskName PunktfunkScripting | Out-Null; ' +
      'Start-ScheduledTask -TaskName PunktfunkScripting | Out-Null; ';
  Result := Result + '"';
end;

function NeedsTaskRestore: Boolean;
begin
  Result := WebTaskWasEnabled or ScriptingTaskWasEnabled;
end;
#endif

const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

{ Is the install dir missing from the machine PATH? Guards the [Registry] append so a repair or
  upgrade can't add a second copy. Compared case-insensitively and semicolon-delimited so a path
  that merely CONTAINS ours as a substring (...\punktfunk-old) doesn't count as a match.
  NOTE: never write a braced Inno constant inside a Pascal comment - these comments do NOT nest,
  so its closing brace ends the comment early and the rest of the line is parsed as code. }
function PathNeedsAdd(Param: String): Boolean;
var
  OrigPath: String;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', OrigPath) then
  begin
    Result := True;   { no Path value at all - the append creates it }
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

{ Remove exactly our install-dir entry from the machine PATH on uninstall, leaving every other
  entry (and their order) intact. Rebuilds the value entry-by-entry rather than doing a substring
  delete, so a partial match can never corrupt a neighbouring path. }
procedure RemoveAppFromPath;
var
  OrigPath, NewPath, Entry: String;
  Target: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', OrigPath) then
    exit;
  Target := Uppercase(ExpandConstant('{app}'));
  NewPath := '';
  { Walk the semicolon-delimited list, copying through everything that isn't ours. }
  OrigPath := OrigPath + ';';
  repeat
    P := Pos(';', OrigPath);
    Entry := Trim(Copy(OrigPath, 1, P - 1));
    OrigPath := Copy(OrigPath, P + 1, Length(OrigPath));
    if (Entry <> '') and (Uppercase(Entry) <> Target) then
    begin
      if NewPath <> '' then NewPath := NewPath + ';';
      NewPath := NewPath + Entry;
    end;
  until OrigPath = '';
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', NewPath);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveAppFromPath;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    StopHostServiceAndWait;
    StopTrays;   { upgrade-safe: unlock punktfunk-tray.exe before the copy }
#if defined(WithWeb) || defined(WithScripting)
    StopBunRuntimes;   { upgrade-safe: unlock the bundled bun.exe + free :47992 before the copy }
#endif
#ifdef WithWeb
    { Stash the chosen password for `web setup` (fresh install only); the temp copy is auto-cleaned. }
    if FreshWebInstall then
      SaveStringToFile(ExpandConstant('{tmp}\webpw.txt'), Trim(WebPwPage.Values[0]), False);
#endif
  end;
end;

#if defined(WithWeb) || defined(WithScripting)
{ The safety net under StopBunRuntimes' Disable-ScheduledTask. Inno calls this even when the user
  cancels or an install fails, which is the case that matters: the [Run] restore would never have run,
  and a task left DISABLED does not come back at the next boot the way a merely-stopped one does - an
  aborted update would take the console down for good. Re-runs the same restore the [Run] entry
  builds, so in the normal flow it is a no-op (enabling an enabled task does nothing).
  Also called when Setup exits before ssInstall, where both flags are still False and this does
  nothing at all. }
procedure DeinitializeSetup;
var
  ResultCode: Integer;
begin
  if NeedsTaskRestore then
    Exec('powershell.exe', RestoreTasksParams(''), '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
#endif
