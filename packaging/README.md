# Windows 构建与发布

`windows.yml` 在 push、PR 和手动触发时运行 Windows 单元测试、MSVC release 构建、静态 CRT/资源检查，并生成便携版与安装版。只有 `v*` tag 的成功构建会发布 Release；tag 必须与 Cargo workspace 当前版本一致。版本没有写死在脚本里。

GitHub runner 使用 `windows-2022`，原生 x64 MSVC、Rust stable 和 `-C target-feature=+crt-static`。通过预装 Visual Studio developer shell 提供 cl/link/rc，Opus 和 VAD 从 Cargo 已锁定的源码静态构建。`verify_payload.py` 检查三份 PE 为 x64、无动态 CRT/Opus 依赖，以及 GUI 图标资源 100/101、manifest 的 highestAvailable/PerMonitorV2；不运行 GUI。

## 本地调用

在已安装 Visual Studio C++ 工具、Windows SDK、Rust MSVC、Python 3 和 Inno Setup 6 的 Windows 上，用 PowerShell 7：

```powershell
./packaging/build-windows.ps1
```

不自动安装系统工具。ISCC 依次查找 `Program Files (x86)\Inno Setup 6`、`Program Files\Inno Setup 6` 和 `LOCALAPPDATA\Programs\Inno Setup 6`。Python 只参与构建检查，不进入发布包。

`dist/release/` 仅输出：

- `weiwin-<Cargo版本>-windows-x64-portable.zip`
- `weiwin-<Cargo版本>-windows-x64-setup.exe`
- `SHA256SUMS.txt`：上述两包的 SHA256。

便携包根目录和安装 payload 使用同一白名单：`微win.exe`、`wechat-winh.exe`、`wechat_winh_bridge-<版本>.dll`、`README.md`、`LICENSE`、`Start.cmd`、`Stop.cmd`、`OCTICONS-LICENSE.txt`。不递归复制仓库或 target 目录；不会包含 private、research、设备身份、配置、录音或日志。

## 安装与卸载契约

用户级默认目录为 `%LOCALAPPDATA%\Programs\微win`，开始菜单名称为“微win”，桌面快捷方式可选。升级安装前提取新 GUI 和 broker 到临时目录，调用新 GUI `--quit`；卸载前调用安装目录 GUI `--quit`。两者都等待进程退出码 0，失败中止。ShellExec 允许 GUI 自身 highestAvailable 清单在需要时提示 UAC，拒绝授权不会继续覆盖文件。

编译器目录存在 `Languages\ChineseSimplified.isl` 时提供简体中文向导，否则使用内置英文向导；两种情况下应用名称、目录和快捷方式均为中文，不依赖下载社区译本。

GUI 的 `--quit` 必须先让本产品旧 GUI 退出、让 broker 完成 Windows 原生恢复，并在确认后返回 0；失败返回非 0。临时 GUI 被命名为 `weiwin-shutdown.exe`，其同目录有 `wechat-winh.exe`，该退出逻辑不应依赖 GUI 文件名。

安装脚本关闭 Inno Restart Manager 的自动关闭/重启功能，不结束微信、输入法或 TextInputHost。版本化 bridge 不覆盖同版本已有文件；卸载时若 Windows 仍映射 DLL，只安排重启后删除，不强制退出其宿主。安装器不删除 `%LOCALAPPDATA%\WeChatWinH` 用户数据，不增加开机启动。

## Release 一致性

只有 Release job 获得 `contents: write`；PR/build job 只有读取权限。GitHub 官方 checkout/upload/download actions 固定到完整 commit，checkout 不持久化凭据。发布脚本先校验本地两包校验和，创建 draft，用 `gh release upload` 上传三项，核对远端名称和大小后才取消 draft 并将稳定版设为 latest。上传失败时保留 draft，不会让只有一个包的版本出现在 latest。已公开版本的重跑只核对资产，不覆盖已发布文件。

## 验证边界与交接

本地可运行 `python3 -m unittest discover -s packaging -p 'test_*.py'`，验证白名单、校验和失败不访问 GitHub、draft 发布顺序。`verify_payload.py --stage ... --version ... --allow-dynamic-crt` 仅用于已有 GNU 产物的资源回归，CI 不使用这个豁免。

已通过 4 项本地打包回归测试、PowerShell 源码语法解析、工作流 YAML/权限/官方 Action commit 固定检查，以及 GUI GNU 交叉检查和现有 GNU PE 的图标 100/101、manifest 读取。用户级 Inno Setup 编译器已实际编译出安装包（仅使用现有 GNU 三份二进制作为 fixture，未执行安装器）。

完整 MSVC 静态 CRT 构建与资源链接仍需首次 CI 确认；安装、升级、卸载的退出与恢复交互需后续 Windows 验收。此提交没有执行 `--quit`、安装器或任何 GitHub 写操作。root README 需补充下载双包与 SHA256 的入口；版本与 tag 由主代理管理。

参考：

- [GitHub runner Windows 2022 软件清单（含 Inno Setup）](https://github.com/actions/runner-images/blob/main/images/windows/Windows2022-Readme.md)
- [微软构建脚本的 ISCC 常规路径示例](https://github.com/microsoft/coreutils/blob/main/build.ps1)
- [GitHub workflow 语法](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax)
- [GITHUB_TOKEN 最小权限](https://docs.github.com/en/actions/tutorials/authenticate-with-github_token)
- [Inno PrepareToInstall / InitializeUninstall](https://jrsoftware.org/ishelp/topic_scriptevents.htm)
- [Inno 自动关闭应用设置](https://jrsoftware.org/ishelp/topic_setup_closeapplications.htm)
- [Inno ShellExec 底层等待与退出码实现](https://github.com/jrsoftware/issrc/blob/main/Projects/Src/Setup.InstFunc.pas)
- [Octicons MIT 许可证来源](https://github.com/github/octicons/blob/main/LICENSE)
