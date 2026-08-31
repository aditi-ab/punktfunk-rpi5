; punktfunk Windows CLIENT installer (Inno Setup 6) — the default download.
;
; A classic per-user setup.exe, NOT because MSIX failed technically (the app is full-trust Win32
; either way) but because the MSIX install SHAPE breaks the most-reported use case: the exe lands
; under the ACL'd C:\Program Files\WindowsApps, which Steam's "Add a Non-Steam Game" picker cannot
; browse and whose activation path defeats the overlay's GameOverlayRenderer64.dll injection —
; Steam has to spawn the process itself from a normal path for the overlay (and a Big Picture
; launch) to work. This installs to {userpf}\Punktfunk: user-writable-visible, no UAC, and a
; stable path Steam can target. The MSIX is kept for Microsoft Store compatibility
; (clients/windows/packaging/pack-msix.ps1 — both are packed from the same layout every build).
;
; Built by pack-client-installer.ps1, e.g.:
;   ISCC.exe /DMyAppVersion=0.2.137.0 /DArch=x64 /DLayoutDir=C:\t\installer\portable \
;            /DBrandingDir=C:\t\installer\branding /DOutputDir=C:\t\installer punktfunk-client.iss
;
; What the MSIX manifest granted declaratively is re-created here per-user (all HKCU, so no
; elevation and uninstall leaves nothing behind):
;   punktfunk:// protocol        -> HKCU\Software\Classes\punktfunk (deeplink.rs positional parse)
;   Start entries                -> {userprograms} shortcuts (Punktfunk + Punktfunk Console)
;   punktfunk.exe CLI alias      -> {app} appended to the HKCU PATH (Playnite importer shells to it)
;   punktfunk-client.exe alias   -> unnecessary: deeplink.rs targets current_exe() when unpackaged
;   Microsoft.WindowsAppRuntime.2 PackageDependency
;                                -> download + run the runtime installer when missing ([Code])

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef LayoutDir
  #define LayoutDir "."
#endif
#ifndef BrandingDir
  #define BrandingDir "..\..\..\packaging\windows\branding"
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
; The unpackaged app resolves an INSTALLED Windows App SDK runtime via the bootstrap DLL
; (windows-reactor pins WINDOWSAPPSDK_RELEASE_MAJORMINOR = 0x20000; the MSIX manifest's
; PackageDependency floor is 2.2 — keep the two in sync with packaging/AppxManifest.xml).
#define AppRuntimeUrl "https://aka.ms/windowsappsdk/2.2/latest/windowsappruntimeinstall-" + Arch + ".exe"

