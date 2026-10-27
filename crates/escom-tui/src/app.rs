use crate::editor::{LineEditor, SendHistory};
use crate::i18n::{Key, Language, Message};
use crate::msg;
use crate::{config::Config, demo::DemoBackend, display::Display};
use chrono::Local;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use escom_core::{
    capture::CaptureHandle,
    formatting::{encode_text_typed, parse_send_input_typed},
    model::{LineEnding, SendMode},
    search::SearchMatcher,
    serial_worker::{ProductionBackend, SerialBackend, WorkerEvent, WorkerHandle, WorkerOptions},
    store::ReceiveStore,
};
use ratatui::layout::{Position, Rect};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Port,
    Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClickAction {
    OpenPicker(Picker),
    Port(String),
    Mode(&'static str),
    Shortcut(KeyCode),
    Editor,
    Cancel,
    ClosePicker,
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
    pub input_focused: bool,
    pub input: LineEditor,
    send_draft: LineEditor,
    send_history: SendHistory,
    pub send_mode: SendMode,
    pub search_regex: bool,
    pub matches: Vec<usize>,
    pub selected_match: usize,
    pub search_truncated: bool,
    pub notice: Message,
    pub last_tx: Message,
    pub help: bool,
    pub click_targets: Vec<(Rect, ClickAction)>,
    pub receive_area: Rect,
    /// Text from the last rendered RX viewport, without borders or overlays.
    pub visible_receive: Option<String>,
    pub picker: Option<Picker>,
    pub picker_area: Rect,
    pub picker_index: usize,
    pub picker_offset: usize,
    pub picker_rows: usize,
    pub capture: Option<CaptureHandle>,
    pub capture_path: Option<PathBuf>,
    pub capture_result: Message,
    pub capture_failed: bool,
    next_id: u64,
    cycle_port_on_refresh: bool,
    search_was_paused: bool,
    observed_rx: u64,
    observed_capture: (u64, usize, bool),
    pending_clipboard: Option<String>,
}

impl App {
    pub fn new(mut config: Config) -> Result<Self, Message> {
        let send_mode = config.send_mode()?;
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
            input_focused: false,
            input: LineEditor::default(),
            send_draft: LineEditor::default(),
            send_history: SendHistory::default(),
            send_mode,
            search_regex: false,
            matches: Vec::new(),
            selected_match: 0,
            search_truncated: false,
            notice: Key::Startup.into(),
            last_tx: Key::Empty.into(),
            help: false,
            click_targets: Vec::new(),
            receive_area: Rect::default(),
            visible_receive: None,
            picker: None,
            picker_area: Rect::default(),
            picker_index: 0,
            picker_offset: 0,
            picker_rows: 1,
            capture: None,
            capture_path: None,
            capture_result: Key::RecordingOff.into(),
            capture_failed: false,
            next_id: 1,
            cycle_port_on_refresh: false,
            search_was_paused: false,
            observed_rx: 0,
            observed_capture: (0, 0, false),
            pending_clipboard: None,
        };
        if let Some(path) = app.config.record.clone() {
            app.start_capture(&path)?;
        }
        app.worker.refresh_ports()?;
        if app.config.demo || app.config.connect_on_start {
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
                            self.notice = Key::NoPortsHint.into();
                        } else {
                            let next = self
                                .ports
                                .iter()
                                .position(|p| p == &self.config.port)
                                .map_or(0, |i| (i + 1) % self.ports.len());
                            self.config.port.clone_from(&self.ports[next]);
                            self.notice = msg!(PortSelected, self.config.port);
                        }
                    } else if self.config.port.is_empty() {
                        self.config.port = self.ports.first().cloned().unwrap_or_default();
                    }
                    self.cycle_port_on_refresh = false;
                }
                WorkerEvent::Opened(port) => {
                    self.connected = true;
                    self.connecting = false;
                    self.notice = msg!(PortConnected, port);
                }
                WorkerEvent::Closed { error } => {
                    self.connected = false;
                    self.connecting = false;
                    self.notice = error
                        .map(Message::from)
                        .unwrap_or_else(|| Key::Disconnected.into());
                }
                WorkerEvent::TxCompleted { id, count } => {
                    self.last_tx = msg!(TxCompleted, id, count)
                }
                WorkerEvent::TxFailed { id, message } => {
                    self.last_tx = Message::TxFailed { id, error: message }
                }
                WorkerEvent::ControlError(message) => self.notice = message.into(),
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

    pub fn toggle_connection(&mut self) -> Result<(), Message> {
        if self.connected || self.connecting {
            self.worker.close()?;
            self.notice = Key::Disconnecting.into();
        } else {
            let config = self.config.serial_config()?;
            config.validate_typed()?;
            self.worker.open(config)?;
            self.connecting = true;
            self.notice = Key::ConnectingNotice.into();
        }
        Ok(())
    }

    fn choose_port(&mut self) -> Result<(), Message> {
        if self.connected || self.connecting {
            return Err(Key::DisconnectPort.into());
        }
        self.worker.refresh_ports()?;
        self.cycle_port_on_refresh = true;
        self.notice = Key::RefreshingPorts.into();
        Ok(())
    }

    pub fn start_capture(&mut self, path: &Path) -> Result<(), Message> {
        if self.capture.is_some() {
            return Err(Key::StopRecordingFirst.into());
        }
        let capture = CaptureHandle::start(path, self.config.record_queue_kib * 1024)
            .map_err(|error| Message::Context(Key::RecordStartError, Box::new(error.into())))?;
        self.worker.set_capture(Some(capture.sink()))?;
        self.capture = Some(capture);
        self.capture_path = Some(path.into());
        self.capture_result = Key::RecordingRx.into();
        self.capture_failed = false;
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Result<(), Message> {
        self.worker.set_capture(None)?;
        if let Some(mut capture) = self.capture.take() {
            let result = capture.finish().map_err(Message::from);
            self.capture_failed = result.is_err();
            self.capture_result = match &result {
                Ok(()) => msg!(SavedBytes, capture.progress().written_bytes),
                Err(error) => error.clone(),
            };
            result?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), Message> {
        self.worker.shutdown();
        if let Some(mut capture) = self.capture.take() {
            capture.finish().map_err(Message::from)?;
        }
        Ok(())
    }

    fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), Message> {
        if !self.connected {
            return Err(Key::ConnectBeforeSend.into());
        }
        let id = self.next_id;
        self.worker.send(id, bytes)?;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.last_tx = msg!(TxQueued, id);
        Ok(())
    }

    pub fn search(&mut self) -> Result<(), Message> {
        self.matches.clear();
        self.selected_match = 0;
        self.search_truncated = false;
        if let Some(matcher) = SearchMatcher::new_with_limits_typed(
            self.input.text(),
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
        self.notice = Message::SearchResults {
            count: self.matches.len(),
            capped: self.search_truncated,
        };
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
        self.notice = Key::Following.into();
    }

    fn begin_input(&mut self, mode: InputMode) {
        self.input_focused = true;
        // Refocusing the same editor must keep its draft and search pause state.
        if self.input_mode == mode {
            return;
        }
        if self.input_mode == InputMode::Send {
            self.send_history.reset_navigation();
            self.send_draft = std::mem::take(&mut self.input);
        } else {
            self.input.clear();
        }
        if mode == InputMode::Send {
            self.input = std::mem::take(&mut self.send_draft);
        }
        self.input_mode = mode;
        if mode == InputMode::Search {
            self.search_was_paused = self.paused;
            self.paused = true;
        }
    }

    pub fn insert_input(&mut self, text: &str) {
        let limit = if self.input_mode == InputMode::Send {
            self.config.send_kib * 1024
        } else {
            1024
        };
        // Reject the whole paste rather than silently sending a truncated command.
        if !self.input.insert(text, limit) {
            self.notice = msg!(InputLimit, limit);
            return;
        }
        self.send_history.reset_navigation();
    }

    pub fn handle_event(&mut self, event: Event) -> bool {
        let result = match event {
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => {
                // The next draw establishes fresh hit regions for the new size.
                self.click_targets.clear();
                self.receive_area = Rect::default();
                self.visible_receive = None;
                self.picker_area = Rect::default();
                Ok(false)
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                    return true;
                }
                self.handle_key(key)
            }
            Event::Paste(_) if self.help || self.picker.is_some() => Ok(false),
            Event::Paste(text)
                if self.input_focused
                    && matches!(
                        self.input_mode,
                        InputMode::Send | InputMode::Search | InputMode::Command
                    ) =>
            {
                self.insert_input(&text);
                Ok(false)
            }
            Event::Paste(text) if self.input_focused && self.input_mode == InputMode::Direct => {
                if text.len() > self.config.send_kib * 1024 {
                    Err(Key::PasteLimit.into())
                } else {
                    self.config
                        .text_encoding()
                        .and_then(|encoding| {
                            encode_text_typed(&text, encoding, LineEnding::None).map_err(Into::into)
                        })
                        .and_then(|bytes| self.send_bytes(bytes))
                        .map(|()| false)
                }
            }
            _ => Ok(false),
        };
        match result {
            Ok(quit) => quit,
            Err(error) => {
                self.notice = error;
                false
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool, Message> {
        // Theme switching is local in every mode, including direct serial input and overlays.
        if key.code == KeyCode::F(10) && key.modifiers.is_empty() {
            if key.kind == KeyEventKind::Press {
                self.cycle_theme();
            }
            return Ok(false);
        }
        if self.help {
            self.help = false;
            return Ok(false);
        }
        if self.picker.is_some() {
            match key.code {
                KeyCode::Esc => self.close_picker(),
                KeyCode::Up => self.move_picker(false),
                KeyCode::Down => self.move_picker(true),
                KeyCode::Enter => {
                    let action = match self.picker {
                        Some(Picker::Port) => self
                            .ports
                            .get(self.picker_index)
                            .cloned()
                            .map(ClickAction::Port),
                        Some(Picker::Mode) => ["text", "hex", "terminal"]
                            .get(self.picker_index)
                            .copied()
                            .map(ClickAction::Mode),
                        None => None,
                    };
                    if let Some(action) = action {
                        self.activate_click(action)?;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        if matches!(key.code, KeyCode::Char('f' | 'F'))
            && (!self.input_focused || self.input_mode == InputMode::View)
        {
            // Keep modifiers here: handle_view_key receives only the key code.
            if key.kind == KeyEventKind::Press && KeyModifiers::SHIFT.contains(key.modifiers) {
                self.pending_clipboard = Some(
                    self.visible_receive
                        .as_ref()
                        .filter(|text| !text.is_empty())
                        .ok_or(Key::NothingVisibleToCopy)?
                        .clone(),
                );
            }
            return Ok(false);
        }
        if !self.input_focused && key.code != KeyCode::Esc {
            return self.handle_view_key(key.code);
        }
        if self.input_mode == InputMode::Direct {
            if key.code == KeyCode::Esc {
                self.input_mode = InputMode::View;
                self.input_focused = false;
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
                    self.input_focused = false;
                    self.input.clear();
                    self.send_history.reset_navigation();
                }
                KeyCode::Enter => {
                    match self.input_mode {
                        InputMode::Send => {
                            self.send_bytes(parse_send_input_typed(
                                self.input.text(),
                                self.send_mode,
                                self.config.text_encoding()?,
                                self.config.ending()?,
                            )?)?;
                            self.send_history.record(self.input.text(), self.send_mode);
                        }
                        InputMode::Search => self.search()?,
                        InputMode::Command => self.command()?,
                        _ => {}
                    }
                    self.input_mode = InputMode::View;
                    self.input_focused = false;
                    self.input.clear();
                }
                KeyCode::Backspace => {
                    if self.input.backspace() {
                        self.send_history.reset_navigation();
                    }
                }
                KeyCode::Delete => {
                    if self.input.delete() {
                        self.send_history.reset_navigation();
                    }
                }
                KeyCode::Left => self.input.left(),
                KeyCode::Right => self.input.right(),
                KeyCode::Home => self.input.home(),
                KeyCode::End => self.input.end(),
                KeyCode::Up | KeyCode::Down if self.input_mode == InputMode::Send => {
                    self.send_history.navigate(
                        key.code == KeyCode::Up,
                        self.send_mode,
                        &mut self.input,
                        self.config.send_kib * 1024,
                    );
                }
                KeyCode::Tab if self.input_mode == InputMode::Command => {
                    let choices = self
                        .input
                        .complete_command(1024)
                        .map_err(|()| msg!(InputLimit, 1024))?;
                    self.notice = if choices.is_empty() {
                        Key::NoCompletion.into()
                    } else {
                        msg!(CompletionChoices, choices.join(" | "))
                    };
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                    self.send_history.reset_navigation();
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
                    self.insert_input(&ch.to_string())
                }
                _ => {}
            }
            return Ok(false);
        }
        self.handle_view_key(key.code)
    }

    /// Drain immediately after handling input, before the next tick or draw.
    pub fn take_clipboard_text(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    fn handle_view_key(&mut self, code: KeyCode) -> Result<bool, Message> {
        match code {
            KeyCode::F(10) => self.cycle_theme(),
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
                self.send_history.reset_navigation();
                self.send_mode = if self.send_mode == SendMode::Text {
                    SendMode::Hex
                } else {
                    SendMode::Text
                };
                self.config.send_mode = if self.send_mode == SendMode::Text {
                    "text"
                } else {
                    "hex"
                }
                .into();
            }
            KeyCode::F(8) => {
                if !self.connected {
                    return Err(Key::ConnectBeforeDirect.into());
                }
                self.config.mode = "terminal".into();
                self.display.invalidate();
                self.resume();
                self.begin_input(InputMode::Direct);
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
                self.offset = self.offset.saturating_sub(if code == KeyCode::Up {
                    1
                } else {
                    self.page_rows
                });
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.paused = true;
                self.offset = (self.offset
                    + if code == KeyCode::Down {
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
                self.selected_match = if code == KeyCode::Char('N') {
                    (self.selected_match + count - 1) % count
                } else {
                    (self.selected_match + 1) % count
                };
                self.jump_match();
            }
            KeyCode::Char('c') => {
                self.store
                    .lock()
                    .map_err(|_| Key::ReceiveStoreUnavailable)?
                    .clear();
                self.display = Display::default();
                self.resume();
                self.notice = Key::HistoryCleared.into();
            }
            _ => {}
        }
        Ok(false)
    }

    fn cycle_theme(&mut self) {
        self.config.theme.preset = self.config.theme.preset.next();
        self.notice = Message::Context(
            Key::ThemeChanged,
            Box::new(self.config.theme.preset.label().into()),
        );
    }

    fn close_picker(&mut self) {
        self.picker = None;
        self.click_targets.clear();
    }

    fn move_picker(&mut self, down: bool) {
        let count = match self.picker {
            Some(Picker::Port) => self.ports.len(),
            Some(Picker::Mode) => 3,
            None => return,
        };
        self.picker_index = if down {
            self.picker_index
                .saturating_add(1)
                .min(count.saturating_sub(1))
        } else {
            self.picker_index.saturating_sub(1)
        };
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<bool, Message> {
        let position = Position::new(mouse.column, mouse.row);
        let action = if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.click_targets
                .iter()
                .rev()
                .find(|(rect, _)| rect.contains(position))
                .map(|(_, action)| action.clone())
        } else {
            None
        };
        // Blur before handling outside clicks, including modal dismissal and errors.
        // Editor-opening actions explicitly restore focus below.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && (self.help || self.picker.is_some() || action != Some(ClickAction::Editor))
        {
            self.input_focused = false;
        }
        if self.help {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.help = false;
                self.click_targets.clear();
            }
            return Ok(false);
        }
        if self.picker.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    if self.picker_area.contains(position) =>
                {
                    self.move_picker(mouse.kind == MouseEventKind::ScrollDown);
                    return Ok(false);
                }
                MouseEventKind::Down(MouseButton::Left) if !self.picker_area.contains(position) => {
                    self.close_picker();
                    return Ok(false);
                }
                _ => {}
            }
        } else if self.receive_area.contains(position) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.paused = true;
                    self.offset = self.offset.saturating_sub(3);
                    return Ok(false);
                }
                MouseEventKind::ScrollDown => {
                    self.paused = true;
                    self.offset = self
                        .offset
                        .saturating_add(3)
                        .min(self.display.rows.len().saturating_sub(1));
                    return Ok(false);
                }
                _ => {}
            }
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Ok(false);
        }
        if let Some(action) = action {
            return self.activate_click(action);
        }
        Ok(false)
    }

    fn activate_click(&mut self, action: ClickAction) -> Result<bool, Message> {
        match action {
            ClickAction::OpenPicker(picker) => {
                if picker == Picker::Port {
                    if self.connected || self.connecting {
                        return Err(Key::DisconnectPort.into());
                    }
                    self.worker.refresh_ports()?;
                    self.cycle_port_on_refresh = false;
                }
                self.picker_index = match picker {
                    Picker::Port => self
                        .ports
                        .iter()
                        .position(|port| port == &self.config.port)
                        .unwrap_or(0),
                    Picker::Mode => ["text", "hex", "terminal"]
                        .iter()
                        .position(|mode| *mode == self.config.mode)
                        .unwrap_or(0),
                };
                self.picker_offset = 0;
                self.picker = Some(picker);
                self.click_targets.clear();
            }
            ClickAction::Port(port) => {
                if self.connected || self.connecting {
                    return Err(Key::DisconnectPort.into());
                }
                self.config.port = port;
                self.notice = msg!(PortSelected, self.config.port);
                self.close_picker();
            }
            ClickAction::Mode(mode) => {
                self.config.mode = mode.into();
                self.display.invalidate();
                self.resume();
                self.close_picker();
            }
            ClickAction::ClosePicker => self.close_picker(),
            ClickAction::Cancel => {
                self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?;
            }
            ClickAction::Editor => {
                if self.input_mode == InputMode::View {
                    self.begin_input(InputMode::Send);
                } else {
                    self.input_focused = true;
                }
            }
            ClickAction::Shortcut(code) => {
                match (code, self.input_mode) {
                    (KeyCode::Char('s'), InputMode::Send) => {
                        self.input_focused = true;
                        self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))?;
                    }
                    // Clicking the active editor's mode must not erase its draft.
                    (KeyCode::Char('/'), InputMode::Search)
                    | (KeyCode::Char(':'), InputMode::Command) => self.input_focused = true,
                    _ => {
                        return self.handle_view_key(code);
                    }
                }
            }
        }
        Ok(false)
    }

    fn command(&mut self) -> Result<(), Message> {
        let input = self.input.text().to_owned();
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
            "disconnect" => return self.worker.close().map_err(Into::into),
            "lang" => {
                self.config.language = Language::parse(value)?;
                return Ok(());
            }
            "ports" => {
                self.worker.refresh_ports()?;
                self.notice = Message::Raw(self.ports.join(" | "));
                return Ok(());
            }
            _ => {}
        }
        let mut config = self.config.clone();
        match name {
            "port" => config.port = value.into(),
            "baud" => config.baud = value.parse().map_err(|_| Key::InvalidBaud)?,
            "data" => config.data_bits = value.parse().map_err(|_| Key::InvalidDataBits)?,
            "stop" => config.stop_bits = value.parse().map_err(|_| Key::InvalidStopBits)?,
            "parity" => config.parity = value.into(),
            "flow" => config.flow = value.into(),
            "dtr" | "rts" => {
                let level = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(Key::UseOnOff.into()),
                };
                if name == "dtr" {
                    self.worker.set_dtr(level)?;
                    config.dtr = level;
                } else {
                    if config.flow == "hardware" {
                        return Err(Key::HardwareRts.into());
                    }
                    self.worker.set_rts(level)?;
                    config.rts = level;
                }
            }
            "mode" => config.mode = value.into(),
            "encoding" => config.encoding = value.into(),
            "eol" => config.line_ending = value.into(),
            _ => return Err(Key::UnknownCommand.into()),
        }
        if matches!(name, "port" | "baud" | "data" | "stop" | "parity" | "flow")
            && (self.connected || self.connecting)
        {
            return Err(Key::DisconnectSettings.into());
        }
        config.validate()?;
        self.config = config;
        if matches!(name, "mode" | "encoding") {
            self.display.invalidate();
            self.resume();
        }
        self.notice = msg!(SettingChanged, name, value);
        Ok(())
    }
}

