mod catalog;
mod errors;
pub use catalog::Key;
use escom_core::error::{CoreError, ErrorKind};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, fmt};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Language {
    pub fn parse(value: &str) -> Result<Self, Message> {
        match value {
            "en" => Ok(Self::En),
            "zh-CN" => Ok(Self::ZhCn),
            _ => Err(Message::with(Key::InvalidLanguage, [value.to_owned()])),
        }
    }
    pub const fn text(self, key: Key) -> &'static str {
        key.text(self)
    }
    pub fn format(self, key: Key, args: &[String]) -> String {
        interpolate(key.text(self), args)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Context(Key, Box<Message>),
    Localized(Key, Vec<String>),
    Core(CoreError),
    Raw(String),
    SearchResults { count: usize, capped: bool },
    TxFailed { id: u64, error: CoreError },
}
impl Message {
    pub fn with(key: Key, args: impl IntoIterator<Item = String>) -> Self {
        Self::Localized(key, args.into_iter().collect())
    }
    pub fn render(&self, language: Language) -> Cow<'_, str> {
        match self {
            Self::Context(key, detail) => {
                Cow::Owned(language.format(*key, &[detail.render(language).into_owned()]))
            }
            Self::Localized(key, args) if args.is_empty() => Cow::Borrowed(language.text(*key)),
            Self::Localized(key, args) => Cow::Owned(language.format(*key, args)),
            Self::Core(error) => Cow::Owned(errors::render(error, language)),
            Self::Raw(text) => Cow::Borrowed(text),
            Self::TxFailed { id, error } => Cow::Owned(language.format(
                Key::TxFailed,
                &[id.to_string(), errors::render(error, language)],
            )),
            Self::SearchResults { count, capped } => Cow::Owned(language.format(
                Key::SearchResults,
                &[
                    count.to_string(),
                    if *capped {
                        language.text(Key::Capped).into()
                    } else {
                        String::new()
                    },
                ],
            )),
        }
    }
}
impl From<Key> for Message {
    fn from(key: Key) -> Self {
        Self::Localized(key, Vec::new())
    }
}
impl From<CoreError> for Message {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}
impl From<ErrorKind> for Message {
    fn from(error: ErrorKind) -> Self {
        Self::Core(error.into())
    }
}
impl From<std::io::Error> for Message {
    fn from(error: std::io::Error) -> Self {
        if let Some(core) = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<CoreError>())
        {
            return Self::Core(core.clone());
        }
        Self::Raw(error.to_string())
    }
}
impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(Language::En))
    }
}
impl std::error::Error for Message {}

/// Interpolate only the template; values containing braces are literal data.
fn interpolate(template: &str, args: &[String]) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let token = &rest[start + 1..];
        if let Some(end) = token.find('}')
            && let Ok(index) = token[..end].parse::<usize>()
            && let Some(value) = args.get(index)
        {
            result.push_str(value);
            rest = &token[end + 1..];
        } else {
            result.push('{');
            rest = token;
        }
    }
    result.push_str(rest);
    result
}

#[macro_export]
macro_rules! msg {
    ($key:ident $(, $arg:expr)* $(,)?) => {
        $crate::i18n::Message::with($crate::i18n::Key::$key, vec![$($arg.to_string()),*])
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_errors_keep_parameters_and_survive_io_wrapping() {
        let core = CoreError::Cancelled {
            id: 7,
            written: 2,
            total: 8,
        };
        let message = Message::from(std::io::Error::other(core));
        assert_eq!(
            message.render(Language::En),
            "Send #7 cancelled: 2/8 bytes written"
        );
        assert_eq!(
            message.render(Language::ZhCn),
            "发送 #7 已取消：已写入 2/8 字节"
        );
        let message = Message::from(CoreError::Open {
            port: "设备{1}".into(),
            detail: "driver detail".into(),
        });
        assert_eq!(
            message.render(Language::En),
            "Unable to open 设备{1}: driver detail"
        );
    }
    #[test]
    fn every_language_has_matching_parameters() {
        fn parameters(text: &str) -> Vec<usize> {
            let mut values: Vec<_> = text
                .split('{')
                .skip(1)
                .filter_map(|s| s.split_once('}').and_then(|(s, _)| s.parse::<usize>().ok()))
                .collect();
            values.sort();
            values
        }
        for key in Key::ALL {
            assert_eq!(
                parameters(key.text(Language::En)),
                parameters(key.text(Language::ZhCn)),
                "{key:?}"
            );
        }
    }
    #[test]
    fn switching_rerenders_messages_without_touching_parameters() {
        let message = msg!(SettingChanged, "port", "设备{0}{1}");
        assert_eq!(message.render(Language::En), "Set port = 设备{0}{1}");
        assert_eq!(message.render(Language::ZhCn), "已设置 port = 设备{0}{1}");
    }
}
