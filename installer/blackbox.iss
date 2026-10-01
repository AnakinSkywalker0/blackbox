; Inno Setup script for the blackbox Windows installer.
;
; Build:  iscc /DAppVersion=0.3.0 installer\blackbox.iss     (output goes to dist\)
;
; It installs per-user, with no admin rights, into the same folder `bb install`
; uses, then runs `bb install` to add PATH, start recording and start at login.
; Uninstalling runs `bb uninstall` first. Recorded data is kept.

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
; Adds the folder to PATH, registers start at login and starts recording.
Filename: "{app}\bb.exe"; Parameters: "install"; StatusMsg: "Setting up blackbox..."; Flags: runhidden

[UninstallRun]
; Stops the recorder and removes start at login and the PATH entry, before files are deleted.
Filename: "{app}\bb.exe"; Parameters: "uninstall"; RunOnceId: "BlackboxUninstall"; Flags: runhidden

[UninstallDelete]
; Left behind by `bb update`.
Type: files; Name: "{app}\bb.exe.old"

[Code]
// An upgrade replaces bb.exe, which the background recorder holds open. Stop it first.
// `bb install` starts it again at the end of setup.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  if FileExists(ExpandConstant('{app}\bb.exe')) then
    Exec(ExpandConstant('{app}\bb.exe'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;
