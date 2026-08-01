use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use eframe::egui::ThemePreference;
use serde::{Deserialize, Serialize};

use crate::fonts::DEFAULT_FONT_WEIGHT;
use crate::model::{LineEnding, ReceiveMode, SendMode, TextEncoding};

pub const SETTINGS_SCHEMA_VERSION: u32 = 6;
pub const SETTINGS_FILE_NAME: &str = "settings.toml";
const LEGACY_SETTINGS_FILE_NAME: &str = "settings.json";
pub const BUFFER_LIMIT_OPTIONS_MIB: [usize; 4] = [5, 20, 100, 500];
pub const DEFAULT_DATA_LINE_SPACING: f32 = 3.0;
pub const MIN_DATA_LINE_SPACING: f32 = 0.0;
pub const MAX_DATA_LINE_SPACING: f32 = 24.0;
pub const MIN_FONT_SIZE: f32 = 10.0;
pub const MAX_UI_FONT_SIZE: f32 = 18.0;
pub const MAX_DATA_FONT_SIZE: f32 = 48.0;
pub const DEFAULT_BACKGROUND_LIGHT_OPACITY: f32 = 0.22;
pub const DEFAULT_BACKGROUND_DARK_OPACITY: f32 = 0.16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppBackgroundSource {
    #[default]
    None,
    Local,
    Online,
}

impl AppBackgroundSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "不使用背景",
            Self::Local => "本地图片",
            Self::Online => "在线图片",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPreferences {
    pub schema_version: u32,
    pub ui_font_family: String,
    pub data_font_family: String,
    pub ui_font_weight: u16,
    pub data_font_weight: u16,
    pub ui_font_size: f32,
    pub data_font_size: f32,
    pub data_line_spacing: f32,
    pub receive_mode: ReceiveMode,
    pub send_mode: SendMode,
    pub text_encoding: TextEncoding,
    pub timestamps: bool,
    pub auto_scroll: bool,
    pub line_ending: LineEnding,
    pub repeat_interval_ms: u64,
    pub buffer_limit_mib: usize,
    pub theme_preference: ThemePreference,
    pub background_source: AppBackgroundSource,
    pub background_local_path: String,
    pub background_online_url: String,
    pub background_light_opacity: f32,
    pub background_dark_opacity: f32,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            ui_font_family: String::new(),
            data_font_family: String::new(),
            ui_font_weight: DEFAULT_FONT_WEIGHT,
            data_font_weight: DEFAULT_FONT_WEIGHT,
            ui_font_size: 15.0,
            data_font_size: 15.0,
            data_line_spacing: DEFAULT_DATA_LINE_SPACING,
            receive_mode: ReceiveMode::Text,
            send_mode: SendMode::Text,
            text_encoding: TextEncoding::Utf8,
            timestamps: false,
            auto_scroll: true,
            line_ending: LineEnding::CrLf,
            repeat_interval_ms: 1_000,
            buffer_limit_mib: 20,
            theme_preference: ThemePreference::System,
            background_source: AppBackgroundSource::None,
            background_local_path: String::new(),
            background_online_url: String::new(),
            background_light_opacity: DEFAULT_BACKGROUND_LIGHT_OPACITY,
            background_dark_opacity: DEFAULT_BACKGROUND_DARK_OPACITY,
        }
    }
}

