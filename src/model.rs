use serde::{Deserialize, Serialize};
use serialport::{DataBits, FlowControl, Parity, StopBits};

pub const MIN_BAUD_RATE: u32 = 1;
pub const MAX_BAUD_RATE: u32 = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiveMode {
    Text,
    Hex,
    Terminal,
}

impl ReceiveMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Hex => "HEX",
            Self::Terminal => "终端",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
    Text,
    Hex,
}

impl SendMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Hex => "HEX",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextEncoding {
    Utf8,
    Gbk,
}

impl TextEncoding {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gbk => "GBK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    None,
    Cr,
    Lf,
    CrLf,
}

impl LineEnding {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "无",
            Self::Cr => "CR",
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }

    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::None => b"",
            Self::Cr => b"\r",
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialConfig {
    pub port_name: String,
    pub baud_rate: u32,
    pub data_bits: DataBits,
    pub stop_bits: StopBits,
    pub parity: Parity,
    pub flow_control: FlowControl,
    pub dtr: bool,
    pub rts: bool,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port_name: String::new(),
            baud_rate: 115_200,
            data_bits: DataBits::Eight,
            stop_bits: StopBits::One,
            parity: Parity::None,
            flow_control: FlowControl::None,
            dtr: false,
            rts: false,
        }
    }
}

impl SerialConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.port_name.trim().is_empty() {
            return Err("请选择串口");
        }
        if !(MIN_BAUD_RATE..=MAX_BAUD_RATE).contains(&self.baud_rate) {
            return Err("波特率必须在 1 到 4,000,000 之间");
        }
        Ok(())
    }
}

pub fn parse_baud_rate(input: &str) -> Result<u32, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Err("请输入波特率");
    }
    if !input.chars().all(|character| character.is_ascii_digit()) {
        return Err("波特率只能包含数字");
    }
    let value = input
        .parse::<u32>()
        .map_err(|_| "波特率必须在 1 到 4,000,000 之间")?;
    if !(MIN_BAUD_RATE..=MAX_BAUD_RATE).contains(&value) {
        return Err("波特率必须在 1 到 4,000,000 之间");
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    pub mode: SendMode,
    pub input: String,
}

pub fn data_bits_label(value: DataBits) -> &'static str {
    match value {
        DataBits::Five => "5",
        DataBits::Six => "6",
        DataBits::Seven => "7",
        DataBits::Eight => "8",
    }
}

pub fn stop_bits_label(value: StopBits) -> &'static str {
    match value {
        StopBits::One => "1",
        StopBits::Two => "2",
    }
}

pub fn parity_label(value: Parity) -> &'static str {
    match value {
        Parity::None => "无校验",
        Parity::Odd => "奇校验",
        Parity::Even => "偶校验",
    }
}

pub fn flow_control_label(value: FlowControl) -> &'static str {
    match value {
        FlowControl::None => "无流控",
        FlowControl::Software => "软件",
        FlowControl::Hardware => "硬件",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_config_validation_rejects_missing_port_and_bad_baud() {
        let mut config = SerialConfig::default();
        assert!(config.validate().is_err());
        config.port_name = "COM5".into();
        assert!(config.validate().is_ok());
        config.baud_rate = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn baud_rate_parser_accepts_common_and_custom_values() {
        assert_eq!(parse_baud_rate("115200"), Ok(115_200));
        assert_eq!(parse_baud_rate(" 1234567 "), Ok(1_234_567));
        assert_eq!(parse_baud_rate("4000000"), Ok(MAX_BAUD_RATE));
    }

    #[test]
    fn baud_rate_parser_rejects_invalid_values() {
        assert_eq!(parse_baud_rate(""), Err("请输入波特率"));
        assert_eq!(parse_baud_rate("115.2k"), Err("波特率只能包含数字"));
        assert!(parse_baud_rate("0").is_err());
        assert!(parse_baud_rate("4000001").is_err());
        assert!(parse_baud_rate("999999999999").is_err());
    }
}
