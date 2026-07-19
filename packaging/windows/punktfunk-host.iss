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
; The web console launcher (the PunktfunkWeb task action) + its post-install provisioner - committed
; scripts staged next to the .iss by pack-host-installer.ps1 (absolute paths passed in).
#ifndef WebRunCmd
  #define WebRunCmd "..\..\scripts\windows\web-run.cmd"
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
AppName=punktfunk host
AppVersion={#MyAppVersion}
AppPublisher=unom
AppPublisherURL=https://git.unom.io/unom/punktfunk
DefaultDirName={autopf}\punktfunk
DefaultGroupName=punktfunk
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
UninstallDisplayName=punktfunk host {#MyAppVersion}
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
WinVersionTooLowError=punktfunk host requires Windows 11 22H2 (build 22621) or newer.%n%nIts virtual display driver needs the IddCx 1.10 framework, which is not available on older Windows — including all editions of Windows 10 (LTSC too) and Windows 11 21H2.

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
; host (the common Windows setup); unchecked = the secure native-only host (punktfunk clients only).
Name: "gamestream"; Description: "Enable GameStream (Moonlight) compatibility - lets stock Moonlight clients connect (uses legacy plain-HTTP pairing; for trusted LANs)"
; Firewall scope, forwarded as `--allow-public-network` to `service install` / `web setup`. Unchecked
; (default) = accept connections on Private + Domain networks only (the trusted-network profiles
; punktfunk is meant for). Check ONLY for a network you trust that Windows classifies as Public (e.g.
; some headless / no-gateway LAN setups) - it opens the streaming + console ports on Public too.
Name: "allowpublicfw"; Description: "Allow connections on Public networks (only for a trusted network Windows marks as Public)"; Flags: unchecked
Name: "startservice"; Description: "Start the punktfunk host service now (also starts on every boot)"
; The per-user status tray (punktfunk-tray.exe): shows running/stopped/failed at a glance and
; offers open-console / start / stop / restart without a terminal. HKLM Run = every user who signs
; in to this host box gets one (each session keeps exactly one via a Local\ mutex).
Name: "trayicon"; Description: "Show the punktfunk status icon in the notification area at sign-in"

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
#if defined(WithWeb) || defined(WithScripting)
Source: "{#BunExe}"; DestDir: "{app}\bun"; DestName: "bun.exe"; Flags: ignoreversion
#endif
#ifdef WithWeb
; The web management console: the self-contained Nitro SSR bundle (.output = server + public; deps
; bundled in, no node_modules) -> {app}\web\.output, and the launcher the PunktfunkWeb task runs ->
; {app}\web\web-run.cmd. (`punktfunk-host.exe web setup` provisions the console at install time - no
; staged provisioner script.)
Source: "{#WebDir}\*"; DestDir: "{app}\web\.output"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#WebRunCmd}"; DestDir: "{app}\web"; DestName: "web-run.cmd"; Flags: ignoreversion
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

[Registry]
; Auto-start the status tray at sign-in (all users of this host box; uninsdeletevalue removes it
; with the app). Operators who moved --mgmt-bind can append --mgmt-addr/--mgmt-port here.
Root: HKLM64; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
  ValueName: "PunktfunkTray"; ValueData: """{app}\punktfunk-tray.exe"""; Flags: uninsdeletevalue; Tasks: trayicon
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
  StatusMsg: "Registering the punktfunk host service..."; Flags: runhidden waituntilterminated
Filename: "{app}\punktfunk-host.exe"; Parameters: "service start"; WorkingDir: "{app}"; \
  StatusMsg: "Starting the punktfunk host service..."; Flags: runhidden waituntilterminated; Tasks: startservice
#ifdef WithWeb
; Provision the console AFTER the host service is up (so the mgmt token exists): write the ACL'd
; login password, register the PunktfunkWeb scheduled task (boot, SYSTEM, restart-on-failure),
; open TCP 47992, and start it. {code:WebSetupParams} appends -PasswordFile only on a fresh install.
Filename: "{app}\punktfunk-host.exe"; Parameters: "web setup {code:WebSetupParams}{code:PublicFwParam}"; WorkingDir: "{app}"; \
  StatusMsg: "Setting up the punktfunk web console..."; Flags: runhidden waituntilterminated
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
  StatusMsg: "Registering the punktfunk script runner (disabled; opt-in)..."; Flags: runhidden waituntilterminated
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
; Stop + remove the PunktfunkWeb task and its firewall rule (leaves %ProgramData%\punktfunk config,
; like the host uninstall does).
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName PunktfunkWeb -ErrorAction SilentlyContinue; Get-NetTCPConnection -LocalPort 47992,3000 -State Listen -ErrorAction SilentlyContinue | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }; Unregister-ScheduledTask -TaskName PunktfunkWeb -Confirm:$false -ErrorAction SilentlyContinue; Get-NetFirewallRule -DisplayName 'punktfunk web console (*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"""; \
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
{ True if another Moonlight-compatible streaming host is installed - by its SCM service key or its
  Program Files directory. Sunshine and its forks all register a "<Name>Service" and install under
  Program Files, so a registry + directory probe catches them without enumerating processes (which
  pure Pascal can't do cleanly). }
function StreamHostPresent(SvcKey, DirName: String): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\' + SvcKey)
    or DirExists(ExpandConstant('{commonpf}\' + DirName))
    or DirExists(ExpandConstant('{commonpf32}\' + DirName));
end;

{ Runs before any wizard page - the earliest point we can warn. Detect a conflicting host and let
  the user abort (default) or continue. Returning False cancels setup. }
function InitializeSetup(): Boolean;
var
  Found: String;
begin
  Result := True;
  Found := '';
  if StreamHostPresent('SunshineService', 'Sunshine') then Found := Found + '    - Sunshine' + #13#10;
  if StreamHostPresent('ApolloService', 'Apollo') then Found := Found + '    - Apollo' + #13#10;
  if StreamHostPresent('VibeshineService', 'Vibeshine') then Found := Found + '    - Vibeshine' + #13#10;
  if StreamHostPresent('VibepolloService', 'Vibepollo') then Found := Found + '    - Vibepollo' + #13#10;
  if StreamHostPresent('LuminalShineService', 'LuminalShine') then Found := Found + '    - LuminalShine' + #13#10;
  if Found <> '' then
    Result := MsgBox(
      'Another game-streaming host is already installed on this PC:' + #13#10#13#10 + Found + #13#10 +
      'Running Punktfunk alongside Sunshine / Apollo / other Moonlight-compatible hosts is NOT ' +
      'supported. They bind the same GameStream network ports (47984, 47989, 47998-48010) and ' +
      'install a conflicting virtual-display driver, which causes pairing failures, "address ' +
      'already in use" errors and capture glitches.' + #13#10#13#10 +
      'Stop and uninstall the other host before using Punktfunk.' + #13#10#13#10 +
      'Continue with the installation anyway?',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

{ The GameStream task choice, forwarded to `service install` (which writes host.env's
  PUNKTFUNK_HOST_CMD - only if it is unset or still one of the two canonical values, so a
  hand-customized command line survives upgrades). }
function GamestreamParam(Param: String): String;
begin
  if WizardIsTaskSelected('gamestream') then
    Result := '--gamestream=on'
  else
    Result := '--gamestream=off';
end;

{ Firewall scope: the "allowpublicfw" task opens the streaming + console ports on Public networks too
  (default = Private/Domain only). Forwarded to both `service install` and `web setup`. Returns a
  LEADING SPACE so it concatenates after the preceding code-substitution param without a gap.
  (Do NOT write a literal code-constant token in this comment: Inno's brace comments do not nest,
  so its closing brace would end the comment early and break the [Code] parse.) }
function PublicFwParam(Param: String): String;
begin
  if WizardIsTaskSelected('allowpublicfw') then
    Result := ' --allow-public-network'
  else
    Result := '';
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
    'Web console', 'Set the punktfunk web console login password',
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

#ifdef WithWeb
{ Stop a running web console + free its port BEFORE the file copy, so the old server doesn't lock
  .output / web-run.cmd / bun.exe and the new task can bind. Killing the listener owner is
  runtime-agnostic (an early install may have run node on :3000, the current one runs bun on
  :47992 - sweep both). `web setup` repeats this idempotently after the copy. Best-effort; a
  fresh install is a no-op. }
procedure StopWebConsole;
var
  ResultCode: Integer;
begin
  Exec('powershell.exe',
    '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''SilentlyContinue''; ' +
    'Stop-ScheduledTask -TaskName PunktfunkWeb; ' +
    'Get-NetTCPConnection -LocalPort 47992,3000 -State Listen | ForEach-Object { Stop-Process -Id $_.OwningProcess -Force }"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
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
#ifdef WithWeb
    StopWebConsole;   { upgrade-safe: free :3000 + unlock the web files before the copy }
    { Stash the chosen password for `web setup` (fresh install only); the temp copy is auto-cleaned. }
    if FreshWebInstall then
      SaveStringToFile(ExpandConstant('{tmp}\webpw.txt'), Trim(WebPwPage.Values[0]), False);
#endif
  end;
end;
