use std::path::PathBuf;

/// Shared application data directory; frontends keep separate configuration files.
pub fn settings_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("ESCOM"))
        .unwrap_or_else(|| PathBuf::from("."))
}
