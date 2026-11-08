; WinPods installer (Inno Setup 6).
;
; Per-user install into %LOCALAPPDATA%\Programs\WinPods without administrator rights,
; so WinPods can keep updating itself in place (the updater swaps WinPods.exe and
; winpods-cli.exe next to each other). Start with Windows stays the app's own setting
; (Settings > Start with Windows); the uninstaller removes its Run entry if it points
; into the installed folder, and offers to delete settings and the log.
;
; Build: iscc /DAppVersion=0.2.1 /DSourceDir=<folder with the exes and docs> WinPods.iss
; CI passes the packaged release folder (see .github/workflows/build.yml). Locally,
; /DSourceDir=..\target\release /DDocsDir=.. takes a release build and the repository's
; README and LICENSE; the setup lands in target\installer.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; Numeric part only (the exe version resource): 0.3.0 for 0.3.0-rc1.
#ifndef FileVersion
  #define FileVersion AppVersion
#endif
#ifndef SourceDir
  #define SourceDir "..\target\release"
#endif
#ifndef DocsDir
  #define DocsDir SourceDir
#endif
#ifndef OutputDir
  #define OutputDir "..\target\installer"
#endif

; Keep in sync with crates/app/src/install.rs (the registration WinPods refreshes after
; a self-update).
#define AppId "5C2004B3-E8ED-45AF-89C6-CE42A6C1310D"

[Setup]
AppId={{{#AppId}}
AppName=WinPods
AppVersion={#AppVersion}
AppVerName=WinPods {#AppVersion}
AppPublisher=sheepy928
AppPublisherURL=https://github.com/sheepy928/WinPods
AppSupportURL=https://github.com/sheepy928/WinPods/issues
AppUpdatesURL=https://github.com/sheepy928/WinPods/releases
VersionInfoVersion={#FileVersion}
DefaultDirName={autopf}\WinPods
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Setup and the uninstaller wait until WinPods (and its tray icon) has been quit.
AppMutex=WinPods-single-instance
SetupIconFile=..\apps\winpods\assets\winpods.ico
UninstallDisplayIcon={app}\WinPods.exe
UninstallDisplayName=WinPods
OutputDir={#OutputDir}
OutputBaseFilename=WinPods-v{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: desktopicon; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\WinPods.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\winpods-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DocsDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DocsDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
#ifexist AddBackslash(DocsDir) + "THIRD-PARTY-NOTICES.html"
Source: "{#DocsDir}\THIRD-PARTY-NOTICES.html"; DestDir: "{app}"; Flags: ignoreversion
#endif

[Icons]
Name: "{autoprograms}\WinPods"; Filename: "{app}\WinPods.exe"
Name: "{autodesktop}\WinPods"; Filename: "{app}\WinPods.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\WinPods.exe"; Description: "Start WinPods"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Leftovers of an interrupted self-update.
Type: files; Name: "{app}\*.old"
Type: files; Name: "{app}\*.new"

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';

{ The executable a Run command line names: the quoted part, or the whole line. }
function RunTarget(const Data: String): String;
var
  S: String;
  P: Integer;
begin
  S := Trim(Data);
  if (Length(S) > 0) and (S[1] = '"') then
  begin
    Delete(S, 1, 1);
    P := Pos('"', S);
    if P > 0 then
      S := Copy(S, 1, P - 1);
  end;
  Result := S;
end;

procedure RemoveStartWithWindows;
var
  Data: String;
begin
  if RegQueryStringValue(HKCU, RunKey, 'WinPods', Data) and
     (CompareText(RunTarget(Data), ExpandConstant('{app}\WinPods.exe')) = 0) then
    RegDeleteValue(HKCU, RunKey, 'WinPods');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Settings: String;
begin
  if CurUninstallStep <> usPostUninstall then
    exit;
  RemoveStartWithWindows;
  Settings := ExpandConstant('{userappdata}\WinPods');
  if DirExists(Settings) and not UninstallSilent and
     (MsgBox('Also delete your WinPods settings and log?' #13#10#13#10 + Settings,
             mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
    DelTree(Settings, True, True, True);
end;
