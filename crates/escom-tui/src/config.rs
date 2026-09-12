use escom_core::model::{LineEnding, ReceiveMode, SerialConfig, TextEncoding, parse_baud_rate};
use serde::{Deserialize, Serialize};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use std::io::{self, Read};
use std::path::PathBuf;

pub const HELP: &str = "ESCOM TUI - bounded-memory serial viewer

Usage: escom-tui [--config FILE] [options]
  --port COM3              Connect on startup (Linux: /dev/ttyUSB0)
  --baud 115200            Baud rate; other serial settings in TOML or :commands
  --mode text|hex|terminal Receive presentation
  --encoding utf8|gbk      Receive and send encoding
  --history-kib 2048       Raw RX history payload budget
  --history-records 8192   Raw record count budget (includes boundaries)
  --display-kib 512        Formatted text budget
  --display-rows 2000      Formatted row count budget
  --line-kib 8             Maximum formatted line length
  --tx-kib 256             Queued + active TX payload budget
  --send-kib 64            Maximum single send / editor payload budget
  --record-queue-kib 256   Recording queue + active block payload budget
  --record FILE            Stream raw RX bytes to a NEW file
  --list                   List ports and exit
  --demo                   Synthetic port for trying the UI without hardware
  --print-config           Print effective TOML and exit
  --help / --version

