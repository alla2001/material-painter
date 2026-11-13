; Inno Setup script for the Windows installer. Build the staged folder first
; (scripts/package.ps1), then:  iscc /DAppVersion=0.1.0 installer\material-painter.iss

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{6E4B7C2A-3F1D-4C8E-9A5B-2D7F1E0C9B34}
AppName=Material Painter
AppVersion={#AppVersion}
AppPublisher=Material Painter
AppPublisherURL=https://github.com/alla2001/material-painter
AppSupportURL=https://github.com/alla2001/material-painter/issues
DefaultDirName={autopf}\Material Painter
DefaultGroupName=Material Painter
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=MaterialPainter-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern
UninstallDisplayIcon={app}\MaterialPainter.exe

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\dist\Material Painter\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Material Painter"; Filename: "{app}\MaterialPainter.exe"
Name: "{group}\Uninstall Material Painter"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Material Painter"; Filename: "{app}\MaterialPainter.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\MaterialPainter.exe"; Description: "{cm:LaunchProgram,Material Painter}"; Flags: nowait postinstall skipifsilent
; An update from inside the app (/RELAUNCH=1) starts the app again, as the user.
Filename: "{app}\MaterialPainter.exe"; Flags: nowait runasoriginaluser; Check: RelaunchAfterUpdate

[Code]
// The app starts this setup as it closes and passes its process id (/WAITPID=1234):
// wait for it to end so its files can be replaced. A minute at most, should it hang.
const
  SYNCHRONIZE = $00100000;

function OpenProcess(DesiredAccess: DWORD; InheritHandle: BOOL; ProcessId: DWORD): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(Handle: THandle; Milliseconds: DWORD): DWORD;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';

function InitializeSetup(): Boolean;
var
  Pid: Integer;
  Process: THandle;
begin
  Pid := StrToIntDef(ExpandConstant('{param:WAITPID|0}'), 0);
  if Pid > 0 then
  begin
    Process := OpenProcess(SYNCHRONIZE, False, Pid);
    if Process <> 0 then
    begin
      WaitForSingleObject(Process, 60000);
      CloseHandle(Process);
    end;
  end;
  Result := True;
end;

function RelaunchAfterUpdate(): Boolean;
begin
  Result := ExpandConstant('{param:RELAUNCH|0}') = '1';
end;
