use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText};
use serialport::{DataBits, FlowControl, Parity, StopBits};

use crate::fonts::{FontCatalog, data_font_family};
use crate::formatting::{
    FormattedRow, display_text, format_snapshot, parse_send_input, render_export,
};
use crate::model::{
    HistoryItem, LineEnding, ReceiveMode, SendMode, SerialConfig, TextEncoding, data_bits_label,
    flow_control_label, parity_label, stop_bits_label,
};
use crate::serial_worker::{WorkerEvent, WorkerHandle};
use crate::settings::{self, BUFFER_LIMIT_OPTIONS_MIB, UiPreferences};
use crate::store::ReceiveStore;

const FORMAT_DEBOUNCE: Duration = Duration::from_millis(80);
const PORT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const MAX_HISTORY: usize = 50;
const CONNECTION_CONTROL_HEIGHT: f32 = 32.0;
const CONFIG_LABEL_WIDTH: f32 = 48.0;
const CONFIG_COMBO_WIDTH: f32 = 100.0;
const SETTINGS_LABEL_WIDTH: f32 = 60.0;
const SETTINGS_CONTROLS_WIDTH: f32 = 378.0;

#[derive(Debug, Clone)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected(String),
}

impl ConnectionState {
    fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }

    fn is_disconnected(&self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

struct RepeatState {
    bytes: Vec<u8>,
    history: HistoryItem,
    next_send: Instant,
}

struct Notice {
    message: String,
    expires_at: Instant,
    error: bool,
}

enum BackgroundEvent {
    Formatted {
        token: u64,
        generation: u64,
        rows: Vec<FormattedRow>,
    },
    Exported(Result<PathBuf, String>),
}

pub struct EscomApp {
    preferences: UiPreferences,
    serial_config: SerialConfig,
    connection: ConnectionState,
    ports: Vec<String>,
    store: Arc<Mutex<ReceiveStore>>,
    worker: WorkerHandle,
    font_catalog: FontCatalog,

    send_input: String,
    terminal_input: String,
    terminal_history_index: Option<usize>,
    focus_terminal_input: bool,
    send_error: Option<String>,
    history: VecDeque<HistoryItem>,
    pending_history: HashMap<u64, HistoryItem>,
    next_send_id: u64,
    repeat: Option<RepeatState>,

    display_rows: Arc<Vec<FormattedRow>>,
    display_generation: u64,
    format_token: u64,
    format_in_progress: bool,
    force_format: bool,
    last_format_started: Instant,
    paused: bool,
    force_scroll_bottom: bool,
    background_tx: Sender<BackgroundEvent>,
    background_rx: Receiver<BackgroundEvent>,

    settings_open: bool,
    preferences_dirty_since: Option<Instant>,
    last_port_refresh: Instant,
    notice: Option<Notice>,
}

impl EscomApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let mut preferences = settings::load();
        preferences.sanitize();
        let font_catalog = FontCatalog::load();
        let applied_fonts = font_catalog.apply(
            &creation_context.egui_ctx,
            &preferences.ui_font_family,
            &preferences.data_font_family,
            preferences.ui_font_weight,
            preferences.data_font_weight,
            preferences.ui_font_size,
        );
        preferences.ui_font_family = applied_fonts.ui_family;
        preferences.data_font_family = applied_fonts.data_family;
        preferences.ui_font_weight = applied_fonts.ui_weight;
        preferences.data_font_weight = applied_fonts.data_weight;

        let store = Arc::new(Mutex::new(ReceiveStore::new(
            preferences.buffer_limit_bytes(),
        )));
        let worker = WorkerHandle::spawn(Arc::clone(&store));
        let _ = worker.refresh_ports();
        let (background_tx, background_rx) = unbounded();
        let notice = (!applied_fonts.warnings.is_empty()).then(|| Notice {
            message: applied_fonts.warnings.join("；"),
            expires_at: Instant::now() + NOTICE_DURATION,
            error: true,
        });

        Self {
            preferences,
            serial_config: SerialConfig::default(),
            connection: ConnectionState::Disconnected,
            ports: Vec::new(),
            store,
            worker,
            font_catalog,
            send_input: String::new(),
            terminal_input: String::new(),
            terminal_history_index: None,
            focus_terminal_input: false,
            send_error: None,
            history: VecDeque::new(),
            pending_history: HashMap::new(),
            next_send_id: 1,
            repeat: None,
            display_rows: Arc::new(Vec::new()),
            display_generation: u64::MAX,
            format_token: 0,
            format_in_progress: false,
            force_format: true,
            last_format_started: Instant::now() - FORMAT_DEBOUNCE,
            paused: false,
            force_scroll_bottom: false,
            background_tx,
            background_rx,
            settings_open: false,
            preferences_dirty_since: None,
            last_port_refresh: Instant::now() - PORT_REFRESH_INTERVAL,
            notice,
        }
    }

    fn process_worker_events(&mut self) {
        let events: Vec<_> = self.worker.events.try_iter().collect();
        for event in events {
            match event {
                WorkerEvent::Ports(ports) => {
                    self.ports = ports;
                    if self.serial_config.port_name.is_empty()
                        && let Some(first) = self.ports.first()
                    {
                        self.serial_config.port_name.clone_from(first);
                    }
                }
                WorkerEvent::Opened(port_name) => {
                    self.connection = ConnectionState::Connected(port_name.clone());
                    self.send_error = None;
                    self.set_notice(format!("已连接 {port_name}"), false);
                }
                WorkerEvent::Closed => {
                    let was_active = !self.connection.is_disconnected();
                    self.connection = ConnectionState::Disconnected;
                    self.repeat = None;
                    self.pending_history.clear();
                    if was_active {
                        self.set_notice("串口已断开", false);
                    }
                }
                WorkerEvent::TxCompleted { id, count } => {
                    if let Some(item) = self.pending_history.remove(&id) {
                        self.push_history(item);
                    }
                    self.send_error = None;
                    if count == 0 {
                        self.set_notice("没有数据被发送", true);
                    }
                }
                WorkerEvent::PortError(message) => {
                    self.repeat = None;
                    self.send_error = Some(message.clone());
                    self.set_notice(message, true);
                }
                WorkerEvent::ControlError(message) => self.set_notice(message, true),
            }
        }
    }

    fn process_background_events(&mut self) {
        let events: Vec<_> = self.background_rx.try_iter().collect();
        for event in events {
            match event {
                BackgroundEvent::Formatted {
                    token,
                    generation,
                    rows,
                } => {
                    self.format_in_progress = false;
                    if token == self.format_token && !self.paused {
                        self.display_rows = Arc::new(rows);
                        self.display_generation = generation;
                        self.force_format = false;
                    } else {
                        self.force_format = true;
                    }
                }
                BackgroundEvent::Exported(result) => match result {
                    Ok(path) => {
                        self.set_notice(format!("已导出到 {}", path.display()), false);
                    }
                    Err(message) => self.set_notice(message, true),
                },
            }
        }
    }

    fn maybe_refresh_ports(&mut self) {
        if self.connection.is_disconnected()
            && self.last_port_refresh.elapsed() >= PORT_REFRESH_INTERVAL
        {
            if let Err(message) = self.worker.refresh_ports() {
                self.set_notice(message, true);
            }
            self.last_port_refresh = Instant::now();
        }
    }

    fn maybe_start_format(&mut self, context: &egui::Context) {
        if self.paused || self.format_in_progress {
            return;
        }
        let generation = self
            .store
            .lock()
            .map(|store| store.generation())
            .unwrap_or(self.display_generation);
        if generation == self.display_generation && !self.force_format {
            return;
        }
        if !self.force_format && self.last_format_started.elapsed() < FORMAT_DEBOUNCE {
            context.request_repaint_after(FORMAT_DEBOUNCE - self.last_format_started.elapsed());
            return;
        }

        let snapshot = self.store.lock().ok().map(|store| store.snapshot());
        let Some(snapshot) = snapshot else {
            self.set_notice("接收缓存不可用", true);
            return;
        };
        let mode = self.preferences.receive_mode;
        let encoding = self.preferences.text_encoding;
        let token = self.format_token;
        let sender = self.background_tx.clone();
        let repaint_context = context.clone();
        self.format_in_progress = true;
        self.force_format = false;
        self.last_format_started = Instant::now();

        thread::Builder::new()
            .name("escom-format".into())
            .spawn(move || {
                let generation = snapshot.generation;
                let rows = format_snapshot(&snapshot, mode, encoding);
                let _ = sender.send(BackgroundEvent::Formatted {
                    token,
                    generation,
                    rows,
                });
                repaint_context.request_repaint();
            })
            .expect("failed to start formatting task");
    }

    fn process_repeat(&mut self, context: &egui::Context) {
        let Some(repeat) = self.repeat.as_ref() else {
            return;
        };
        if !self.connection.is_connected() {
            self.repeat = None;
            return;
        }
        let now = Instant::now();
        if now < repeat.next_send {
            context.request_repaint_after(repeat.next_send - now);
            return;
        }

        let bytes = repeat.bytes.clone();
        let history = repeat.history.clone();
        let interval = Duration::from_millis(self.preferences.repeat_interval_ms);
        if self.queue_payload(bytes, history).is_ok() {
            if let Some(repeat) = self.repeat.as_mut() {
                repeat.next_send = now + interval;
            }
            context.request_repaint_after(interval);
        } else {
            self.repeat = None;
        }
    }

    fn queue_current_input(&mut self) -> Result<(), String> {
        let bytes = self.current_payload()?;
        let history = HistoryItem {
            mode: self.preferences.send_mode,
            input: self.send_input.clone(),
        };
        self.queue_payload(bytes, history)
    }

    fn current_payload(&self) -> Result<Vec<u8>, String> {
        if !self.connection.is_connected() {
            return Err("请先连接串口".into());
        }
        parse_send_input(
            &self.send_input,
            self.preferences.send_mode,
            self.preferences.text_encoding,
            self.preferences.line_ending,
        )
    }

    fn queue_payload(&mut self, bytes: Vec<u8>, history: HistoryItem) -> Result<(), String> {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);
        self.worker.send(id, bytes)?;
        self.pending_history.insert(id, history);
        self.send_error = None;
        Ok(())
    }

    fn push_history(&mut self, item: HistoryItem) {
        if self.history.front() == Some(&item) {
            return;
        }
        self.history.retain(|existing| existing != &item);
        self.history.push_front(item);
        self.history.truncate(MAX_HISTORY);
    }

    fn start_repeat(&mut self, context: &egui::Context) {
        match self.current_payload() {
            Ok(bytes) => {
                let history = HistoryItem {
                    mode: self.preferences.send_mode,
                    input: self.send_input.clone(),
                };
                if let Err(message) = self.queue_payload(bytes.clone(), history.clone()) {
                    self.send_error = Some(message);
                    return;
                }
                let interval = Duration::from_millis(self.preferences.repeat_interval_ms);
                self.repeat = Some(RepeatState {
                    bytes,
                    history,
                    next_send: Instant::now() + interval,
                });
                context.request_repaint_after(interval);
            }
            Err(message) => self.send_error = Some(message),
        }
    }

    fn set_notice(&mut self, message: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            message: message.into(),
            expires_at: Instant::now() + NOTICE_DURATION,
            error,
        });
    }

    fn invalidate_format(&mut self) {
        self.format_token = self.format_token.wrapping_add(1);
        self.force_format = true;
    }

    fn mark_preferences_dirty(&mut self) {
        self.preferences_dirty_since = Some(Instant::now());
    }

    fn save_preferences_if_due(&mut self) {
        let Some(dirty_since) = self.preferences_dirty_since else {
            return;
        };
        if dirty_since.elapsed() < Duration::from_millis(750) {
            return;
        }
        match settings::save(&self.preferences) {
            Ok(()) => self.preferences_dirty_since = None,
            Err(message) => {
                self.preferences_dirty_since = None;
                self.set_notice(message, true);
            }
        }
    }

    fn show_connection_panel(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::top("connection_panel")
            .resizable(false)
            .show(root_ui, |ui| {
                ui.add_space(4.0);
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(76.0, CONNECTION_CONTROL_HEIGHT),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.label(RichText::new("ESCOM").size(20.0).strong());
                        },
                    );
                    ui.separator();

                    let editable = self.connection.is_disconnected();
                    ui.add_enabled_ui(editable, |ui| {
                        egui::ComboBox::from_id_salt("serial_port")
                            .selected_text(self.port_display_name())
                            .width(112.0)
                            .show_ui(ui, |ui| {
                                for port in &self.ports {
                                    ui.selectable_value(
                                        &mut self.serial_config.port_name,
                                        port.clone(),
                                        port,
                                    );
                                }
                            });
                        if ui
                            .add_sized([56.0, CONNECTION_CONTROL_HEIGHT], egui::Button::new("刷新"))
                            .on_hover_text("重新扫描可用串口")
                            .clicked()
                        {
                            if let Err(message) = self.worker.refresh_ports() {
                                self.set_notice(message, true);
                            }
                            self.last_port_refresh = Instant::now();
                        }

                        toolbar_label(ui, "波特率", 52.0);
                        ui.add_sized(
                            [88.0, CONNECTION_CONTROL_HEIGHT],
                            egui::DragValue::new(&mut self.serial_config.baud_rate)
                                .range(1..=4_000_000)
                                .speed(100.0),
                        );
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let button_text = match self.connection {
                            ConnectionState::Disconnected => "打开串口",
                            ConnectionState::Connecting => "正在连接...",
                            ConnectionState::Connected(_) => "关闭串口",
                        };
                        let enabled = !matches!(self.connection, ConnectionState::Connecting);
                        if ui
                            .add_enabled_ui(enabled, |ui| {
                                ui.add_sized(
                                    [88.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Button::new(button_text),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            if self.connection.is_connected() {
                                self.repeat = None;
                                if let Err(message) = self.worker.close() {
                                    self.set_notice(message, true);
                                }
                            } else {
                                self.open_selected_port();
                            }
                        }
                        if ui
                            .add_sized([60.0, CONNECTION_CONTROL_HEIGHT], egui::Button::new("设置"))
                            .clicked()
                        {
                            self.settings_open = true;
                        }
                    });
                });

                ui.horizontal(|ui| {
                    ui.set_min_height(CONNECTION_CONTROL_HEIGHT);
                    let editable = self.connection.is_disconnected();
                    ui.add_enabled_ui(editable, |ui| {
                        config_combo(
                            ui,
                            "data_bits",
                            "数据位",
                            data_bits_label(self.serial_config.data_bits),
                            |ui| {
                                for value in [
                                    DataBits::Five,
                                    DataBits::Six,
                                    DataBits::Seven,
                                    DataBits::Eight,
                                ] {
                                    ui.selectable_value(
                                        &mut self.serial_config.data_bits,
                                        value,
                                        data_bits_label(value),
                                    );
                                }
                            },
                        );
                        config_combo(
                            ui,
                            "stop_bits",
                            "停止位",
                            stop_bits_label(self.serial_config.stop_bits),
                            |ui| {
                                for value in [StopBits::One, StopBits::Two] {
                                    ui.selectable_value(
                                        &mut self.serial_config.stop_bits,
                                        value,
                                        stop_bits_label(value),
                                    );
                                }
                            },
                        );
                        config_combo(
                            ui,
                            "parity",
                            "校验",
                            parity_label(self.serial_config.parity),
                            |ui| {
                                for value in [Parity::None, Parity::Odd, Parity::Even] {
                                    ui.selectable_value(
                                        &mut self.serial_config.parity,
                                        value,
                                        parity_label(value),
                                    );
                                }
                            },
                        );
                        config_combo(
                            ui,
                            "flow",
                            "流控",
                            flow_control_label(self.serial_config.flow_control),
                            |ui| {
                                for value in [
                                    FlowControl::None,
                                    FlowControl::Software,
                                    FlowControl::Hardware,
                                ] {
                                    ui.selectable_value(
                                        &mut self.serial_config.flow_control,
                                        value,
                                        flow_control_label(value),
                                    );
                                }
                            },
                        );
                    });

                    let dtr_changed = ui.checkbox(&mut self.serial_config.dtr, "DTR").changed();
                    if dtr_changed
                        && self.connection.is_connected()
                        && let Err(message) = self.worker.set_dtr(self.serial_config.dtr)
                    {
                        self.set_notice(message, true);
                    }
                    let rts_enabled = self.serial_config.flow_control != FlowControl::Hardware;
                    let rts_changed = ui
                        .add_enabled(
                            rts_enabled,
                            egui::Checkbox::new(&mut self.serial_config.rts, "RTS"),
                        )
                        .on_hover_text("硬件流控启用时 RTS 由驱动管理")
                        .changed();
                    if rts_changed
                        && self.connection.is_connected()
                        && let Err(message) = self.worker.set_rts(self.serial_config.rts)
                    {
                        self.set_notice(message, true);
                    }
                });
                ui.add_space(2.0);
            });
    }

    fn show_output_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        egui::CentralPanel::default().show(root_ui, |ui| {
            let mut preferences_changed = false;
            let mut display_changed = false;
            ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
            ui.horizontal(|ui| {
                toolbar_label(ui, "接收", 38.0);
                for mode in [ReceiveMode::Text, ReceiveMode::Hex, ReceiveMode::Terminal] {
                    let width = if mode == ReceiveMode::Terminal {
                        64.0
                    } else {
                        50.0
                    };
                    if ui
                        .add_sized(
                            [width, CONNECTION_CONTROL_HEIGHT],
                            egui::Button::selectable(
                                self.preferences.receive_mode == mode,
                                mode.label(),
                            ),
                        )
                        .clicked()
                        && self.preferences.receive_mode != mode
                    {
                        self.preferences.receive_mode = mode;
                        if mode == ReceiveMode::Terminal {
                            self.repeat = None;
                            self.focus_terminal_input = true;
                        }
                        display_changed = true;
                        preferences_changed = true;
                    }
                }
                egui::ComboBox::from_id_salt("text_encoding")
                    .selected_text(self.preferences.text_encoding.label())
                    .width(92.0)
                    .show_ui(ui, |ui| {
                        for encoding in [TextEncoding::Utf8, TextEncoding::Gbk] {
                            if ui
                                .selectable_value(
                                    &mut self.preferences.text_encoding,
                                    encoding,
                                    encoding.label(),
                                )
                                .changed()
                            {
                                display_changed = true;
                                preferences_changed = true;
                            }
                        }
                    });
                preferences_changed |= ui
                    .add_sized(
                        [76.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Checkbox::new(&mut self.preferences.timestamps, "时间戳"),
                    )
                    .changed();
                preferences_changed |= ui
                    .add_sized(
                        [88.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Checkbox::new(&mut self.preferences.auto_scroll, "自动滚动"),
                    )
                    .changed();

                let pause_label = if self.paused {
                    "继续显示"
                } else {
                    "暂停显示"
                };
                if ui
                    .add_sized(
                        [80.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Button::new(pause_label),
                    )
                    .clicked()
                {
                    self.paused = !self.paused;
                    self.invalidate_format();
                }
                if ui
                    .add_sized(
                        [64.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Button::new("到底部"),
                    )
                    .clicked()
                {
                    self.force_scroll_bottom = true;
                    self.preferences.auto_scroll = true;
                    preferences_changed = true;
                }

                toolbar_separator(ui);
                if ui
                    .add_sized(
                        [64.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Button::new("清接收"),
                    )
                    .clicked()
                {
                    self.clear_receive();
                }
                if ui
                    .add_sized(
                        [80.0, CONNECTION_CONTROL_HEIGHT],
                        egui::Button::new("全部清空"),
                    )
                    .clicked()
                {
                    self.clear_receive();
                    self.send_input.clear();
                    self.send_error = None;
                }
                if ui
                    .add_enabled_ui(self.receive_bytes_len() > 0, |ui| {
                        ui.add_sized(
                            [80.0, CONNECTION_CONTROL_HEIGHT],
                            egui::Button::new("导出 TXT"),
                        )
                    })
                    .inner
                    .clicked()
                {
                    self.export_snapshot(&context);
                }
            });
            if display_changed {
                self.invalidate_format();
            }
            if preferences_changed {
                self.mark_preferences_dirty();
            }

            ui.separator();
            if self.preferences.receive_mode == ReceiveMode::Terminal {
                let input_row_height = CONNECTION_CONTROL_HEIGHT + 12.0;
                let content_height = (ui.available_height() - input_row_height).max(0.0);
                let content = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), content_height),
                    Layout::top_down(Align::LEFT),
                    |ui| self.show_receive_content(ui),
                );
                let surface_clicked = ui.input(|input| {
                    input.pointer.primary_clicked()
                        && input
                            .pointer
                            .interact_pos()
                            .is_some_and(|position| content.response.rect.contains(position))
                });
                ui.separator();
                self.show_terminal_input(ui, surface_clicked);
            } else {
                self.show_receive_content(ui);
            }
        });
    }

    fn show_receive_content(&mut self, ui: &mut egui::Ui) {
        if self.display_rows.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(if self.paused {
                        "显示已暂停，接收仍在继续"
                    } else {
                        "等待串口数据"
                    })
                    .color(ui.visuals().weak_text_color()),
                );
            });
            return;
        }

        let row_height = self.preferences.data_font_size + 7.0;
        let data_font = FontId::new(self.preferences.data_font_size, data_font_family());
        let rows = Arc::clone(&self.display_rows);
        let timestamps = self.preferences.timestamps;
        let mut scroll_area = egui::ScrollArea::both()
            .id_salt("receive_output")
            .auto_shrink([false, false])
            .stick_to_bottom(self.preferences.auto_scroll);
        if self.force_scroll_bottom {
            scroll_area = scroll_area.vertical_scroll_offset(f32::INFINITY);
            self.force_scroll_bottom = false;
        }
        scroll_area.show_rows(ui, row_height, rows.len(), |ui, range| {
            for row in &rows[range] {
                let text = display_text(row, timestamps);
                ui.add(
                    egui::Label::new(RichText::new(text).font(data_font.clone()))
                        .selectable(true)
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            }
        });
    }

    fn show_terminal_input(&mut self, ui: &mut egui::Ui, surface_clicked: bool) {
        let data_font = FontId::new(self.preferences.data_font_size, data_font_family());
        let mut ending_changed = false;
        let input_response = ui
            .horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(20.0, CONNECTION_CONTROL_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.label(RichText::new(">").font(data_font.clone()).strong());
                    },
                );
                let input_width = (ui.available_width() - 148.0).max(120.0);
                let response = ui.add_sized(
                    [input_width, CONNECTION_CONTROL_HEIGHT],
                    egui::TextEdit::singleline(&mut self.terminal_input)
                        .font(data_font)
                        .hint_text("输入命令"),
                );
                toolbar_label(ui, "行尾", 40.0);
                egui::ComboBox::from_id_salt("terminal_line_ending")
                    .selected_text(self.preferences.line_ending.label())
                    .width(92.0)
                    .show_ui(ui, |ui| {
                        for ending in [
                            LineEnding::None,
                            LineEnding::Cr,
                            LineEnding::Lf,
                            LineEnding::CrLf,
                        ] {
                            ending_changed |= ui
                                .selectable_value(
                                    &mut self.preferences.line_ending,
                                    ending,
                                    ending.label(),
                                )
                                .changed();
                        }
                    });
                response
            })
            .inner;

        if ending_changed {
            self.mark_preferences_dirty();
        }
        if surface_clicked {
            self.focus_terminal_input = true;
        }
        let pointer_clicked = ui.input(|input| input.pointer.primary_clicked());
        if self.focus_terminal_input && !pointer_clicked {
            input_response.request_focus();
            ui.ctx().request_repaint();
        }
        if input_response.has_focus() && !pointer_clicked {
            self.focus_terminal_input = false;
        } else if self.focus_terminal_input && pointer_clicked {
            ui.ctx().request_repaint();
        }

        let enter_pressed = ui.input(|input| input.key_pressed(egui::Key::Enter));
        let history_up =
            input_response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::ArrowUp));
        let history_down =
            input_response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::ArrowDown));

        if history_up {
            self.navigate_terminal_history(true);
        } else if history_down {
            self.navigate_terminal_history(false);
        } else if input_response.changed() {
            self.terminal_history_index = None;
        }

        if enter_pressed && (input_response.has_focus() || input_response.lost_focus()) {
            self.submit_terminal_input();
        }
    }

    fn submit_terminal_input(&mut self) {
        let result = if self.connection.is_connected() {
            parse_send_input(
                &self.terminal_input,
                SendMode::Text,
                self.preferences.text_encoding,
                self.preferences.line_ending,
            )
        } else {
            Err("请先连接串口".into())
        };

        match result {
            Ok(bytes) => {
                let history = HistoryItem {
                    mode: SendMode::Text,
                    input: self.terminal_input.clone(),
                };
                match self.queue_payload(bytes, history) {
                    Ok(()) => {
                        self.terminal_input.clear();
                        self.terminal_history_index = None;
                        self.focus_terminal_input = true;
                        self.force_scroll_bottom = true;
                    }
                    Err(message) => {
                        self.send_error = Some(message.clone());
                        self.set_notice(message, true);
                    }
                }
            }
            Err(message) => {
                self.send_error = Some(message.clone());
                self.set_notice(message, true);
                self.focus_terminal_input = true;
            }
        }
    }

    fn navigate_terminal_history(&mut self, older: bool) {
        let commands: Vec<_> = self
            .history
            .iter()
            .filter(|item| item.mode == SendMode::Text)
            .map(|item| item.input.clone())
            .collect();
        if commands.is_empty() {
            return;
        }

        if older {
            let next_index = self
                .terminal_history_index
                .map_or(0, |index| (index + 1).min(commands.len() - 1));
            self.terminal_history_index = Some(next_index);
            self.terminal_input.clone_from(&commands[next_index]);
        } else {
            match self.terminal_history_index {
                Some(0) => {
                    self.terminal_history_index = None;
                    self.terminal_input.clear();
                }
                Some(index) => {
                    let next_index = index - 1;
                    self.terminal_history_index = Some(next_index);
                    self.terminal_input.clone_from(&commands[next_index]);
                }
                None => {}
            }
        }
        self.focus_terminal_input = true;
    }

    fn show_send_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        egui::Panel::bottom("send_panel")
            .resizable(true)
            .size_range(150.0..=280.0)
            .show(root_ui, |ui| {
                let repeat_running = self.repeat.is_some();
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                ui.horizontal(|ui| {
                    toolbar_label(ui, "发送", 38.0);
                    ui.add_enabled_ui(!repeat_running, |ui| {
                        for mode in [SendMode::Text, SendMode::Hex] {
                            if ui
                                .add_sized(
                                    [50.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Button::selectable(
                                        self.preferences.send_mode == mode,
                                        mode.label(),
                                    ),
                                )
                                .clicked()
                                && self.preferences.send_mode != mode
                            {
                                self.preferences.send_mode = mode;
                                self.mark_preferences_dirty();
                                self.send_error = None;
                            }
                        }
                    });

                    toolbar_label(ui, "行尾", 40.0);
                    let mut ending_changed = false;
                    ui.add_enabled_ui(
                        !repeat_running && self.preferences.send_mode == SendMode::Text,
                        |ui| {
                            egui::ComboBox::from_id_salt("line_ending")
                                .selected_text(self.preferences.line_ending.label())
                                .width(92.0)
                                .show_ui(ui, |ui| {
                                    for ending in [
                                        LineEnding::None,
                                        LineEnding::Cr,
                                        LineEnding::Lf,
                                        LineEnding::CrLf,
                                    ] {
                                        ending_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.line_ending,
                                                ending,
                                                ending.label(),
                                            )
                                            .changed();
                                    }
                                });
                        },
                    );
                    if ending_changed {
                        self.mark_preferences_dirty();
                    }

                    let selected_history = self.history_menu(ui);
                    if let Some(item) = selected_history {
                        self.preferences.send_mode = item.mode;
                        self.send_input = item.input;
                        self.send_error = None;
                        self.mark_preferences_dirty();
                    }
                    if ui
                        .add_sized(
                            [72.0, CONNECTION_CONTROL_HEIGHT],
                            egui::Button::new("清发送"),
                        )
                        .clicked()
                    {
                        self.send_input.clear();
                        self.send_error = None;
                    }

                    toolbar_separator(ui);
                    toolbar_label(ui, "间隔 ms", 64.0);
                    if ui
                        .add_enabled_ui(!repeat_running, |ui| {
                            ui.add_sized(
                                [72.0, CONNECTION_CONTROL_HEIGHT],
                                egui::DragValue::new(&mut self.preferences.repeat_interval_ms)
                                    .range(20..=3_600_000)
                                    .speed(10.0),
                            )
                        })
                        .inner
                        .changed()
                    {
                        self.mark_preferences_dirty();
                    }
                    let mut repeat_checkbox = repeat_running;
                    if ui
                        .add_sized(
                            [96.0, CONNECTION_CONTROL_HEIGHT],
                            egui::Checkbox::new(&mut repeat_checkbox, "循环发送"),
                        )
                        .changed()
                    {
                        if repeat_checkbox {
                            self.start_repeat(&context);
                        } else {
                            self.repeat = None;
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled_ui(
                                self.connection.is_connected() && !repeat_running,
                                |ui| {
                                    ui.add_sized(
                                        [64.0, CONNECTION_CONTROL_HEIGHT],
                                        egui::Button::new("发送"),
                                    )
                                },
                            )
                            .inner
                            .clicked()
                            && let Err(message) = self.queue_current_input()
                        {
                            self.send_error = Some(message);
                        }
                    });
                });

                let hint = match self.preferences.send_mode {
                    SendMode::Text => "输入要发送的文本",
                    SendMode::Hex => "例如：AA 01 FF 或 AA01FF",
                };
                let text_edit = egui::TextEdit::multiline(&mut self.send_input)
                    .font(FontId::new(
                        self.preferences.data_font_size,
                        data_font_family(),
                    ))
                    .hint_text(hint)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY);
                ui.add_enabled(!repeat_running, text_edit);
                if let Some(message) = &self.send_error {
                    ui.label(RichText::new(message).color(ui.visuals().error_fg_color));
                }
            });
    }

    fn show_status_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        egui::Panel::bottom("status_panel")
            .resizable(false)
            .exact_size(30.0)
            .show(root_ui, |ui| {
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                ui.horizontal(|ui| {
                    let (status, color) = match &self.connection {
                        ConnectionState::Disconnected => ("未连接".to_owned(), Color32::GRAY),
                        ConnectionState::Connecting => ("正在连接".to_owned(), Color32::YELLOW),
                        ConnectionState::Connected(port) => {
                            (format!("已连接 {port}"), Color32::from_rgb(40, 170, 90))
                        }
                    };
                    ui.label(RichText::new("●").color(color));
                    ui.label(status);
                    toolbar_separator(ui);
                    ui.label(format!("RX {} B", self.worker.stats.rx_bytes()));
                    ui.label(format!("TX {} B", self.worker.stats.tx_bytes()));

                    let (bytes_len, dropped) = self.store_status();
                    toolbar_separator(ui);
                    ui.label(format!("缓存 {}", human_bytes(bytes_len as u64)));
                    if dropped > 0 {
                        ui.label(
                            RichText::new(format!("已淘汰 {}", human_bytes(dropped)))
                                .color(Color32::YELLOW),
                        );
                    }
                    if self.format_in_progress {
                        ui.label("正在整理显示...");
                    }

                    if let Some(notice) = &self.notice
                        && Instant::now() < notice.expires_at
                    {
                        toolbar_separator(ui);
                        let color = if notice.error {
                            ui.visuals().error_fg_color
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(RichText::new(&notice.message).color(color));
                        context.request_repaint_after(
                            notice.expires_at.saturating_duration_since(Instant::now()),
                        );
                    }
                });
            });
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.notice = None;
        }
    }

    fn show_settings_window(&mut self, context: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = self.settings_open;
        let mut font_changed = false;
        let mut preferences_changed = false;
        let mut buffer_changed = false;
        egui::Window::new("界面设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .min_width(480.0)
            .show(context, |ui| {
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([18.0, 12.0])
                    .show(ui, |ui| {
                        settings_row_label(ui, "界面字体");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("ui_font")
                                .selected_text(&self.preferences.ui_font_family)
                                .width(260.0)
                                .show_ui(ui, |ui| {
                                    for family in self.font_catalog.ui_families() {
                                        font_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.ui_font_family,
                                                family.clone(),
                                                family,
                                            )
                                            .changed();
                                    }
                                });
                            let options = self
                                .font_catalog
                                .weight_options(&self.preferences.ui_font_family);
                            egui::ComboBox::from_id_salt("ui_font_weight")
                                .selected_text(self.font_catalog.weight_label(
                                    &self.preferences.ui_font_family,
                                    self.preferences.ui_font_weight,
                                ))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for option in options {
                                        font_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.ui_font_weight,
                                                option.value,
                                                option.label,
                                            )
                                            .changed();
                                    }
                                });
                        });
                        ui.end_row();

                        settings_row_label(ui, "界面字号");
                        settings_controls(ui, |ui| {
                            font_changed |= ui
                                .add_sized(
                                    [158.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Slider::new(
                                        &mut self.preferences.ui_font_size,
                                        10.0..=32.0,
                                    )
                                    .integer(),
                                )
                                .changed();
                        });
                        ui.end_row();

                        settings_row_label(ui, "数据字体");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("data_font")
                                .selected_text(&self.preferences.data_font_family)
                                .width(260.0)
                                .show_ui(ui, |ui| {
                                    for family in self.font_catalog.mono_families() {
                                        font_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.data_font_family,
                                                family.clone(),
                                                family,
                                            )
                                            .changed();
                                    }
                                });
                            let options = self
                                .font_catalog
                                .weight_options(&self.preferences.data_font_family);
                            egui::ComboBox::from_id_salt("data_font_weight")
                                .selected_text(self.font_catalog.weight_label(
                                    &self.preferences.data_font_family,
                                    self.preferences.data_font_weight,
                                ))
                                .width(110.0)
                                .show_ui(ui, |ui| {
                                    for option in options {
                                        font_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.data_font_weight,
                                                option.value,
                                                option.label,
                                            )
                                            .changed();
                                    }
                                });
                        });
                        ui.end_row();

                        settings_row_label(ui, "数据字号");
                        settings_controls(ui, |ui| {
                            if ui
                                .add_sized(
                                    [158.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Slider::new(
                                        &mut self.preferences.data_font_size,
                                        10.0..=32.0,
                                    )
                                    .integer(),
                                )
                                .changed()
                            {
                                preferences_changed = true;
                            }
                        });
                        ui.end_row();

                        settings_row_label(ui, "接收缓存");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("buffer_limit")
                                .selected_text(format!(
                                    "{} MiB",
                                    self.preferences.buffer_limit_mib
                                ))
                                .show_ui(ui, |ui| {
                                    for limit in BUFFER_LIMIT_OPTIONS_MIB {
                                        buffer_changed |= ui
                                            .selectable_value(
                                                &mut self.preferences.buffer_limit_mib,
                                                limit,
                                                format!("{limit} MiB"),
                                            )
                                            .changed();
                                    }
                                });
                        });
                        ui.end_row();
                    });

                ui.separator();
                ui.label(
                    RichText::new(
                        "字体仅从 Windows 已安装字体中选择；不支持的字重会自动恢复为该字体的默认字重。",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
            });
        self.settings_open = open;

        if font_changed {
            let applied = self.font_catalog.apply(
                context,
                &self.preferences.ui_font_family,
                &self.preferences.data_font_family,
                self.preferences.ui_font_weight,
                self.preferences.data_font_weight,
                self.preferences.ui_font_size,
            );
            self.preferences.ui_font_family = applied.ui_family;
            self.preferences.data_font_family = applied.data_family;
            self.preferences.ui_font_weight = applied.ui_weight;
            self.preferences.data_font_weight = applied.data_weight;
            if !applied.warnings.is_empty() {
                self.set_notice(applied.warnings.join("；"), true);
            }
            preferences_changed = true;
        }
        if buffer_changed {
            if let Ok(mut store) = self.store.lock() {
                store.set_limit(self.preferences.buffer_limit_bytes());
            }
            self.invalidate_format();
            preferences_changed = true;
        }
        if preferences_changed {
            self.mark_preferences_dirty();
        }
    }

    fn history_menu(&mut self, ui: &mut egui::Ui) -> Option<HistoryItem> {
        let mut selected = None;
        let button = egui::Button::new(format!("历史 ({})", self.history.len()))
            .min_size(egui::vec2(82.0, CONNECTION_CONTROL_HEIGHT));
        egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            if self.history.is_empty() {
                ui.label("暂无成功发送记录");
            } else {
                for item in &self.history {
                    let preview = history_preview(item);
                    if ui.button(preview).clicked() {
                        selected = Some(item.clone());
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("清除发送历史").clicked() {
                    self.history.clear();
                    ui.close();
                }
            }
        });
        selected
    }

    fn open_selected_port(&mut self) {
        if let Err(message) = self.serial_config.validate() {
            self.set_notice(message, true);
            return;
        }
        if !self.ports.contains(&self.serial_config.port_name) {
            self.set_notice("所选串口当前不可用，请刷新后重选", true);
            return;
        }
        match self.worker.open(self.serial_config.clone()) {
            Ok(()) => self.connection = ConnectionState::Connecting,
            Err(message) => self.set_notice(message, true),
        }
    }

    fn port_display_name(&self) -> String {
        if self.serial_config.port_name.is_empty() {
            return "选择串口".into();
        }
        if self.ports.contains(&self.serial_config.port_name) {
            self.serial_config.port_name.clone()
        } else {
            format!("{}（不可用）", self.serial_config.port_name)
        }
    }

    fn clear_receive(&mut self) {
        if let Ok(mut store) = self.store.lock() {
            store.clear();
        }
        self.invalidate_format();
        self.display_rows = Arc::new(Vec::new());
    }

    fn export_snapshot(&mut self, context: &egui::Context) {
        let snapshot = self.store.lock().ok().map(|store| store.snapshot());
        let Some(snapshot) = snapshot else {
            self.set_notice("无法读取接收缓存", true);
            return;
        };
        if snapshot.bytes_len == 0 {
            self.set_notice("接收区没有可导出的数据", true);
            return;
        }

        let file_name = format!("ESCOM_{}.txt", Local::now().format("%Y%m%d_%H%M%S"));
        let Some(path) = rfd::FileDialog::new()
            .set_title("导出接收数据")
            .add_filter("文本文件", &["txt"])
            .set_file_name(&file_name)
            .save_file()
        else {
            return;
        };

        let mode = self.preferences.receive_mode;
        let encoding = self.preferences.text_encoding;
        let timestamps = self.preferences.timestamps;
        let sender = self.background_tx.clone();
        let repaint_context = context.clone();
        thread::Builder::new()
            .name("escom-export".into())
            .spawn(move || {
                let rows = format_snapshot(&snapshot, mode, encoding);
                let bytes = render_export(&rows, timestamps);
                let result = std::fs::write(&path, bytes)
                    .map(|()| path)
                    .map_err(|error| format!("导出失败：{error}"));
                let _ = sender.send(BackgroundEvent::Exported(result));
                repaint_context.request_repaint();
            })
            .expect("failed to start export task");
    }

    fn receive_bytes_len(&self) -> usize {
        self.store
            .lock()
            .map(|store| store.bytes_len())
            .unwrap_or(0)
    }

    fn store_status(&self) -> (usize, u64) {
        self.store
            .lock()
            .map(|store| (store.bytes_len(), store.dropped_bytes()))
            .unwrap_or((0, 0))
    }
}

