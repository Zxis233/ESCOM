use crate::i18n::{Key, Language, Message};
use crate::msg;
use crate::persistence::{self, Persistence};
use escom_core::model::{
    LineEnding, ReceiveMode, SendMode, SerialConfig, TextEncoding, parse_baud_rate_typed,
};
use serde::{Deserialize, Serialize};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub language: Language,
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
    pub send_mode: String,
    pub timestamps: bool,
    pub history_kib: usize,
    pub history_records: usize,
    pub display_kib: usize,
    pub display_rows: usize,
    pub line_kib: usize,
    pub tx_kib: usize,
    pub send_kib: usize,
    pub record_queue_kib: usize,
    #[serde(skip_serializing)]
    pub record: Option<PathBuf>,
    #[serde(skip_serializing)]
    pub demo: bool,
    #[serde(skip)]
    pub connect_on_start: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: Language::En,
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
            send_mode: "text".into(),
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
            connect_on_start: false,
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
    ConfigPath,
}

impl Config {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
    ) -> Result<(Self, Action, Persistence), ConfigError> {
        Self::parse_at(args, &persistence::default_path())
    }

    pub fn parse_at(
        args: impl IntoIterator<Item = String>,
        default_path: &Path,
    ) -> Result<(Self, Action, Persistence), ConfigError> {
        let args: Vec<_> = args.into_iter().collect();
        let mut language = Language::En;
        Self::parse_inner(&args, &mut language, default_path)
            .map_err(|message| ConfigError { language, message })
    }

    fn parse_inner(
        args: &[String],
        language: &mut Language,
        default_path: &Path,
    ) -> Result<(Self, Action, Persistence), Message> {
        let mut options = Vec::new();
        let mut iter = args.iter();
        let mut override_language = None;
        while let Some(arg) = iter.next() {
            if matches!(
                arg.as_str(),
                "--demo"
                    | "--connect"
                    | "--config-path"
                    | "--list"
                    | "--print-config"
                    | "--help"
                    | "-h"
                    | "--version"
            ) {
                options.push((arg.as_str(), None));
            } else {
                let value = iter.next().ok_or_else(|| msg!(MissingValue, arg))?;
                if arg == "--lang" {
                    let selected = Language::parse(value)?;
                    override_language = Some(selected);
                    *language = selected;
                }
                options.push((arg.as_str(), Some(value.as_str())));
            }
        }
        let selected = options
            .iter()
            .rev()
            .find(|(arg, _)| *arg == "--config")
            .and_then(|(_, value)| *value)
            .map(PathBuf::from)
            .unwrap_or_else(|| default_path.to_path_buf());
        let path =
            std::path::absolute(&selected).map_err(|e| msg!(ConfigError, selected.display(), e))?;
        let loaded = persistence::read_config(&path)?;
        let mut config = loaded
            .as_ref()
            .map(|(config, _)| config.clone())
            .unwrap_or_default();
        config.language = override_language.unwrap_or(config.language);
        *language = config.language;
        let mut action = Action::Run;
        for (arg, value) in options {
            match arg {
                "--demo" => config.demo = true,
                "--connect" => config.connect_on_start = true,
                "--config-path" => action = Action::ConfigPath,
                "--list" => action = Action::List,
                "--print-config" => action = Action::PrintConfig,
                "--help" | "-h" => action = Action::Help,
                "--version" => action = Action::Version,
                "--config" | "--lang" => {}
                _ => {
                    let value = value.expect("value options have a value");
                    let number = || value.parse::<usize>().map_err(|_| msg!(InvalidNumber, arg));
                    match arg {
                        "--port" => {
                            config.port = value.into();
                            config.connect_on_start = true;
                        }
                        "--baud" => config.baud = parse_baud_rate_typed(value)?,
                        "--mode" => config.mode = value.into(),
                        "--encoding" => config.encoding = value.into(),
                        "--history-kib" => config.history_kib = number()?,
                        "--history-records" => config.history_records = number()?,
                        "--display-kib" => config.display_kib = number()?,
                        "--display-rows" => config.display_rows = number()?,
                        "--line-kib" => config.line_kib = number()?,
                        "--tx-kib" => config.tx_kib = number()?,
                        "--send-kib" => config.send_kib = number()?,
                        "--record-queue-kib" => config.record_queue_kib = number()?,
                        "--record" => config.record = Some(value.into()),
                        _ => return Err(msg!(UnknownOption, arg)),
                    }
                }
            }
        }
        config.validate()?;
        let persistence = Persistence::new(path, loaded, &config);
        Ok((config, action, persistence))
    }

    pub fn validate(&self) -> Result<(), Message> {
        self.serial_config()?;
        self.receive_mode()?;
        self.text_encoding()?;
        self.ending()?;
        self.send_mode()?;
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
                return Err(msg!(Range, name, min, max));
            }
        }
        if self.line_kib > self.display_kib {
            return Err(Key::LineBudget.into());
        }
        if self.send_kib > self.tx_kib {
            return Err(Key::SendBudget.into());
        }
        Ok(())
    }

    pub fn receive_mode(&self) -> Result<ReceiveMode, Message> {
        match self.mode.as_str() {
            "text" => Ok(ReceiveMode::Text),
            "hex" => Ok(ReceiveMode::Hex),
            "terminal" => Ok(ReceiveMode::Terminal),
            _ => Err(Key::ModeValues.into()),
        }
    }
    pub fn text_encoding(&self) -> Result<TextEncoding, Message> {
        match self.encoding.as_str() {
            "utf8" => Ok(TextEncoding::Utf8),
            "gbk" => Ok(TextEncoding::Gbk),
            _ => Err(Key::EncodingValues.into()),
        }
    }
    pub fn ending(&self) -> Result<LineEnding, Message> {
        match self.line_ending.as_str() {
            "none" => Ok(LineEnding::None),
            "cr" => Ok(LineEnding::Cr),
            "lf" => Ok(LineEnding::Lf),
            "crlf" => Ok(LineEnding::CrLf),
            _ => Err(Key::EndingValues.into()),
        }
    }
    pub fn send_mode(&self) -> Result<SendMode, Message> {
        match self.send_mode.as_str() {
            "text" => Ok(SendMode::Text),
            "hex" => Ok(SendMode::Hex),
            _ => Err(Key::SendModeValues.into()),
        }
    }
    pub fn serial_config(&self) -> Result<SerialConfig, Message> {
        Ok(SerialConfig {
            port_name: self.port.clone(),
            baud_rate: parse_baud_rate_typed(&self.baud.to_string())?,
            data_bits: match self.data_bits {
                5 => DataBits::Five,
                6 => DataBits::Six,
                7 => DataBits::Seven,
                8 => DataBits::Eight,
                _ => return Err(Key::DataValues.into()),
            },
            stop_bits: match self.stop_bits {
                1 => StopBits::One,
                2 => StopBits::Two,
                _ => return Err(Key::StopValues.into()),
            },
            parity: match self.parity.as_str() {
                "none" => Parity::None,
                "odd" => Parity::Odd,
                "even" => Parity::Even,
                _ => return Err(Key::ParityValues.into()),
            },
            flow_control: match self.flow.as_str() {
                "none" => FlowControl::None,
                "software" => FlowControl::Software,
                "hardware" => FlowControl::Hardware,
                _ => return Err(Key::FlowValues.into()),
            },
            dtr: self.dtr,
            rts: self.rts,
        })
    }
}

