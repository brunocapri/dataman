; Dataman's Windows installer (Inno Setup 6). Built by scripts/windows/package.ps1:
;   ISCC /DVersion=0.3.6 /DArch=x86_64 /DSourceDir=<folder with the exes> /DOutputDir=<dist> dataman.iss
; Per-user by default (no administrator needed), per-machine when chosen.

#ifndef Version
  #error Version is not defined
#endif
#ifndef Arch
  #define Arch "x86_64"
#endif

[Setup]
AppId={{C9AF68F1-847F-455B-9E7F-9AF048C6A989}
AppName=Dataman
AppVersion={#Version}
AppVerName=Dataman {#Version}
AppPublisher=Dataman
AppPublisherURL=https://getdataman.com
DefaultDirName={autopf}\Dataman
DefaultGroupName=Dataman
DisableProgramGroupPage=yes
DisableReadyPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir={#OutputDir}
OutputBaseFilename=Dataman-{#Version}-windows-{#Arch}-setup
SetupIconFile=dataman.ico
UninstallDisplayIcon={app}\dataman.exe
UninstallDisplayName=Dataman
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
MinVersion=10.0.17763
CloseApplications=yes
ChangesAssociations=yes
VersionInfoVersion={#Version}
#if Arch == "aarch64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\dataman.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\dataman-mcp.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\dataman_duckdb.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "{#SourceDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Dataman"; Filename: "{app}\dataman.exe"; AppUserModelID: "dev.dataman.Dataman"
Name: "{autodesktop}\Dataman"; Filename: "{app}\dataman.exe"; Tasks: desktopicon

; "Open with" Dataman for its files (the extensions of the Linux .desktop's MIME types and
; Engine::file_extensions); never made the default program.
[Registry]
Root: HKA; Subkey: "Software\Classes\Dataman.File"; ValueType: string; ValueName: ""; ValueData: "Dataman file"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Dataman.File\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\dataman.exe,0"
Root: HKA; Subkey: "Software\Classes\Dataman.File\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\dataman.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.sql\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.sqlite\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.sqlite3\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.db\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.duckdb\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.ddb\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\.db3\OpenWithProgids"; ValueType: string; ValueName: "Dataman.File"; ValueData: ""; Flags: uninsdeletevalue

[Run]
Filename: "{app}\dataman.exe"; Description: "{cm:LaunchProgram,Dataman}"; Flags: nowait postinstall skipifsilent
