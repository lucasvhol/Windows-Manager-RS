; Win Manager RS installer (Inno Setup 6).
; Build:  ISCC.exe /DAppVersion=0.1.0 installer\win-manager-rs.iss
; Expects target\release\win-manager.exe; writes dist\WinManagerRS-<version>-setup.exe.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "Win Manager RS"
#define AppExe "win-manager.exe"

[Setup]
; Never change AppId: Windows uses it to recognise upgrades and the uninstaller.
AppId={{6B1F3A52-8C0D-4E47-9A3B-2F5D7C9E1A64}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Win Manager RS contributors
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=WinManagerRS-{#AppVersion}-setup
SetupIconFile=..\app\assets\app.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion isreadme

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; Not skipped in silent mode, so the app comes back after an in-app update.
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall runasoriginaluser

[UninstallRun]
; Stops the rule monitor and removes the sign-in entry, scheduled tasks and Explorer
; right-click entries the app created.
Filename: "{app}\{#AppExe}"; Parameters: "--uninstall-cleanup"; Flags: runhidden waituntilterminated; RunOnceId: "WinManagerRSCleanup"

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Data := ExpandConstant('{userappdata}\win-manager-rs');
    if DirExists(Data) and not UninstallSilent then
      if MsgBox('Also remove your Win Manager RS settings, backups and Change Journal?' + #13#10 +
                'Keep them if you plan to reinstall: the journal lets you undo past changes.',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(Data, True, True, True);
  end;
end;
