use crate::text::english_error;
use crate::{config::Config, demo::DemoBackend, display::Display};
use chrono::Local;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use escom_core::{
    capture::CaptureHandle,
    formatting::{encode_text, parse_send_input},
    model::{LineEnding, SendMode},
    search::SearchMatcher,
    serial_worker::{ProductionBackend, SerialBackend, WorkerEvent, WorkerHandle, WorkerOptions},
    store::ReceiveStore,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const SEARCH_LIMIT: usize = 1024;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputMode {
    View,
    Send,
    Search,
    Command,
    Direct,
}

pub struct App {
    pub config: Config,
    pub store: Arc<Mutex<ReceiveStore>>,
    pub worker: WorkerHandle,
    pub display: Display,
    pub connected: bool,
    pub connecting: bool,
    pub ports: Vec<String>,
    pub paused: bool,
    pub offset: usize,
    pub horizontal: u16,
    pub page_rows: usize,
    pub input_mode: InputMode,
    pub input: String,
    pub send_mode: SendMode,
    pub search_regex: bool,
    pub matches: Vec<usize>,
    pub selected_match: usize,
    pub search_truncated: bool,
    pub notice: String,
    pub last_tx: String,
    pub help: bool,
    pub capture: Option<CaptureHandle>,
    pub capture_path: Option<PathBuf>,
    pub capture_result: String,
    pub capture_failed: bool,
    next_id: u64,
    cycle_port_on_refresh: bool,
    search_was_paused: bool,
    observed_rx: u64,
    observed_capture: (u64, usize, bool),
}

impl App {
    pub fn new(mut config: Config) -> Result<Self, String> {
        let backend: Arc<dyn SerialBackend> = if config.demo {
            config.port = "DEMO".into();
            Arc::new(DemoBackend)
        } else {
            Arc::new(ProductionBackend)
        };
        let store = Arc::new(Mutex::new(ReceiveStore::with_limits(
            config.history_kib * 1024,
            config.history_records,
        )));
        let worker = WorkerHandle::spawn_with_options(
            Arc::clone(&store),
            backend,
            Arc::new(|| {}),
            WorkerOptions {
                max_queued_write_bytes: config.tx_kib * 1024,
                max_write_bytes: config.send_kib * 1024,
            },
        );
        let mut app = Self {
            config,
            store,
            worker,
            display: Display::default(),
            connected: false,
            connecting: false,
            ports: Vec::new(),
            paused: false,
            offset: 0,
            horizontal: 0,
            page_rows: 20,
            input_mode: InputMode::View,
            input: String::new(),
            send_mode: SendMode::Text,
            search_regex: false,
            matches: Vec::new(),
            selected_match: 0,
            search_truncated: false,
            notice: "F2 choose port | F3 connect | ? help".into(),
            last_tx: String::new(),
            help: false,
            capture: None,
            capture_path: None,
            capture_result: "Recording off".into(),
            capture_failed: false,
            next_id: 1,
            cycle_port_on_refresh: false,
            search_was_paused: false,
            observed_rx: 0,
            observed_capture: (0, 0, false),
        };
        if let Some(path) = app.config.record.clone() {
            app.start_capture(&path)?;
        }
        app.worker.refresh_ports()?;
        if !app.config.port.is_empty() {
            app.toggle_connection()?;
        }
        Ok(app)
    }

    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        let previous_generation = self.display.generation;
        for _ in 0..256 {
            let Ok(event) = self.worker.events.try_recv() else {
                break;
            };
            changed = true;
            match event {
                WorkerEvent::Ports(ports) => {
                    self.ports = ports;
                    if self.cycle_port_on_refresh && !self.connected && !self.connecting {
                        if self.ports.is_empty() {
                            self.notice =
                                "No ports found; use :port COM3 to enter one manually".into();
                        } else {
                            let next = self
                                .ports
                                .iter()
                                .position(|p| p == &self.config.port)
                                .map_or(0, |i| (i + 1) % self.ports.len());
                            self.config.port.clone_from(&self.ports[next]);
                            self.notice = format!("Selected {} | F3 connect", self.config.port);
                        }
                    } else if self.config.port.is_empty() {
                        self.config.port = self.ports.first().cloned().unwrap_or_default();
                    }
                    self.cycle_port_on_refresh = false;
                }
                WorkerEvent::Opened(port) => {
                    self.connected = true;
                    self.connecting = false;
                    self.notice = format!("Connected: {port}");
                }
                WorkerEvent::Closed { error } => {
                    self.connected = false;
                    self.connecting = false;
                    self.notice = error
                        .map(|error| english_error(&error))
                        .unwrap_or_else(|| "Disconnected".into());
                }
                WorkerEvent::TxCompleted { id, count } => {
                    self.last_tx = format!("TX #{id}: {count} bytes written")
                }
                WorkerEvent::TxFailed { id, message } => {
                    self.last_tx = format!("TX #{id}: {}", english_error(&message))
                }
                WorkerEvent::ControlError(message) => self.notice = english_error(&message),
            }
        }
        if !self.paused {
            if let Err(error) = self.display.update(&self.store, &self.config) {
                self.notice = error;
            }
            self.offset = self.display.rows.len().saturating_sub(self.page_rows);
        }
        let rx = self.worker.stats.rx_bytes();
        let capture = self
            .capture
            .as_ref()
            .map(|capture| {
                let progress = capture.progress();
                (
                    progress.written_bytes,
                    progress.queued_bytes,
                    progress.error.is_some(),
                )
            })
            .unwrap_or_default();
        changed |= rx != self.observed_rx
            || capture != self.observed_capture
            || previous_generation != self.display.generation;
        self.observed_rx = rx;
        self.observed_capture = capture;
        changed
    }

    pub fn toggle_connection(&mut self) -> Result<(), String> {
        if self.connected || self.connecting {
            self.worker.close()?;
            self.notice = "Disconnecting...".into();
        } else {
            let config = self.config.serial_config()?;
            config.validate().map_err(str::to_owned)?;
            self.worker.open(config)?;
            self.connecting = true;
            self.notice = "Connecting...".into();
        }
        Ok(())
    }

    fn choose_port(&mut self) -> Result<(), String> {
        if self.connected || self.connecting {
            return Err("Disconnect before changing port".into());
        }
        self.worker.refresh_ports()?;
        self.cycle_port_on_refresh = true;
        self.notice = "Refreshing ports...".into();
        Ok(())
    }

    pub fn start_capture(&mut self, path: &Path) -> Result<(), String> {
        if self.capture.is_some() {
            return Err("Stop the current recording first (:stop-record)".into());
        }
        let capture = CaptureHandle::start(path, self.config.record_queue_kib * 1024)
            .map_err(|e| e.to_string())?;
        self.worker.set_capture(Some(capture.sink()))?;
        self.capture = Some(capture);
        self.capture_path = Some(path.into());
        self.capture_result = "Recording RX bytes".into();
        self.capture_failed = false;
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Result<(), String> {
        self.worker.set_capture(None)?;
        if let Some(mut capture) = self.capture.take() {
            let result = capture.finish().map_err(|e| e.to_string());
            self.capture_failed = result.is_err();
            self.capture_result = match &result {
                Ok(()) => format!("Saved {} bytes", capture.progress().written_bytes),
                Err(error) => error.clone(),
            };
            result?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        self.worker.shutdown();
        if let Some(mut capture) = self.capture.take() {
            capture.finish().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if !self.connected {
            return Err("Connect before sending".into());
        }
        let id = self.next_id;
        self.worker.send(id, bytes)?;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.last_tx = format!("TX #{id}: queued");
        Ok(())
    }

    pub fn search(&mut self) -> Result<(), String> {
        self.matches.clear();
        self.selected_match = 0;
        self.search_truncated = false;
        if let Some(matcher) = SearchMatcher::new_with_limits(
            &self.input,
            false,
            self.search_regex,
            1024 * 1024,
            256 * 1024,
        )? {
            for (index, row) in self.display.rows.iter().enumerate() {
                if matcher.is_match(&row.text) {
                    if self.matches.len() == SEARCH_LIMIT {
                        self.search_truncated = true;
                        break;
                    }
                    self.matches.push(index);
                }
            }
        }
        self.jump_match();
        self.notice = format!(
            "{} matching rows{} | n/N navigate | Space resume (clears results)",
            self.matches.len(),
            if self.search_truncated {
                " (capped)"
            } else {
                ""
            }
        );
        Ok(())
    }

    fn jump_match(&mut self) {
        if let Some(row) = self.matches.get(self.selected_match) {
            self.offset = row.saturating_sub(self.page_rows / 2);
        }
    }
    pub fn resume(&mut self) {
        self.paused = false;
        self.matches.clear();
        self.search_truncated = false;
        self.notice = "Following latest RX; search results cleared".into();
    }

    fn begin_input(&mut self, mode: InputMode) {
        self.input.clear();
        self.input_mode = mode;
        if mode == InputMode::Search {
            self.search_was_paused = self.paused;
            self.paused = true;
        }
    }

    pub fn append_input(&mut self, text: &str) {
        let limit = if self.input_mode == InputMode::Send {
            self.config.send_kib * 1024
        } else {
            1024
        };
        // Reject the whole paste rather than silently sending a truncated command.
        if text.len() > limit.saturating_sub(self.input.len()) {
            self.notice = format!("Input rejected: editor limit is {limit} UTF-8 bytes");
            return;
        }
        self.input.push_str(text);
    }

    pub fn handle_event(&mut self, event: Event) -> bool {
        let result = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                    return true;
                }
                self.handle_key(key)
            }
            Event::Paste(text)
                if matches!(
                    self.input_mode,
                    InputMode::Send | InputMode::Search | InputMode::Command
                ) =>
            {
                self.append_input(&text);
                Ok(false)
            }
            Event::Paste(text) if self.input_mode == InputMode::Direct => {
                if text.len() > self.config.send_kib * 1024 {
                    Err("Paste exceeds send limit".into())
                } else {
                    self.config
                        .text_encoding()
                        .and_then(|encoding| encode_text(&text, encoding, LineEnding::None))
                        .and_then(|bytes| self.send_bytes(bytes))
                        .map(|()| false)
                }
            }
            _ => Ok(false),
        };
        match result {
            Ok(quit) => quit,
            Err(error) => {
                self.notice = english_error(&error);
                false
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        if self.help {
            self.help = false;
            return Ok(false);
        }
        if self.input_mode == InputMode::Direct {
            if key.code == KeyCode::Esc {
                self.input_mode = InputMode::View;
                return Ok(false);
            }
            if let Some(bytes) = direct_key(key, self.config.text_encoding()?)? {
                self.send_bytes(bytes)?;
            }
            return Ok(false);
        }
        if self.input_mode != InputMode::View {
            match key.code {
                KeyCode::Esc => {
                    if self.input_mode == InputMode::Search && !self.search_was_paused {
                        self.resume();
                    }
                    self.input_mode = InputMode::View;
                    self.input.clear();
                }
                KeyCode::Enter => {
                    match self.input_mode {
                        InputMode::Send => self.send_bytes(parse_send_input(
                            &self.input,
                            self.send_mode,
                            self.config.text_encoding()?,
                            self.config.ending()?,
                        )?)?,
                        InputMode::Search => self.search()?,
                        InputMode::Command => self.command()?,
                        _ => {}
                    }
                    self.input_mode = InputMode::View;
                    self.input.clear();
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear()
                }
                KeyCode::Char('r')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && self.input_mode == InputMode::Search =>
                {
                    self.search_regex = !self.search_regex
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.append_input(&ch.to_string())
                }
                _ => {}
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::F(2) => self.choose_port()?,
            KeyCode::F(3) => self.toggle_connection()?,
            KeyCode::F(4) => {
                self.config.mode = match self.config.mode.as_str() {
                    "text" => "hex",
                    "hex" => "terminal",
                    _ => "text",
                }
                .into();
                self.display.invalidate();
                self.resume();
            }
            KeyCode::F(5) => {
                self.config.encoding = if self.config.encoding == "utf8" {
                    "gbk"
                } else {
                    "utf8"
                }
                .into();
                self.display.invalidate();
                self.resume();
            }
            KeyCode::F(6) => {
                if self.capture.is_some() {
                    self.stop_capture()?;
                } else {
                    self.start_capture(Path::new(&format!(
                        "ESCOM-RX-{}.bin",
                        Local::now().format("%Y%m%d-%H%M%S-%f")
                    )))?;
                }
            }
            KeyCode::F(7) => {
                self.send_mode = if self.send_mode == SendMode::Text {
                    SendMode::Hex
                } else {
                    SendMode::Text
                }
            }
            KeyCode::F(8) => {
                if !self.connected {
                    return Err("Connect before entering direct input".into());
                }
                self.config.mode = "terminal".into();
                self.display.invalidate();
                self.resume();
                self.input_mode = InputMode::Direct;
            }
            KeyCode::Char('s') => self.begin_input(InputMode::Send),
            KeyCode::Char('/') => self.begin_input(InputMode::Search),
            KeyCode::Char(':') => self.begin_input(InputMode::Command),
            KeyCode::Char('t') => self.config.timestamps = !self.config.timestamps,
            KeyCode::Char(' ') => {
                if self.paused {
                    self.resume();
                } else {
                    self.paused = true;
                }
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.paused = true;
                self.offset = self.offset.saturating_sub(if key.code == KeyCode::Up {
                    1
                } else {
                    self.page_rows
                });
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.paused = true;
                self.offset = (self.offset
                    + if key.code == KeyCode::Down {
                        1
                    } else {
                        self.page_rows
                    })
                .min(self.display.rows.len().saturating_sub(1));
            }
            KeyCode::Left => self.horizontal = self.horizontal.saturating_sub(8),
            KeyCode::Right => self.horizontal = self.horizontal.saturating_add(8),
            KeyCode::Home => {
                self.paused = true;
                self.offset = 0;
            }
            KeyCode::End => self.resume(),
            KeyCode::Char('n' | 'N') if !self.matches.is_empty() => {
                let count = self.matches.len();
                self.selected_match = if key.code == KeyCode::Char('N') {
                    (self.selected_match + count - 1) % count
                } else {
                    (self.selected_match + 1) % count
                };
                self.jump_match();
            }
            KeyCode::Char('c') => {
                self.store
                    .lock()
                    .map_err(|_| "Receive store unavailable")?
                    .clear();
                self.display = Display::default();
                self.resume();
                self.notice = "History cleared; recording continues".into();
            }
            _ => {}
        }
        Ok(false)
    }

    fn command(&mut self) -> Result<(), String> {
        let input = self.input.clone();
        let (name, value) = input.trim().split_once(' ').unwrap_or((input.trim(), ""));
        let value = value.trim();
        match name {
            "record" if !value.is_empty() => return self.start_capture(Path::new(value)),
            "stop-record" => return self.stop_capture(),
            "connect" => {
                if !self.connected && !self.connecting {
                    return self.toggle_connection();
                }
                return Ok(());
            }
            "disconnect" => return self.worker.close(),
            "ports" => {
                self.worker.refresh_ports()?;
                self.notice = self.ports.join(" | ");
                return Ok(());
            }
            _ => {}
        }
        let mut config = self.config.clone();
        match name {
            "port" => config.port = value.into(),
            "baud" => config.baud = value.parse().map_err(|_| "Invalid baud")?,
            "data" => config.data_bits = value.parse().map_err(|_| "Invalid data bits")?,
            "stop" => config.stop_bits = value.parse().map_err(|_| "Invalid stop bits")?,
            "parity" => config.parity = value.into(),
            "flow" => config.flow = value.into(),
            "dtr" | "rts" => {
                let level = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err("Use on or off".into()),
                };
                if name == "dtr" {
                    self.worker.set_dtr(level)?;
                    config.dtr = level;
                } else {
                    if config.flow == "hardware" {
                        return Err("RTS is controlled by hardware flow control".into());
                    }
                    self.worker.set_rts(level)?;
                    config.rts = level;
                }
            }
            "mode" => config.mode = value.into(),
            "encoding" => config.encoding = value.into(),
            "eol" => config.line_ending = value.into(),
            _ => return Err("Unknown command; ? shows command help".into()),
        }
        if matches!(name, "port" | "baud" | "data" | "stop" | "parity" | "flow")
            && (self.connected || self.connecting)
        {
            return Err("Disconnect before changing serial settings".into());
        }
        config.validate()?;
        self.config = config;
        if matches!(name, "mode" | "encoding") {
            self.display.invalidate();
            self.resume();
        }
        self.notice = format!("Set {name} = {value}");
        Ok(())
    }
}

fn direct_key(
    key: KeyEvent,
    encoding: escom_core::model::TextEncoding,
) -> Result<Option<Vec<u8>>, String> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(ch) = key.code
    {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_lowercase() {
            return Ok(Some(vec![ch as u8 - b'a' + 1]));
        }
    }
    let bytes: &[u8] = match key.code {
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::Char(ch) => {
            return encode_text(&ch.to_string(), encoding, LineEnding::None).map(Some);
        }
        _ => return Ok(None),
    };
    Ok(Some(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use escom_core::model::TextEncoding;
    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn search_is_on_submit_bounded_and_frozen_until_resume() {
        let mut app = App::new(Config::default()).unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"WARN row\n".repeat(1500));
        app.tick();
        app.handle_event(key(KeyCode::Char('/')));
        app.append_input("warn");
        assert!(app.matches.is_empty());
        let generation = app.display.generation;
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"new arrival\n".to_vec());
        app.tick();
        assert_eq!(app.display.generation, generation);
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.matches.len(), SEARCH_LIMIT);
        assert!(app.search_truncated);
        app.handle_event(key(KeyCode::Char(' ')));
        app.tick();
        assert!(app.matches.is_empty());
        assert_ne!(app.display.generation, generation);
        app.shutdown().unwrap();
    }

    #[test]
    fn oversized_paste_is_rejected_whole_and_unicode_editing_is_safe() {
        let mut app = App::new(Config::default()).unwrap();
        app.handle_event(key(KeyCode::Char('s')));
        app.append_input("串口");
        app.append_input(&"x".repeat(app.config.send_kib * 1024));
        assert_eq!(app.input, "串口");
        app.handle_event(key(KeyCode::Backspace));
        assert_eq!(app.input, "串");
        app.shutdown().unwrap();
    }

    #[test]
    fn direct_input_maps_control_navigation_and_encoding() {
        assert_eq!(
            direct_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                TextEncoding::Utf8
            )
            .unwrap(),
            Some(vec![3])
        );
        assert_eq!(
            direct_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                TextEncoding::Utf8
            )
            .unwrap(),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            direct_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                TextEncoding::Utf8
            )
            .unwrap(),
            Some(vec![13])
        );
        assert_eq!(
            direct_key(
                KeyEvent::new(KeyCode::Char('中'), KeyModifiers::NONE),
                TextEncoding::Gbk
            )
            .unwrap(),
            Some(vec![0xd6, 0xd0])
        );
    }

    #[test]
    fn cancelling_search_restores_previous_pause_state() {
        let mut app = App::new(Config::default()).unwrap();
        app.handle_event(key(KeyCode::Char('/')));
        app.handle_event(key(KeyCode::Esc));
        assert!(!app.paused);
        app.paused = true;
        app.handle_event(key(KeyCode::Char('/')));
        app.handle_event(key(KeyCode::Esc));
        assert!(app.paused);
        app.shutdown().unwrap();
    }
}
