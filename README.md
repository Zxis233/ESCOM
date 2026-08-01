# ESCOM

[![Platform](https://img.shields.io/badge/platform-Windows-0078D4?logo=windows)](https://www.microsoft.com/windows)
[![Rust](https://img.shields.io/badge/built%20with-Rust-000000?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

ESCOM 是一款使用 Rust 和 `eframe/egui` 编写的 Windows 串口查看器，面向嵌入式开发、设备联调和持续日志观察场景。串口读写在独立线程中执行，避免阻塞界面；接收数据采用有界缓存，并针对持续数据流进行了增量格式化和搜索优化。

> 当前版本：`0.1.0`。项目处于早期开发阶段，欢迎提交 Issue 和 Pull Request。

## 功能特性

### 串口连接

- 自动枚举可用串口，并支持手动刷新。
- 支持 `1` 至 `4,000,000` 的自定义波特率。
- 支持数据位、停止位、奇偶校验和软件/硬件流控。
- 支持 DTR、RTS 控制。

### 数据接收

- 文本、HEX 和终端三种接收模式。
- 文本模式支持 UTF-8、GBK 编码。
- 支持可配置格式的时间戳、暂停显示、自动滚动和收发字节计数。
- 提供 5、20、100、500 MiB 可选原始数据缓存；显示窗口最多保留最近 100,000 行或 16 MiB 文本，避免大缓存被完整物化为 UI 对象。
- 支持普通文本或正则表达式搜索、大小写匹配、结果导航和过滤显示。
- 支持通过 TOML 配置整行高亮规则。
- 可按当前显示格式将完整接收缓存流式导出为带 UTF-8 BOM 的 TXT 文件。

### 数据发送

- 支持文本和 HEX 发送。
- 文本发送支持无行尾、CR、LF 和 CRLF。
- 保存最近 50 条成功发送记录，并自动去重。
- 支持可调间隔的循环发送，间隔范围为 20 ms 至 1 小时。
- 终端模式支持 ANSI/VT100 清行、清屏和光标控制；可直接在接收区输入，使用方向键浏览历史和移动光标，并正确处理退格、Home、End、Delete 等按键。

### 界面与个性化

- 支持跟随系统、亮色和暗色主题。
- 可分别设置界面字体和串口数据字体，包括字号、字重和数据行距。
- 支持本地图片或 HTTP(S) 在线图片作为应用背景。
- 可分别调整亮色、暗色主题下的背景不透明度。
- 可从背景图片自动提取强调色，并动态调整按钮、选项和搜索高亮颜色。
- 自绘标题栏与应用主题保持一致，支持窗口拖动、缩放、双击最大化和窗口快捷菜单。

## 获取与运行

### 下载可执行文件

从仓库的 **Releases** 页面下载最新的 `escom.exe`，双击即可运行，无需安装 Rust。

ESCOM 不会自动连接设备。启动后请选择串口和通信参数，再点击“打开串口”。如果列表中没有目标设备，请确认串口驱动已安装、设备未被其他程序占用，然后点击刷新按钮重新扫描。

### 从源码构建

构建环境：

- Windows
- Rust `1.96` 或更新版本
- 包含 `rc.exe` 的 Windows SDK

克隆或下载本仓库并进入项目目录后运行：

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

构建完成后，可执行文件位于：

```text
target\release\escom.exe
```

开发时也可以直接运行：

```powershell
cargo run
```

## 基本使用

1. 连接串口设备并启动 ESCOM。
2. 从顶部工具栏选择串口、波特率和其他通信参数。
3. 点击“打开串口”开始接收数据。
4. 根据设备输出选择文本、HEX 或终端模式；文本乱码时可切换 UTF-8 与 GBK。
5. 在底部发送区输入文本或 HEX 数据并发送，需要持续发包时启用“循环发送”。
6. 使用接收区工具栏完成搜索、过滤、清空或导出。

HEX 发送支持带空格或连续输入，例如以下两种写法等价：

```text
AA 01 FF
AA01FF
```

## 高亮规则

首次启动时，ESCOM 会自动创建高亮配置：

```text
%APPDATA%\ESCOM\highlight.toml
```

规则按照文件中的书写顺序匹配，第一条命中的规则决定整行样式。修改文件后，在接收区的“高亮”菜单中点击“重新加载”。

```toml
version = 1

[[rules]]
name = "错误"
enabled = true
mode = "regex" # contains 或 regex
pattern = '\b(ERROR|FATAL)\b|错误'
case_sensitive = false
foreground = "#FF6B6B"
background = "#E5484D40" # #RRGGBB 或 #RRGGBBAA
underline = false
```

每条启用的规则至少需要设置 `foreground`、`background` 或 `underline` 之一。

## 配置与隐私

ESCOM 的本地文件保存在 `%APPDATA%\ESCOM\`：

| 文件 | 用途 |
| --- | --- |
| `settings.toml` | 界面、字体、收发显示和背景偏好 |
| `highlight.toml` | 接收内容高亮规则 |
| `window.ron` | 窗口位置与尺寸 |

`settings.toml` 按功能分组，首次启动时会自动创建。建议关闭 ESCOM 后再手动修改，重新启动后生效：

```toml
schema_version = 6

[interface]
theme = "system" # system、light 或 dark

[fonts]
ui_family = "" # 留空表示自动选择
data_family = ""
ui_weight = 400 # 1-1000
data_weight = 400
ui_size = 15.0 # 10-18
data_size = 15.0 # 10-48
data_line_spacing = 3.0 # 0-24

[receive]
mode = "text" # text、hex 或 terminal
encoding = "utf8" # utf8 或 gbk
timestamps = false
timestamp_format = "%Y-%m-%d %H:%M:%S%.3f" # chrono/strftime 格式
auto_scroll = true
buffer_limit_mib = 20 # 5、20、100 或 500

[send]
mode = "text" # text 或 hex
line_ending = "crlf" # none、cr、lf 或 crlf
repeat_interval_ms = 1000 # 20-3600000

[background]
source = "none" # none、local 或 online
local_path = ""
online_url = ""
light_opacity = 0.22 # 0.0-1.0
dark_opacity = 0.16 # 0.0-1.0
dynamic_accent = true # 根据背景图片自动生成按钮与选项的强调色
```

配置中的字段可以省略，缺失字段会使用默认值。字段名或枚举值无效时，应用启动后会显示错误提示。旧版 `settings.json` 会在首次启动新版时自动迁移，成功后归档为 `settings.json.migrated.bak`。

- 应用不会持久化串口参数、发送内容、发送历史或接收数据。
- 应用启动时不会自动连接串口。
- 本地背景仅记录原始文件路径；在线背景仅记录图片地址。
- 使用在线背景时，应用需要访问对应的 HTTP(S) 图片地址。

## 项目结构

```text
src/
├── app.rs            # 应用状态、后台事件与生命周期
├── app/              # 连接、接收、搜索、发送和设置界面
├── serial_worker.rs  # 串口后台任务
├── store.rs          # 有界接收缓存
├── formatting.rs     # 文本/HEX 格式化与导出
├── search.rs         # 搜索与增量索引
├── highlight.rs      # TOML 高亮规则
├── settings.rs       # 用户偏好与配置存储
├── fonts.rs          # 系统字体加载
└── window_chrome.rs  # 自绘标题栏与窗口交互
```

## 当前边界

当前版本暂不包含：

- 协议解析和数据绘图
- 自动日志记录
- 多命令预设
- 插件系统
- 串口断线自动重连
- 应用内在线更新

接收区只显示设备返回的数据；已发送内容通过发送历史和 TX 计数查看。

## 参与贡献

提交问题前，请尽量附上 Windows 版本、串口设备或芯片型号、通信参数、复现步骤以及相关日志。代码贡献建议先创建 Issue 说明需求或问题，再提交范围清晰的 Pull Request。

提交前请运行：

```powershell
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

## 许可证

本项目基于 [MIT License](LICENSE) 开源。
