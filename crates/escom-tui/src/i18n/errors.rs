use super::Language;
use escom_core::error::{CoreError, ErrorKind, Operation};

pub fn render(error: &CoreError, language: Language) -> String {
    if language == Language::ZhCn {
        return error.to_string();
    }
    match error {
        CoreError::Simple(kind) => match kind {
            ErrorKind::MissingPort => "Select a serial port",
            ErrorKind::BaudMissing => "Enter a baud rate",
            ErrorKind::BaudDigits => "Baud rate must contain digits only",
            ErrorKind::BaudRange => "Baud rate must be between 1 and 4,000,000",
            ErrorKind::HexEmpty => "Enter HEX data to send",
            ErrorKind::HexOdd => "HEX data must contain an even number of digits",
            ErrorKind::HexChars => "HEX data may contain only 0-9, A-F and whitespace",
            ErrorKind::HexInvalid => "Invalid HEX data",
            ErrorKind::Unencodable => "Text contains characters that cannot be encoded in GBK",
            ErrorKind::SendEmpty => "Enter data to send",
            ErrorKind::TxBudget => "TX byte budget reached; try again later",
            ErrorKind::QueueFull => "TX queue is full; try again later",
            ErrorKind::WorkerStopped => "Serial worker has stopped",
            ErrorKind::CaptureAck => "Serial worker did not acknowledge the recording change",
            ErrorKind::Disconnected => "Serial port is not connected",
            ErrorKind::InvalidWrite => "Serial driver returned an invalid write byte count",
            ErrorKind::QueueCancelled => "Send cancelled: port closed or reopened; 0 bytes written",
            ErrorKind::CaptureBudget => {
                "Recording stopped: disk queue byte limit reached; file is incomplete"
            }
            ErrorKind::CaptureQueue => {
                "Recording stopped: disk queue unavailable/full; file is incomplete"
            }
            ErrorKind::CaptureRange => "Capture queue must be 8 KiB..64 MiB",
            ErrorKind::CapturePanic => "Capture thread panicked",
        }
        .into(),
        CoreError::SendTooLarge { limit } => format!("Single send exceeds the {limit}-byte limit"),
        CoreError::Cancelled { id, written, total } => {
            format!("Send #{id} cancelled: {written}/{total} bytes written")
        }
        CoreError::Open { port, detail } => format!("Unable to open {port}: {detail}"),
        CoreError::External(detail) => detail.clone(),
        CoreError::Operation { operation, detail } => {
            let description = match operation {
                Operation::ListPorts => "Unable to list serial ports",
                Operation::SetDtr => "Unable to set DTR",
                Operation::SetRts => "Unable to set RTS",
                Operation::Read => "Serial read failed",
                Operation::Write => "Serial write failed",
                Operation::Regex => "Invalid regular expression",
                Operation::CaptureWrite => "Recording write failed; file is incomplete",
                Operation::CaptureFlush => "Recording flush failed; file is incomplete",
                Operation::CaptureSync => "Recording final flush failed; file may be incomplete",
            };
            format!("{description}: {detail}")
        }
    }
}
