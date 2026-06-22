; punktfunk host installer (Inno Setup 6).
;
; Produces a signed setup.exe that lays the host into Program Files, optionally installs the bundled
; SudoVDA virtual-display driver, and DELEGATES service registration to `punktfunk-host service
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
; The web console launcher (the PunktfunkWeb task action) + its post-install provisioner — committed
; scripts staged next to the .iss by pack-host-installer.ps1 (absolute paths passed in).
#ifndef WebRunCmd
  #define WebRunCmd "..\..\scripts\windows\web-run.cmd"
#endif
#ifndef WebSetup
  #define WebSetup "..\..\scripts\windows\web-setup.ps1"
#endif
; StageDir (the staged SudoVDA payload + nefconc.exe + install-sudovda.ps1) is optional.
#ifdef StageDir
  #define WithDriver
#endif
; GamepadStageDir (the vendored UMDF gamepad drivers + install-gamepad-drivers.ps1) is optional.
#ifdef GamepadStageDir
  #define WithGamepad
#endif
; FfmpegBin (a dir of FFmpeg shared DLLs) is optional — present when the host is built with
; --features amf-qsv (the AMD/Intel AMF/QSV encode backend link-imports the FFmpeg libs).
#ifdef FfmpegBin
  #define WithFfmpeg
#endif
; WebDir (the built web .output tree) + NodeExe (a portable node.exe) are passed together by
; pack-host-installer.ps1 to bundle the management console. Both required → WithWeb.
#ifdef WebDir
  #ifdef NodeExe
    #define WithWeb
  #endif
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
Name: "installdriver"; Description: "Install the SudoVDA virtual display driver (required for native-resolution streaming)"
#endif
#ifdef WithGamepad
Name: "installgamepad"; Description: "Install the virtual gamepad drivers (DualSense / DualShock 4 / Xbox 360 — no ViGEmBus needed)"
#endif
Name: "startservice"; Description: "Start the punktfunk host service now (also starts on every boot)"

[Files]
Source: "{#BinDir}\punktfunk-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#HostEnv}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Readme}"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion
#ifdef WithFfmpeg
; FFmpeg shared DLLs (avcodec/avutil/swscale/...) laid down next to the exe — the AMD/Intel
; (AMF/QSV) encode backend link-imports them, so the exe won't start without them. NVENC/software-
; only builds simply omit this block.
Source: "{#FfmpegBin}\*.dll"; DestDir: "{app}"; Flags: ignoreversion
#endif
#ifdef WithWeb
; The web management console: the built Nitro/Node SSR bundle (.output = server + public assets) →
; {app}\web\.output, a portable Node runtime → {app}\node\node.exe, and the launcher the
; PunktfunkWeb task runs → {app}\web\web-run.cmd. web-setup.ps1 (the provisioner) goes to {tmp} and
; is removed after install.
Source: "{#WebDir}\*"; DestDir: "{app}\web\.output"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#NodeExe}"; DestDir: "{app}\node"; DestName: "node.exe"; Flags: ignoreversion
Source: "{#WebRunCmd}"; DestDir: "{app}\web"; DestName: "web-run.cmd"; Flags: ignoreversion
Source: "{#WebSetup}"; DestDir: "{tmp}"; DestName: "web-setup.ps1"; Flags: deleteafterinstall
#endif
#ifdef WithDriver
; The driver payload + nefconc.exe + install-sudovda.ps1, extracted to {tmp} and removed after install.
Source: "{#StageDir}\*"; DestDir: "{tmp}\sudovda"; Flags: deleteafterinstall recursesubdirs createallsubdirs; Tasks: installdriver
#endif
#ifdef WithGamepad
; The vendored UMDF gamepad drivers + install-gamepad-drivers.ps1, extracted to {tmp}, removed after.
Source: "{#GamepadStageDir}\*"; DestDir: "{tmp}\gamepad"; Flags: deleteafterinstall recursesubdirs createallsubdirs; Tasks: installgamepad
#endif

[Run]
#ifdef WithDriver
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\sudovda\install-sudovda.ps1"" -Dir ""{tmp}\sudovda"""; \
  StatusMsg: "Installing the SudoVDA virtual display driver..."; \
  Flags: runhidden waituntilterminated; Tasks: installdriver
#endif
#ifdef WithGamepad
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\gamepad\install-gamepad-drivers.ps1"" -Dir ""{tmp}\gamepad"""; \
  StatusMsg: "Installing the virtual gamepad drivers..."; \
  Flags: runhidden waituntilterminated; Tasks: installgamepad
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
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\web-setup.ps1"" {code:WebSetupParams}"; \
  StatusMsg: "Setting up the punktfunk web console..."; Flags: runhidden waituntilterminated
#endif

[UninstallRun]
Filename: "{app}\punktfunk-host.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkHostServiceUninstall"
#ifdef WithWeb
; Stop + remove the PunktfunkWeb task and its firewall rule (leaves %ProgramData%\punktfunk config,
; like the host uninstall does).
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Stop-ScheduledTask -TaskName PunktfunkWeb -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName PunktfunkWeb -Confirm:$false -ErrorAction SilentlyContinue; Get-NetFirewallRule -Name 'PunktfunkWeb-TCP-3000' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "PunktfunkWebCleanup"
#endif

[Code]
#ifdef WithWeb
var
  WebPwPage: TInputQueryWizardPage;
  FreshWebInstall: Boolean;   { captured at start — web-setup creates the file mid-run }

function WebPasswordPath: String;
begin
  Result := ExpandConstant('{commonappdata}\punktfunk\web-password');
end;

{ Pre-fill the console password field with a crypto-strong default (Inno has no RNG): a one-shot
  PowerShell writes 12 random bytes as dashed hex; strip the dashes → a 24-char hex password. }
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
    'secure password generated below (it is shown again on the final page) or enter your own — you ' +
    'can change it later in %ProgramData%\punktfunk\web-password.');
  WebPwPage.Add('Console password:', False);   { visible, so the admin can read the generated default }
  DefaultPw := '';
  GenerateRandomWebPassword(DefaultPw);
  WebPwPage.Values[0] := DefaultPw;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  { On upgrade the password already exists — keep it, don't re-prompt. }
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
    log). Only on a fresh install — on upgrade web-setup keeps the existing file. }
  Result := '-AppDir "' + ExpandConstant('{app}') + '"';
  if FreshWebInstall then
    Result := Result + ' -PasswordFile "' + ExpandConstant('{tmp}\webpw.txt') + '"';
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

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    StopHostServiceAndWait;
#ifdef WithWeb
    { Stash the chosen password for web-setup.ps1 (fresh install only); {tmp} is auto-cleaned. }
    if FreshWebInstall then
      SaveStringToFile(ExpandConstant('{tmp}\webpw.txt'), Trim(WebPwPage.Values[0]), False);
#endif
  end;
end;
