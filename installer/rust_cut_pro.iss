; Inno Setup 6 script for Rust Cut Pro.
; Built by scripts/package_windows.ps1, which passes:
;   /DAppVersion=0.1.0  /DStageDir=<staged files>  /DOutputDir=<dist>
; Manual build:  ISCC.exe /DAppVersion=0.1.0 /DStageDir=..\dist\stage /DOutputDir=..\dist installer\rust_cut_pro.iss

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef StageDir
  #define StageDir "..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

#define AppName "Rust Cut Pro"
#define AppExe "rust-cut-pro.exe"
#define ProgId "RustCutPro.Project"

[Setup]
; Never change AppId: upgrades and the uninstaller are keyed on it.
AppId={{2C8EC40F-E4BE-43BB-9672-FD3DECDD4C97}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Rust Cut Pro contributors
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; per-user install by default (no admin prompt); the dialog offers all-users
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir={#OutputDir}
OutputBaseFilename=RustCutPro-v{#AppVersion}-win64-setup
SetupIconFile=..\crates\nle-ui\assets\rust-cut-pro.ico
UninstallDisplayIcon={app}\{#AppExe}
LicenseFile={#StageDir}\LICENSE-MIT
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
CloseApplications=yes
#ifdef SignSetup
; package_windows.ps1 defines the "rcp" sign tool (/Srcp=...) when a certificate
; is configured: setup.exe and the uninstaller are both signed.
SignTool=rcp
SignedUninstaller=yes
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "rcpassoc"; Description: "Open .rcp project files with {#AppName}"; GroupDescription: "File associations:"

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; HKA = HKLM for all-users installs, HKCU for per-user installs
Root: HKA; Subkey: "Software\Classes\.rcp"; ValueType: string; ValueName: ""; ValueData: "{#ProgId}"; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: rcpassoc
Root: HKA; Subkey: "Software\Classes\.rcp\OpenWithProgids"; ValueType: string; ValueName: "{#ProgId}"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty; Tasks: rcpassoc
Root: HKA; Subkey: "Software\Classes\{#ProgId}"; ValueType: string; ValueName: ""; ValueData: "Rust Cut Pro Project"; Flags: uninsdeletekey; Tasks: rcpassoc
Root: HKA; Subkey: "Software\Classes\{#ProgId}\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe},0"; Tasks: rcpassoc
Root: HKA; Subkey: "Software\Classes\{#ProgId}\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""; Tasks: rcpassoc
Root: HKA; Subkey: "Software\Classes\Applications\{#AppExe}"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\{#AppExe}\SupportedTypes"; ValueType: string; ValueName: ".rcp"; ValueData: ""

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

; User data (%LOCALAPPDATA%\RustCutPro: logs, autosaves, downloaded FFmpeg,
; Whisper models) and the API key in Windows Credential Manager are left in
; place on uninstall so a reinstall keeps them. README explains how to remove them.
