use std::fmt;

/// Stable error identity and parameters. Display preserves the GUI's Chinese text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    MissingPort,
    BaudMissing,
    BaudDigits,
    BaudRange,
    HexEmpty,
    HexOdd,
    HexChars,
    HexInvalid,
    Unencodable,
    SendEmpty,
    TxBudget,
    QueueFull,
    WorkerStopped,
    CaptureAck,
    Disconnected,
    InvalidWrite,
    QueueCancelled,
    CaptureBudget,
    CaptureQueue,
    CaptureRange,
    CapturePanic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    ListPorts,
    SetDtr,
    SetRts,
    Read,
    Write,
    Regex,
    CaptureWrite,
    CaptureFlush,
    CaptureSync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    Simple(ErrorKind),
    SendTooLarge {
        limit: usize,
    },
    Cancelled {
        id: u64,
        written: usize,
        total: usize,
    },
    Open {
        port: String,
        detail: String,
    },
    Operation {
        operation: Operation,
        detail: String,
    },
    External(String),
}

impl From<ErrorKind> for CoreError {
    fn from(kind: ErrorKind) -> Self {
        Self::Simple(kind)
    }
}
impl From<String> for CoreError {
    fn from(detail: String) -> Self {
        Self::External(detail)
    }
}
impl From<CoreError> for String {
    fn from(error: CoreError) -> Self {
        error.to_string()
    }
}
impl std::error::Error for CoreError {}

impl CoreError {
    pub fn io(operation: Operation, error: std::io::Error) -> Self {
        if let Some(core) = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<CoreError>())
        {
            return core.clone();
        }
        Self::Operation {
            operation,
            detail: error.to_string(),
        }
    }
}

impl ErrorKind {
    pub const fn chinese(self) -> &'static str {
        match self {
            Self::MissingPort => "请选择串口",
            Self::BaudMissing => "请输入波特率",
            Self::BaudDigits => "波特率只能包含数字",
            Self::BaudRange => "波特率必须在 1 到 4,000,000 之间",
            Self::HexEmpty => "请输入要发送的 HEX 数据",
            Self::HexOdd => "HEX 字符数量必须为偶数",
            Self::HexChars => "HEX 数据只能包含 0-9、A-F 和空格",
            Self::HexInvalid => "HEX 数据格式无效",
            Self::Unencodable => "文本包含 GBK 无法表示的字符",
            Self::SendEmpty => "请输入要发送的数据",
            Self::TxBudget => "串口发送积压已达到字节上限，请稍后重试",
            Self::QueueFull => "串口发送队列已满，请稍后重试",
            Self::WorkerStopped => "串口任务已停止",
            Self::CaptureAck => "串口任务未确认记录切换",
            Self::Disconnected => "串口尚未连接",
            Self::InvalidWrite => "串口驱动返回了无效的写入字节数",
            Self::QueueCancelled => "发送已取消：串口关闭或重新连接，已写入 0 字节",
            Self::CaptureBudget => "记录已停止：写盘队列达到字节上限，文件不完整",
            Self::CaptureQueue => "记录已停止：写盘队列已满或不可用，文件不完整",
            Self::CaptureRange => "记录队列必须为 8 KiB 至 64 MiB",
            Self::CapturePanic => "记录线程异常退出",
        }
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Simple(kind) => f.write_str(kind.chinese()),
            Self::SendTooLarge { limit } => write!(f, "单次发送超过 {limit} 字节限制"),
            Self::Cancelled { id, written, total } => {
                write!(f, "发送 #{id} 已取消：已写入 {written}/{total} 字节")
            }
            Self::Open { port, detail } => write!(f, "打开 {port} 失败：{detail}"),
            Self::External(detail) => f.write_str(detail),
            Self::Operation { operation, detail } => {
                let prefix = match operation {
                    Operation::ListPorts => "无法枚举串口",
                    Operation::SetDtr => "设置 DTR 失败",
                    Operation::SetRts => "设置 RTS 失败",
                    Operation::Read => "串口读取失败",
                    Operation::Write => "串口写入失败",
                    Operation::Regex => "正则表达式无效",
                    Operation::CaptureWrite => "记录写入失败，文件不完整",
                    Operation::CaptureFlush => "记录刷新失败，文件不完整",
                    Operation::CaptureSync => "记录最终落盘失败，文件可能不完整",
                };
                write!(f, "{prefix}：{detail}")
            }
        }
    }
}