impl UiPreferences {
    pub fn sanitize(&mut self) {
        self.schema_version = SETTINGS_SCHEMA_VERSION;
        self.ui_font_size = self.ui_font_size.clamp(MIN_FONT_SIZE, MAX_UI_FONT_SIZE);
        self.data_font_size = self.data_font_size.clamp(MIN_FONT_SIZE, MAX_DATA_FONT_SIZE);
        self.data_line_spacing = if self.data_line_spacing.is_finite() {
            self.data_line_spacing
                .clamp(MIN_DATA_LINE_SPACING, MAX_DATA_LINE_SPACING)
        } else {
            DEFAULT_DATA_LINE_SPACING
        };
        if !(1..=1000).contains(&self.ui_font_weight) {
            self.ui_font_weight = DEFAULT_FONT_WEIGHT;
        }
        if !(1..=1000).contains(&self.data_font_weight) {
            self.data_font_weight = DEFAULT_FONT_WEIGHT;
        }
        self.repeat_interval_ms = self.repeat_interval_ms.clamp(20, 3_600_000);
        if !BUFFER_LIMIT_OPTIONS_MIB.contains(&self.buffer_limit_mib) {
            self.buffer_limit_mib = 20;
        }
        self.background_light_opacity = sanitized_opacity(
            self.background_light_opacity,
            DEFAULT_BACKGROUND_LIGHT_OPACITY,
        );
        self.background_dark_opacity = sanitized_opacity(
            self.background_dark_opacity,
            DEFAULT_BACKGROUND_DARK_OPACITY,
        );
    }

    pub fn buffer_limit_bytes(&self) -> usize {
        self.buffer_limit_mib.saturating_mul(1024 * 1024)
    }
}

fn sanitized_opacity(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsDocument {
    schema_version: u32,
    interface: InterfaceSettings,
    fonts: FontSettings,
    receive: ReceiveSettings,
    send: SendSettings,
    background: BackgroundSettings,
}

impl Default for SettingsDocument {
    fn default() -> Self {
        Self::from(&UiPreferences::default())
    }
}

impl From<&UiPreferences> for SettingsDocument {
    fn from(preferences: &UiPreferences) -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            interface: InterfaceSettings {
                theme: preferences.theme_preference.into(),
            },
            fonts: FontSettings {
                ui_family: preferences.ui_font_family.clone(),
                data_family: preferences.data_font_family.clone(),
                ui_weight: preferences.ui_font_weight,
                data_weight: preferences.data_font_weight,
                ui_size: preferences.ui_font_size,
                data_size: preferences.data_font_size,
                data_line_spacing: preferences.data_line_spacing,
            },
            receive: ReceiveSettings {
                mode: preferences.receive_mode.into(),
                encoding: preferences.text_encoding.into(),
                timestamps: preferences.timestamps,
                auto_scroll: preferences.auto_scroll,
                buffer_limit_mib: preferences.buffer_limit_mib,
            },
            send: SendSettings {
                mode: preferences.send_mode.into(),
                line_ending: preferences.line_ending.into(),
                repeat_interval_ms: preferences.repeat_interval_ms,
            },
            background: BackgroundSettings {
                source: preferences.background_source.into(),
                local_path: preferences.background_local_path.clone(),
                online_url: preferences.background_online_url.clone(),
                light_opacity: preferences.background_light_opacity,
                dark_opacity: preferences.background_dark_opacity,
            },
        }
    }
}

