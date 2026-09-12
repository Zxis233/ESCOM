//! English presentation at the TUI boundary. Shared core messages remain compatible
//! with the Chinese GUI; received data and user input are never translated.
use escom_core::model::SendMode;

pub const fn send_mode_label(mode: SendMode) -> &'static str {
    match mode {
        SendMode::Text => "Text",
        SendMode::Hex => "HEX",
    }
}

pub fn english_error(message: &str) -> String {
    let translated = match message {
        "请选择串口" => "Select a serial port",
        "请输入波特率" => "Enter a baud rate",
        "波特率只能包含数字" => "Baud rate must contain digits only",
        "波特率必须在 1 到 4,000,000 之间" => "Baud rate must be between 1 and 4,000,000",
        "请输入要发送的 HEX 数据" => "Enter HEX data to send",
        "HEX 字符数量必须为偶数" => "HEX data must contain an even number of digits",
        "HEX 数据只能包含 0-9、A-F 和空格" => {
            "HEX data may contain only 0-9, A-F and whitespace"
        }
        "HEX 数据格式无效" => "Invalid HEX data",
        "文本包含 GBK 无法表示的字符" => {
            "Text contains characters that cannot be encoded in GBK"
        }
        "请输入要发送的数据" => "Enter data to send",
        "串口发送积压已达到字节上限，请稍后重试" => {
            "TX byte budget reached; try again later"
        }
        "串口发送队列已满，请稍后重试" => "TX queue is full; try again later",
        "串口任务已停止" => "Serial worker has stopped",
        "串口任务未确认记录切换" => {
            "Serial worker did not acknowledge the recording change"
        }
        "串口尚未连接" => "Serial port is not connected",
        "串口驱动返回了无效的写入字节数" => {
            "Serial driver returned an invalid write byte count"
        }
        "发送已取消：串口关闭或重新连接，已写入 0 字节" => {
            "Send cancelled: port closed or reopened; 0 bytes written"
        }
        _ => {
            for (prefix, english) in [
                ("无法枚举串口：", "Unable to list serial ports: "),
                ("设置 DTR 失败：", "Unable to set DTR: "),
                ("设置 RTS 失败：", "Unable to set RTS: "),
                ("串口写入失败：", "Serial write failed: "),
                ("串口读取失败：", "Serial read failed: "),
                ("正则表达式无效：", "Invalid regular expression: "),
            ] {
                if let Some(detail) = message.strip_prefix(prefix) {
                    return format!("{english}{}", english_error(detail));
                }
            }
            if let Some(detail) = message.strip_prefix("打开 ")
                && let Some((port, error)) = detail.split_once(" 失败：")
            {
                return format!("Unable to open {port}: {}", english_error(error));
            }
            if let Some(limit) = message
                .strip_prefix("单次发送超过 ")
                .and_then(|s| s.strip_suffix(" 字节限制"))
            {
                return format!("Single send exceeds the {limit}-byte limit");
            }
            if let Some(detail) = message.strip_prefix("发送 #")
                && let Some((id, count)) = detail.split_once(" 已取消：已写入 ")
                && let Some(count) = count.strip_suffix(" 字节")
            {
                return format!("Send #{id} cancelled: {count} bytes written");
            }
            message
        }
    };
    translated.to_owned()
}
