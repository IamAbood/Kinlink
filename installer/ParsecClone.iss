; Kinlink installer (Inno Setup 6). Build: sh scripts/build-installer.sh
#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#define ViGEmUrl "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe"
#define ViGEmSha "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a"

[Setup]
AppId={{6D2B6F0E-3C1A-4E7B-9A51-2F7C0B8E4D19}
AppName=Kinlink
AppVersion={#AppVersion}
AppPublisher=Abdulkareem Alsaou
DefaultDirName={autopf}\Kinlink
DisableProgramGroupPage=yes
PrivilegesRequired=admin
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=Kinlink-Setup
SetupIconFile=..\native\assets\kinlink.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
UninstallDisplayIcon={app}\Kinlink.exe
UninstallDisplayName=Kinlink

[Tasks]
Name: startup; Description: "Start with Windows (runs in the tray)"
Name: controllers; Description: "Controller support (downloads the ViGEmBus driver if missing)"
Name: helper; Description: "Control the lock screen and admin prompts (background helper service)"
Name: desktopicon; Description: "Desktop shortcut"; Flags: unchecked

[Files]
Source: "..\native\target\release\pclone.exe"; DestDir: "{app}"; DestName: "Kinlink.exe"; Flags: ignoreversion

[Icons]
; The app makes its own Start menu entry on first run (needed for its notifications).
Name: "{autodesktop}\Kinlink"; Filename: "{app}\Kinlink.exe"; Tasks: desktopicon

[Registry]
; Same value the app's own "Start with Windows" switch uses.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ParsecCloneNative"; ValueData: """{app}\Kinlink.exe"" --minimized"; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{tmp}\vigembus.exe"; Parameters: "/exenoui /qn /norestart"; StatusMsg: "Installing the controller driver (ViGEmBus)..."; Check: ViGEmDownloaded; Flags: waituntilterminated
Filename: "{app}\Kinlink.exe"; Parameters: "--install-service"; StatusMsg: "Installing the lock screen helper..."; Tasks: helper; Flags: runhidden waituntilterminated
Filename: "{app}\Kinlink.exe"; Description: "Start Kinlink"; Flags: postinstall nowait skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{app}\Kinlink.exe"; Parameters: "--uninstall-service"; RunOnceId: "helper"; Flags: runhidden waituntilterminated

[UninstallDelete]
Type: files; Name: "{app}\Kinlink.exe.old"
Type: files; Name: "{userprograms}\Kinlink.lnk"
Type: files; Name: "{userprograms}\Parsec Clone.lnk"

[Code]
var
  DownloadPage: TDownloadWizardPage;

function ViGEmInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ViGEmBus');
end;

function ViGEmDownloaded: Boolean;
begin
  Result := FileExists(ExpandConstant('{tmp}\vigembus.exe'));
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing), SetupMessage(msgPreparingDesc), nil);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpReady) and WizardIsTaskSelected('controllers') and not ViGEmInstalled then begin
    DownloadPage.Clear;
    DownloadPage.Add('{#ViGEmUrl}', 'vigembus.exe', '{#ViGEmSha}');
    DownloadPage.Show;
    try
      try
        DownloadPage.Download;
      except
        if not DownloadPage.AbortedByUser then
          SuppressibleMsgBox('Couldn''t download the controller driver (' + GetExceptionMessage + '). Kinlink will install without it.', mbError, MB_OK, IDOK);
      end;
    finally
      DownloadPage.Hide;
    end;
  end;
end;

// The helper service runs the installed exe: stop it so the file can be replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  // Also covers installs from before the rename (ParsecClone.exe in the same folder).
  if FileExists(ExpandConstant('{app}\Kinlink.exe')) then
    Exec(ExpandConstant('{app}\Kinlink.exe'), '--uninstall-service', '', SW_HIDE, ewWaitUntilTerminated, Code)
  else if FileExists(ExpandConstant('{app}\ParsecClone.exe')) then
    Exec(ExpandConstant('{app}\ParsecClone.exe'), '--uninstall-service', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

// Remove the pre-rename exe once the new one is in place.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    DeleteFile(ExpandConstant('{app}\ParsecClone.exe'));
end;