[Setup]
AppId={{52464E61-68A1-4621-B6B3-5B8BBB823D1A}
AppName=Punktfunk
AppVersion={#MyAppVersion}
AppPublisher=unom
AppPublisherURL=https://git.unom.io/unom/punktfunk
; Per-user, no UAC: {userpf} = %LOCALAPPDATA%\Programs. A browsable, stable path is the point —
; see the header (Steam overlay / Big Picture).
DefaultDirName={userpf}\Punktfunk
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
; Same floor as the MSIX manifest's TargetDeviceFamily MinVersion (10.0.17763).
MinVersion=10.0.17763
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
#endif
OutputDir={#OutputDir}
OutputBaseFilename=punktfunk-client-setup-{#MyAppVersion}_{#Arch}
Compression=lzma2/max
SolidCompression=yes
; Modern branded wizard, same version gate as the host installer (punktfunk-host.iss).
#if VER >= EncodeVer(6,6,0)
WizardStyle=modern dynamic windows11
#else
WizardStyle=modern
#endif
SetupIconFile={#BrandingDir}\punktfunk.ico
WizardImageFile={#BrandingDir}\wizard-image-*.bmp
WizardSmallImageFile={#BrandingDir}\wizard-small-*.bmp
UninstallDisplayName=Punktfunk {#MyAppVersion}
UninstallDisplayIcon={app}\punktfunk-client.exe
; {app} goes on the USER PATH (see [Registry] + PathNeedsAdd/RemoveAppFromPath below) so the
; documented `punktfunk hosts list` / `punktfunk launch` one-liners work by name — same contract
; the MSIX's punktfunk.exe app-execution alias provided. Broadcasts WM_SETTINGCHANGE.
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a Desktop shortcut"; Flags: unchecked

[Files]
; The staged MSIX layout, minus the package-only bits (AppxManifest.xml, the tile Assets — the
; exes embed their own icons via build.rs winresource). pack-client-installer.ps1 signs the four
; exes individually before ISCC runs; the .msix signs only its container, so this cannot be
; skipped by "the MSIX build already signed them".
Source: "{#LayoutDir}\punktfunk-client.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\punktfunk-session.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\punktfunk-console.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\punktfunk.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\Microsoft.WindowsAppRuntime.Bootstrap.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\SDL3.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#LayoutDir}\resources.pri"; DestDir: "{app}"; Flags: ignoreversion
; The Lucide icon font — app/lucide.rs loads ms-appx:///Assets/lucide.ttf, which resolves to
; the install directory when unpackaged. Without it every shell icon is a private-use box.
Source: "{#LayoutDir}\Assets\lucide.ttf"; DestDir: "{app}\Assets"; Flags: ignoreversion
; MIT/Apache + the client-scoped THIRD-PARTY-NOTICES — same payload the MSIX carries.
Source: "{#LayoutDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
; Flat Start-menu entries, mirroring the MSIX's two Application tiles.
Name: "{userprograms}\Punktfunk"; Filename: "{app}\punktfunk-client.exe"
Name: "{userprograms}\Punktfunk Console"; Filename: "{app}\punktfunk-console.exe"; \
  Comment: "Controller-driven couch interface for TVs and HTPCs"
Name: "{userdesktop}\Punktfunk"; Filename: "{app}\punktfunk-client.exe"; Tasks: desktopicon

[Registry]
; The punktfunk:// scheme (design/client-deep-links.md §4.2) — the registry twin of the MSIX
; manifest's windows.protocol extension. Protocol activation delivers the URI as "%1" on the
; command line, so this lands in the same positional URL parse in main() that the packaged
; activation does. HKCU + uninsdeletekey: nothing survives uninstall.
Root: HKCU; Subkey: "Software\Classes\punktfunk"; ValueType: string; \
  ValueData: "URL:Punktfunk stream link"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\punktfunk"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\punktfunk\DefaultIcon"; ValueType: string; \
  ValueData: "{app}\punktfunk-client.exe,0"
Root: HKCU; Subkey: "Software\Classes\punktfunk\shell\open\command"; ValueType: string; \
  ValueData: """{app}\punktfunk-client.exe"" ""%1"""
; Put {app} on the USER PATH so `punktfunk` (the headless CLI) is runnable by name. Appended to
; {olddata} and guarded by PathNeedsAdd so a repair/upgrade never appends a duplicate. NOT
; uninsdeletevalue — that would delete the whole Path value; the uninstaller surgically removes
; just our entry (RemoveAppFromPath). expandsz preserves %VAR%-style entries other software put here.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; Check: PathNeedsAdd(ExpandConstant('{app}'))

[Code]
const
  EnvKey = 'Environment';   { the HKCU per-user environment key }

{ Is the install dir missing from the user PATH? Guards the [Registry] append so a repair or
  upgrade can't add a second copy. Semicolon-delimited, case-insensitive — a path that merely
  CONTAINS ours as a substring doesn't count as a match. (Same helper as punktfunk-host.iss,
  retargeted from the HKLM machine key to HKCU.) }
function PathNeedsAdd(Param: String): Boolean;
var
  OrigPath: String;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath) then
  begin
    Result := True;   { no Path value at all - the append creates it }
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

{ Remove exactly our install-dir entry from the user PATH on uninstall, leaving every other entry
  (and their order) intact. Entry-by-entry rebuild, never a substring delete. }
procedure RemoveAppFromPath;
var
  OrigPath, NewPath, Entry: String;
  Target: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath) then
    exit;
  Target := Uppercase(ExpandConstant('{app}'));
  NewPath := '';
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
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', NewPath);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveAppFromPath;
end;

{ The Windows App SDK runtime the bootstrap DLL resolves at launch (the unpackaged twin of the
  MSIX's PackageDependency). Probe per-user via Get-AppxPackage; when missing, fetch Microsoft's
  runtime installer and run it quietly — it registers Store-signed framework packages, which
  needs no elevation. Every failure path is NON-FATAL and ends in the same message the docs
  carry, because the app itself reports the missing runtime on first launch too. }
function AppRuntimeMissing(): Boolean;
var
  ResultCode: Integer;
begin
  { exit 0 = found, 1 = missing; a powershell failure (rc <> 0/1) counts as missing - the
    download below is idempotent and the runtime installer no-ops when it is present. }
  if not Exec('powershell.exe',
      '-NoProfile -ExecutionPolicy Bypass -Command "if (Get-AppxPackage -Name Microsoft.WindowsAppRuntime.2*) { exit 0 } else { exit 1 }"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    Result := True;
    exit;
  end;
  Result := ResultCode <> 0;
end;

procedure EnsureAppRuntime;
var
  ResultCode: Integer;
  Installer: String;
begin
  if not AppRuntimeMissing() then
    exit;
  Installer := 'windowsappruntimeinstall.exe';
  try
    DownloadTemporaryFile('{#AppRuntimeUrl}', Installer, '', nil);
    if not Exec(ExpandConstant('{tmp}\' + Installer), '--quiet', '',
        SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then
      RaiseException('runtime installer exit code ' + IntToStr(ResultCode));
  except
    SuppressibleMsgBox(
      'The Windows App Runtime 2.x could not be installed automatically.' + #13#10 + #13#10 +
      'Punktfunk needs it to start. Install it from ' + #13#10 +
      'https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads' + #13#10 +
      'and then launch Punktfunk normally.',
      mbInformation, MB_OK, IDOK);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
begin
  { On upgrade a running shell/stream locks the exes; kill them best-effort so the copy succeeds.
    taskkill matches the image NAME, so "punktfunk.exe" hits only the CLI, not the host service. }
  if CurStep = ssInstall then
    Exec(ExpandConstant('{sys}\taskkill.exe'),
      '/F /IM punktfunk-client.exe /IM punktfunk-session.exe /IM punktfunk-console.exe /IM punktfunk.exe',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  { ssPostInstall, NOT a wizard-page hook: silent installs (winget-style /VERYSILENT) show no
    pages, and skipping the runtime there would ship an app that cannot start. This step runs on
    every install mode, and SuppressibleMsgBox keeps the failure path unattended-safe. }
  if CurStep = ssPostInstall then
    EnsureAppRuntime;
end;
