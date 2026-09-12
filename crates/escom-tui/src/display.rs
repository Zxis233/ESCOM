use crate::config::Config;
use escom_core::formatting::{DisplayFormatter, DisplayLimits, FormattedRow};
use escom_core::model::{ReceiveMode, TextEncoding};
use escom_core::store::ReceiveStore;
use std::sync::Mutex;

pub const INCREMENT_BYTES: usize = 32 * 1024;

pub struct Display {
    pub rows: Vec<FormattedRow>,
    pub generation: u64,
    pub limited: bool,
    formatter: Option<DisplayFormatter>,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            generation: u64::MAX,
            limited: false,
            formatter: None,
        }
    }
}

impl Display {
    pub fn invalidate(&mut self) {
        self.formatter = None;
        self.generation = u64::MAX;
    }

    pub fn update(&mut self, store: &Mutex<ReceiveStore>, config: &Config) -> Result<(), String> {
        if self.generation
            == store
                .lock()
                .map_err(|_| "Receive store unavailable")?
                .generation()
        {
            return Ok(());
        }
        if let Some(formatter) = &mut self.formatter {
            let delta = store
                .lock()
                .map_err(|_| "Receive store unavailable")?
                .delta_since_bounded(formatter.cursor(), INCREMENT_BYTES);
            if let Ok(update) = formatter.apply_delta(&delta) {
                self.rows.drain(..update.remove_prefix.min(self.rows.len()));
                self.rows
                    .truncate(self.rows.len().saturating_sub(update.replace_tail));
                self.rows.extend(update.rows);
                self.generation = update.generation;
                self.limited = formatter.is_limited();
                return Ok(());
            }
        }
        let mode = config.receive_mode().unwrap_or(ReceiveMode::Text);
        let rebuild_bytes = match mode {
            ReceiveMode::Hex => (config.display_rows * 16).min(config.display_kib * 1024),
            _ => config.display_kib * 1024,
        };
        let snapshot = store
            .lock()
            .map_err(|_| "Receive store unavailable")?
            .tail_snapshot(rebuild_bytes);
        let (formatter, rows) = DisplayFormatter::rebuild_with_limits(
            &snapshot,
            mode,
            config.text_encoding().unwrap_or(TextEncoding::Utf8),
            DisplayLimits {
                max_rows: config.display_rows,
                max_text_bytes: config.display_kib * 1024,
                max_line_bytes: config.line_kib * 1024,
            },
        );
        self.rows = rows;
        self.generation = snapshot.generation;
        self.limited = formatter.is_limited();
        self.formatter = Some(formatter);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    #[test]
    fn sustained_input_and_rebuild_stay_bounded_in_all_modes() {
        for mode in ["text", "hex", "terminal"] {
            let config = Config {
                mode: mode.into(),
                display_rows: 32,
                display_kib: 16,
                line_kib: 1,
                ..Config::default()
            };
            let store = Mutex::new(ReceiveStore::with_limits(65536, 64));
            let mut display = Display::default();
            for batch in 0..150 {
                for _ in 0..8 {
                    store
                        .lock()
                        .unwrap()
                        .append(Local::now(), b"line\r\n".repeat(100));
                }
                if batch % 9 == 0 {
                    store.lock().unwrap().mark_stream_boundary(Local::now());
                }
                display.update(&store, &config).unwrap();
                assert!(display.rows.len() <= config.display_rows, "{mode}");
                assert!(
                    display.rows.iter().map(|row| row.text.len()).sum::<usize>()
                        <= config.display_kib * 1024,
                    "{mode}"
                );
                assert!(store.lock().unwrap().records_len() <= 64);
                assert!(store.lock().unwrap().bytes_len() <= 65536);
            }
            display.invalidate();
            display.update(&store, &config).unwrap();
            assert!(display.rows.len() <= config.display_rows);
        }
    }
}
