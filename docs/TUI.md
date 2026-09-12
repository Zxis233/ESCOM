# ESCOM TUI

Rust 串口查看器：GUI 与低内存 TUI 共用一套串口和解码核心。

## 构建与运行

```powershell
# 原有 GUI（根目录 cargo run 的行为不变）
cargo run --release -p escom

# 独立 TUI；不构建 GUI 包及其图形、字体、图片依赖
cargo build --release -p escom-tui
.\target\release\escom-tui.exe --list
.\target\release\escom-tui.exe --port COM3 --baud 115200

# 无需串口硬件的演示：接收模拟传感器行，发送后原样回显
.\target\release\escom-tui.exe --demo

# 显式配置文件 + 命令行覆盖，开始前创建记录文件
.\target\release\escom-tui.exe --config escom-tui.example.toml --port COM3 --record session.bin
```

请在支持交互终端的 Windows Terminal / PowerShell 中运行。TUI 最小窗口为 48×12，完整帮助建议 100×34。TUI 也使用跨平台终端和串口库，但本次以 Windows 为验证平台。

`--help` 列出参数；`--print-config` 输出生效的 TOML。只有显式 `--config` 才读取配置；命令行优先于文件。运行期间的设置不自动写回，也不读取或覆盖 GUI 的配置。配置未知字段、无效值和不一致预算会报错。

## TUI 操作

| 按键 | 操作 |
| --- | --- |
| F2 / F3 | 轮换检测到的串口 / 连接或断开 |
| F4 / F5 | 文本、HEX、终端显示 / UTF-8、GBK |
| F6 | 开始记录到当前目录的新 `ESCOM-RX-时间.bin` / 停止并落盘 |
| F7 / s | 切换发送文本、HEX / 编辑发送内容，Enter 发送 |
| F8 | 直接输入到设备，Esc 返回查看模式；Enter 发 CR、Ctrl+C 发 0x03 |
| / | 输入搜索词，Enter 才搜索；编辑时 Ctrl+R 切换正则 |
| n / N | 下一个 / 上一个命中行 |
| Space / End | 暂停或恢复显示 / 回到最新数据 |
| 上下、PgUp/PgDn、Home | 滚动历史、翻页、跳到最早显示行 |
| 左右 | 横向滚动长行 |
| t / c | 显示时间戳 / 清空内存历史（记录文件继续写入） |
| : / ? | 命令编辑器 / 帮助 |
| q / Ctrl+Q | 查看模式退出 / 任何模式退出 |
| Esc / Ctrl+U | 关闭编辑器 / 清空编辑内容 |

命令示例：`:port COM4`、`:baud 921600`、`:data 8`、`:stop 1`、`:parity even`、`:flow hardware`、`:dtr on`、`:rts off`、`:eol crlf`、`:mode terminal`、`:encoding gbk`、`:record D:\captures\session.bin`、`:stop-record`、`:ports`。端口、波特率、数据位、停止位、校验和流控修改前需断开连接。硬件流控时不手动修改 RTS。

终端模式复用现有 ANSI 屏幕解析器，是面向串口 Shell 的有限终端解释器；不是完整的终端模拟器。串口控制字符不会直接传给宿主终端。窗口尺寸不会通过串口自动协商。直接输入模式下 Esc 为本地返回键，Ctrl+Q 为本地退出键；要发送这些字节可通过 HEX 发送。

## 历史与内存预算

| 项目 | 默认值 | 配置 | 行为 |
| --- | --- | --- | --- |
| 原始 RX 历史 | 2 MiB，8192 条记录 | `history_kib`、`history_records` | 任一上限触发即淘汰最早完整记录；小包元数据也有上限 |
| 格式化历史 | 512 KiB 文本，2000 行 | `display_kib`、`display_rows` | 独立淘汰；只为当前可见行构建界面组件 |
| 单行文本 | 8 KiB | `line_kib` | 超长行裁剪；不会无限积累无换行输出 |
| 每次增量 | 32 KiB | 固定 | 最多每 50 ms 更新一次；落后过多则从有限原始尾部重建 |
| 搜索 | 最多 1024 个命中行 | 固定 | Enter 后扫描暂停的格式化历史；不存每个匹配区间，不复制历史，不后台重搜 |
| 发送 | 总共 256 KiB；单次 64 KiB | `tx_kib`、`send_kib` | 原有 128 请求限制也保留；排队和正在发送的完整分配都计费 |
| 记录队列 | 256 KiB；最多 1024 个块（默认） | `record_queue_kib` | 字节预算包含正在写的块；另有 64 KiB 文件写缓冲 |

