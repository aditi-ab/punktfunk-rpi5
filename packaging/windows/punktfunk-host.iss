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
; The web console launcher (the PunktfunkWeb task action) + its post-install provisioner - committed
; scripts staged next to the .iss by pack-host-installer.ps1 (absolute paths passed in).
#ifndef WebRunCmd
  #define WebRunCmd "..\..\scripts\windows\web-run.cmd"
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
MinVersion=10.0
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
OutputDir={#OutputDir}
OutputBaseFilename=punktfunk-host-setup-{#MyAppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=punktfunk host {#MyAppVersion}
UninstallDisplayIcon={app}\punktfunk-host.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
#ifdef WithDriver
Name: "installdriver"; Description: "Install the pf-vdisplay virtual display driver (required for native-resolution streaming)"
#endif
#ifdef WithGamepad
Name: "installgamepad"; Description: "Install the virtual gamepad drivers (DualSense / DualShock 4 / Xbox 360 - no ViGEmBus needed)"
#endif
#ifdef WithAudioCable
Name: "installaudiocable"; Description: "Install VB-CABLE virtual audio (microphone passthrough - VB-Audio donationware, www.vb-cable.com)"
#endif
#ifdef WithVkLayer
Name: "installhdrlayer"; Description: "Install the HDR Vulkan layer (lets Vulkan games like Doom use HDR on the virtual display)"
#endif
Name: "startservice"; Description: "Start the punktfunk host service now (also starts on every boot)"

[Files]
Source: "{#BinDir}\punktfunk-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#HostEnv}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Readme}"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion
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
#ifdef WithWeb
; The web management console: the self-contained Nitro SSR bundle (.output = server + public; deps
; bundled in, no node_modules) -> {app}\web\.output, a portable bun runtime -> {app}\bun\bun.exe, and
; the launcher the PunktfunkWeb task runs -> {app}\web\web-run.cmd. (`punktfunk-host.exe web setup`
; provisions the console at install time - no staged provisioner script.)
Source: "{#WebDir}\*"; DestDir: "{app}\web\.output"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#BunExe}"; DestDir: "{app}\bun"; DestName: "bun.exe"; Flags: ignoreversion
Source: "{#WebRunCmd}"; DestDir: "{app}\web"; DestName: "web-run.cmd"; Flags: ignoreversion
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
Filename: "{app}\punktfunk-host.exe"; Parameters: "service install"; WorkingDir: "{app}"; \
  StatusMsg: "Registering the punktfunk host service..."; Flags: runhidden waituntilterminated
Filename: "{app}\punktfunk-host.exe"; Parameters: "service start"; WorkingDir: "{app}"; \
  StatusMsg: "Starting the punktfunk host service..."; Flags: runhidden waituntilterminated; Tasks: startservice
#ifdef WithWeb
; Provision the console AFTER the host service is up (so the mgmt token exists): write the ACL'd
; login password, register the PunktfunkWeb scheduled task (boot, SYSTEM, restart-on-failure),
; open TCP 3000, and start it. {code:WebSetupParams} appends -PasswordFile only on a fresh install.
Filename: "{app}\punktfunk-host.exe"; Parameters: "web setup {code:WebSetupParams}"; WorkingDir: "{app}"; \
  StatusMsg: "Setting up the punktfunk web console..."; Flags: runhidden waituntilterminated
#endif

[UninstallRun]
Filename: "{app}\punktfunk-host.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkHostServiceUninstall"
#ifdef WithWeb
; Stop + remove the PunktfunkWeb task and its firewall rule (leaves %ProgramData%\punktfunk config,
; like the host uninstall does).
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName PunktfunkWeb -ErrorAction SilentlyContinue; Get-NetTCPConnection -LocalPort 3000 -State Listen -ErrorAction SilentlyContinue | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }; Unregister-ScheduledTask -TaskName PunktfunkWeb -Confirm:$false -ErrorAction SilentlyContinue; Get-NetFirewallRule -Name 'PunktfunkWeb-TCP-3000' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkWebCleanup"
#endif

[Code]
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
    'The management console is served on http://this-computer:3000 and is login-gated. Keep the ' +
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
      'Web console:  http://<this-PC-IP>:3000' + #13#10 +
      'Login password:  ' + Trim(WebPwPage.Values[0]);
end;

function WebSetupParams(Param: String): String;
begin
  { Pass the password to web-setup.ps1 via a temp file, not the cmdline (which lands in the install
    log). Only on a fresh install - on upgrade web-setup keeps the existing file. }
  Result := '--app-dir "' + ExpandConstant('{app}') + '"';
  if FreshWebInstall then
    Result := Result + ' --password-file "' + ExpandConstant('{tmp}\webpw.txt') + '"';
end;
#endif

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
{ Stop a running web console + free :3000 BEFORE the file copy, so the old server doesn't lock
  .output / web-run.cmd / bun.exe and the new task can bind. Killing the :3000 listener owner is
  runtime-agnostic (an early install may have run node, the current one runs bun). web-setup.ps1
  repeats this idempotently after the copy. Best-effort; a fresh install is a no-op. }
procedure StopWebConsole;
var
  ResultCode: Integer;
begin
  Exec('powershell.exe',
    '-NoProfile -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''SilentlyContinue''; ' +
    'Stop-ScheduledTask -TaskName PunktfunkWeb; ' +
    'Get-NetTCPConnection -LocalPort 3000 -State Listen | ForEach-Object { Stop-Process -Id $_.OwningProcess -Force }"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
#endif

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    StopHostServiceAndWait;
#ifdef WithWeb
    StopWebConsole;   { upgrade-safe: free :3000 + unlock the web files before the copy }
    { Stash the chosen password for web-setup.ps1 (fresh install only); the temp copy is auto-cleaned. }
    if FreshWebInstall then
      SaveStringToFile(ExpandConstant('{tmp}\webpw.txt'), Trim(WebPwPage.Values[0]), False);
#endif
  end;
end;