#[derive(Debug)]
pub struct ConfigError {
    pub language: Language,
    pub message: Message,
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message.render(self.language))
    }
}
impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(
        args: impl IntoIterator<Item = String>,
    ) -> Result<(Config, Action, Persistence), ConfigError> {
        let directory = tempfile::tempdir().unwrap();
        Config::parse_at(args, &directory.path().join("tui.toml"))
    }
    #[test]
    fn language_defaults_and_cli_override_are_explicit() {
        assert_eq!(
            parse(Vec::<String>::new()).unwrap().0.language,
            Language::En
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "language = \"zh-CN\"").unwrap();
        let filename = path.to_string_lossy().into_owned();
        let (config, action, _) =
            parse(["--config".into(), filename.clone(), "--help".into()]).unwrap();
        assert_eq!(config.language, Language::ZhCn);
        assert_eq!(action, Action::Help);
        for args in [
            vec![
                "--lang".into(),
                "en".into(),
                "--config".into(),
                filename.clone(),
            ],
            vec![
                "--config".into(),
                filename.clone(),
                "--lang".into(),
                "en".into(),
            ],
        ] {
            assert_eq!(parse(args).unwrap().0.language, Language::En);
        }
        let output = toml::to_string(&config).unwrap();
        assert!(output.contains("language = \"zh-CN\""));
    }

    #[test]
    fn invalid_inputs_use_selected_language() {
        let error = parse(["--lang", "zh-CN", "--baud", "bad"].map(str::to_owned)).unwrap_err();
        assert!(error.to_string().contains("波特率只能包含数字"));
        let error = parse(["--lang", "en", "--baud", "bad"].map(str::to_owned)).unwrap_err();
        assert!(error.to_string().contains("digits only"));
        assert!(parse(["--lang", "fr"].map(str::to_owned)).is_err());
        assert!(parse(["--lang".into()]).is_err());
    }
    #[test]
    fn cli_overrides_file_independent_of_argument_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "baud = 9600\nhistory_kib = 1024").unwrap();
        let (config, _, _) = parse([
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
            assert!(parse(args.map(str::to_owned)).is_err());
        }
        assert!(parse(["--port".into()]).is_err());
        assert!(Config::default().validate().is_ok());
    }
}