impl eframe::App for EscomApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_worker_events();
        self.process_background_events();
        self.maybe_refresh_ports();
        self.process_repeat(context);
        self.maybe_start_format(context);
        self.save_preferences_if_due();

        if self.connection.is_connected() {
            context.request_repaint_after(Duration::from_millis(40));
        }
        if self.preferences_dirty_since.is_some() {
            context.request_repaint_after(Duration::from_millis(750));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.show_connection_panel(ui);
        self.show_status_panel(ui);
        if self.preferences.receive_mode != ReceiveMode::Terminal {
            self.show_send_panel(ui);
        }
        self.show_output_panel(ui);
        self.show_settings_window(&context);
    }
}

impl Drop for EscomApp {
    fn drop(&mut self) {
        let _ = settings::save(&self.preferences);
        self.worker.shutdown();
    }
}

fn config_combo(
    ui: &mut egui::Ui,
    id: &'static str,
    label: &'static str,
    selected_text: &'static str,
    contents: impl FnOnce(&mut egui::Ui),
) {
    toolbar_label(ui, label, CONFIG_LABEL_WIDTH);
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text)
        .width(CONFIG_COMBO_WIDTH)
        .show_ui(ui, contents);
}

fn toolbar_label(ui: &mut egui::Ui, text: &'static str, width: f32) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, CONNECTION_CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(text);
        },
    );
}

fn toolbar_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, 22.0), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

fn settings_row_label(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(SETTINGS_LABEL_WIDTH, CONNECTION_CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(text);
        },
    );
}

fn settings_controls<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(SETTINGS_CONTROLS_WIDTH, CONNECTION_CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        contents,
    )
    .inner
}

fn history_preview(item: &HistoryItem) -> String {
    let normalized = item.input.replace(['\r', '\n'], " ");
    let mut preview: String = normalized.chars().take(36).collect();
    if normalized.chars().count() > 36 {
        preview.push('…');
    }
    format!("{}  {preview}", item.mode.label())
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_preview_is_single_line_and_bounded() {
        let preview = history_preview(&HistoryItem {
            mode: SendMode::Text,
            input: "line one\r\nline two with a very long suffix that is clipped".into(),
        });
        assert!(!preview.contains('\n'));
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human_bytes(7), "7 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
    }
}
