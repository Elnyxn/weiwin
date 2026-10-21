# 微win

保留 Windows 原生 **Win+H** 面板，使用微信输入法云端语音识别。Rust 原生程序，提供控制面板、托盘、流式输入和文字修订。

**需要联网，无需微信或微信输入法账号登录，也无需常驻运行这两个客户端。** 默认后端独立完成匿名设备登记与会话协商，设备名称读取 Windows 真实主机名。

## 下载与使用

从 [GitHub Releases](https://github.com/Elnyxn/weiwin/releases/latest) 下载：

- **便携版**：`weiwin-<版本>-windows-x64-portable.zip`，解压后运行 `微win.exe`。
- **安装版**：`weiwin-<版本>-windows-x64-setup.exe`，安装到当前用户目录，创建开始菜单和可选桌面快捷方式。
- `SHA256SUMS.txt`：两个发行文件的 SHA-256 校验值。

在控制面板开启后台服务，随后照常按 **Win+H**。讲话时流式写入并修订；默认静音3秒收句，继续讲话自动续听。麦克风和静音时间可在下拉菜单选择，设置自动保存，下次唤起听写时生效。

“Windows 原生”恢复微软识别。关闭控制面板后保留在托盘；托盘“退出”会先恢复原生接口。需要向管理员窗口输入时，请以管理员身份运行微win。

## 检测更新

启动后检查 GitHub Releases，之后每6小时检查一次；也可以右键左上角 GitHub 图标，选择“检查更新”。检查在后台进行，不阻塞输入。

新版本的便携包和安装包都发布完整后，GitHub 图标旁会出现更新气泡：

- **下载更新**：打开该版本的发布页，选择便携版或安装版。
- **跳过此版本**：保存该版本的跳过选择，新版本出现后仍会提示。
- **关闭**：隐藏当前提示；手动检查时可再次查看。

安装与卸载会先请求本程序正常退出并恢复 Windows 原生识别，不结束微信、输入法或 TextInputHost。用户设置及匿名设备身份保留在 `%LOCALAPPDATA%\WeChatWinH`。

## 兼容范围

目前提供 **Windows 11 x64** 版本，原生面板适配已在 build26300 的系统组件上验证。桥接按组件哈希核对；其他组件版本需要适配，不匹配时拒绝接管。Windows Terminal 支持流式尾部修订；手动编辑、回车或切换输入目标后停止补写。

## 自动构建与发布

GitHub Actions 在提交和拉取请求时运行检查并生成两种包。推送与 Cargo 版本一致的 `vX.Y.Z` 标签后，自动上传便携 ZIP、安装 EXE 和校验文件，完整上传后公开 Release。仓库和后续发布使用 **Elnyxn** 身份。

```text
cargo test --workspace
cargo build --release --target x86_64-pc-windows-msvc
```

Windows 构建需要 Rust、Visual Studio C++ 构建工具和 CMake；安装器使用 Inno Setup。具体流程见 `.github/workflows` 和 `packaging`。

发布包包含程序及版本化桥接 DLL，不包含设备身份、连接凭据、录音、日志或调查工具。默认后端不会读取聊天内容。
