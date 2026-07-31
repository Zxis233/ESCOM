# ESCOM

ESCOM 是使用 Rust 和 `eframe/egui` 编写的 Windows 串口查看器。它面向持续串口调试场景，串口读写在独立线程中执行，界面不会直接进行阻塞 I/O。

## 功能

- 自动枚举串口，支持自定义波特率、数据位、停止位、奇偶校验、软件/硬件流控、DTR 和 RTS。
- 接收与发送可独立选择文本或 HEX；文本支持 UTF-8 和 GBK。
- 支持时间戳、暂停显示、自动滚动、收发计数和 5/20/100/500 MiB 有界缓存。
- 支持 CR、LF、CRLF 行尾、最近 50 条成功发送历史和定时循环发送。
- 可按当前显示格式导出带 UTF-8 BOM 的 TXT 文件。
- 可分别选择 Windows 系统界面字体和等宽数据字体。

## 构建

需要 Rust 1.96 或更新版本，以及包含 `rc.exe` 的 Windows SDK。

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

便携版程序生成在 `target\release\escom.exe`，无需安装 Rust 即可运行。

## 设置

界面偏好保存在 `%APPDATA%\ESCOM\settings.json`。应用不会保存串口参数、发送内容、发送历史或接收数据，也不会在启动时自动连接设备。

## 首版边界

首版不包含协议解析、搜索过滤、自动日志、多命令预设、插件、自动重连或在线更新。输出区只展示接收数据，发送数据通过历史和 TX 计数查看。

