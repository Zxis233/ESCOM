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
    DisplayFormatter, DisplayUpdate, FormattedRow, MAX_DISPLAY_INCREMENT_BYTES, MAX_DISPLAY_ROWS,
    MAX_DISPLAY_TEXT_BYTES, display_snapshot_limit, display_text, encode_text, format_snapshot,
    parse_send_input, render_export,
};
use crate::highlight::{self, HighlightRules, HighlightStyle};
use crate::icon;
use crate::model::{
    HistoryItem, LineEnding, ReceiveMode, SendMode, SerialConfig, TextEncoding, data_bits_label,
    flow_control_label, parity_label, parse_baud_rate, stop_bits_label,
};
use crate::search::{self, SearchIndex, SearchMatch, SearchMatcher};
use crate::serial_worker::{WorkerEvent, WorkerHandle};
use crate::settings::{
    self, AppBackgroundSource, BUFFER_LIMIT_OPTIONS_MIB, DEFAULT_BACKGROUND_DARK_OPACITY,
    DEFAULT_BACKGROUND_LIGHT_OPACITY, MAX_DATA_FONT_SIZE, MAX_DATA_LINE_SPACING, MAX_UI_FONT_SIZE,
    MIN_DATA_LINE_SPACING, MIN_FONT_SIZE, UiPreferences,
};
use crate::store::ReceiveStore;
use crate::window_chrome;

mod connection;
mod receive;
mod search_ui;
mod send;
mod settings_ui;
mod status;
mod widgets;

const FORMAT_DEBOUNCE: Duration = Duration::from_millis(80);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
const PORT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const MAX_HISTORY: usize = 50;
const MIN_CONTROL_HEIGHT: f32 = 32.0;
const CONFIG_LABEL_WIDTH: f32 = 48.0;
const CONFIG_COMBO_WIDTH: f32 = 100.0;
const SETTINGS_LABEL_WIDTH: f32 = 76.0;
const SETTINGS_WINDOW_WIDTH: f32 = 660.0;
const SETTINGS_WINDOW_HEIGHT: f32 = 454.0;
const SETTINGS_VIEWPORT_MARGIN: f32 = 16.0;
const THEME_ICON_SIZE: f32 = 17.0;
const SEND_EDITOR_LIGHT_ALPHA: u8 = 120;
const SEND_EDITOR_DARK_ALPHA: u8 = 104;
const COMMON_BAUD_RATES: [u32; 18] = [
    300, 600, 1_200, 2_400, 4_800, 9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800,
    576_000, 921_600, 1_000_000, 2_000_000, 3_000_000, 4_000_000,
];

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
        formatter: DisplayFormatter,
    },
    Searched {
        token: u64,
        generation: u64,
        index: SearchIndex,
    },
    Exported(Result<PathBuf, String>),
}

#[derive(Clone, Copy)]
enum HighlightConfigAction {
    Reload,
    Open,
    CopyPath,
}

pub struct EscomApp {
    preferences: UiPreferences,
    serial_config: SerialConfig,
    baud_rate_input: String,
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
    display_formatter: Option<DisplayFormatter>,
    format_token: u64,
    format_in_progress: bool,
    force_format: bool,
    last_format_started: Instant,
    paused: bool,
    force_scroll_bottom: bool,
    search_query: String,
    search_case_sensitive: bool,
    search_regex: bool,
    search_filter: bool,
    search_index: Arc<SearchIndex>,
    search_matcher: Option<SearchMatcher>,
    search_index_generation: Option<u64>,
    search_token: u64,
    search_pending: bool,
    search_in_progress: bool,
    search_wait_for_debounce: bool,
    search_reset_selection: bool,
    search_last_changed: Instant,
    search_selected_match: Option<usize>,
    search_scroll_to_row: Option<usize>,
    focus_search: bool,
    highlight_rules: Arc<HighlightRules>,
    highlight_config_error: Option<String>,
    background_tx: Sender<BackgroundEvent>,
    background_rx: Receiver<BackgroundEvent>,

    settings_open: bool,
    settings_center_on_open: bool,
    settings_tab: SettingsTab,
    background_url_draft: String,
    background_light_opacity_draft: String,
    background_dark_opacity_draft: String,
    background_load_uri: Option<String>,
    background_load_state: BackgroundLoadState,
    preferences_dirty_since: Option<Instant>,
    last_port_refresh: Instant,
    notice: Option<Notice>,
}

