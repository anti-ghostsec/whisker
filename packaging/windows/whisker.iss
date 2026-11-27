; Whisker's installer for Windows (Inno Setup). Built by CI:
;   iscc /DVersion=0.41.25 /DSource=dist\whisker-windows-x86_64 packaging\windows\whisker.iss
;
; It installs for the current user only, in their own Programs folder: no
; administrator needed, and Whisker's updater can replace its own program
; there. ffmpeg and ffprobe come with it, so videos and voice messages play
; inside Whisker.

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Source
  #define Source "..\..\dist\whisker-windows-x86_64"
#endif

[Setup]
AppId={{C0BF2F44-1224-4503-B28C-05395792477F}
AppName=Whisker
AppVersion={#Version}
AppPublisher=anti-ghostsec
AppPublisherURL=https://github.com/anti-ghostsec/whisker
DefaultDirName={localappdata}\Programs\Whisker
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#Source}\..
OutputBaseFilename=whisker-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=Whisker
UninstallDisplayIcon={app}\whisker.exe
; Whisker running is closed first, so its files can be replaced.
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Put Whisker on the desktop"; Flags: unchecked

[Files]
Source: "{#Source}\whisker.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\whisker-agentd.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\ffmpeg.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\ffprobe.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\ffmpeg-LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Source}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Whisker"; Filename: "{app}\whisker.exe"
Name: "{userdesktop}\Whisker"; Filename: "{app}\whisker.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\whisker.exe"; Description: "Open Whisker"; Flags: nowait postinstall skipifsilent
