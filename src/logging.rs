use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};

use crate::settings;

const LOG_DIRECTORY: &str = "logs";
const LOG_FILE: &str = "escom.log";
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
const LOG_ARCHIVES: usize = 3;

struct FileLogger {
    level: LevelFilter,
    writer: Mutex<RollingWriter>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("unnamed");
        let line = format!(
            "{timestamp} {:<5} [{thread_name}] {} - {}\n",
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_record(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

struct RollingWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    max_bytes: u64,
    archives: usize,
}

impl RollingWriter {
    fn open(path: PathBuf, max_bytes: u64, archives: usize) -> io::Result<Self> {
        let file = open_append(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            bytes_written,
            max_bytes,
            archives,
        })
    }

    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.bytes_written != 0
            && self.bytes_written.saturating_add(bytes.len() as u64) > self.max_bytes
        {
            self.rotate()?;
        }
        self.file
            .as_mut()
            .expect("rolling log file must be open")
            .write_all(bytes)?;
        self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("rolling log file must be open")
            .flush()
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        if self.archives != 0 {
            let oldest = archive_path(&self.path, self.archives);
            remove_file_if_present(&oldest)?;
            for index in (1..self.archives).rev() {
                let source = archive_path(&self.path, index);
                if source.exists() {
                    let destination = archive_path(&self.path, index + 1);
                    remove_file_if_present(&destination)?;
                    fs::rename(source, destination)?;
                }
            }
            if self.path.exists() {
                fs::rename(&self.path, archive_path(&self.path, 1))?;
            }
        } else {
            remove_file_if_present(&self.path)?;
        }

        self.file = Some(open_append(&self.path)?);
        self.bytes_written = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(LOG_FILE);
    path.with_file_name(format!("{file_name}.{index}"))
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Starts the process-wide logger and returns the active log path.
pub fn init() -> Result<PathBuf, String> {
    let directory = settings::settings_dir().join(LOG_DIRECTORY);
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建日志目录：{error}"))?;
    let path = directory.join(LOG_FILE);
    let writer = RollingWriter::open(path.clone(), MAX_LOG_BYTES, LOG_ARCHIVES)
        .map_err(|error| format!("无法打开日志文件：{error}"))?;
    let logger = Box::leak(Box::new(FileLogger {
        level: LevelFilter::Info,
        writer: Mutex::new(writer),
    }));
    log::set_logger(logger).map_err(|error| format!("无法初始化日志：{error}"))?;
    log::set_max_level(LevelFilter::Info);
    install_panic_logger();
    Ok(path)
}

fn install_panic_logger() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        log::error!(target: "escom::panic", "{panic_info}");
        log::logger().flush();
        previous_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn temporary_log_path() -> PathBuf {
        let unique = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("escom-logging-{}-{unique}", std::process::id()))
            .join(LOG_FILE)
    }

    #[test]
    fn writer_rotates_and_keeps_the_configured_archive_count() {
        let path = temporary_log_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = RollingWriter::open(path.clone(), 16, 2).unwrap();

        for record in [
            b"first record\n".as_slice(),
            b"second record\n".as_slice(),
            b"third record\n".as_slice(),
        ] {
            writer.write_record(record).unwrap();
        }
        writer.flush().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "third record\n");
        assert_eq!(
            fs::read_to_string(archive_path(&path, 1)).unwrap(),
            "second record\n"
        );
        assert_eq!(
            fs::read_to_string(archive_path(&path, 2)).unwrap(),
            "first record\n"
        );
        assert!(!archive_path(&path, 3).exists());

        drop(writer);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
