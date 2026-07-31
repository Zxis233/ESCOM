use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::model::{LineEnding, ReceiveMode, SendMode, TextEncoding};

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const BUFFER_LIMIT_OPTIONS_MIB: [usize; 4] = [5, 20, 100, 500];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPreferences {
    pub schema_version: u32,
    pub ui_font_family: String,
    pub data_font_family: String,
    pub ui_font_size: f32,
    pub data_font_size: f32,
    pub receive_mode: ReceiveMode,
    pub send_mode: SendMode,
    pub text_encoding: TextEncoding,
    pub timestamps: bool,
    pub auto_scroll: bool,
    pub line_ending: LineEnding,
    pub repeat_interval_ms: u64,
    pub buffer_limit_mib: usize,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            ui_font_family: String::new(),
            data_font_family: String::new(),
            ui_font_size: 15.0,
            data_font_size: 15.0,
            receive_mode: ReceiveMode::Text,
            send_mode: SendMode::Text,
            text_encoding: TextEncoding::Utf8,
            timestamps: false,
            auto_scroll: true,
            line_ending: LineEnding::CrLf,
            repeat_interval_ms: 1_000,
            buffer_limit_mib: 20,
        }
    }
}

impl UiPreferences {
    pub fn sanitize(&mut self) {
        self.schema_version = SETTINGS_SCHEMA_VERSION;
        self.ui_font_size = self.ui_font_size.clamp(10.0, 28.0);
        self.data_font_size = self.data_font_size.clamp(10.0, 32.0);
        self.repeat_interval_ms = self.repeat_interval_ms.clamp(20, 3_600_000);
        if !BUFFER_LIMIT_OPTIONS_MIB.contains(&self.buffer_limit_mib) {
            self.buffer_limit_mib = 20;
        }
    }

    pub fn buffer_limit_bytes(&self) -> usize {
        self.buffer_limit_mib.saturating_mul(1024 * 1024)
    }
}

pub fn settings_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("ESCOM"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    settings_dir().join("settings.json")
}

pub fn eframe_persistence_path() -> PathBuf {
    settings_dir().join("window.ron")
}

pub fn prepare_storage() -> io::Result<()> {
    prepare_storage_at(&settings_dir())
}

pub fn load() -> UiPreferences {
    let _ = prepare_storage();
    load_from(&settings_path()).unwrap_or_default()
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

fn load_from(path: &Path) -> Option<UiPreferences> {
    let backup_path = path.with_extension("json.bak");
    let bytes = fs::read(path).or_else(|_| fs::read(backup_path)).ok()?;
    let mut preferences: UiPreferences = serde_json::from_slice(&bytes).ok()?;
    preferences.sanitize();
    Some(preferences)
}

fn save_to(path: &Path, preferences: &UiPreferences) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("json.tmp");
    let backup_path = path.with_extension("json.bak");
    let bytes = serde_json::to_vec_pretty(preferences).map_err(io::Error::other)?;
    fs::write(&temp_path, bytes)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_only_contains_ui_state() {
        let unique = format!(
            "escom-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let test_dir = std::env::temp_dir().join(unique);
        let test_path = test_dir.join("settings.json");
        let preferences = UiPreferences {
            ui_font_family: "Microsoft YaHei UI".into(),
            timestamps: true,
            ..Default::default()
        };

        save_to(&test_path, &preferences).unwrap();
        let loaded = load_from(&test_path).unwrap();
        assert_eq!(loaded.ui_font_family, "Microsoft YaHei UI");
        assert!(loaded.timestamps);

        let json = fs::read_to_string(&test_path).unwrap();
        assert!(!json.contains("port_name"));
        assert!(!json.contains("send_history"));
        let _ = fs::remove_dir_all(test_dir);
    }

    #[test]
    fn sanitize_restores_supported_limits() {
        let mut preferences = UiPreferences {
            ui_font_size: 100.0,
            repeat_interval_ms: 1,
            buffer_limit_mib: 7,
            ..Default::default()
        };
        preferences.sanitize();
        assert_eq!(preferences.ui_font_size, 28.0);
        assert_eq!(preferences.repeat_interval_ms, 20);
        assert_eq!(preferences.buffer_limit_mib, 20);
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