impl EscomApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        window_chrome::disable_native_window_border(creation_context);
        let (mut preferences, settings_warning) = settings::load();
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
        let highlight_path = highlight::config_path();
        let (highlight_rules, highlight_config_error) = match HighlightRules::load_or_create() {
            Ok(rules) => (rules, None),
            Err(error) => (HighlightRules::empty(highlight_path), Some(error)),
        };
        let mut startup_warnings = applied_fonts.warnings.clone();
        if let Some(error) = settings_warning {
            startup_warnings.push(error);
        }
        if let Some(error) = &highlight_config_error {
            startup_warnings.push(error.clone());
        }
        let notice = (!startup_warnings.is_empty()).then(|| Notice {
            message: startup_warnings.join("；"),
            expires_at: Instant::now() + NOTICE_DURATION,
            error: true,
        });

        let background_url_draft = preferences.background_online_url.clone();
        let background_light_opacity_draft =
            settings_ui::format_background_opacity(preferences.background_light_opacity);
        let background_dark_opacity_draft =
            settings_ui::format_background_opacity(preferences.background_dark_opacity);
        let serial_config = SerialConfig::default();
        let baud_rate_input = serial_config.baud_rate.to_string();

        Self {
            preferences,
            serial_config,
            baud_rate_input,
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
            display_formatter: None,
            format_token: 0,
            format_in_progress: false,
            force_format: true,
            last_format_started: Instant::now() - FORMAT_DEBOUNCE,
            paused: false,
            force_scroll_bottom: false,
            search_query: String::new(),
            search_case_sensitive: false,
            search_regex: false,
            search_filter: false,
            search_index: Arc::new(SearchIndex::default()),
            search_matcher: None,
            search_index_generation: None,
            search_token: 0,
            search_pending: false,
            search_in_progress: false,
            search_wait_for_debounce: false,
            search_reset_selection: false,
            search_last_changed: Instant::now() - SEARCH_DEBOUNCE,
            search_selected_match: None,
            search_scroll_to_row: None,
            focus_search: false,
            highlight_rules: Arc::new(highlight_rules),
            highlight_config_error,
            background_tx,
            background_rx,
            settings_open: false,
            settings_center_on_open: false,
            settings_tab: SettingsTab::Fonts,
            background_url_draft,
            background_light_opacity_draft,
            background_dark_opacity_draft,
            background_load_uri: None,
            background_load_state: BackgroundLoadState::Idle,
            preferences_dirty_since: None,
            last_port_refresh: Instant::now() - PORT_REFRESH_INTERVAL,
            notice,
        }
    }

    fn surface_fill(&self, base: Color32, alpha_with_background: u8) -> Color32 {
        if self.has_configured_background() {
            Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha_with_background)
        } else {
            base
        }
    }

    fn show_title_bar(&self, root_ui: &mut egui::Ui) {
        let title_fill = self.surface_fill(root_ui.visuals().panel_fill, 218);
        window_chrome::show_title_bar(root_ui, &self.title_icon, title_fill);
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
                    formatter,
                } => {
                    self.format_in_progress = false;
                    if token == self.format_token && !self.paused {
                        self.display_rows = Arc::new(rows);
                        self.display_generation = generation;
                        self.display_formatter = Some(formatter);
                        self.force_format = false;
                        self.request_search_for_display();
                    } else {
                        self.force_format = true;
                    }
                }
                BackgroundEvent::Searched {
                    token,
                    generation,
                    index,
                } => {
                    self.search_in_progress = false;
                    if token == self.search_token && generation == self.display_generation {
                        self.search_index = Arc::new(index);
                        self.search_index_generation = Some(generation);
                        if self.search_index.error.is_some() || self.search_index.matches.is_empty()
                        {
                            self.search_selected_match = None;
                            self.search_scroll_to_row = None;
                        } else if self.search_reset_selection {
                            self.search_selected_match = Some(0);
                            self.queue_selected_search_scroll();
                        } else if let Some(selected) = self.search_selected_match {
                            self.search_selected_match =
                                Some(selected.min(self.search_index.matches.len() - 1));
                        }
                        self.search_reset_selection = false;
                    } else if token == self.search_token && self.search_matcher.is_some() {
                        self.search_pending = true;
                        self.search_wait_for_debounce = false;
                        self.search_index_generation = None;
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
}

impl eframe::App for EscomApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_worker_events();
        self.process_background_events();
        self.maybe_refresh_ports();
        self.process_repeat(context);
        self.maybe_start_format(context);
        self.maybe_start_search(context);
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
        window_chrome::handle_window_resize(&context);
        self.paint_app_background(ui);
        self.show_title_bar(ui);
        self.show_connection_panel(ui);
        self.show_status_panel(ui);
        if self.preferences.receive_mode != ReceiveMode::Terminal {
            self.show_send_panel(ui);
        }
        self.show_output_panel(ui);
        self.show_settings_window(&context);
        window_chrome::paint_window_border(&context);
    }
}

impl Drop for EscomApp {
    fn drop(&mut self) {
        let _ = settings::save(&self.preferences);
        self.worker.shutdown();
    }
}

#[cfg(test)]
mod tests;
