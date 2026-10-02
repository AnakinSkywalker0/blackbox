; Inno Setup script for the blackbox Windows installer.
;
; Build:  iscc /DAppVersion=0.4.0 installer\blackbox.iss     (output goes to dist\)
;
; It installs per-user, with no admin rights, into the same folder `bb install` uses, then
; runs `bb install --no-start` to add PATH and register start at login. Uninstalling runs
; `bb uninstall` first. Recorded data is kept.
;
; IMPORTANT: a silent install must not leave a NEW background process running. Automated
; installers (winget's validation, `Start-Process -Wait`) wait for everything the installer
; started, so a recorder that lives on makes them wait forever and report "failed to install
; without user input". So:
;   - a fresh silent install does not start the recorder (it starts at the next login),
;   - an interactive install offers a "Start recording now" checkbox,
;   - an upgrade restarts the recorder only if it was running before.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\bb.exe"
#endif

[Setup]
; Never change this GUID: it is how Windows and winget recognise upgrades.
AppId={{6F291038-731F-4998-AADF-C25DCFC0032C}
AppName=blackbox
AppVersion={#AppVersion}
AppVerName=blackbox {#AppVersion}
AppPublisher=AnakinSkywalker0
AppPublisherURL=https://github.com/AnakinSkywalker0/blackbox
AppSupportURL=https://github.com/AnakinSkywalker0/blackbox/issues
AppUpdatesURL=https://github.com/AnakinSkywalker0/blackbox/releases
DefaultDirName={localappdata}\blackbox\bin
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=bb-v{#AppVersion}-setup-x64
LicenseFile=..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=blackbox
UninstallDisplayIcon={app}\bb.exe
; A running recorder is stopped explicitly in PrepareToInstall.
CloseApplications=no

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion

[Run]
; Adds the folder to PATH and registers start at login. Does NOT start the recorder.
Filename: "{app}\bb.exe"; Parameters: "install --no-start"; StatusMsg: "Setting up blackbox..."; Flags: runhidden
; Upgrade only: put the recorder back if it was running before this install stopped it.
Filename: "{app}\bb.exe"; Parameters: "start --quiet"; Check: RecorderWasRunning; StatusMsg: "Restarting the recorder..."; Flags: runhidden
; Interactive installs only (skipped when silent): offer to start recording right away.
Filename: "{app}\bb.exe"; Parameters: "start"; Description: "Start recording now"; Flags: nowait postinstall skipifsilent runhidden

[UninstallRun]
; Stops the recorder and removes start at login and the PATH entry, before files are deleted.
Filename: "{app}\bb.exe"; Parameters: "uninstall"; RunOnceId: "BlackboxUninstall"; Flags: runhidden

[UninstallDelete]
; Left behind by `bb update`.
Type: files; Name: "{app}\bb.exe.old"

[Code]
var
  WasRunning: Boolean;

function RecorderWasRunning: Boolean;
begin
  Result := WasRunning;
end;

// An upgrade replaces bb.exe, which the background recorder holds open. Note whether a
// recorder is running (by process name, so this works for every earlier version), then
// stop it. The [Run] section restarts it afterwards only if it was running.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  WasRunning := False;
  if Exec(ExpandConstant('{sys}\cmd.exe'), '/C tasklist /FI "IMAGENAME eq bb.exe" /NH | find /I "bb.exe"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    WasRunning := (ResultCode = 0);
  if FileExists(ExpandConstant('{app}\bb.exe')) then
    Exec(ExpandConstant('{app}\bb.exe'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;
