use crate::{config::Config, i18n::Message, msg};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SAVE_DELAY: Duration = Duration::from_millis(750);
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

pub fn default_path() -> PathBuf {
    escom_core::storage::settings_dir().join("tui.toml")
}

/// Read-only loading: help/list/print-config must not create files.
pub fn read_config(path: &Path) -> Result<Option<(Config, Vec<u8>)>, Message> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes).map_err(|e| msg!(ConfigError, path.display(), e))?;
    let config: Config = toml::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| msg!(ConfigError, path.display(), e))?;
    config.validate()?;
    Ok(Some((config, bytes)))
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, Message> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(msg!(ConfigError, path.display(), error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| msg!(ConfigError, path.display(), e))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(crate::i18n::Key::ConfigTooLarge.into());
    }
    Ok(Some(bytes))
}

#[derive(Debug)]
pub struct Persistence {
    pub path: PathBuf,
    original: Option<Vec<u8>>,
    saved: Option<Config>,
    observed: Config,
    configured_port: String,
    dirty_since: Option<Instant>,
    blocked: Option<Message>,
}

impl Persistence {
    pub fn new(path: PathBuf, loaded: Option<(Config, Vec<u8>)>, current: &Config) -> Self {
        let (saved, original) = match loaded {
            Some((config, bytes)) => (Some(config), Some(bytes)),
            None => (None, None),
        };
        Self {
            path,
            original,
            saved,
            observed: current.clone(),
            configured_port: current.port.clone(),
            dirty_since: None,
            blocked: None,
        }
    }

    fn persistent_config(&self, config: &Config) -> Config {
        let mut saved = config.clone();
        if config.demo {
            saved.port.clone_from(&self.configured_port);
        }
        saved.demo = false;
        saved.record = None;
        saved.connect_on_start = false;
        saved
    }

