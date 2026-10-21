# 图标与协议常量

`app-icon.svg` 是微win的可编辑扁平图标源文件：绿色圆角方块、白色麦克风及右下角微信双气泡。`app-icon.png` 和 `app-icon.ico` 从 SVG 渲染并封装，ICO 包含16、24、32、48、64、128、256像素帧。界面、任务栏和托盘共用该图。

`github-mark.svg` 来自 [Primer Octicons](https://github.com/primer/octicons/blob/main/icons/mark-github-16.svg)，PNG/ICO 是相同矢量图的格式封装；许可见 `LICENSE-octicons.txt`。

`wetype-bootstrap.bin` 与 `wetype-sign-salt.bin` 是独立协议实现使用的固定客户端常量，不包含用户设备身份或会话凭据。匿名身份由程序首次运行时随机生成，仅保存在本机应用数据目录。
