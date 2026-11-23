; BootForge installer (Inno Setup 6).

#ifndef AppVersion
  #define AppVersion "1.1.0"
#endif
; Cargo's output folder. scripts\package.ps1 passes CARGO_TARGET_DIR when that is set.
#ifndef TargetDir
  #define TargetDir "..\target"
#endif

[Setup]
AppId={{7C2E9A41-5B0D-4F36-8E12-A9D3C64B7F05}
AppName=BootForge
AppVersion={#AppVersion}
AppVerName=BootForge {#AppVersion}
AppPublisher=Zheer Khalid
AppPublisherURL=https://zheer.dev
AppSupportURL=https://github.com/zheerkhalid/bootforge
AppUpdatesURL=https://github.com/zheerkhalid/bootforge/releases
DefaultDirName={autopf}\BootForge
DefaultGroupName=BootForge
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible 
ArchitecturesInstallIn64BitMode=x64compatible 
OutputDir=..\dist
OutputBaseFilename=BootForge-Setup
SetupIconFile={#TargetDir}\bootforge.ico
UninstallDisplayIcon={app}\BootForge.exe
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
LicenseFile=..\LICENSE
VersionInfoVersion={#AppVersion}
VersionInfoCompany=Zheer Khalid (zheer.dev)
VersionInfoDescription=BootForge Setup

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; 

[Files]
Source: "{#TargetDir}\release\BootForge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\NOTICE"; DestDir: "{app}"; DestName: "NOTICE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\BootForge"; Filename: "{app}\BootForge.exe"
Name: "{group}\Uninstall BootForge"; Filename: "{uninstallexe}"
Name: "{autodesktop}\BootForge"; Filename: "{app}\BootForge.exe"; Tasks: desktopicon


[Run]
Filename: "{app}\BootForge.exe"; Description: "{cm:LaunchProgram,BootForge}"; Flags: nowait postinstall skipifsilent shellexec

[UninstallRun]
Filename: "{cmd}"; Parameters: "/C taskkill /IM BootForge.exe /F"; Flags: runhidden; RunOnceId: "KillBootForge"
