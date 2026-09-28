use crate::i18n::Key;
use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    #[default]
    Classic,
    Pink,
    Midnight,
    Custom,
}

impl Preset {
    pub fn next(self) -> Self {
        match self {
            Self::Classic => Self::Pink,
            Self::Pink => Self::Midnight,
            Self::Midnight => Self::Custom,
            Self::Custom => Self::Classic,
        }
    }

    pub fn label(self) -> Key {
        match self {
            Self::Classic => Key::ThemeClassic,
            Self::Pink => Key::ThemePink,
            Self::Midnight => Key::ThemeMidnight,
            Self::Custom => Key::ThemeCustom,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub preset: Preset,
    pub colors: ColorOverrides,
}

// Keep configurable roles, resolved roles and override application in sync.
macro_rules! color_roles {
    ($($role:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct ColorOverrides {
            $(#[serde(skip_serializing_if = "Option::is_none")]
            pub $role: Option<ColorValue>,)+
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Palette { $(pub $role: Color,)+ }

        impl ColorOverrides {
            fn apply(&self, palette: &mut Palette) {
                $(if let Some(value) = &self.$role { palette.$role = value.color; })+
            }
        }
    };
}

color_roles! {
    background, foreground, muted, accent, border, inactive_border,
    button_background, header_foreground, editor_background, popup_background,
    selection_background, selection_foreground, search_foreground,
    search_selected_background, search_selected_foreground, success, warning, error,
}

impl Palette {
    pub fn base(self) -> Style {
        Style::default().fg(self.foreground).bg(self.background)
    }

    pub fn popup(self) -> Style {
        Style::default()
            .fg(self.foreground)
            .bg(self.popup_background)
    }

    pub fn button(self) -> Style {
        Style::default().fg(self.accent).bg(self.button_background)
    }
}

impl Theme {
    pub fn palette(&self) -> Palette {
        let mut palette = match self.preset {
            Preset::Classic => Palette {
                background: Color::Reset,
                foreground: Color::Reset,
                muted: Color::DarkGray,
                accent: Color::Cyan,
                border: Color::Cyan,
                inactive_border: Color::DarkGray,
                button_background: Color::DarkGray,
                header_foreground: Color::Black,
                editor_background: Color::Reset,
                popup_background: Color::Reset,
                selection_background: Color::Cyan,
                selection_foreground: Color::Black,
                search_foreground: Color::Yellow,
                search_selected_background: Color::Yellow,
                search_selected_foreground: Color::Black,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
            },
            Preset::Pink | Preset::Custom => Palette {
                background: rgb(0x1f1722),
                foreground: rgb(0xf8edf3),
                muted: rgb(0xbca5b5),
                accent: rgb(0xff80ac),
                border: rgb(0xb86586),
                inactive_border: rgb(0x77576c),
                button_background: rgb(0x382636),
                header_foreground: rgb(0x281521),
                editor_background: rgb(0x281d2b),
                popup_background: rgb(0x302230),
                selection_background: rgb(0xff80ac),
                selection_foreground: rgb(0x281521),
                search_foreground: rgb(0xffd580),
                search_selected_background: rgb(0xffd580),
                search_selected_foreground: rgb(0x281521),
                success: rgb(0xa6d9b4),
                warning: rgb(0xffd580),
                error: rgb(0xff8b94),
            },
            Preset::Midnight => Palette {
                background: rgb(0x101a2b),
                foreground: rgb(0xe2eaf4),
                muted: rgb(0x99adc6),
                accent: rgb(0x82b8ff),
                border: rgb(0x4f7faf),
                inactive_border: rgb(0x4d607c),
                button_background: rgb(0x20334d),
                header_foreground: rgb(0x101a2b),
                editor_background: rgb(0x15233a),
                popup_background: rgb(0x1b2b43),
                selection_background: rgb(0xe9bd69),
                selection_foreground: rgb(0x101a2b),
                search_foreground: rgb(0xe9bd69),
                search_selected_background: rgb(0xe9bd69),
                search_selected_foreground: rgb(0x101a2b),
                success: rgb(0x90c9ac),
                warning: rgb(0xe9bd69),
                error: rgb(0xf08f90),
            },
        };
        if self.preset == Preset::Custom {
            self.colors.apply(&mut palette);
        }
        palette
    }
}

const fn rgb(value: u32) -> Color {
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// Validated at deserialization; preserve the user's spelling on autosave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ColorValue {
    text: String,
    color: Color,
}

impl TryFrom<String> for ColorValue {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let color = match text.as_str() {
            "default" => Color::Reset,
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" => Color::Gray,
            "dark_gray" => Color::DarkGray,
            "light_red" => Color::LightRed,
            "light_green" => Color::LightGreen,
            "light_yellow" => Color::LightYellow,
            "light_blue" => Color::LightBlue,
            "light_magenta" => Color::LightMagenta,
            "light_cyan" => Color::LightCyan,
            "white" => Color::White,
            _ => {
                let hex = text.strip_prefix('#').filter(|hex| {
                    hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
                let value = hex.and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| format!("invalid theme color {text:?}: expected #RRGGBB, default, or an ANSI color name"))?;
                rgb(value)
            }
        };
        Ok(Self { text, color })
    }
}

impl From<ColorValue> for String {
    fn from(value: ColorValue) -> Self {
        value.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn old_configs_keep_classic_and_partial_overrides_round_trip() {
        let old: Config = toml::from_str("baud = 9600").unwrap();
        assert_eq!(old.theme.preset, Preset::Classic);
        assert_eq!(old.theme.palette().accent, Color::Cyan);
        let custom: Config = toml::from_str(
            r##"
[theme]
preset = "custom"
[theme.colors]
accent = "#A1b2C3"
background = "default"
error = "light_red"
"##,
        )
        .unwrap();
        assert_eq!(custom.theme.palette().accent, Color::Rgb(161, 178, 195));
        assert_eq!(custom.theme.palette().background, Color::Reset);
        assert_eq!(custom.theme.palette().error, Color::LightRed);
        assert_eq!(custom.theme.palette().foreground, rgb(0xf8edf3));
        let saved = toml::to_string_pretty(&custom).unwrap();
        assert!(saved.contains("#A1b2C3"));
        assert_eq!(toml::from_str::<Config>(&saved).unwrap(), custom);
    }

    #[test]
    fn custom_colors_do_not_override_presets() {
        let mut theme = Theme::default();
        theme.colors.accent = Some("#123456".to_owned().try_into().unwrap());
        for preset in [Preset::Classic, Preset::Pink, Preset::Midnight] {
            theme.preset = preset;
            assert_eq!(
                theme.palette(),
                Theme {
                    preset,
                    ..Theme::default()
                }
                .palette()
            );
        }
        theme.preset = Preset::Custom;
        assert_eq!(theme.palette().accent, rgb(0x123456));
        assert_eq!(theme.palette().background, rgb(0x1f1722));
        assert_eq!(Preset::Classic.next().next().next(), Preset::Custom);
        assert_eq!(Preset::Custom.next(), Preset::Classic);
    }

    #[test]
    fn malformed_theme_settings_are_rejected() {
        for source in [
            "[theme]\npreset = 'unknown'",
            "[theme]\nunknown = 'pink'",
            "[theme.colors]\nunknown = '#ff80ac'",
            "[theme.colors]\naccent = '#fff'",
            "[theme.colors]\naccent = '#gg80ac'",
            "[theme.colors]\naccent = '#粉红'",
            "[theme.colors]\naccent = '#ff80ac00'",
            "[theme.colors]\naccent = 123",
        ] {
            assert!(toml::from_str::<Config>(source).is_err(), "{source}");
        }
    }
}
