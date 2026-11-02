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
Source: "{#SourceDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "{#SourceDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Dataman"; Filename: "{app}\dataman.exe"; AppUserModelID: "dev.dataman.Dataman"
Name: "{autodesktop}\Dataman"; Filename: "{app}\dataman.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\dataman.exe"; Description: "{cm:LaunchProgram,Dataman}"; Flags: nowait postinstall skipifsilent
