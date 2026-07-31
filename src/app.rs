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
    FormattedRow, display_text, encode_text, format_snapshot, parse_send_input, render_export,
};
use crate::icon;
use crate::model::{
    HistoryItem, LineEnding, ReceiveMode, SendMode, SerialConfig, TextEncoding, data_bits_label,
    flow_control_label, parity_label, stop_bits_label,
};
use crate::serial_worker::{WorkerEvent, WorkerHandle};
use crate::settings::{
    self, AppBackgroundSource, BUFFER_LIMIT_OPTIONS_MIB, DEFAULT_BACKGROUND_DARK_OPACITY,
    DEFAULT_BACKGROUND_LIGHT_OPACITY, UiPreferences,
};
use crate::store::ReceiveStore;

const FORMAT_DEBOUNCE: Duration = Duration::from_millis(80);
const PORT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const MAX_HISTORY: usize = 50;
const CONNECTION_CONTROL_HEIGHT: f32 = 32.0;
const CONFIG_LABEL_WIDTH: f32 = 48.0;
const CONFIG_COMBO_WIDTH: f32 = 100.0;
const SETTINGS_LABEL_WIDTH: f32 = 76.0;
const SETTINGS_CONTROLS_WIDTH: f32 = 476.0;
const SETTINGS_WINDOW_WIDTH: f32 = 660.0;
const SETTINGS_WINDOW_HEIGHT: f32 = 454.0;
const SEND_EDITOR_LIGHT_ALPHA: u8 = 120;
const SEND_EDITOR_DARK_ALPHA: u8 = 104;
const TITLE_BAR_HEIGHT: f32 = 36.0;
const TITLE_BAR_BUTTON_WIDTH: f32 = 46.0;
const TITLE_BAR_CONTROLS_WIDTH: f32 = TITLE_BAR_BUTTON_WIDTH * 3.0;
const WINDOW_RESIZE_BORDER: f32 = 5.0;

#[derive(Clone, Copy)]
enum TitleBarControl {
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Fonts,
    Background,
}

enum BackgroundLoadState {
    Idle,
    Loading,
    Ready,
    Error(String),
}

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
    title_icon: egui::TextureHandle,

    send_input: String,
    focus_terminal_surface: bool,
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
    settings_tab: SettingsTab,
    background_url_draft: String,
    background_load_uri: Option<String>,
    background_load_state: BackgroundLoadState,
    preferences_dirty_since: Option<Instant>,
    last_port_refresh: Instant,
    notice: Option<Notice>,
}