impl From<SettingsDocument> for UiPreferences {
    fn from(settings: SettingsDocument) -> Self {
        Self {
            schema_version: settings.schema_version,
            ui_font_family: settings.fonts.ui_family,
            data_font_family: settings.fonts.data_family,
            ui_font_weight: settings.fonts.ui_weight,
            data_font_weight: settings.fonts.data_weight,
            ui_font_size: settings.fonts.ui_size,
            data_font_size: settings.fonts.data_size,
            data_line_spacing: settings.fonts.data_line_spacing,
            receive_mode: settings.receive.mode.into(),
            send_mode: settings.send.mode.into(),
            text_encoding: settings.receive.encoding.into(),
            timestamps: settings.receive.timestamps,
            auto_scroll: settings.receive.auto_scroll,
            line_ending: settings.send.line_ending.into(),
            repeat_interval_ms: settings.send.repeat_interval_ms,
            buffer_limit_mib: settings.receive.buffer_limit_mib,
            theme_preference: settings.interface.theme.into(),
            background_source: settings.background.source.into(),
            background_local_path: settings.background.local_path,
            background_online_url: settings.background.online_url,
            background_light_opacity: settings.background.light_opacity,
            background_dark_opacity: settings.background.dark_opacity,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct InterfaceSettings {
    theme: ThemeValue,
}

impl Default for InterfaceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeValue::System,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FontSettings {
    ui_family: String,
    data_family: String,
    ui_weight: u16,
    data_weight: u16,
    ui_size: f32,
    data_size: f32,
    data_line_spacing: f32,
}

impl Default for FontSettings {
    fn default() -> Self {
        let preferences = UiPreferences::default();
        Self {
            ui_family: preferences.ui_font_family,
            data_family: preferences.data_font_family,
            ui_weight: preferences.ui_font_weight,
            data_weight: preferences.data_font_weight,
            ui_size: preferences.ui_font_size,
            data_size: preferences.data_font_size,
            data_line_spacing: preferences.data_line_spacing,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReceiveSettings {
    mode: ReceiveModeValue,
    encoding: TextEncodingValue,
    timestamps: bool,
    auto_scroll: bool,
    buffer_limit_mib: usize,
}

impl Default for ReceiveSettings {
    fn default() -> Self {
        let preferences = UiPreferences::default();
        Self {
            mode: preferences.receive_mode.into(),
            encoding: preferences.text_encoding.into(),
            timestamps: preferences.timestamps,
            auto_scroll: preferences.auto_scroll,
            buffer_limit_mib: preferences.buffer_limit_mib,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SendSettings {
    mode: SendModeValue,
    line_ending: LineEndingValue,
    repeat_interval_ms: u64,
}

impl Default for SendSettings {
    fn default() -> Self {
        let preferences = UiPreferences::default();
        Self {
            mode: preferences.send_mode.into(),
            line_ending: preferences.line_ending.into(),
            repeat_interval_ms: preferences.repeat_interval_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackgroundSettings {
    source: BackgroundSourceValue,
    local_path: String,
    online_url: String,
    light_opacity: f32,
    dark_opacity: f32,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        let preferences = UiPreferences::default();
        Self {
            source: preferences.background_source.into(),
            local_path: preferences.background_local_path,
            online_url: preferences.background_online_url,
            light_opacity: preferences.background_light_opacity,
            dark_opacity: preferences.background_dark_opacity,
        }
    }
}

macro_rules! setting_value {
    ($name:ident, $runtime:ty, $default:ident, { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum $name {
            #[default]
            $default,
            $($variant,)+
        }

        impl From<$runtime> for $name {
            fn from(value: $runtime) -> Self {
                match value {
                    <$runtime>::$default => Self::$default,
                    $(<$runtime>::$variant => Self::$variant,)+
                }
            }
        }

        impl From<$name> for $runtime {
            fn from(value: $name) -> Self {
                match value {
                    $name::$default => Self::$default,
                    $($name::$variant => Self::$variant,)+
                }
            }
        }
    };
}

setting_value!(ThemeValue, ThemePreference, System, { Light, Dark });
setting_value!(ReceiveModeValue, ReceiveMode, Text, { Hex, Terminal });
setting_value!(SendModeValue, SendMode, Text, { Hex });
setting_value!(TextEncodingValue, TextEncoding, Utf8, { Gbk });
setting_value!(LineEndingValue, LineEnding, CrLf, { None, Cr, Lf });
setting_value!(BackgroundSourceValue, AppBackgroundSource, None, {
    Local,
    Online
});

pub fn settings_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("ESCOM"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    settings_dir().join(SETTINGS_FILE_NAME)
}

fn legacy_settings_path() -> PathBuf {
    settings_dir().join(LEGACY_SETTINGS_FILE_NAME)
}

pub fn eframe_persistence_path() -> PathBuf {
    settings_dir().join("window.ron")
}

pub fn prepare_storage() -> io::Result<()> {
    prepare_storage_at(&settings_dir())
}

pub fn load() -> (UiPreferences, Option<String>) {
    if let Err(error) = prepare_storage() {
        return (
            UiPreferences::default(),
            Some(format!("初始化配置目录失败：{error}")),
        );
    }
    load_at(&settings_path(), &legacy_settings_path())
}

pub fn save(preferences: &UiPreferences) -> Result<(), String> {
    prepare_storage().map_err(|error| format!("初始化配置目录失败：{error}"))?;
    save_to(&settings_path(), preferences).map_err(|error| format!("保存设置失败：{error}"))
}

fn prepare_storage_at(directory: &Path) -> io::Result<()> {
    let migration_path = directory.with_extension("migration.tmp");
    let window_state_path = directory.join("window.ron");

    if directory.is_dir() {
        recover_staged_window_state(&migration_path, &window_state_path)?;
        return Ok(());
    }

    if !directory.exists() && migration_path.is_file() {
        fs::create_dir_all(directory)?;
        fs::rename(&migration_path, &window_state_path)?;
        return Ok(());
    }

    if directory.is_file() {
        if migration_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "迁移文件 {} 已存在，请先确认其内容",
                    migration_path.display()
                ),
            ));
        }

        fs::rename(directory, &migration_path)?;
        if let Err(error) = fs::create_dir_all(directory) {
            let _ = fs::rename(&migration_path, directory);
            return Err(error);
        }
        if let Err(error) = fs::rename(&migration_path, &window_state_path) {
            let _ = fs::remove_dir(directory);
            let _ = fs::rename(&migration_path, directory);
            return Err(error);
        }
        return Ok(());
    }

    fs::create_dir_all(directory)
}

fn recover_staged_window_state(migration_path: &Path, window_state_path: &Path) -> io::Result<()> {
    if migration_path.is_file() && !window_state_path.exists() {
        fs::rename(migration_path, window_state_path)?;
    }
    Ok(())
}

fn load_at(path: &Path, legacy_path: &Path) -> (UiPreferences, Option<String>) {
    let backup_path = path.with_extension("toml.bak");
    let mut primary_error = None;

    if path.is_file() {
        match load_toml_from(path) {
            Ok(preferences) => return (preferences, None),
            Err(error) => primary_error = Some(error),
        }
    }

    if backup_path.is_file() {
        match load_toml_from(&backup_path) {
            Ok(preferences) => {
                return (
                    preferences,
                    Some(format!(
                        "主配置无法读取，已使用备份 {}：{}",
                        backup_path.display(),
                        primary_error.unwrap_or_else(|| "主配置文件不存在".to_owned())
                    )),
                );
            }
            Err(backup_error) => {
                if let Some(primary_error) = primary_error {
                    return (
                        UiPreferences::default(),
                        Some(format!(
                            "配置文件和备份均无法读取：{primary_error}；{backup_error}"
                        )),
                    );
                }
                return (UiPreferences::default(), Some(backup_error));
            }
        }
    }

    if let Some(error) = primary_error {
        return (UiPreferences::default(), Some(error));
    }

    match load_legacy_json(legacy_path) {
        Ok(Some((preferences, source_path))) => {
            let warning = match save_to(path, &preferences) {
                Ok(()) => archive_legacy_json(&source_path, legacy_path).err(),
                Err(error) => Some(format!(
                    "已读取旧版 JSON 配置，但无法写入 {}：{error}",
                    path.display()
                )),
            };
            (preferences, warning)
        }
        Ok(None) => {
            let preferences = UiPreferences::default();
            let warning = save_to(path, &preferences)
                .err()
                .map(|error| format!("无法创建默认配置 {}：{error}", path.display()));
            (preferences, warning)
        }
        Err(error) => (UiPreferences::default(), Some(error)),
    }
}

fn load_toml_from(path: &Path) -> Result<UiPreferences, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("无法读取配置 {}：{error}", path.display()))?;
    let document: SettingsDocument =
        toml::from_str(source.strip_prefix('\u{feff}').unwrap_or(&source))
            .map_err(|error| format!("配置 {} 的 TOML 格式无效：{error}", path.display()))?;
    if document.schema_version != SETTINGS_SCHEMA_VERSION {
        return Err(format!(
            "不支持配置 {} 的 schema_version {}，当前支持版本 {}",
            path.display(),
            document.schema_version,
            SETTINGS_SCHEMA_VERSION
        ));
    }
    let mut preferences = UiPreferences::from(document);
    preferences.sanitize();
    Ok(preferences)
}

fn load_legacy_json(path: &Path) -> Result<Option<(UiPreferences, PathBuf)>, String> {
    let backup_path = path.with_extension("json.bak");
    let mut errors = Vec::new();

    for candidate in [path, backup_path.as_path()] {
        if !candidate.is_file() {
            continue;
        }
        match fs::read(candidate) {
            Ok(bytes) => match serde_json::from_slice::<UiPreferences>(&bytes) {
                Ok(mut preferences) => {
                    preferences.sanitize();
                    return Ok(Some((preferences, candidate.to_path_buf())));
                }
                Err(error) => errors.push(format!("{}：{error}", candidate.display())),
            },
            Err(error) => errors.push(format!("{}：{error}", candidate.display())),
        }
    }

    if errors.is_empty() {
        Ok(None)
    } else {
        Err(format!("旧版 JSON 配置无法读取：{}", errors.join("；")))
    }
}

fn archive_legacy_json(source_path: &Path, legacy_path: &Path) -> Result<(), String> {
    let archive_path = legacy_path.with_extension("json.migrated.bak");
    if archive_path.exists() {
        return Err(format!(
            "TOML 配置已创建，但旧版配置未归档：{} 已存在",
            archive_path.display()
        ));
    }
    fs::rename(source_path, &archive_path).map_err(|error| {
        format!(
            "TOML 配置已创建，但无法将旧版配置归档到 {}：{error}",
            archive_path.display()
        )
    })
}

fn save_to(path: &Path, preferences: &UiPreferences) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("toml.tmp");
    let backup_path = path.with_extension("toml.bak");
    let source = render_toml(preferences).map_err(io::Error::other)?;
    fs::write(&temp_path, source.as_bytes())?;

    let _ = fs::remove_file(&backup_path);
    if path.exists() {
        fs::rename(path, &backup_path)?;
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::rename(&backup_path, path);
        return Err(error);
    }
    let _ = fs::remove_file(backup_path);
    Ok(())
}

fn render_toml(preferences: &UiPreferences) -> Result<String, toml::ser::Error> {
    let source = toml::to_string_pretty(&SettingsDocument::from(preferences))?;
    let mut annotated = String::with_capacity(source.len() + 400);
    for line in source.lines() {
        let comment = match line {
            "[interface]" => Some("# theme: system、light 或 dark"),
            "[fonts]" => Some("# family 留空表示自动选择；weight 范围 1-1000"),
            "[receive]" => Some(
                "# mode: text、hex 或 terminal；encoding: utf8 或 gbk\n# buffer_limit_mib: 5、20、100 或 500",
            ),
            "[send]" => Some("# mode: text 或 hex；line_ending: none、cr、lf 或 crlf"),
            "[background]" => Some("# source: none、local 或 online；opacity 范围 0.0-1.0"),
            _ => None,
        };
        if let Some(comment) = comment {
            annotated.push_str(comment);
            annotated.push('\n');
        }
        annotated.push_str(line);
        annotated.push('\n');
    }
    Ok(format!(
        "# ESCOM 用户配置\n# 修改后请重启应用；缺失字段会使用默认值。\n\n{annotated}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_only_contains_ui_state() {
        let test_dir = unique_test_dir("toml-round-trip");
        let test_path = test_dir.join(SETTINGS_FILE_NAME);
        let preferences = UiPreferences {
            ui_font_family: "Microsoft YaHei UI".into(),
            ui_font_weight: 500,
            data_line_spacing: 8.0,
            receive_mode: ReceiveMode::Terminal,
            send_mode: SendMode::Hex,
            timestamps: true,
            background_local_path: r"C:\images\[send]\background.png".into(),
            ..Default::default()
        };

        save_to(&test_path, &preferences).unwrap();
        let loaded = load_toml_from(&test_path).unwrap();
        assert_eq!(loaded.ui_font_family, "Microsoft YaHei UI");
        assert_eq!(loaded.ui_font_weight, 500);
        assert_eq!(loaded.data_line_spacing, 8.0);
        assert_eq!(loaded.receive_mode, ReceiveMode::Terminal);
        assert_eq!(loaded.send_mode, SendMode::Hex);
        assert_eq!(
            loaded.background_local_path,
            r"C:\images\[send]\background.png"
        );
        assert!(loaded.timestamps);
        assert_eq!(loaded.theme_preference, ThemePreference::System);

        let toml = fs::read_to_string(&test_path).unwrap();
        for section in [
            "[interface]",
            "[fonts]",
            "[receive]",
            "[send]",
            "[background]",
        ] {
            assert!(toml.contains(section), "missing {section}");
        }
        assert!(toml.contains("mode = \"terminal\""));
        assert!(toml.contains("mode = \"hex\""));
        assert!(!toml.contains("port_name"));
        assert!(!toml.contains("send_history"));
        let _ = fs::remove_dir_all(test_dir);
    }

    #[test]
    fn sanitize_restores_supported_limits() {
        let mut preferences = UiPreferences {
            ui_font_size: 100.0,
            data_font_size: 100.0,
            data_line_spacing: 100.0,
            ui_font_weight: 0,
            data_font_weight: 1001,
            repeat_interval_ms: 1,
            buffer_limit_mib: 7,
            ..Default::default()
        };
        preferences.sanitize();
        assert_eq!(preferences.ui_font_size, MAX_UI_FONT_SIZE);
        assert_eq!(preferences.data_font_size, MAX_DATA_FONT_SIZE);
        assert_eq!(preferences.data_line_spacing, MAX_DATA_LINE_SPACING);
        assert_eq!(preferences.ui_font_weight, DEFAULT_FONT_WEIGHT);
        assert_eq!(preferences.data_font_weight, DEFAULT_FONT_WEIGHT);
        assert_eq!(preferences.repeat_interval_ms, 20);
        assert_eq!(preferences.buffer_limit_mib, 20);
    }

    #[test]
    fn partial_toml_uses_defaults_for_omitted_fields() {
        let document: SettingsDocument = toml::from_str(
            r#"
schema_version = 6

[receive]
mode = "hex"
"#,
        )
        .unwrap();
        let preferences = UiPreferences::from(document);
        assert_eq!(preferences.receive_mode, ReceiveMode::Hex);
        assert_eq!(preferences.send_mode, SendMode::Text);
        assert_eq!(preferences.text_encoding, TextEncoding::Utf8);
        assert_eq!(preferences.theme_preference, ThemePreference::System);
    }

    #[test]
    fn invalid_manual_toml_returns_an_actionable_warning() {
        let test_dir = unique_test_dir("invalid-toml");
        fs::create_dir_all(&test_dir).unwrap();
        let toml_path = test_dir.join(SETTINGS_FILE_NAME);
        let json_path = test_dir.join(LEGACY_SETTINGS_FILE_NAME);
        fs::write(
            &toml_path,
            "schema_version = 6\n[receive]\nmode = \"binary\"\n",
        )
        .unwrap();

        let (preferences, warning) = load_at(&toml_path, &json_path);

        assert_eq!(preferences.receive_mode, ReceiveMode::Text);
        let warning = warning.expect("invalid TOML should produce a warning");
        assert!(warning.contains("settings.toml"));
        assert!(warning.contains("binary"));
        assert!(toml_path.is_file());
        let _ = fs::remove_dir_all(test_dir);
    }

    #[test]
    fn theme_preference_round_trips_through_toml() {
        for theme_preference in [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ] {
            let preferences = UiPreferences {
                theme_preference,
                ..Default::default()
            };
            let source = render_toml(&preferences).unwrap();
            let document: SettingsDocument = toml::from_str(&source).unwrap();
            let loaded = UiPreferences::from(document);
            assert_eq!(loaded.theme_preference, theme_preference);
        }
    }

    #[test]
    fn legacy_json_is_migrated_and_archived() {
        let test_dir = unique_test_dir("json-migration");
        fs::create_dir_all(&test_dir).unwrap();
        let toml_path = test_dir.join(SETTINGS_FILE_NAME);
        let json_path = test_dir.join(LEGACY_SETTINGS_FILE_NAME);
        let preferences = UiPreferences {
            receive_mode: ReceiveMode::Terminal,
            background_source: AppBackgroundSource::Online,
            background_online_url: "https://example.com/background.png".into(),
            ..Default::default()
        };
        fs::write(&json_path, serde_json::to_vec_pretty(&preferences).unwrap()).unwrap();

        let (loaded, warning) = load_at(&toml_path, &json_path);

        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(loaded.receive_mode, ReceiveMode::Terminal);
        assert_eq!(loaded.background_source, AppBackgroundSource::Online);
        assert!(toml_path.is_file());
        assert!(!json_path.exists());
        assert!(json_path.with_extension("json.migrated.bak").is_file());
        assert!(
            fs::read_to_string(toml_path)
                .unwrap()
                .contains("source = \"online\"")
        );
        let _ = fs::remove_dir_all(test_dir);
    }

    #[test]
    fn legacy_preferences_default_to_system_theme() {
        let loaded: UiPreferences = serde_json::from_str(r#"{"schema_version":2}"#).unwrap();
        assert_eq!(loaded.theme_preference, ThemePreference::System);
    }

    #[test]
    fn legacy_preferences_default_to_no_background() {
        let loaded: UiPreferences = serde_json::from_str(r#"{"schema_version":3}"#).unwrap();
        assert_eq!(loaded.background_source, AppBackgroundSource::None);
        assert_eq!(
            loaded.background_light_opacity,
            DEFAULT_BACKGROUND_LIGHT_OPACITY
        );
        assert_eq!(
            loaded.background_dark_opacity,
            DEFAULT_BACKGROUND_DARK_OPACITY
        );
    }

    #[test]
    fn legacy_preferences_default_to_standard_data_line_spacing() {
        let loaded: UiPreferences = serde_json::from_str(r#"{"schema_version":4}"#).unwrap();
        assert_eq!(loaded.data_line_spacing, DEFAULT_DATA_LINE_SPACING);
    }

    #[test]
    fn sanitize_clamps_background_opacity() {
        let mut preferences = UiPreferences {
            background_light_opacity: 2.0,
            background_dark_opacity: f32::NAN,
            ..Default::default()
        };

        preferences.sanitize();

        assert_eq!(preferences.background_light_opacity, 1.0);
        assert_eq!(
            preferences.background_dark_opacity,
            DEFAULT_BACKGROUND_DARK_OPACITY
        );
    }

    #[test]
    fn legacy_eframe_file_is_migrated_into_settings_directory() {
        let test_root = unique_test_dir("migration");
        let legacy_path = test_root.join("ESCOM");
        fs::create_dir_all(&test_root).unwrap();
        fs::write(&legacy_path, b"legacy window state").unwrap();

        prepare_storage_at(&legacy_path).unwrap();

        assert!(legacy_path.is_dir());
        assert_eq!(
            fs::read(legacy_path.join("window.ron")).unwrap(),
            b"legacy window state"
        );
        let _ = fs::remove_dir_all(test_root);
    }

    #[test]
    fn interrupted_migration_is_recovered() {
        let test_root = unique_test_dir("recovery");
        let settings_directory = test_root.join("ESCOM");
        let migration_path = settings_directory.with_extension("migration.tmp");
        fs::create_dir_all(&test_root).unwrap();
        fs::write(&migration_path, b"staged window state").unwrap();

        prepare_storage_at(&settings_directory).unwrap();

        assert!(settings_directory.is_dir());
        assert_eq!(
            fs::read(settings_directory.join("window.ron")).unwrap(),
            b"staged window state"
        );
        let _ = fs::remove_dir_all(test_root);
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "escom-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
