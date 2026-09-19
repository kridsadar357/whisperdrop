; Build on a Windows release workstation with Inno Setup 6:
;   iscc installer\WhisperDrop.iss
; The executable must first be built into dist\WhisperDrop.exe.

#define AppName "WhisperDrop"
#define AppVersion "0.2.0"
#define AppExeName "WhisperDrop.exe"

[Setup]
AppId={{6D887F75-D99E-47E7-85BC-93FE51EFA2EE}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=WhisperDrop
DefaultDirName={autopf}\WhisperDrop
DefaultGroupName=WhisperDrop
UninstallDisplayIcon={app}\{#AppExeName}
OutputDir=..\dist
OutputBaseFilename=WhisperDrop-Setup-{#AppVersion}-x64
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\src-tauri\icons\icon.ico

[Files]
Source: "..\dist\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\WhisperDrop"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\WhisperDrop"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch WhisperDrop"; Flags: nowait postinstall skipifsilent