impl EscomApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let mut preferences = settings::load();
        preferences.sanitize();
        egui_extras::install_image_loaders(&creation_context.egui_ctx);
        creation_context
            .egui_ctx
            .set_theme(preferences.theme_preference);
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

        let icon_data = icon::icon_data();
        let title_icon = creation_context.egui_ctx.load_texture(
            "escom-title-icon",
            egui::ColorImage::from_rgba_unmultiplied(
                [icon_data.width as usize, icon_data.height as usize],
                &icon_data.rgba,
            ),
            egui::TextureOptions::LINEAR,
        );

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

        let background_url_draft = preferences.background_online_url.clone();

        Self {
            preferences,
            serial_config: SerialConfig::default(),
            connection: ConnectionState::Disconnected,
            ports: Vec::new(),
            store,
            worker,
            font_catalog,
            title_icon,
            send_input: String::new(),
            focus_terminal_surface: false,
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
            settings_tab: SettingsTab::Fonts,
            background_url_draft,
            background_load_uri: None,
            background_load_state: BackgroundLoadState::Idle,
            preferences_dirty_since: None,
            last_port_refresh: Instant::now() - PORT_REFRESH_INTERVAL,
            notice,
        }
    }

    fn background_image_uri(&self) -> Option<String> {
        match self.preferences.background_source {
            AppBackgroundSource::None => None,
            AppBackgroundSource::Local => {
                let path = self.preferences.background_local_path.trim();
                (!path.is_empty()).then(|| local_background_uri(path))
            }
            AppBackgroundSource::Online => {
                let url = self.preferences.background_online_url.trim();
                (!url.is_empty()).then(|| url.to_owned())
            }
        }
    }

    fn has_configured_background(&self) -> bool {
        match self.preferences.background_source {
            AppBackgroundSource::None => false,
            AppBackgroundSource::Local => !self.preferences.background_local_path.trim().is_empty(),
            AppBackgroundSource::Online => {
                !self.preferences.background_online_url.trim().is_empty()
            }
        }
    }

    fn active_background_opacity(&self, dark_mode: bool) -> f32 {
        if dark_mode {
            self.preferences.background_dark_opacity
        } else {
            self.preferences.background_light_opacity
        }
    }

    fn surface_fill(&self, base: Color32, alpha_with_background: u8) -> Color32 {
        if self.has_configured_background() {
            Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha_with_background)
        } else {
            base
        }
    }

    fn paint_app_background(&mut self, root_ui: &mut egui::Ui) {
        let rect = root_ui.max_rect();
        let base_fill = root_ui.visuals().panel_fill;
        root_ui.painter().rect_filled(rect, 0.0, base_fill);

        let Some(uri) = self.background_image_uri() else {
            self.background_load_uri = None;
            self.background_load_state = BackgroundLoadState::Idle;
            return;
        };

        if self.background_load_uri.as_deref() != Some(uri.as_str()) {
            self.background_load_uri = Some(uri.clone());
            self.background_load_state = BackgroundLoadState::Loading;
        }

        let context = root_ui.ctx().clone();
        match context.try_load_texture(
            &uri,
            egui::TextureOptions::LINEAR,
            egui::load::SizeHint::default(),
        ) {
            Ok(egui::load::TexturePoll::Ready { texture }) => {
                self.background_load_state = BackgroundLoadState::Ready;
                paint_texture_cover(
                    root_ui.painter(),
                    rect,
                    texture,
                    self.active_background_opacity(root_ui.visuals().dark_mode),
                );
            }
            Ok(egui::load::TexturePoll::Pending { .. }) => {
                self.background_load_state = BackgroundLoadState::Loading;
                context.request_repaint_after(Duration::from_millis(100));
            }
            Err(error) => {
                let message = error.to_string();
                let is_new_error = !matches!(
                    &self.background_load_state,
                    BackgroundLoadState::Error(previous) if previous == &message
                );
                self.background_load_state = BackgroundLoadState::Error(message.clone());
                if is_new_error {
                    self.set_notice(format!("背景图片加载失败：{message}"), true);
                }
            }
        }
    }

    fn reload_background_image(&mut self, context: &egui::Context) {
        if let Some(uri) = self.background_load_uri.take() {
            context.forget_image(&uri);
        }
        self.background_load_state = BackgroundLoadState::Idle;
        context.request_repaint();
    }

    fn show_title_bar(&self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
        let title_fill = if root_ui.visuals().dark_mode {
            Color32::from_rgb(31, 34, 40)
        } else {
            Color32::from_rgb(246, 247, 249)
        };
        let title_fill = self.surface_fill(title_fill, 218);

        egui::Panel::top("custom_title_bar")
            .resizable(false)
            .exact_size(TITLE_BAR_HEIGHT)
            .frame(egui::Frame::new().fill(title_fill).inner_margin(0.0))
            .show(root_ui, |ui| {
                let rect = ui.max_rect();
                let controls_left = rect.right() - TITLE_BAR_CONTROLS_WIDTH;
                let drag_rect =
                    egui::Rect::from_min_max(rect.min, egui::pos2(controls_left, rect.bottom()));
                let drag_response = ui.interact(
                    drag_rect,
                    ui.id().with("window_drag_area"),
                    egui::Sense::click_and_drag(),
                );
                if drag_response.double_clicked_by(egui::PointerButton::Primary) {
                    context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                } else if drag_response.is_pointer_button_down_on() {
                    context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(rect.left() + 18.0, rect.center().y),
                    egui::vec2(18.0, 18.0),
                );
                ui.painter().image(
                    self.title_icon.id(),
                    icon_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                ui.painter().text(
                    egui::pos2(icon_rect.right() + 8.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    "ESCOM",
                    FontId::new(14.0, egui::FontFamily::Proportional),
                    ui.visuals().text_color(),
                );

                let close_rect = title_bar_control_rect(rect, 0);
                let maximize_rect = title_bar_control_rect(rect, 1);
                let minimize_rect = title_bar_control_rect(rect, 2);

                if title_bar_control(ui, minimize_rect, TitleBarControl::Minimize).clicked() {
                    context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                let maximize_control = if maximized {
                    TitleBarControl::Restore
                } else {
                    TitleBarControl::Maximize
                };
                if title_bar_control(ui, maximize_rect, maximize_control).clicked() {
                    context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if title_bar_control(ui, close_rect, TitleBarControl::Close).clicked() {
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                ui.painter().hline(
                    rect.x_range(),
                    rect.bottom() - 0.5,
                    ui.visuals().widgets.noninteractive.bg_stroke,
                );
            });
    }

    fn handle_window_resize(&self, context: &egui::Context) {
        let (maximized, fullscreen, pointer_position, primary_pressed, viewport_rect) = context
            .input(|input| {
                (
                    input.viewport().maximized.unwrap_or(false),
                    input.viewport().fullscreen.unwrap_or(false),
                    input.pointer.hover_pos(),
                    input.pointer.primary_pressed(),
                    input.viewport_rect(),
                )
            });
        if maximized || fullscreen {
            return;
        }
        let Some(pointer_position) = pointer_position else {
            return;
        };
        let Some(direction) = window_resize_direction(viewport_rect, pointer_position) else {
            return;
        };

        context.set_cursor_icon(resize_cursor(direction));
        if primary_pressed {
            context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }

    fn paint_window_border(&self, context: &egui::Context) {
        if context.input(|input| input.viewport().maximized.unwrap_or(false)) {
            return;
        }
        let rect = context.input(|input| input.viewport_rect()).shrink(0.5);
        context
            .layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("custom_window_border"),
            ))
            .rect_stroke(
                rect,
                0.0,
                context.style_of(context.theme()).visuals.window_stroke,
                egui::StrokeKind::Inside,
            );
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
                    if self.preferences.receive_mode == ReceiveMode::Terminal {
                        self.focus_terminal_surface = true;
                    }
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

    fn queue_terminal_payload(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if !self.connection.is_connected() {
            return Err("请先连接串口".into());
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);
        self.worker.send(id, bytes)?;
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
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 202));
        egui::Panel::top("connection_panel")
            .resizable(false)
            .frame(panel_frame)
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
                            self.background_url_draft
                                .clone_from(&self.preferences.background_online_url);
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
        let panel_frame = egui::Frame::central_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 82));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(root_ui, |ui| {
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
                                self.focus_terminal_surface = true;
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
                    self.show_terminal_surface(ui);
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

    fn show_terminal_surface(&mut self, ui: &mut egui::Ui) {
        let content =
            ui.allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::LEFT), |ui| {
                self.show_receive_content(ui)
            });
        let response = ui
            .interact(
                content.response.rect,
                ui.id().with("terminal_surface"),
                egui::Sense::focusable_noninteractive(),
            )
            .on_hover_cursor(egui::CursorIcon::Text);
        let surface_clicked = ui.input(|input| {
            input.pointer.primary_clicked()
                && input
                    .pointer
                    .interact_pos()
                    .is_some_and(|position| response.rect.contains(position))
        });
        if surface_clicked || self.focus_terminal_surface {
            response.request_focus();
            self.focus_terminal_surface = false;
        }

        if !response.has_focus() {
            return;
        }
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });

        let events = ui.input(|input| input.events.clone());
        match terminal_bytes_from_events(
            &events,
            self.preferences.text_encoding,
            self.preferences.line_ending,
        ) {
            Ok(bytes) if !bytes.is_empty() => {
                if let Err(message) = self.queue_terminal_payload(bytes) {
                    self.send_error = Some(message.clone());
                    self.set_notice(message, true);
                }
            }
            Ok(_) => {}
            Err(message) => {
                self.send_error = Some(message.clone());
                self.set_notice(message, true);
            }
        }
    }

    fn show_send_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        let toolbar_gap = root_ui.spacing().item_spacing.y.round() as i8;
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .inner_margin(egui::Margin {
                left: 8,
                right: 8,
                top: toolbar_gap,
                bottom: 2,
            })
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 112));
        egui::Panel::bottom("send_panel")
            .resizable(true)
            .size_range(150.0..=280.0)
            .frame(panel_frame)
            .show(root_ui, |ui| {
                let repeat_running = self.repeat.is_some();
                let mut send_requested = false;
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                ui.horizontal(|ui| {
                    toolbar_label(ui, "发送", 38.0);
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
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
                    });

                    toolbar_separator(ui);
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

                    let selected_history = ui
                        .add_enabled_ui(!repeat_running, |ui| self.history_menu(ui))
                        .inner;
                    if let Some(item) = selected_history {
                        self.preferences.send_mode = item.mode;
                        self.send_input = item.input;
                        self.send_error = None;
                        self.mark_preferences_dirty();
                    }
                    let can_clear = !repeat_running
                        && (!self.send_input.is_empty() || self.send_error.is_some());
                    if ui
                        .add_enabled(
                            can_clear,
                            egui::Button::new("清发送")
                                .min_size(egui::vec2(72.0, CONNECTION_CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        self.send_input.clear();
                        self.send_error = None;
                    }

                    toolbar_separator(ui);
                    toolbar_label(ui, "循环间隔", 64.0);
                    if ui
                        .add_enabled_ui(!repeat_running, |ui| {
                            ui.add_sized(
                                [92.0, CONNECTION_CONTROL_HEIGHT],
                                egui::DragValue::new(&mut self.preferences.repeat_interval_ms)
                                    .range(20..=3_600_000)
                                    .speed(10.0)
                                    .suffix(" ms"),
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
                        let can_send = self.connection.is_connected() && !repeat_running;
                        let mut button = egui::Button::new(RichText::new("发送").strong());
                        if can_send {
                            button = button.fill(ui.visuals().selection.bg_fill);
                        }
                        send_requested = ui
                            .add_enabled(
                                can_send,
                                button.min_size(egui::vec2(72.0, CONNECTION_CONTROL_HEIGHT)),
                            )
                            .clicked();
                    });
                });

                let editor_id = ui.make_persistent_id("send_input_editor");
                let shortcut_requested = ui.memory(|memory| memory.has_focus(editor_id))
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                    });
                if (send_requested || shortcut_requested)
                    && let Err(message) = self.queue_current_input()
                {
                    self.send_error = Some(message);
                }

                let hint = match self.preferences.send_mode {
                    SendMode::Text => "输入要发送的文本",
                    SendMode::Hex => "例如：AA 01 FF 或 AA01FF",
                };
                let editor_alpha = if ui.visuals().dark_mode {
                    SEND_EDITOR_DARK_ALPHA
                } else {
                    SEND_EDITOR_LIGHT_ALPHA
                };
                let editor_fill =
                    self.surface_fill(ui.visuals().text_edit_bg_color(), editor_alpha);
                let text_edit = egui::TextEdit::multiline(&mut self.send_input)
                    .font(FontId::new(
                        self.preferences.data_font_size,
                        data_font_family(),
                    ))
                    .background_color(editor_fill)
                    .hint_text(hint)
                    .id(editor_id)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY);
                let error_height = if self.send_error.is_some() {
                    ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y
                } else {
                    0.0
                };
                let editor_height = (ui.available_height() - error_height).max(64.0);
                ui.add_enabled_ui(!repeat_running, |ui| {
                    ui.add_sized([ui.available_width(), editor_height], text_edit)
                });
                if let Some(message) = &self.send_error {
                    ui.label(RichText::new(message).color(ui.visuals().error_fg_color));
                }
            });
    }

    fn show_status_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        let mut theme_preference = self.preferences.theme_preference;
        let mut theme_changed = false;
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 210));
        egui::Panel::bottom("status_panel")
            .resizable(false)
            .exact_size(30.0)
            .frame(panel_frame)
            .show(root_ui, |ui| {
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                egui::containers::Sides::new().shrink_left().show(
                    ui,
                    |ui| {
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
                    },
                    |ui| {
                        theme_changed = show_theme_menu(ui, &mut theme_preference);
                    },
                );
            });
        if theme_changed {
            self.preferences.theme_preference = theme_preference;
            context.set_theme(theme_preference);
            self.mark_preferences_dirty();
        }
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
        let max_window_height = context.input(|input| {
            (input.viewport_rect().height() - 32.0).max(SETTINGS_WINDOW_HEIGHT + 48.0)
        });
        egui::Window::new("设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(SETTINGS_WINDOW_WIDTH)
            .min_width(SETTINGS_WINDOW_WIDTH)
            .max_height(max_window_height)
            .vscroll(true)
            .show(context, |ui| {
                ui.spacing_mut().interact_size.y = CONNECTION_CONTROL_HEIGHT;
                ui.set_min_size(egui::vec2(
                    SETTINGS_WINDOW_WIDTH - 32.0,
                    SETTINGS_WINDOW_HEIGHT,
                ));
                self.show_settings_tabs(ui);
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                match self.settings_tab {
                    SettingsTab::Fonts => self.show_font_settings_tab(ui, context),
                    SettingsTab::Background => self.show_background_settings_tab(ui, context),
                }
            });
        self.settings_open = open;
    }

    fn show_settings_tabs(&mut self, ui: &mut egui::Ui) {
        let gap = ui.spacing().item_spacing.x;
        let tab_width = (ui.available_width() - gap) * 0.5;
        ui.horizontal(|ui| {
            for (tab, label) in [
                (SettingsTab::Fonts, "字体与显示"),
                (SettingsTab::Background, "应用背景"),
            ] {
                if ui
                    .add_sized(
                        [tab_width, 38.0],
                        egui::Button::selectable(self.settings_tab == tab, label),
                    )
                    .clicked()
                {
                    self.settings_tab = tab;
                }
            }
        });
    }

    fn show_font_settings_tab(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let mut font_changed = false;
        let mut preferences_changed = false;
        let mut buffer_changed = false;

        settings_card(
            ui,
            "字体",
            "分别设置界面控件与串口数据所使用的字体。",
            |ui| {
                egui::Grid::new("font_settings_grid")
                    .num_columns(2)
                    .spacing([16.0, 12.0])
                    .show(ui, |ui| {
                        settings_row_label(ui, "界面字体");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("ui_font")
                                .selected_text(&self.preferences.ui_font_family)
                                .width(292.0)
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
                                .width(118.0)
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
                                    [220.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Slider::new(
                                        &mut self.preferences.ui_font_size,
                                        10.0..=32.0,
                                    )
                                    .integer()
                                    .suffix(" pt"),
                                )
                                .changed();
                        });
                        ui.end_row();

                        settings_row_label(ui, "数据字体");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("data_font")
                                .selected_text(&self.preferences.data_font_family)
                                .width(292.0)
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
                                .width(118.0)
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
                            preferences_changed |= ui
                                .add_sized(
                                    [220.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Slider::new(
                                        &mut self.preferences.data_font_size,
                                        10.0..=32.0,
                                    )
                                    .integer()
                                    .suffix(" pt"),
                                )
                                .changed();
                        });
                        ui.end_row();
                    });
            },
        );

        ui.add_space(10.0);
        settings_card(
            ui,
            "显示缓存",
            "限制接收缓存占用，达到上限后自动淘汰最早的数据。",
            |ui| {
                egui::Grid::new("buffer_settings_grid")
                    .num_columns(2)
                    .spacing([16.0, 0.0])
                    .show(ui, |ui| {
                        settings_row_label(ui, "接收缓存");
                        settings_controls(ui, |ui| {
                            egui::ComboBox::from_id_salt("buffer_limit")
                                .selected_text(format!("{} MiB", self.preferences.buffer_limit_mib))
                                .width(140.0)
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
            },
        );

        ui.add_space(9.0);
        ui.label(
            RichText::new(
                "字体来自 Windows 已安装字体；不支持的字重会自动恢复为该字体的默认字重。",
            )
            .small()
            .color(ui.visuals().weak_text_color()),
        );

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

    fn show_background_settings_tab(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let mut preferences_changed = false;
        let mut reload_requested = false;

        settings_card(
            ui,
            "背景来源",
            "背景会居中裁剪并铺满整个应用窗口。",
            |ui| {
                let gap = ui.spacing().item_spacing.x;
                let option_width = (ui.available_width() - gap * 2.0) / 3.0;
                ui.horizontal(|ui| {
                    for source in [
                        AppBackgroundSource::None,
                        AppBackgroundSource::Local,
                        AppBackgroundSource::Online,
                    ] {
                        if ui
                            .add_sized(
                                [option_width, CONNECTION_CONTROL_HEIGHT],
                                egui::Button::selectable(
                                    self.preferences.background_source == source,
                                    source.label(),
                                ),
                            )
                            .clicked()
                            && self.preferences.background_source != source
                        {
                            self.preferences.background_source = source;
                            preferences_changed = true;
                            reload_requested = true;
                        }
                    }
                });

                ui.add_space(10.0);
                match self.preferences.background_source {
                    AppBackgroundSource::None => {
                        ui.label(
                            RichText::new("当前使用主题的默认纯色背景。")
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                    AppBackgroundSource::Local => {
                        ui.horizontal(|ui| {
                            let mut path_display =
                                if self.preferences.background_local_path.trim().is_empty() {
                                    "尚未选择图片".to_owned()
                                } else {
                                    self.preferences.background_local_path.clone()
                                };
                            let field_width = (ui.available_width() - 184.0).max(220.0);
                            ui.add_sized(
                                [field_width, CONNECTION_CONTROL_HEIGHT],
                                egui::TextEdit::singleline(&mut path_display).interactive(false),
                            )
                            .on_hover_text(&path_display);
                            if ui
                                .add_sized(
                                    [88.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Button::new("选择图片"),
                                )
                                .clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .set_title("选择应用背景")
                                    .add_filter(
                                        "图片文件",
                                        &[
                                            "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif",
                                            "tiff",
                                        ],
                                    )
                                    .pick_file()
                            {
                                self.preferences.background_local_path =
                                    path.to_string_lossy().into_owned();
                                self.preferences.background_source = AppBackgroundSource::Local;
                                preferences_changed = true;
                                reload_requested = true;
                            }
                            if ui
                                .add_enabled(
                                    !self.preferences.background_local_path.trim().is_empty(),
                                    egui::Button::new("重新加载")
                                        .min_size(egui::vec2(80.0, CONNECTION_CONTROL_HEIGHT)),
                                )
                                .clicked()
                            {
                                reload_requested = true;
                            }
                        });
                        self.show_background_load_status(ui, false);
                    }
                    AppBackgroundSource::Online => {
                        ui.horizontal(|ui| {
                            let field_width = (ui.available_width() - 92.0).max(260.0);
                            let response = ui.add_sized(
                                [field_width, CONNECTION_CONTROL_HEIGHT],
                                egui::TextEdit::singleline(&mut self.background_url_draft)
                                    .hint_text("https://example.com/background.jpg"),
                            );
                            let enter_pressed = response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter));
                            let apply_clicked = ui
                                .add_sized(
                                    [84.0, CONNECTION_CONTROL_HEIGHT],
                                    egui::Button::new("应用地址"),
                                )
                                .clicked();
                            if apply_clicked || enter_pressed {
                                match normalized_online_image_url(&self.background_url_draft) {
                                    Ok(url) => {
                                        self.background_url_draft.clone_from(&url);
                                        if self.preferences.background_online_url != url
                                            || self.preferences.background_source
                                                != AppBackgroundSource::Online
                                        {
                                            self.preferences.background_online_url = url;
                                            self.preferences.background_source =
                                                AppBackgroundSource::Online;
                                            preferences_changed = true;
                                        }
                                        reload_requested = true;
                                    }
                                    Err(message) => self.set_notice(message, true),
                                }
                            }
                        });
                        self.show_background_load_status(ui, true);
                    }
                }
            },
        );

        ui.add_space(10.0);
        settings_card(
            ui,
            "背景不透明度",
            "为亮色与暗色主题分别设置，切换主题时自动使用对应数值。",
            |ui| {
                egui::Grid::new("background_opacity_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        settings_row_label(ui, "亮色模式");
                        settings_controls(ui, |ui| {
                            preferences_changed |= background_opacity_control(
                                ui,
                                &mut self.preferences.background_light_opacity,
                            );
                        });
                        ui.end_row();

                        settings_row_label(ui, "暗色模式");
                        settings_controls(ui, |ui| {
                            preferences_changed |= background_opacity_control(
                                ui,
                                &mut self.preferences.background_dark_opacity,
                            );
                        });
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    let is_default = (self.preferences.background_light_opacity
                        - DEFAULT_BACKGROUND_LIGHT_OPACITY)
                        .abs()
                        < f32::EPSILON
                        && (self.preferences.background_dark_opacity
                            - DEFAULT_BACKGROUND_DARK_OPACITY)
                            .abs()
                            < f32::EPSILON;
                    ui.add_space((ui.available_width() - 104.0).max(0.0));
                    if ui
                        .add_enabled(
                            !is_default,
                            egui::Button::new("恢复推荐值")
                                .min_size(egui::vec2(104.0, CONNECTION_CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        self.preferences.background_light_opacity =
                            DEFAULT_BACKGROUND_LIGHT_OPACITY;
                        self.preferences.background_dark_opacity = DEFAULT_BACKGROUND_DARK_OPACITY;
                        preferences_changed = true;
                    }
                });
            },
        );

        ui.add_space(10.0);
        settings_card(
            ui,
            "效果预览",
            "预览亮色与暗色模式下的实际透明度。",
            |ui| {
                self.show_background_previews(ui);
            },
        );

        if reload_requested {
            self.reload_background_image(context);
        }
        if preferences_changed {
            self.mark_preferences_dirty();
        }
    }

    fn show_background_load_status(&self, ui: &mut egui::Ui, check_draft: bool) {
        let draft_changed = check_draft
            && self.background_url_draft.trim() != self.preferences.background_online_url.trim();
        let (message, color) = if draft_changed {
            (
                "地址有修改，点击“应用地址”后生效。".to_owned(),
                Color32::from_rgb(210, 140, 30),
            )
        } else {
            match &self.background_load_state {
                BackgroundLoadState::Idle => (
                    if check_draft {
                        "输入图片地址并点击“应用地址”。".to_owned()
                    } else {
                        "选择图片后将在这里显示加载状态。".to_owned()
                    },
                    ui.visuals().weak_text_color(),
                ),
                BackgroundLoadState::Loading => (
                    "正在加载背景图片…".to_owned(),
                    ui.visuals().weak_text_color(),
                ),
                BackgroundLoadState::Ready => (
                    "背景图片已加载。".to_owned(),
                    Color32::from_rgb(45, 155, 90),
                ),
                BackgroundLoadState::Error(message) => {
                    (format!("加载失败：{message}"), ui.visuals().error_fg_color)
                }
            }
        };
        ui.add_space(5.0);
        ui.label(RichText::new(message).small().color(color));
    }

    fn show_background_previews(&self, ui: &mut egui::Ui) {
        let texture = self.background_image_uri().and_then(|uri| {
            match ui.ctx().try_load_texture(
                &uri,
                egui::TextureOptions::LINEAR,
                egui::load::SizeHint::default(),
            ) {
                Ok(egui::load::TexturePoll::Ready { texture }) => Some(texture),
                Ok(egui::load::TexturePoll::Pending { .. }) => {
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                    None
                }
                Err(_) => None,
            }
        });
        let gap = ui.spacing().item_spacing.x;
        let preview_width = (ui.available_width() - gap) * 0.5;
        ui.horizontal(|ui| {
            background_preview(
                ui,
                preview_width,
                "亮色",
                false,
                texture,
                self.preferences.background_light_opacity,
            );
            background_preview(
                ui,
                preview_width,
                "暗色",
                true,
                texture,
                self.preferences.background_dark_opacity,
            );
        });
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
        self.paint_app_background(ui);
        self.show_title_bar(ui);
        self.show_connection_panel(ui);
        self.show_status_panel(ui);
        if self.preferences.receive_mode != ReceiveMode::Terminal {
            self.show_send_panel(ui);
        }
        self.show_output_panel(ui);
        self.show_settings_window(&context);
        self.handle_window_resize(&context);
        self.paint_window_border(&context);
    }
}

impl Drop for EscomApp {
    fn drop(&mut self) {
        let _ = settings::save(&self.preferences);
        self.worker.shutdown();
    }
}

fn title_bar_control_rect(title_rect: egui::Rect, index_from_right: usize) -> egui::Rect {
    let right = title_rect.right() - index_from_right as f32 * TITLE_BAR_BUTTON_WIDTH;
    egui::Rect::from_min_max(
        egui::pos2(right - TITLE_BAR_BUTTON_WIDTH, title_rect.top()),
        egui::pos2(right, title_rect.bottom()),
    )
}

fn title_bar_control(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    control: TitleBarControl,
) -> egui::Response {
    let label = match control {
        TitleBarControl::Minimize => "最小化",
        TitleBarControl::Maximize => "最大化",
        TitleBarControl::Restore => "还原",
        TitleBarControl::Close => "关闭",
    };
    let response = ui.interact(rect, ui.id().with(label), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));

    let is_close = matches!(control, TitleBarControl::Close);
    let icon_color = if is_close && response.hovered() {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    let fill = if is_close && response.is_pointer_button_down_on() {
        Some(Color32::from_rgb(150, 25, 20))
    } else if is_close && response.hovered() {
        Some(Color32::from_rgb(196, 43, 28))
    } else if response.is_pointer_button_down_on() {
        Some(ui.visuals().widgets.active.weak_bg_fill)
    } else if response.hovered() {
        Some(ui.visuals().widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, 0.0, fill);
    }

    let center = rect.center();
    let stroke = egui::Stroke::new(1.25, icon_color);
    match control {
        TitleBarControl::Minimize => {
            ui.painter()
                .hline((center.x - 5.0)..=(center.x + 5.0), center.y + 4.0, stroke);
        }
        TitleBarControl::Maximize => {
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        TitleBarControl::Restore => {
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(center + egui::vec2(1.5, -1.5), egui::vec2(8.0, 8.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(center + egui::vec2(-1.5, 1.5), egui::vec2(8.0, 8.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        TitleBarControl::Close => {
            ui.painter().line_segment(
                [
                    center + egui::vec2(-5.0, -5.0),
                    center + egui::vec2(5.0, 5.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    center + egui::vec2(5.0, -5.0),
                    center + egui::vec2(-5.0, 5.0),
                ],
                stroke,
            );
        }
    }

    response.on_hover_text(label)
}

fn window_resize_direction(
    viewport_rect: egui::Rect,
    pointer_position: egui::Pos2,
) -> Option<egui::ResizeDirection> {
    let in_title_controls = pointer_position.y < viewport_rect.top() + TITLE_BAR_HEIGHT
        && pointer_position.x >= viewport_rect.right() - TITLE_BAR_CONTROLS_WIDTH;
    let left = pointer_position.x <= viewport_rect.left() + WINDOW_RESIZE_BORDER;
    let right = pointer_position.x >= viewport_rect.right() - WINDOW_RESIZE_BORDER
        && pointer_position.y >= viewport_rect.top() + TITLE_BAR_HEIGHT;
    let top =
        pointer_position.y <= viewport_rect.top() + WINDOW_RESIZE_BORDER && !in_title_controls;
    let bottom = pointer_position.y >= viewport_rect.bottom() - WINDOW_RESIZE_BORDER;

    match (left, right, top, bottom) {
        (true, _, true, _) => Some(egui::ResizeDirection::NorthWest),
        (_, true, true, _) => Some(egui::ResizeDirection::NorthEast),
        (true, _, _, true) => Some(egui::ResizeDirection::SouthWest),
        (_, true, _, true) => Some(egui::ResizeDirection::SouthEast),
        (true, _, _, _) => Some(egui::ResizeDirection::West),
        (_, true, _, _) => Some(egui::ResizeDirection::East),
        (_, _, true, _) => Some(egui::ResizeDirection::North),
        (_, _, _, true) => Some(egui::ResizeDirection::South),
        _ => None,
    }
}

fn resize_cursor(direction: egui::ResizeDirection) -> egui::CursorIcon {
    match direction {
        egui::ResizeDirection::North | egui::ResizeDirection::South => {
            egui::CursorIcon::ResizeVertical
        }
        egui::ResizeDirection::East | egui::ResizeDirection::West => {
            egui::CursorIcon::ResizeHorizontal
        }
        egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
            egui::CursorIcon::ResizeNeSw
        }
        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
            egui::CursorIcon::ResizeNwSe
        }
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

fn terminal_bytes_from_events(
    events: &[egui::Event],
    encoding: TextEncoding,
    line_ending: LineEnding,
) -> Result<Vec<u8>, String> {
    let has_copy = events
        .iter()
        .any(|event| matches!(event, egui::Event::Copy));
    let has_paste = events
        .iter()
        .any(|event| matches!(event, egui::Event::Paste(_)));
    let text_contains_tab = events
        .iter()
        .any(|event| matches!(event, egui::Event::Text(text) if text.contains('\t')));
    let mut bytes = Vec::new();

    for event in events {
        match event {
            egui::Event::Text(text) | egui::Event::Paste(text) => {
                append_terminal_text(&mut bytes, text, encoding)?;
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                append_terminal_text(&mut bytes, text, encoding)?;
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.ctrl && !modifiers.alt {
                    if (*key == egui::Key::C && has_copy) || (*key == egui::Key::V && has_paste) {
                        continue;
                    }
                    if let Some(byte) = terminal_control_byte(*key) {
                        bytes.push(byte);
                    }
                    continue;
                }

                match key {
                    egui::Key::Enter => bytes.extend_from_slice(line_ending.bytes()),
                    egui::Key::Tab if !text_contains_tab => bytes.push(b'\t'),
                    egui::Key::Backspace => bytes.push(0x08),
                    egui::Key::Escape => bytes.push(0x1B),
                    egui::Key::Delete => bytes.push(0x7F),
                    egui::Key::ArrowUp => bytes.extend_from_slice(b"\x1B[A"),
                    egui::Key::ArrowDown => bytes.extend_from_slice(b"\x1B[B"),
                    egui::Key::ArrowRight => bytes.extend_from_slice(b"\x1B[C"),
                    egui::Key::ArrowLeft => bytes.extend_from_slice(b"\x1B[D"),
                    egui::Key::Home => bytes.extend_from_slice(b"\x1B[H"),
                    egui::Key::End => bytes.extend_from_slice(b"\x1B[F"),
                    egui::Key::Insert => bytes.extend_from_slice(b"\x1B[2~"),
                    egui::Key::PageUp => bytes.extend_from_slice(b"\x1B[5~"),
                    egui::Key::PageDown => bytes.extend_from_slice(b"\x1B[6~"),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(bytes)
}

fn append_terminal_text(
    output: &mut Vec<u8>,
    text: &str,
    encoding: TextEncoding,
) -> Result<(), String> {
    if !text.is_empty() {
        output.extend(encode_text(text, encoding, LineEnding::None)?);
    }
    Ok(())
}

fn terminal_control_byte(key: egui::Key) -> Option<u8> {
    Some(match key {
        egui::Key::A => 0x01,
        egui::Key::B => 0x02,
        egui::Key::C => 0x03,
        egui::Key::D => 0x04,
        egui::Key::E => 0x05,
        egui::Key::F => 0x06,
        egui::Key::G => 0x07,
        egui::Key::H => 0x08,
        egui::Key::I => 0x09,
        egui::Key::J => 0x0A,
        egui::Key::K => 0x0B,
        egui::Key::L => 0x0C,
        egui::Key::M => 0x0D,
        egui::Key::N => 0x0E,
        egui::Key::O => 0x0F,
        egui::Key::P => 0x10,
        egui::Key::Q => 0x11,
        egui::Key::R => 0x12,
        egui::Key::S => 0x13,
        egui::Key::T => 0x14,
        egui::Key::U => 0x15,
        egui::Key::V => 0x16,
        egui::Key::W => 0x17,
        egui::Key::X => 0x18,
        egui::Key::Y => 0x19,
        egui::Key::Z => 0x1A,
        egui::Key::OpenBracket => 0x1B,
        egui::Key::Backslash => 0x1C,
        egui::Key::CloseBracket => 0x1D,
        egui::Key::Minus => 0x1F,
        egui::Key::Space => 0x00,
        _ => return None,
    })
}

fn settings_card<R>(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let frame = egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(8)
        .inner_margin(egui::Margin::same(12));
    frame
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(16.0));
            ui.label(
                RichText::new(description)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);
            contents(ui)
        })
        .inner
}

fn normalized_online_image_url(input: &str) -> Result<String, &'static str> {
    let value = input.trim();
    if value.is_empty() {
        return Err("请输入在线图片地址");
    }
    if value.chars().any(char::is_whitespace) {
        return Err("在线图片地址不能包含空格");
    }
    let lowercase = value.to_ascii_lowercase();
    let prefix_len = if lowercase.starts_with("https://") {
        "https://".len()
    } else if lowercase.starts_with("http://") {
        "http://".len()
    } else {
        return Err("在线图片地址必须以 http:// 或 https:// 开头");
    };
    let authority = value[prefix_len..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return Err("在线图片地址缺少有效的主机名");
    }
    Ok(value.to_owned())
}

fn local_background_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(unc_path) = normalized.strip_prefix("//") {
        format!("file://{unc_path}")
    } else {
        format!("file:///{normalized}")
    }
}

fn background_opacity_control(ui: &mut egui::Ui, opacity: &mut f32) -> bool {
    let changed = ui
        .add_sized(
            [390.0, CONNECTION_CONTROL_HEIGHT],
            egui::Slider::new(opacity, 0.0..=1.0).show_value(false),
        )
        .changed();
    ui.allocate_ui_with_layout(
        egui::vec2(60.0, CONNECTION_CONTROL_HEIGHT),
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.label(RichText::new(format!("{:.0}%", *opacity * 100.0)).monospace());
        },
    );
    changed
}

fn background_preview(
    ui: &mut egui::Ui,
    width: f32,
    label: &str,
    dark_mode: bool,
    texture: Option<egui::load::SizedTexture>,
    opacity: f32,
) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 96.0), egui::Sense::hover());
    let base = if dark_mode {
        Color32::from_gray(27)
    } else {
        Color32::from_gray(248)
    };
    ui.painter().rect_filled(rect, 6.0, base);
    if let Some(texture) = texture {
        paint_texture_cover(ui.painter(), rect, texture, opacity);
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "暂无背景",
            FontId::proportional(13.0),
            if dark_mode {
                Color32::from_gray(150)
            } else {
                Color32::from_gray(110)
            },
        );
    }

    let toolbar_rect =
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.top() + 26.0));
    ui.painter().rect_filled(
        toolbar_rect,
        egui::CornerRadius {
            nw: 6,
            ne: 6,
            sw: 0,
            se: 0,
        },
        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 218),
    );
    let text_color = if dark_mode {
        Color32::from_gray(235)
    } else {
        Color32::from_gray(30)
    };
    ui.painter().text(
        toolbar_rect.left_center() + egui::vec2(9.0, 0.0),
        egui::Align2::LEFT_CENTER,
        format!("{label}模式"),
        FontId::proportional(13.0),
        text_color,
    );
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 10.0, toolbar_rect.bottom() + 10.0),
        egui::pos2(rect.right() - 10.0, rect.bottom() - 10.0),
    );
    ui.painter().rect_filled(
        content_rect,
        4.0,
        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 118),
    );
    ui.painter().rect_stroke(
        rect,
        6.0,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    response.on_hover_text(format!("背景不透明度 {:.0}%", opacity * 100.0));
}

fn paint_texture_cover(
    painter: &egui::Painter,
    rect: egui::Rect,
    texture: egui::load::SizedTexture,
    opacity: f32,
) {
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    if alpha == 0 {
        return;
    }
    painter.image(
        texture.id,
        rect,
        cover_uv(texture.size, rect.size()),
        Color32::from_white_alpha(alpha),
    );
}

fn cover_uv(image_size: egui::Vec2, target_size: egui::Vec2) -> egui::Rect {
    if image_size.x <= 0.0 || image_size.y <= 0.0 || target_size.x <= 0.0 || target_size.y <= 0.0 {
        return egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    }

    let image_aspect = image_size.x / image_size.y;
    let target_aspect = target_size.x / target_size.y;
    if image_aspect > target_aspect {
        let visible_width = target_aspect / image_aspect;
        let inset = (1.0 - visible_width) * 0.5;
        egui::Rect::from_min_max(egui::pos2(inset, 0.0), egui::pos2(1.0 - inset, 1.0))
    } else {
        let visible_height = image_aspect / target_aspect;
        let inset = (1.0 - visible_height) * 0.5;
        egui::Rect::from_min_max(egui::pos2(0.0, inset), egui::pos2(1.0, 1.0 - inset))
    }
}

fn settings_row_label(ui: &mut egui::Ui, text: &str) {
    ui.allocate_ui_with_layout(
        egui::vec2(SETTINGS_LABEL_WIDTH, CONNECTION_CONTROL_HEIGHT),
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.label(text);
        },
    );
}

