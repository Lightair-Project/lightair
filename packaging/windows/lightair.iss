; Lightair's classic Windows installer (Inno Setup 6 or later), built by `cargo xtask dist`:
;   iscc /DVersion=<version> /DSource=<target\dist\Lightair> /DOutput=<target\dist> lightair.iss
; Installs for every user into Program Files (as administrator), and on Windows 11 registers
; Lightair Virtual Camera (lightair_camera.dll: its class in HKLM, where Windows' Frame Server
; looks), unregistered again on uninstall. Or, chosen in its first page or with /CURRENTUSER,
; for this user only, without administrator rights: the camera is then registered by Lightair
; when it's first turned on (Windows asks then). What Lightair keeps for its users (%APPDATA%\Lightair)
; stays when it's uninstalled.

#ifndef Version
  #error Version isn't set (/DVersion=...)
#endif
#ifndef Source
  #error Source isn't set (/DSource=...: the folder cargo xtask dist made)
#endif
#ifndef Output
  #define Output "."
#endif
; /DCompression=none for a quick test build
#ifndef Compression
  #define Compression "lzma2"
#endif

[Setup]
AppId={{6B0E7F2A-3C41-4B8E-9A55-1F0C2D7E8A31}
AppName=Lightair
AppVersion={#Version}
AppVerName=Lightair {#Version}
AppPublisher=Lightair Project
AppPublisherURL=https://github.com/Lightair-Project
DefaultDirName={autopf}\Lightair
DefaultGroupName=Lightair
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1903 (DirectML in the system) and later
MinVersion=10.0.18362
LicenseFile=..\..\LICENSE
SetupIconFile=..\..\branding\final\icon\Lightair.ico
UninstallDisplayIcon={app}\lightair.exe
UninstallDisplayName=Lightair
OutputDir={#Output}
OutputBaseFilename=Lightair-{#Version}-windows-x64-setup
Compression={#Compression}
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#Version}
VersionInfoProductName=Lightair

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Source}\*"; DestDir: "{app}"; Excludes: "lightair_camera.dll"; Flags: ignoreversion recursesubdirs createallsubdirs
; the virtual camera: Windows' Frame Server may have it loaded, so it's replaced or removed on
; restart if it's in use; registered only where Windows can add cameras (Windows 11)
Source: "{#Source}\lightair_camera.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete regserver; Check: CanAddCameras and IsAdminInstallMode
Source: "{#Source}\lightair_camera.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete; Check: not (CanAddCameras and IsAdminInstallMode)

[Icons]
Name: "{autoprograms}\Lightair"; Filename: "{app}\lightair.exe"
Name: "{autodesktop}\Lightair"; Filename: "{app}\lightair.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\lightair.exe"; Description: "{cm:LaunchProgram,Lightair}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
// MFCreateVirtualCamera: Windows 11 (build 22000) and later
function CanAddCameras: Boolean;
var
  V: TWindowsVersion;
begin
  GetWindowsVersionEx(V);
  Result := V.Build >= 22000;
end;