    pub fn save_if_due(&mut self, config: &Config, now: Instant) -> Result<bool, Message> {
        if &self.observed != config {
            self.observed = config.clone();
            self.dirty_since = Some(now);
        }
        if self.blocked.is_some() {
            return Ok(false);
        }
        if self
            .dirty_since
            .is_some_and(|since| now.saturating_duration_since(since) >= SAVE_DELAY)
        {
            self.save(config)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Atomic replacement plus a last-known-good backup. Refuse externally edited files.
    pub fn save(&mut self, config: &Config) -> Result<(), Message> {
        if let Some(error) = &self.blocked {
            return Err(error.clone());
        }
        let result = self.save_inner(config);
        if let Err(error) = &result {
            self.blocked = Some(error.clone());
        }
        result
    }

    fn save_inner(&mut self, config: &Config) -> Result<(), Message> {
        let desired = self.persistent_config(config);
        desired.validate()?;
        if self.saved.as_ref() == Some(&desired) {
            self.dirty_since = None;
            return Ok(());
        }
        let bytes = toml::to_string_pretty(&desired)
            .map_err(|e| msg!(ConfigSaveError, self.path.display(), e))?
            .into_bytes();
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(crate::i18n::Key::ConfigTooLarge.into());
        }
        if read_bytes(&self.path)? != self.original {
            return Err(msg!(ConfigChanged, self.path.display()));
        }
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| msg!(ConfigSaveError, self.path.display(), e))?;
        if let Some(original) = &self.original {
            write_atomic(&self.path.with_extension("toml.bak"), original, true)
                .map_err(|e| msg!(ConfigSaveError, self.path.display(), e))?;
        }
        write_atomic(&self.path, &bytes, self.original.is_some())
            .map_err(|e| msg!(ConfigSaveError, self.path.display(), e))?;
        self.original = Some(bytes);
        self.saved = Some(desired);
        self.observed = config.clone();
        self.dirty_since = None;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8], replace: bool) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".escom-tui-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    let result = if replace {
        temp.persist(path)
    } else {
        temp.persist_noclobber(path)
    };
    result.map(|_| ()).map_err(|e| e.error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Action, i18n::Language};

    #[test]
    fn default_config_is_created_and_changes_reload_without_touching_gui_files() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("ESCOM");
        fs::create_dir_all(&folder).unwrap();
        let gui = folder.join("settings.toml");
        fs::write(&gui, "gui-specific settings").unwrap();
        let path = folder.join("tui.toml");
        let (mut config, _, mut persistence) =
            Config::parse_at(Vec::<String>::new(), &path).unwrap();
        assert!(!path.exists());
        persistence.save(&config).unwrap();
        let first = fs::read(&path).unwrap();
        config.language = Language::ZhCn;
        config.baud = 9600;
        config.port = "COM42".into();
        config.send_mode = "hex".into();
        config.history_kib = 512;
        persistence.save(&config).unwrap();
        assert_eq!(fs::read(path.with_extension("toml.bak")).unwrap(), first);
        let (loaded, _, _) = Config::parse_at(Vec::<String>::new(), &path).unwrap();
        assert_eq!(loaded, config);
        assert!(!loaded.connect_on_start);
        assert_eq!(fs::read_to_string(gui).unwrap(), "gui-specific settings");
    }

    #[test]
    fn explicit_config_overrides_default_and_read_only_actions_do_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("default.toml");
        let custom = dir.path().join("custom.toml");
        fs::write(&default, "this is an invalid default file").unwrap();
        let (config, _, mut persistence) = Config::parse_at(
            [
                "--config".into(),
                custom.to_string_lossy().into_owned(),
                "--lang".into(),
                "zh-CN".into(),
            ],
            &default,
        )
        .unwrap();
        assert_eq!(persistence.path, custom);
        persistence.save(&config).unwrap();
        assert_eq!(
            read_config(&custom).unwrap().unwrap().0.language,
            Language::ZhCn
        );
        assert_eq!(
            fs::read_to_string(&default).unwrap(),
            "this is an invalid default file"
        );
        let absent = dir.path().join("absent").join("tui.toml");
        for flag in [
            "--help",
            "--version",
            "--list",
            "--print-config",
            "--config-path",
        ] {
            let (_, action, _) = Config::parse_at([flag.into()], &absent).unwrap();
            assert_ne!(action, Action::Run);
            assert!(!absent.parent().unwrap().exists());
        }
    }

    #[test]
    fn invalid_and_externally_modified_configs_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        fs::write(&path, "language = [invalid").unwrap();
        assert!(Config::parse_at(Vec::<String>::new(), &path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "language = [invalid");
        fs::write(&path, "language = \"en\"").unwrap();
        let (mut config, _, mut persistence) =
            Config::parse_at(Vec::<String>::new(), &path).unwrap();
        fs::write(&path, "language = \"zh-CN\" # manual change").unwrap();
        config.baud = 9600;
        assert!(persistence.save(&config).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "language = \"zh-CN\" # manual change"
        );
        assert!(persistence.save(&config).is_err());
    }

    #[test]
    fn demo_recording_and_connection_intent_are_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        fs::write(&path, "port = \"COM7\"\n").unwrap();
        let (mut config, _, mut persistence) = Config::parse_at(
            ["--demo".into(), "--record".into(), "session.bin".into()],
            &path,
        )
        .unwrap();
        config.port = "DEMO".into(); // the frontend's synthetic port
        config.connect_on_start = true;
        config.language = Language::ZhCn;
        persistence.save(&config).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("demo ="));
        assert!(!text.contains("record ="));
        assert!(!text.contains("connect_on_start"));
        let (loaded, _, _) = Config::parse_at(Vec::<String>::new(), &path).unwrap();
        assert_eq!(loaded.port, "COM7");
        assert!(!loaded.demo);
        assert!(loaded.record.is_none());
        assert!(!loaded.connect_on_start);
        assert!(
            Config::parse_at(["--connect".into()], &path)
                .unwrap()
                .0
                .connect_on_start
        );
        assert!(
            Config::parse_at(["--port".into(), "COM8".into()], &path)
                .unwrap()
                .0
                .connect_on_start
        );
    }

    #[test]
    fn autosave_debounces_and_final_save_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        let (mut config, _, mut persistence) =
            Config::parse_at(Vec::<String>::new(), &path).unwrap();
        persistence.save(&config).unwrap();
        config.language = Language::ZhCn;
        let now = Instant::now();
        assert!(!persistence.save_if_due(&config, now).unwrap());
        assert!(
            !persistence
                .save_if_due(&config, now + Duration::from_millis(749))
                .unwrap()
        );
        assert!(persistence.save_if_due(&config, now + SAVE_DELAY).unwrap());
        assert_eq!(
            read_config(&path).unwrap().unwrap().0.language,
            Language::ZhCn
        );
        config.mode = "hex".into();
        persistence.save(&config).unwrap();
        assert_eq!(read_config(&path).unwrap().unwrap().0.mode, "hex");
    }

    #[test]
    fn theme_shortcut_and_custom_colors_survive_autosave_and_reload() {
        use crate::{app::App, theme::Preset};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use ratatui::style::Color;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        fs::write(
            &path,
            "[theme]\npreset = 'midnight'\n[theme.colors]\naccent = '#123456'\n",
        )
        .unwrap();
        let (config, _, mut persistence) = Config::parse_at(Vec::<String>::new(), &path).unwrap();
        let mut app = App::new(config).unwrap();
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::F(10),
            KeyModifiers::NONE,
        )));
        let now = Instant::now();
        assert!(!persistence.save_if_due(&app.config, now).unwrap());
        assert!(
            persistence
                .save_if_due(&app.config, now + SAVE_DELAY)
                .unwrap()
        );
        let (loaded, _, _) = Config::parse_at(Vec::<String>::new(), &path).unwrap();
        assert_eq!(loaded.theme.preset, Preset::Custom);
        assert_eq!(loaded.theme.palette().accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(loaded.theme.colors, app.config.theme.colors);
        app.shutdown().unwrap();
    }
}
