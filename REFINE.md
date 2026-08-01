**结论**

整体架构已经比较成熟：串口 I/O、接收缓存、增量格式化和搜索索引职责清晰，P0 修复有效。当前主要问题集中在大缓存、导出和搜索调度，而不是常规场景下的明显正确性错误。

**优先级建议**

- **已解决（原高）：500 MiB 缓存不会再被完整物化为显示行。** 原始缓存容量保持不变；显示重建只读取尾部窗口，并限制为最多 100,000 行、16 MiB 文本和 512 KiB 单行。超过 128 KiB 的增量积压会转为后台尾部重建，避免阻塞 UI 线程。[receive.rs](D:/ESCOM/src/app/receive.rs) [formatting.rs](D:/ESCOM/src/formatting.rs) [store.rs](D:/ESCOM/src/store.rs)

- **已解决（原高）：导出不再构造完整格式化行和输出缓冲。** 文本按最多 64 KiB 输入块增量解码，HEX 使用固定 47 字节行缓冲，并通过 64 KiB `BufWriter` 直接写文件；快照所有权随任务转移，已写 chunk 会立即释放。同一时间只允许运行一个导出任务。[receive.rs](D:/ESCOM/src/app/receive.rs) [formatting.rs](D:/ESCOM/src/formatting.rs)

- **中高：搜索期间会暂停显示格式化。** 格式化等待搜索结束，搜索也等待格式化结束；大缓存、复杂正则或持续数据流下可能导致显示冻结、缓存断档，随后触发完整重建。[app.rs:531](D:/ESCOM/src/app.rs:531) [app.rs:710](D:/ESCOM/src/app.rs:710)  
  更好的方式是显示更新与搜索解耦：搜索针对不可变快照运行，结果只在 generation 仍匹配时启用，并合并或取消过期搜索任务。

- **中：连接状态下固定每 40 ms 重绘。** 即使没有新数据也持续消耗 CPU。[app.rs:2735](D:/ESCOM/src/app.rs:2735)  
  建议通过数据到达通知触发重绘，空闲时降低到 100–250 ms。

- **中：后台格式化、搜索和导出线程没有统一取消/回收机制。** 关闭窗口时只等待串口线程，超大任务仍可能继续运行。[app.rs:597](D:/ESCOM/src/app.rs:597) [app.rs:737](D:/ESCOM/src/app.rs:737)  
  建议增加任务协调器、取消标志和 `JoinHandle` 管理。

- **中：接收路径每次读取都会复制一次数据。** `read_buffer[..count].to_vec()` 在高速接收时会产生大量短生命周期分配。[serial_worker.rs:240](D:/ESCOM/src/serial_worker.rs:240)  
  可考虑转移缓冲区所有权、复用 chunk 或建立小型 buffer pool。

- **中：超长无换行文本仍会反复复制尾行。** `push_partial_row` 会复制当前整行，搜索也会重复扫描。[formatting.rs:258](D:/ESCOM/src/formatting.rs:258)  
  建议限制单行长度，或使用分段字符串/rope，仅更新尾部。

- **低：`Mutex` 错误被静默忽略。** 接收线程可能直接丢数据，UI 也只使用默认值，不会报告缓存异常。[serial_worker.rs:248](D:/ESCOM/src/serial_worker.rs:248)  
  建议统一转成 `WorkerEvent`，或使用不会 poison 的锁实现。

**工程化建议**

`app.rs` 已超过 3,600 行，建议拆出连接、接收显示、搜索、发送、设置窗口等模块；同时补充 20/100/500 MiB 数据量下的格式化、搜索、导出基准测试。CI 目前只有测试和构建，[workflow](D:/ESCOM/.github/workflows/windows-release.yml:81) 还应加入 `cargo fmt --check` 和 Clippy。

已验证：63 个测试通过，Clippy、fmt、release 构建，以及 Windows 目标的 locked 测试均通过。工作树未修改。
