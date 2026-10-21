#ifndef AppVersion
  #error AppVersion must be supplied from Cargo metadata
#endif
#ifndef StageDir
  #error StageDir must contain the verified release allowlist
#endif

[Setup]
AppId={{5E0A49D9-2132-488E-B1ED-7B5CF144AE44}
AppName=微win
AppVersion={#AppVersion}
AppPublisher=Elnyxn
AppPublisherURL=https://github.com/Elnyxn/weiwin
AppSupportURL=https://github.com/Elnyxn/weiwin/issues
AppUpdatesURL=https://github.com/Elnyxn/weiwin/releases/latest
DefaultDirName={localappdata}\Programs\微win
DefaultGroupName=微win
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
OutputBaseFilename=weiwin-{#AppVersion}-windows-x64-setup
SetupIconFile=..\assets\app-icon.ico
UninstallDisplayIcon={app}\微win.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
AlwaysRestart=no
Uninstallable=yes
UsePreviousAppDir=yes

[Languages]
#if FileExists(AddBackslash(CompilerPath) + "Languages\ChineseSimplified.isl")
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
#endif
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "快捷方式"; Flags: unchecked

[Files]
Source: "{#StageDir}\微win.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\wechat-winh.exe"; DestDir: "{app}"; Flags: ignoreversion
; Versioned bridge can remain mapped by Windows after cooperative Disable.
; Never overwrite it during same-version reinstall or force-stop its host.
Source: "{#StageDir}\wechat_winh_bridge-{#AppVersion}.dll"; DestDir: "{app}"; Flags: onlyifdoesntexist uninsrestartdelete
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\Start.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\Stop.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\OCTICONS-LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
; Only the new GUI implements the current cooperative-shutdown contract.
Source: "{#StageDir}\微win.exe"; DestName: "weiwin-shutdown.exe"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\微win"; Filename: "{app}\微win.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\微win"; Filename: "{app}\微win.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\微win.exe"; Description: "打开微win"; Flags: nowait postinstall skipifsilent shellexec runasoriginaluser

[Code]
var
  ShutdownComplete: Boolean;

function CooperativeShutdown(const Application: String; const WorkingDirectory: String): Boolean;
var
  ExitCode: Integer;
begin
  Result := ShellExec('open', Application, '--quit', WorkingDirectory, SW_HIDE, ewWaitUntilTerminated, ExitCode);
  if Result then
    Result := ExitCode = 0;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if ShutdownComplete then
    Exit;
  ExtractTemporaryFile('weiwin-shutdown.exe');
  ExtractTemporaryFile('wechat-winh.exe');
  if not CooperativeShutdown(ExpandConstant('{tmp}\weiwin-shutdown.exe'), ExpandConstant('{tmp}')) then
  begin
    Result := '微win 尚未完成退出与恢复，请先退出微win后重试。';
    Exit;
  end;
  ShutdownComplete := True;
end;

function InitializeUninstall: Boolean;
begin
  if not FileExists(ExpandConstant('{app}\微win.exe')) then
  begin
    MsgBox('找不到微win退出程序。请先恢复安装，再执行卸载。', mbError, MB_OK);
    Result := False;
    Exit;
  end;
  Result := CooperativeShutdown(ExpandConstant('{app}\微win.exe'), ExpandConstant('{app}'));
  if not Result then
    MsgBox('微win 尚未完成退出与恢复，卸载已中止。', mbError, MB_OK);
end;

// No [UninstallDelete] for LOCALAPPDATA\WeChatWinH: user identity/settings/logs stay intact.