fn direct_key(
    key: KeyEvent,
    encoding: escom_core::model::TextEncoding,
) -> Result<Option<Vec<u8>>, Message> {
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
            return encode_text_typed(&ch.to_string(), encoding, LineEnding::None)
                .map(Some)
                .map_err(Into::into);
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
    fn copy_shortcut_requires_a_press_with_only_optional_shift() {
        let mut app = App::new(Config::default()).unwrap();
        app.visible_receive = Some("last frame".into());
        for code in [KeyCode::Char('f'), KeyCode::Char('F')] {
            for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
                app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
                assert_eq!(app.take_clipboard_text().as_deref(), Some("last frame"));
            }
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ] {
                app.handle_event(Event::Key(KeyEvent::new(code, modifiers)));
                assert!(app.take_clipboard_text().is_none());
            }
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                app.handle_event(Event::Key(KeyEvent::new_with_kind(
                    code,
                    KeyModifiers::NONE,
                    kind,
                )));
                assert!(app.take_clipboard_text().is_none());
            }
        }
        app.shutdown().unwrap();
    }

    #[test]
    fn copy_shortcut_does_not_intercept_focused_editors_or_direct_input() {
        let mut app = App::new(Config {
            demo: true,
            ..Config::default()
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !app.connected {
            app.tick();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.visible_receive = Some("visible RX".into());
        for mode in [InputMode::Send, InputMode::Search, InputMode::Command] {
            app.begin_input(mode);
            app.input.clear();
            app.handle_event(key(KeyCode::Char('f')));
            app.handle_event(key(KeyCode::Char('F')));
            assert_eq!(app.input.text(), "fF");
            assert!(app.take_clipboard_text().is_none());
            app.input_focused = false;
            app.handle_event(key(KeyCode::Char('f')));
            assert_eq!(app.take_clipboard_text().as_deref(), Some("visible RX"));
            assert_eq!(app.input.text(), "fF");
        }
        app.begin_input(InputMode::Direct);
        app.handle_event(key(KeyCode::Char('f')));
        app.handle_event(key(KeyCode::Char('F')));
        assert!(app.take_clipboard_text().is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while app.worker.stats.tx_bytes() < 2 {
            app.tick();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.worker.stats.tx_bytes(), 2);
        app.shutdown().unwrap();
    }

    #[test]
    fn editor_inserts_at_cursor_and_preserves_send_draft_across_other_modes() {
        let mut app = App::new(Config::default()).unwrap();
        app.handle_event(key(KeyCode::Char('s')));
        app.handle_event(Event::Paste("A中B".into()));
        app.handle_event(key(KeyCode::Left));
        app.activate_click(ClickAction::Shortcut(KeyCode::Char(':')))
            .unwrap();
        app.insert_input("enc");
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.input.text(), "encoding ");
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Char('s')));
        app.handle_event(Event::Paste("X".into()));
        assert_eq!(app.input.text(), "A中XB");
        app.handle_event(key(KeyCode::Home));
        app.handle_event(key(KeyCode::Delete));
        assert_eq!(app.input.text(), "中XB");
        app.handle_event(key(KeyCode::End));
        app.handle_event(key(KeyCode::Backspace));
        assert_eq!(app.input.text(), "中X");
        app.activate_click(ClickAction::Shortcut(KeyCode::Char('/')))
            .unwrap();
        app.insert_input("query");
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Char('s')));
        assert_eq!(app.input.text(), "中X");
        app.handle_event(key(KeyCode::Esc));
        app.handle_event(key(KeyCode::Char('s')));
        assert!(app.input.text().is_empty());
        app.shutdown().unwrap();
    }

    #[test]
    fn failed_submission_preserves_edit_position_and_does_not_add_history() {
        let mut app = App::new(Config::default()).unwrap();
        app.handle_event(key(KeyCode::Char('s')));
        app.insert_input("AT");
        app.handle_event(key(KeyCode::Home));
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.input_mode, InputMode::Send);
        assert!(app.input_focused);
        app.insert_input("X");
        assert_eq!(app.input.text(), "XAT");
        app.handle_event(key(KeyCode::Up));
        assert_eq!(app.input.text(), "XAT");
        app.shutdown().unwrap();
    }

    #[test]
    fn accepted_sends_enter_history_and_browsing_does_not_send_or_scroll_rx() {
        let mut app = App::new(Config {
            demo: true,
            ..Config::default()
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !app.connected {
            app.tick();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.handle_event(key(KeyCode::Char('s')));
        app.insert_input("AT");
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.input_mode, InputMode::View);
        let next_id = app.next_id;
        app.handle_event(key(KeyCode::Char('s')));
        app.insert_input("draft");
        app.handle_event(key(KeyCode::Home));
        app.offset = 7;
        app.handle_event(key(KeyCode::Up));
        assert_eq!(app.input.text(), "AT");
        assert_eq!(app.offset, 7);
        assert!(!app.paused);
        assert_eq!(app.next_id, next_id);
        app.handle_event(key(KeyCode::Down));
        app.insert_input("original ");
        assert_eq!(app.input.text(), "original draft");
        app.handle_event(key(KeyCode::Up));
        app.insert_input("+GMR");
        assert_eq!(app.input.text(), "AT+GMR");
        app.handle_event(key(KeyCode::Up));
        assert_eq!(app.input.text(), "AT");
        app.handle_event(key(KeyCode::Down));
        assert_eq!(app.input.text(), "AT+GMR");
        app.shutdown().unwrap();
    }

    #[test]
    fn f10_cycles_themes_in_every_mode_without_changing_session_or_drafts() {
        use crate::theme::Preset;
        let mut app = App::new(Config::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        app.start_capture(&dir.path().join("theme.bin")).unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"keep history\n".to_vec());
        app.paused = true;
        app.offset = 7;
        app.matches = vec![1, 3];
        app.input = "AT+草稿".into();
        for mode in [
            InputMode::View,
            InputMode::Send,
            InputMode::Search,
            InputMode::Command,
            InputMode::Direct,
        ] {
            app.input_mode = mode;
            let original_config = app.config.clone();
            let initial_preset = app.config.theme.preset;
            for expected in [
                initial_preset.next(),
                initial_preset.next().next(),
                initial_preset.next().next().next(),
                initial_preset,
            ] {
                assert!(!app.handle_event(key(KeyCode::F(10))));
                assert_eq!(app.config.theme.preset, expected);
                assert_eq!(app.input_mode, mode);
                assert_eq!(app.input.text(), "AT+草稿");
                assert!(app.paused);
                assert_eq!(app.offset, 7);
                assert_eq!(app.matches, vec![1, 3]);
                assert!(app.capture.is_some());
                assert_eq!(app.store.lock().unwrap().bytes_len(), 13);
                assert_eq!(app.worker.stats.tx_bytes(), 0);
            }
            assert_eq!(app.config, original_config);
        }
        app.help = true;
        app.handle_event(key(KeyCode::F(10)));
        assert!(app.help);
        assert_eq!(app.config.theme.preset, Preset::Pink);
        app.help = false;
        app.picker = Some(Picker::Mode);
        app.picker_index = 2;
        app.handle_event(key(KeyCode::F(10)));
        assert_eq!(app.picker, Some(Picker::Mode));
        assert_eq!(app.picker_index, 2);
        assert_eq!(app.config.theme.preset, Preset::Midnight);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            app.handle_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::F(10),
                KeyModifiers::NONE,
                kind,
            )));
            assert_eq!(app.config.theme.preset, Preset::Midnight);
        }
        app.shutdown().unwrap();
    }

    #[test]
    fn saved_send_mode_loads_and_toggle_updates_config_without_auto_connecting() {
        let mut app = App::new(Config {
            port: "COM42".into(),
            send_mode: "hex".into(),
            ..Config::default()
        })
        .unwrap();
        assert_eq!(app.send_mode, SendMode::Hex);
        assert!(!app.connecting);
        assert!(!app.connected);
        app.handle_event(key(KeyCode::F(7)));
        assert_eq!(app.send_mode, SendMode::Text);
        assert_eq!(app.config.send_mode, "text");
        app.shutdown().unwrap();
    }

    #[test]
    fn switching_language_preserves_history_search_encoding_and_recording() {
        let mut app = App::new(Config {
            encoding: "gbk".into(),
            ..Config::default()
        })
        .unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), vec![0xd6, 0xd0, b'\n']);
        app.tick();
        app.paused = true;
        app.matches = vec![0];
        app.notice = msg!(PortSelected, "设备{0}");
        let original_rows = app.display.rows.clone();
        let dir = tempfile::tempdir().unwrap();
        app.start_capture(&dir.path().join("rx.bin")).unwrap();
        app.handle_event(key(KeyCode::Char(':')));
        app.insert_input("lang zh-CN");
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.config.language, Language::ZhCn);
        assert_eq!(app.config.encoding, "gbk");
        assert!(app.paused);
        assert_eq!(app.display.rows, original_rows);
        assert_eq!(app.matches, vec![0]);
        assert!(app.capture.is_some());
        assert_eq!(
            app.notice.render(app.config.language),
            "已选择 设备{0} | F3 连接"
        );
        app.handle_event(key(KeyCode::Char(':')));
        app.insert_input("lang fr");
        app.handle_event(key(KeyCode::Enter));
        assert_eq!(app.config.language, Language::ZhCn);
        assert!(
            app.notice
                .render(app.config.language)
                .contains("不支持的语言")
        );
        app.shutdown().unwrap();
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
        app.insert_input("warn");
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
        app.insert_input("串口");
        app.insert_input(&"x".repeat(app.config.send_kib * 1024));
        assert_eq!(app.input.text(), "串口");
        app.handle_event(key(KeyCode::Backspace));
        assert_eq!(app.input.text(), "串");
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
