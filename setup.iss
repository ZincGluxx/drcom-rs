; DrCom 校园网助手（Rust 版） - Inno Setup 安装脚本
;
; 构建流程:
;   1. cd drcom-rs
;   2. cargo build --release --offline
;   3. ISCC.exe setup.iss
;
; 与 C# 版 (setup.iss) 使用不同的 AppId 和安装目录，两者可以共存；
; 但同一个 UDP 61440 端口只能被其中一个占用，同时运行时后启动的会报错。

#define MyAppName "DrCom 校园网助手 (Rust)"
#define MyAppVersion "1.0.0"
#define MyAppPublisher "ZincGluxx"
#define MyAppExeName "drcom-campus.exe"
#define MyAppCliName "drcom-cli.exe"
#define MyAppId "{{5B7E2C41-8A9D-4F3E-B1C6-2D4E7A8F0C93}"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..
OutputBaseFilename=DrComRust_v{#MyAppVersion}_Setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
; 安装包图标。与 exe 内嵌的图标同源：都取自 C# 版的
; DrComCampus\Resources\icon.ico，副本放在 drcom-rs\assets\ 下。
; 快捷方式不写 IconFilename，直接用 exe 内嵌图标。
SetupIconFile=assets\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
VersionInfoDescription={#MyAppName}
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
; Inno 不会从 AppVersion 推导文件版本，缺了这一项时 Setup.exe 的「文件版本」
; 是空的、资源区里是 0.0.0.0 —— 只有产品版本有值，属性页看起来像坏掉的构建。
VersionInfoVersion={#MyAppVersion}

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式(&D)"; GroupDescription: "附加选项："

[Files]
Source: "target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\release\{#MyAppCliName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "drcom.ini.example"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\卸载 {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "立即运行 {#MyAppName}"; Flags: nowait skipifsilent postinstall

[UninstallDelete]
; 只清理装在 {app} 里的旧日志：0.1.0 及更早版本把日志写在程序目录，0.1.1 起
; 改到 %LOCALAPPDATA%\DrComCampusRust（drcom.log 与 preferences.dat）。
; 那个目录**故意不删**：preferences.dat 是 DPAPI 加密的账号密码，卸载程序
; 悄悄删掉用户凭据造成的意外，比留下一个几十 KB 的目录严重得多。
Type: files; Name: "{app}\*.log"

[Code]
// 安装前结束正在运行的本程序，否则文件会被占用而替换失败。
procedure KillRunning;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{cmd}'), '/C taskkill /IM {#MyAppExeName} /F', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{cmd}'), '/C taskkill /IM {#MyAppCliName} /F', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  KillRunning;
  Sleep(400);
end;