Keys: F2 port, F3 connect/disconnect, F4 RX mode, F5 encoding, F6 record,
F7 TX text/hex, F8 direct terminal input, s send, / search, : command,
Space pause/resume, arrows/PgUp/PgDn scroll, End follow, c clear, ? help, q quit.
Search runs on Enter over the frozen formatted history; n/N moves between rows.
Recording continues while paused. Ctrl+Q always exits. See README.md for budgets.
";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: String,
    pub flow: String,
    pub dtr: bool,
    pub rts: bool,
    pub mode: String,
    pub encoding: String,
    pub line_ending: String,
    pub timestamps: bool,
    pub history_kib: usize,
    pub history_records: usize,
    pub display_kib: usize,
    pub display_rows: usize,
    pub line_kib: usize,
    pub tx_kib: usize,
    pub send_kib: usize,
    pub record_queue_kib: usize,
    pub record: Option<PathBuf>,
    pub demo: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud: 115200,
            data_bits: 8,
            stop_bits: 1,
            parity: "none".into(),
            flow: "none".into(),
            dtr: false,
            rts: false,
            mode: "text".into(),
            encoding: "utf8".into(),
            line_ending: "crlf".into(),
            timestamps: true,
            history_kib: 2048,
            history_records: 8192,
            display_kib: 512,
            display_rows: 2000,
            line_kib: 8,
            tx_kib: 256,
            send_kib: 64,
            record_queue_kib: 256,
            record: None,
            demo: false,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Run,
    Help,
    Version,
    List,
    PrintConfig,
}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<(Self, Action), String> {
        let args: Vec<_> = args.into_iter().collect();
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok((Self::default(), Action::Help));
        }
        if args.iter().any(|arg| arg == "--version") {
            return Ok((Self::default(), Action::Version));
        }
        let mut config = Self::default();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--config" {
                let path = args.get(i + 1).ok_or("--config needs a file path")?;
                let file = std::fs::File::open(path).map_err(|e| format!("Config {path}: {e}"))?;
                let mut source = String::new();
                file.take(65537)
                    .read_to_string(&mut source)
                    .map_err(|e| e.to_string())?;
                if source.len() > 65536 {
                    return Err("Config exceeds 64 KiB".into());
                }
                config = toml::from_str(source.trim_start_matches('\u{feff}'))
                    .map_err(|e| format!("Config {path}: {e}"))?;
                i += 1;
            }
            i += 1;
        }
        let mut action = Action::Run;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => config.demo = true,
                "--list" => action = Action::List,
                "--print-config" => action = Action::PrintConfig,
                "--config" => {
                    args.next().ok_or("--config needs a path")?;
                }
                _ => {
                    let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    let number = || {
                        value
                            .parse::<usize>()
                            .map_err(|_| format!("Invalid number for {arg}"))
                    };
                    match arg.as_str() {
                        "--port" => config.port = value,
                        "--baud" => config.baud = parse_baud_rate(&value).map_err(str::to_owned)?,
                        "--mode" => config.mode = value,
                        "--encoding" => config.encoding = value,
                        "--history-kib" => config.history_kib = number()?,
                        "--history-records" => config.history_records = number()?,
                        "--display-kib" => config.display_kib = number()?,
                        "--display-rows" => config.display_rows = number()?,
                        "--line-kib" => config.line_kib = number()?,
                        "--tx-kib" => config.tx_kib = number()?,
                        "--send-kib" => config.send_kib = number()?,
                        "--record-queue-kib" => config.record_queue_kib = number()?,
                        "--record" => config.record = Some(value.into()),
                        _ => return Err(format!("Unknown option: {arg}")),
                    }
                }
            }
        }
        config.validate()?;
        Ok((config, action))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.serial_config()?;
        self.receive_mode()?;
        self.text_encoding()?;
        self.ending()?;
        for (name, value, min, max) in [
            ("history_kib", self.history_kib, 64, 65536),
            ("history_records", self.history_records, 64, 65536),
            ("display_kib", self.display_kib, 16, 16384),
            ("display_rows", self.display_rows, 32, 100000),
            ("line_kib", self.line_kib, 1, 64),
            ("tx_kib", self.tx_kib, 1, 65536),
            ("send_kib", self.send_kib, 1, 1024),
            ("record_queue_kib", self.record_queue_kib, 8, 65536),
        ] {
            if !(min..=max).contains(&value) {
                return Err(format!("{name} must be {min}..{max}"));
            }
        }
        if self.line_kib > self.display_kib {
            return Err("line_kib must not exceed display_kib".into());
        }
        if self.send_kib > self.tx_kib {
            return Err("send_kib must not exceed tx_kib".into());
        }
        Ok(())
    }

    pub fn receive_mode(&self) -> Result<ReceiveMode, String> {
        match self.mode.as_str() {
            "text" => Ok(ReceiveMode::Text),
            "hex" => Ok(ReceiveMode::Hex),
            "terminal" => Ok(ReceiveMode::Terminal),
            _ => Err("mode must be text, hex or terminal".into()),
        }
    }
    pub fn text_encoding(&self) -> Result<TextEncoding, String> {
        match self.encoding.as_str() {
            "utf8" => Ok(TextEncoding::Utf8),
            "gbk" => Ok(TextEncoding::Gbk),
            _ => Err("encoding must be utf8 or gbk".into()),
        }
    }
    pub fn ending(&self) -> Result<LineEnding, String> {
        match self.line_ending.as_str() {
            "none" => Ok(LineEnding::None),
            "cr" => Ok(LineEnding::Cr),
            "lf" => Ok(LineEnding::Lf),
            "crlf" => Ok(LineEnding::CrLf),
            _ => Err("line_ending must be none, cr, lf or crlf".into()),
        }
    }
    pub fn serial_config(&self) -> Result<SerialConfig, String> {
        Ok(SerialConfig {
            port_name: self.port.clone(),
            baud_rate: parse_baud_rate(&self.baud.to_string()).map_err(str::to_owned)?,
            data_bits: match self.data_bits {
                5 => DataBits::Five,
                6 => DataBits::Six,
                7 => DataBits::Seven,
                8 => DataBits::Eight,
                _ => return Err("data_bits must be 5..8".into()),
            },
            stop_bits: match self.stop_bits {
                1 => StopBits::One,
                2 => StopBits::Two,
                _ => return Err("stop_bits must be 1 or 2".into()),
            },
            parity: match self.parity.as_str() {
                "none" => Parity::None,
                "odd" => Parity::Odd,
                "even" => Parity::Even,
                _ => return Err("parity must be none, odd or even".into()),
            },
            flow_control: match self.flow.as_str() {
                "none" => FlowControl::None,
                "software" => FlowControl::Software,
                "hardware" => FlowControl::Hardware,
                _ => return Err("flow must be none, software or hardware".into()),
            },
            dtr: self.dtr,
            rts: self.rts,
        })
    }
}

pub fn io_error(message: String) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_overrides_file_independent_of_argument_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "baud = 9600\nhistory_kib = 1024").unwrap();
        let (config, _) = Config::parse([
            "--baud".into(),
            "115200".into(),
            "--config".into(),
            path.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(config.baud, 115200);
        assert_eq!(config.history_kib, 1024);
    }
    #[test]
    fn rejects_invalid_and_inconsistent_budgets() {
        for args in [
            ["--history-kib", "0"],
            ["--send-kib", "512"],
            ["--tx-kib", "18446744073709551615"],
            ["--mode", "bad"],
        ] {
            assert!(Config::parse(args.map(str::to_owned)).is_err());
        }
        assert!(Config::parse(["--port".into()]).is_err());
        assert!(Config::default().validate().is_ok());
    }
}