原始预算可设 64 KiB–64 MiB，记录数 64–65536；显示预算 16 KiB–16 MiB，行数 32–100000；单行 1–64 KiB；发送总额度 1 KiB–64 MiB，单次 1 KiB–1 MiB；记录队列 8 KiB–64 MiB。单次发送不得超过总发送预算，单行不得超过总显示预算。

低内存配置示例：

```powershell
.\target\release\escom-tui.exe --port COM3 --history-kib 512 --history-records 2048 --display-kib 128 --display-rows 500 --line-kib 2 --tx-kib 64 --send-kib 16 --record-queue-kib 64
```

这些是各类数据的预算，不是进程 RSS 承诺。还包括记录/行元数据、容器容量、线程栈、终端渲染缓冲、解码器、临时快照、搜索正则等。终端模式还维护字符屏幕；总占用与显示文本字节数不同。快照通过 Arc 共享字节，但暂时延长淘汰块寿命；重建只取最多 `display_kib` 的原始尾部，HEX 还受 `display_rows × 16` 限制。格式化按接收记录释放被淘汰行，每次仍可能有一个接收块的临时输出。

因此原始历史较大不代表 TUI 保留同样长度的可回看文字。搜索只覆盖当前格式化历史，时间戳不参与匹配；开始搜索会暂停显示，接收和记录继续。Esc 取消搜索编辑时恢复进入前的暂停状态。提交后保持暂停，恢复实时显示则清除命中索引。搜索词最多 1024 UTF-8 字节，正则编译大小限制 1 MiB、DFA 缓存限制 256 KiB；超限表达式报错，扫描后释放匹配器。如果暂停期间原始数据被淘汰，显示会重建，开头不完整的字符/控制序列可能无法恢复。

发送编辑器最多接受 `send_kib` 大小的 UTF-8 输入；真正入队前还会校验编码转换、行尾追加之后的字节数。默认 CRLF 占 2 字节，因此接近单次发送上限时需为行尾留出空间。超长粘贴整体拒绝，不截断后发送。空闲时只轮询状态，不持续重绘；有输入、接收数据或状态变化才刷新界面。

## 长时间记录

记录保存从启用之后读取到的**原始 RX 字节**，包括不可打印字节和 ANSI 序列。它不是 TXT 导出，也不含 TX、时间戳和会话分隔符；断开重连期间保持同一个文件，重新收到的数据继续追加。磁盘记录不依赖内存历史，清屏、暂停、搜索和历史淘汰均不影响它。

串口线程把数据送入有界队列，独立线程流式写入。每秒 flush，正常停止/退出时先停止生产，再排空已接受数据、flush、sync。停止切换由串口线程确认后才排空记录，避免生产者还在写。断电或进程被强杀仍可能丢失未持久化的尾部。

写入变慢导致队列满，或磁盘写入/刷新失败时，当前记录被标为 **不完整**，界面保持红色错误。记录不会静默跳过数据再恢复；需停止后新建文件。接收与显示继续运行。已有文件永远不覆盖。单个文件持续增长，不自动轮转或删除旧文件；请规划磁盘空间。正常退出若落盘失败会返回非零退出码。

发送总额度通过预留令牌实现：入队失败、发送完成、取消、断连和线程退出都会释放。部分发送不会提前退还仍占用内存的剩余分配；失败反馈包含当前请求已写入字节数，成功表示交给驱动，不代表设备协议确认。GUI 也得到相同的预算保护，默认总发送额度 4 MiB、单次 1 MiB。

## 工作区与验证

- 根包 `escom`：原有 eframe GUI、字体、主题、图片、配置和图标资源。
- `crates/escom-core`：串口、原始缓存、流式解码、终端解释、搜索、导出、记录。
- `crates/escom-tui`：Ratatui/Crossterm 界面、独立配置、键盘输入和演示后端。

根库重导出原模块路径，现有 GUI 和基准的调用路径保持兼容。TUI 单独构建不依赖根包。

```powershell
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo tree -p escom-tui
```

测试覆盖小包/会话元数据淘汰、三个显示模式的持续输入、发送预算与部分取消、记录队列饱和、记录不受历史淘汰影响、按需搜索和终端界面渲染。演示和模拟串口测试不能代替真实设备拔插、驱动流控和长时间实机测试。