fn show_theme_menu(ui: &mut egui::Ui, theme_preference: &mut egui::ThemePreference) -> bool {
    let current = *theme_preference;
    let mut changed = false;
    let response = ui
        .menu_button(
            RichText::new(theme_preference_icon(current)).size(17.0),
            |ui| {
                ui.set_min_width(132.0);
                for (preference, label) in [
                    (egui::ThemePreference::System, "跟随系统"),
                    (egui::ThemePreference::Light, "亮色"),
                    (egui::ThemePreference::Dark, "暗色"),
                ] {
                    if ui
                        .selectable_value(theme_preference, preference, label)
                        .changed()
                    {
                        changed = true;
                        ui.close();
                    }
                }
            },
        )
        .response;
    let accessible_label = format!("主题：{}", theme_preference_label(current));
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, accessible_label.clone())
    });
    response.on_hover_text(accessible_label);
    changed
}

fn theme_preference_icon(preference: egui::ThemePreference) -> &'static str {
    match preference {
        egui::ThemePreference::System => "💻",
        egui::ThemePreference::Light => "☀",
        egui::ThemePreference::Dark => "🌙",
    }
}

fn theme_preference_label(preference: egui::ThemePreference) -> &'static str {
    match preference {
        egui::ThemePreference::System => "跟随系统",
        egui::ThemePreference::Light => "亮色",
        egui::ThemePreference::Dark => "暗色",
    }
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

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

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

    #[test]
    fn borderless_resize_hit_zones_map_to_edges_and_corners() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0));

        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1.0, 1.0)),
            Some(egui::ResizeDirection::NorthWest)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1.0, 819.0)),
            Some(egui::ResizeDirection::SouthWest)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1279.0, 819.0)),
            Some(egui::ResizeDirection::SouthEast)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1.0, 400.0)),
            Some(egui::ResizeDirection::West)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1279.0, 400.0)),
            Some(egui::ResizeDirection::East)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(640.0, 1.0)),
            Some(egui::ResizeDirection::North)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(640.0, 819.0)),
            Some(egui::ResizeDirection::South)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(640.0, 400.0)),
            None
        );
    }

    #[test]
    fn title_bar_controls_take_priority_over_resize_hit_zone() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0));

        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1279.0, 1.0)),
            None
        );
    }

    #[test]
    fn cover_uv_center_crops_wide_and_tall_images() {
        let wide = cover_uv(egui::vec2(200.0, 100.0), egui::vec2(100.0, 100.0));
        assert_eq!(wide.min, egui::pos2(0.25, 0.0));
        assert_eq!(wide.max, egui::pos2(0.75, 1.0));

        let tall = cover_uv(egui::vec2(100.0, 200.0), egui::vec2(100.0, 100.0));
        assert_eq!(tall.min, egui::pos2(0.0, 0.25));
        assert_eq!(tall.max, egui::pos2(1.0, 0.75));
    }

    #[test]
    fn online_background_url_requires_http_and_a_host() {
        assert_eq!(
            normalized_online_image_url(" https://example.com/image.png ").unwrap(),
            "https://example.com/image.png"
        );
        assert!(normalized_online_image_url("file:///tmp/image.png").is_err());
        assert!(normalized_online_image_url("https://").is_err());
        assert!(normalized_online_image_url("https://example.com/a b.png").is_err());
    }

    #[test]
    fn local_background_paths_use_windows_file_uri_shape() {
        assert_eq!(
            local_background_uri(r"D:\Pictures\background image.png"),
            "file:///D:/Pictures/background image.png"
        );
        assert_eq!(
            local_background_uri(r"\\server\share\background.png"),
            "file://server/share/background.png"
        );
    }

    #[test]
    fn terminal_printable_key_is_sent_once_without_local_echo() {
        let events = [
            key_event(egui::Key::H, egui::Modifiers::NONE),
            egui::Event::Text("H".into()),
        ];
        let bytes =
            terminal_bytes_from_events(&events, TextEncoding::Utf8, LineEnding::CrLf).unwrap();
        assert_eq!(bytes, b"H");

        let store = ReceiveStore::new(1024);
        let rows = format_snapshot(&store.snapshot(), ReceiveMode::Terminal, TextEncoding::Utf8);
        assert!(rows.is_empty());
    }

    #[test]
    fn terminal_special_and_control_keys_map_to_serial_bytes() {
        let events = [
            key_event(egui::Key::Enter, egui::Modifiers::NONE),
            key_event(egui::Key::Backspace, egui::Modifiers::NONE),
            key_event(egui::Key::ArrowUp, egui::Modifiers::NONE),
            key_event(egui::Key::C, egui::Modifiers::CTRL),
        ];
        let bytes =
            terminal_bytes_from_events(&events, TextEncoding::Utf8, LineEnding::CrLf).unwrap();
        assert_eq!(bytes, b"\r\n\x08\x1B[A\x03");
    }

    #[test]
    fn terminal_text_respects_selected_encoding() {
        let bytes = terminal_bytes_from_events(
            &[egui::Event::Text("中".into())],
            TextEncoding::Gbk,
            LineEnding::CrLf,
        )
        .unwrap();
        assert_eq!(bytes, [0xD6, 0xD0]);

        assert!(
            terminal_bytes_from_events(
                &[egui::Event::Text("🙂".into())],
                TextEncoding::Gbk,
                LineEnding::CrLf,
            )
            .is_err()
        );
    }
}
