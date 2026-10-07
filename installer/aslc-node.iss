; ASLC Node - Windows installer (Win11 x64)
; Bundles: release exe + WinUSB driver package (self-signed dev catalog) + cert trust import.
; After install: plug the phone in, PnP auto-binds WinUSB, the node talks AOA. No Zadig, no adb.
;
; Build: 1) cargo build --release
;        2) powershell -File tools\make-driver-package.ps1
;        3) ISCC installer\aslc-node.iss   (Inno Setup 6)

#define MyAppName "ASLC Node"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "Simple Audio Stream"

[Setup]
AppId={{B6E1D4B2-9F43-4B7A-8A2D-5C31A7E90D11}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\ASLC Node
DisableProgramGroupPage=yes
DisableWelcomePage=no
OutputDir=output
OutputBaseFilename=ASLC-Node-Setup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
MinVersion=10.0
UninstallDisplayIcon={app}\aslc_app.exe
;SetupIconFile=
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"

[Files]
Source: "..\target\release\aslc_app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\aslc_node.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\platform\winusb\aslc_aoa.inf"; DestDir: "{app}\driver"; Flags: ignoreversion
Source: "..\platform\winusb\aslc_aoa.cat"; DestDir: "{app}\driver"; Flags: ignoreversion
Source: "cert\aslc-node-dev.cer"; DestDir: "{tmp}"; Flags: deleteafterinstall
Source: "uninstall-driver.ps1"; DestDir: "{tmp}"; Flags: deleteafterinstall
Source: "clean-driver.ps1"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Icons]
Name: "{autoprograms}\ASLC Node"; Filename: "{app}\aslc_app.exe"
Name: "{autodesktop}\ASLC Node"; Filename: "{app}\aslc_app.exe"; Tasks: desktopicon
Name: "{autoprograms}\ASLC Node - Device Status (CLI)"; Filename: "{sys}\cmd.exe"; Parameters: "/k ""{app}\aslc_node.exe"" probe"

[Run]
; 1) Trust the catalog signing cert (root + trusted people) - the Zadig trick, done properly in the installer.
Filename: "{sys}\certutil.exe"; Parameters: "-addstore -f Root ""{tmp}\aslc-node-dev.cer"""; Flags: runhidden; StatusMsg: "Trusting ASLC driver certificate..."
Filename: "{sys}\certutil.exe"; Parameters: "-addstore -f TrustedPeople ""{tmp}\aslc-node-dev.cer"""; Flags: runhidden; StatusMsg: "Trusting ASLC driver certificate..."
; 2) Remove any previous ASLC AOA package (a stale/unsigned catalog can outrank the new one).
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\clean-driver.ps1"""; Flags: runhidden; StatusMsg: "Removing previous ASLC driver package..."
; 3) Stage + install the driver package (also rebinds a currently connected phone).
Filename: "{sys}\pnputil.exe"; Parameters: "/add-driver ""{app}\driver\aslc_aoa.inf"" /install"; Flags: runhidden; StatusMsg: "Installing ASLC USB driver..."
Filename: "{sys}\pnputil.exe"; Parameters: "/scan-devices"; Flags: runhidden; StatusMsg: "Rescanning USB devices..."
; 4) Offer to launch the control window.
Filename: "{app}\aslc_app.exe"; Description: "Launch ASLC Node"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Remove driver package (by original file name) and the trust certs, so nothing is left behind.
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\uninstall-driver.ps1"""; Flags: runhidden; RunOnceId: "DelAslcDriver"
