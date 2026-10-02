; Inno Setup script for Indicative.
;
; Build from the repo root after `cargo build --release`:
;   iscc /DAppVersion=0.1.0 installer\indicative.iss
;
; Installs per-user by default (no UAC prompt) into %LOCALAPPDATA%\Programs.
; Pass /ALLUSERS to install for every user under Program Files instead.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\x86_64-pc-windows-gnu\release\indicative.exe"
#endif

#define AppName "Indicative"
#define AppPublisher "ExplodingCB"
#define AppURL "https://github.com/ExplodingCB/indicative"
#define AppExe "indicative.exe"
#define WndClass "IndicativeSpotlight"
#define WM_APP_QUIT "$8004"

[Setup]
AppId={{8F3C2A51-6B7E-4D2A-9C41-2E5B7A9D3F10}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
LicenseFile=..\LICENSE
SetupIconFile=..\assets\indicative.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=..\dist
OutputBaseFilename=indicative-setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in (recommended)"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"

[Registry]
; autostart state marker written by `indicative.exe --autostart`
Root: HKCU; Subkey: "Software\{#AppName}"; Flags: uninsdeletekey dontcreatekey

[Run]
; Start at sign-in uses a per-user Task Scheduler logon task (the exe registers
; it with schtasks), because Explorer can silently skip new HKCU Run entries.
Filename: "{app}\{#AppExe}"; Parameters: "--autostart on"; Flags: runhidden waituntilterminated; Tasks: autostart
Filename: "{app}\{#AppExe}"; Parameters: "--autostart off"; Flags: runhidden waituntilterminated; Tasks: not autostart
; Interactive installs offer to open the panel; silent installs (winget) just
; start it in the background so Win+Space works right away.
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName} now (Win+Space)"; Flags: nowait postinstall skipifsilent
Filename: "{app}\{#AppExe}"; Parameters: "--background"; Flags: nowait skipifnotsilent

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "--autostart off"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveAutostart"

[UninstallDelete]
; app/icon cache and launch history
Type: filesandordirs; Name: "{localappdata}\{#AppName}"

[Code]
// Indicative has no visible window while idle. Ask a running copy (installed
// or not) to exit before files are replaced or removed, and wait for it.
procedure QuitRunning();
var
  W: HWND;
  I: Integer;
begin
  W := FindWindowByClassName('{#WndClass}');
  if W = 0 then exit;
  PostMessage(W, {#WM_APP_QUIT}, 0, 0);
  I := 0;
  while (FindWindowByClassName('{#WndClass}') <> 0) and (I < 50) do
  begin
    Sleep(100);
    I := I + 1;
  end;
  Sleep(200);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitRunning();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  QuitRunning();
  Result := True;
end;

// 0.1.0 and 0.1.1 used an HKCU Run value. Remove it on uninstall only when it
// points at this install.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Value: String;
begin
  if CurUninstallStep = usUninstall then
    if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}', Value) then
      if Pos(Lowercase(ExpandConstant('{app}\{#AppExe}')), Lowercase(Value)) > 0 then
        RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
end;
