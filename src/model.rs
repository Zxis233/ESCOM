use serde::{Deserialize, Serialize};
use serialport::{DataBits, FlowControl, Parity, StopBits};

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
        if !(1..=4_000_000).contains(&self.baud_rate) {
            return Err("波特率必须在 1 到 4,000,000 之间");
        }
        Ok(())
    }
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
}
