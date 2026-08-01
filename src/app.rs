use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText};
use serialport::{DataBits, FlowControl, Parity, StopBits};

use crate::fonts::{FontCatalog, data_font_family};
use crate::formatting::{
    DisplayFormatter, DisplayUpdate, FormattedRow, display_text, encode_text, format_snapshot,
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
        let highlight_path = highlight::config_path();
        let (highlight_rules, highlight_config_error) = match HighlightRules::load_or_create() {
            Ok(rules) => (rules, None),
            Err(error) => (HighlightRules::empty(highlight_path), Some(error)),
        };
        let mut startup_warnings = applied_fonts.warnings.clone();
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
            format_background_opacity(preferences.background_light_opacity);
        let background_dark_opacity_draft =
            format_background_opacity(preferences.background_dark_opacity);
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

    fn sync_background_opacity_drafts(&mut self) {
        self.background_light_opacity_draft =
            format_background_opacity(self.preferences.background_light_opacity);
        self.background_dark_opacity_draft =
            format_background_opacity(self.preferences.background_dark_opacity);
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
        if self.paused || self.format_in_progress || self.search_in_progress {
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

        let mode = self.preferences.receive_mode;
        let encoding = self.preferences.text_encoding;
        let can_apply_delta = !self.force_format
            && self
                .display_formatter
                .as_ref()
                .is_some_and(|formatter| formatter.is_compatible(mode, encoding));
        if can_apply_delta {
            let cursor = self
                .display_formatter
                .as_ref()
                .expect("delta formatting requires an initialized formatter")
                .cursor();
            let delta = self
                .store
                .lock()
                .ok()
                .map(|store| store.delta_since(cursor));
            let Some(delta) = delta else {
                self.set_notice("接收缓存不可用", true);
                return;
            };
            if !delta.reset_or_gap {
                let update = self
                    .display_formatter
                    .as_mut()
                    .expect("delta formatting requires an initialized formatter")
                    .apply_delta(&delta);
                if let Ok(update) = update {
                    self.apply_display_update(update);
                    self.last_format_started = Instant::now();
                    return;
                }
            }
            self.force_format = true;
        }

        let snapshot = self.store.lock().ok().map(|store| store.snapshot());
        let Some(snapshot) = snapshot else {
            self.set_notice("接收缓存不可用", true);
            return;
        };
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
                let (formatter, rows) = DisplayFormatter::rebuild(&snapshot, mode, encoding);
                let _ = sender.send(BackgroundEvent::Formatted {
                    token,
                    generation,
                    rows,
                    formatter,
                });
                repaint_context.request_repaint();
            })
            .expect("failed to start formatting task");
    }

    fn apply_display_update(&mut self, update: DisplayUpdate) {
        let DisplayUpdate {
            generation,
            remove_prefix,
            replace_tail,
            rows,
        } = update;
        let old_generation = self.display_generation;
        let old_row_count = self.display_rows.len();
        let search_was_current = self.search_index_generation == Some(old_generation);

        if search_was_current {
            if let Some(matcher) = &self.search_matcher {
                Arc::make_mut(&mut self.search_index).apply_display_update(
                    old_row_count,
                    remove_prefix,
                    replace_tail,
                    &rows,
                    matcher,
                    self.preferences.timestamps,
                );
            }
            self.search_index_generation = Some(generation);
        }

        let display_rows = Arc::make_mut(&mut self.display_rows);
        display_rows.drain(..remove_prefix);
        display_rows.truncate(display_rows.len().saturating_sub(replace_tail));
        display_rows.extend(rows);
        self.display_generation = generation;
        self.force_format = false;

        if search_was_current {
            self.clamp_search_selection();
        }
    }

    fn request_search(&mut self, reset_selection: bool) {
        self.search_token = self.search_token.wrapping_add(1);
        self.search_last_changed = Instant::now();
        self.search_wait_for_debounce = true;
        self.search_reset_selection |= reset_selection;
        self.search_index_generation = None;
        if reset_selection {
            self.search_index = Arc::new(SearchIndex::default());
            self.search_selected_match = None;
            self.search_scroll_to_row = None;
        }

        match SearchMatcher::new(
            &self.search_query,
            self.search_case_sensitive,
            self.search_regex,
        ) {
            Ok(Some(matcher)) => {
                self.search_matcher = Some(matcher);
                self.search_pending = true;
            }
            Ok(None) => {
                self.search_matcher = None;
                self.search_pending = false;
                self.search_wait_for_debounce = false;
                self.search_reset_selection = false;
                self.search_index = Arc::new(SearchIndex::default());
                self.search_index_generation = Some(self.display_generation);
            }
            Err(error) => {
                self.search_matcher = None;
                self.search_pending = false;
                self.search_wait_for_debounce = false;
                self.search_reset_selection = false;
                self.search_index = Arc::new(SearchIndex {
                    error: Some(error),
                    ..Default::default()
                });
                self.search_index_generation = Some(self.display_generation);
                self.search_selected_match = None;
                self.search_scroll_to_row = None;
            }
        }
    }

    fn request_search_for_display(&mut self) {
        self.search_index_generation = None;
        if self.search_query.is_empty() {
            self.search_index = Arc::new(SearchIndex::default());
            self.search_pending = false;
            self.search_wait_for_debounce = false;
            self.search_index_generation = Some(self.display_generation);
        } else if self.search_matcher.is_some() {
            self.search_token = self.search_token.wrapping_add(1);
            self.search_pending = true;
        } else if self.search_index.error.is_some() {
            self.search_index_generation = Some(self.display_generation);
        }
    }

    fn maybe_start_search(&mut self, context: &egui::Context) {
        if !self.search_pending
            || self.search_in_progress
            || self.format_in_progress
            || self.search_query.is_empty()
        {
            return;
        }
        if self.search_wait_for_debounce && self.search_last_changed.elapsed() < SEARCH_DEBOUNCE {
            context.request_repaint_after(SEARCH_DEBOUNCE - self.search_last_changed.elapsed());
            return;
        }

        let Some(matcher) = self.search_matcher.clone() else {
            self.search_pending = false;
            return;
        };
        let token = self.search_token;
        let generation = self.display_generation;
        let rows = Arc::clone(&self.display_rows);
        let timestamps = self.preferences.timestamps;
        let sender = self.background_tx.clone();
        let repaint_context = context.clone();
        self.search_pending = false;
        self.search_in_progress = true;
        self.search_wait_for_debounce = false;

        thread::Builder::new()
            .name("escom-search".into())
            .spawn(move || {
                let index = search::search_rows_with_matcher(&rows, &matcher, timestamps);
                drop(rows);
                let _ = sender.send(BackgroundEvent::Searched {
                    token,
                    generation,
                    index,
                });
                repaint_context.request_repaint();
            })
            .expect("failed to start search task");
    }

    fn search_index_is_current(&self) -> bool {
        self.search_query.is_empty()
            || self.search_index_generation == Some(self.display_generation)
    }

    fn clamp_search_selection(&mut self) {
        if self.search_index.matches.is_empty() {
            self.search_selected_match = None;
            self.search_scroll_to_row = None;
        } else if let Some(selected) = self.search_selected_match {
            self.search_selected_match = Some(selected.min(self.search_index.matches.len() - 1));
        }
    }

    fn navigate_search(&mut self, direction: isize) {
        if !self.search_index_is_current() || self.search_pending || self.search_in_progress {
            return;
        }
        let count = self.search_index.matches.len();
        if count == 0 {
            return;
        }
        let current = self.search_selected_match.unwrap_or(0);
        let next = if direction < 0 {
            current.checked_sub(1).unwrap_or(count - 1)
        } else {
            (current + 1) % count
        };
        self.search_selected_match = Some(next);
        self.queue_selected_search_scroll();
    }

    fn queue_selected_search_scroll(&mut self) {
        if !self.search_index_is_current() {
            self.search_scroll_to_row = None;
            return;
        }
        let Some(selected) = self.search_selected_match else {
            return;
        };
        let Some(found) = self.search_index.matches.get(selected) else {
            return;
        };
        let visible_row = if self.search_filter {
            self.search_index
                .matched_rows
                .binary_search(&found.row_index)
                .ok()
        } else {
            Some(found.row_index)
        };
        self.search_scroll_to_row = visible_row;
    }

    fn reload_highlight_rules(&mut self) {
        match HighlightRules::load_or_create() {
            Ok(rules) => {
                let count = rules.len();
                self.highlight_rules = Arc::new(rules);
                self.highlight_config_error = None;
                self.set_notice(format!("已加载 {count} 条高亮规则"), false);
            }
            Err(error) => {
                self.highlight_config_error = Some(error.clone());
                self.set_notice(error, true);
            }
        }
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

    fn show_baud_rate_control(&mut self, ui: &mut egui::Ui) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let control_height = toolbar_control_height(ui);
            let input_width = text_field_width(ui, "4000000", 96.0);

            let input_color = if parse_baud_rate(&self.baud_rate_input).is_err() {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().text_color()
            };
            let response = ui.add_sized(
                [input_width, control_height],
                egui::TextEdit::singleline(&mut self.baud_rate_input)
                    .char_limit(10)
                    .text_color(input_color)
                    .horizontal_align(Align::Center)
                    .vertical_align(Align::Center),
            );
            let input_changed = response.changed();
            let input_lost_focus = response.lost_focus();

            if input_changed && let Ok(baud_rate) = parse_baud_rate(&self.baud_rate_input) {
                self.serial_config.baud_rate = baud_rate;
            }
            if input_lost_focus && let Ok(baud_rate) = parse_baud_rate(&self.baud_rate_input) {
                self.baud_rate_input = baud_rate.to_string();
            }

            match parse_baud_rate(&self.baud_rate_input) {
                Ok(_) => {
                    response.on_hover_text("可直接输入 1 到 4,000,000 之间的自定义波特率");
                }
                Err(message) => {
                    response.on_hover_text(message);
                }
            }

            let mut selected_rate = None;
            let combo_response = egui::ComboBox::from_id_salt("baud_rate_presets")
                .selected_text("常用")
                .width(combo_width(ui, "常用", 58.0))
                .height(300.0)
                .show_ui(ui, |ui| {
                    ui.set_min_width(116.0);
                    for baud_rate in COMMON_BAUD_RATES {
                        if ui
                            .selectable_label(
                                self.serial_config.baud_rate == baud_rate,
                                baud_rate.to_string(),
                            )
                            .clicked()
                        {
                            selected_rate = Some(baud_rate);
                            ui.close();
                        }
                    }
                })
                .response;
            combo_response.on_hover_text("选择常用波特率");

            if let Some(baud_rate) = selected_rate {
                self.serial_config.baud_rate = baud_rate;
                self.baud_rate_input = baud_rate.to_string();
            }
        });
    }

    fn show_connection_panel(&mut self, root_ui: &mut egui::Ui) {
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 202));
        egui::Panel::top("connection_panel")
            .resizable(false)
            .frame(panel_frame)
            .show(root_ui, |ui| {
                ui.add_space(4.0);
                let control_height = toolbar_control_height(ui);
                ui.spacing_mut().interact_size.y = control_height;
                ui.horizontal_wrapped(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(76.0, control_height),
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
                            .width(combo_width(ui, &self.port_display_name(), 112.0))
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
                            .add(
                                egui::Button::new("刷新")
                                    .min_size(egui::vec2(56.0, control_height)),
                            )
                            .on_hover_text("重新扫描可用串口")
                            .clicked()
                        {
                            if let Err(message) = self.worker.refresh_ports() {
                                self.set_notice(message, true);
                            }
                            self.last_port_refresh = Instant::now();
                        }

                        toolbar_label(ui, "波特率", 52.0);
                        self.show_baud_rate_control(ui);
                    });

                    let button_text = match self.connection {
                        ConnectionState::Disconnected => "打开串口",
                        ConnectionState::Connecting => "正在连接...",
                        ConnectionState::Connected(_) => "关闭串口",
                    };
                    let enabled = !matches!(self.connection, ConnectionState::Connecting);
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(button_text)
                                .min_size(egui::vec2(88.0, control_height)),
                        )
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
                        .add(egui::Button::new("设置").min_size(egui::vec2(60.0, control_height)))
                        .clicked()
                    {
                        self.background_url_draft
                            .clone_from(&self.preferences.background_online_url);
                        self.sync_background_opacity_drafts();
                        self.settings_open = true;
                        self.settings_center_on_open = true;
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    ui.set_min_height(control_height);
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
                let control_height = toolbar_control_height(ui);
                ui.spacing_mut().interact_size.y = control_height;
                ui.horizontal_wrapped(|ui| {
                    toolbar_label(ui, "接收", 38.0);
                    for mode in [ReceiveMode::Text, ReceiveMode::Hex, ReceiveMode::Terminal] {
                        let width = if mode == ReceiveMode::Terminal {
                            64.0
                        } else {
                            50.0
                        };
                        if ui
                            .add(
                                egui::Button::selectable(
                                    self.preferences.receive_mode == mode,
                                    mode.label(),
                                )
                                .min_size(egui::vec2(width, control_height)),
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
                    let encoding_label = self.preferences.text_encoding.label();
                    egui::ComboBox::from_id_salt("text_encoding")
                        .selected_text(encoding_label)
                        .width(combo_width(ui, encoding_label, 92.0))
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
                    let timestamps_changed = ui
                        .checkbox(&mut self.preferences.timestamps, "时间戳")
                        .changed();
                    preferences_changed |= timestamps_changed;
                    if timestamps_changed {
                        self.request_search(true);
                    }
                    preferences_changed |= ui
                        .checkbox(&mut self.preferences.auto_scroll, "自动滚动")
                        .changed();

                    let pause_label = if self.paused {
                        "继续显示"
                    } else {
                        "暂停显示"
                    };
                    if ui
                        .add(
                            egui::Button::new(pause_label)
                                .min_size(egui::vec2(80.0, control_height)),
                        )
                        .clicked()
                    {
                        self.paused = !self.paused;
                        self.invalidate_format();
                    }
                    if ui
                        .add(egui::Button::new("到底部").min_size(egui::vec2(64.0, control_height)))
                        .clicked()
                    {
                        self.force_scroll_bottom = true;
                        self.preferences.auto_scroll = true;
                        preferences_changed = true;
                    }

                    toolbar_separator(ui);
                    if ui
                        .add(egui::Button::new("清接收").min_size(egui::vec2(64.0, control_height)))
                        .clicked()
                    {
                        self.clear_receive();
                    }
                    if ui
                        .add(
                            egui::Button::new("全部清空")
                                .min_size(egui::vec2(80.0, control_height)),
                        )
                        .clicked()
                    {
                        self.clear_receive();
                        self.send_input.clear();
                        self.send_error = None;
                    }
                    if ui
                        .add_enabled_ui(self.receive_bytes_len() > 0, |ui| {
                            ui.add(
                                egui::Button::new("导出 TXT")
                                    .min_size(egui::vec2(80.0, control_height)),
                            )
                        })
                        .inner
                        .clicked()
                    {
                        self.export_snapshot(&context);
                    }
                });
                self.show_search_bar(ui, &context);
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

    fn show_search_bar(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        if context.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::F)) {
            self.focus_search = true;
        }

        let mut search_changed = false;
        let mut navigation = 0_isize;
        let mut highlight_action = None;
        let search_current = self.search_index_is_current();
        let highlight_rules = Arc::clone(&self.highlight_rules);
        let highlight_config_error = self.highlight_config_error.clone();
        let control_height = toolbar_control_height(ui);
        egui::containers::Sides::new()
            .height(control_height)
            .shrink_left()
            .show(
                ui,
                |ui| {
                    toolbar_label(ui, "查找", 38.0);
                    let status_width = label_width(ui, "表达式错误", 88.0);
                    let trailing_width = 32.0
                        + 44.0
                        + 44.0
                        + 64.0
                        + 56.0
                        + 56.0
                        + status_width
                        + ui.spacing().item_spacing.x * 7.0;
                    let search_width = (ui.available_width() - trailing_width).max(160.0);
                    let search_id = ui.make_persistent_id("receive_search_input");
                    let response = ui.add_sized(
                        [search_width, control_height],
                        egui::TextEdit::singleline(&mut self.search_query)
                            .id(search_id)
                            .hint_text("输入文本或正则表达式（Ctrl+F）")
                            .vertical_align(Align::Center),
                    );
                    if self.focus_search {
                        response.request_focus();
                        self.focus_search = false;
                    }
                    search_changed |= response.changed();
                    if response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        navigation = if ui.input(|input| input.modifiers.shift) {
                            -1
                        } else {
                            1
                        };
                    }

                    if ui
                        .add_enabled(
                            !self.search_query.is_empty(),
                            egui::Button::new("×").min_size(egui::vec2(32.0, control_height)),
                        )
                        .on_hover_text("清除搜索")
                        .clicked()
                    {
                        self.search_query.clear();
                        search_changed = true;
                    }

                    if ui
                        .add(
                            egui::Button::selectable(self.search_case_sensitive, "Aa")
                                .min_size(egui::vec2(44.0, control_height)),
                        )
                        .on_hover_text("区分大小写")
                        .clicked()
                    {
                        self.search_case_sensitive = !self.search_case_sensitive;
                        search_changed = true;
                    }
                    if ui
                        .add(
                            egui::Button::selectable(self.search_regex, ".*")
                                .min_size(egui::vec2(44.0, control_height)),
                        )
                        .on_hover_text("使用正则表达式")
                        .clicked()
                    {
                        self.search_regex = !self.search_regex;
                        search_changed = true;
                    }

                    let filter_response = ui
                        .add_enabled(
                            !self.search_query.is_empty()
                                && search_current
                                && !self.search_pending
                                && !self.search_in_progress
                                && self.search_index.error.is_none(),
                            egui::Button::selectable(self.search_filter, "过滤")
                                .min_size(egui::vec2(64.0, control_height)),
                        )
                        .on_hover_text("只显示包含匹配项的行");
                    if filter_response.clicked() {
                        self.search_filter = !self.search_filter;
                        self.queue_selected_search_scroll();
                    }

                    let can_navigate = search_current
                        && !self.search_pending
                        && !self.search_in_progress
                        && !self.search_index.matches.is_empty();
                    if ui
                        .add_enabled(
                            can_navigate,
                            egui::Button::new("上一处").min_size(egui::vec2(56.0, control_height)),
                        )
                        .clicked()
                    {
                        navigation = -1;
                    }
                    if ui
                        .add_enabled(
                            can_navigate,
                            egui::Button::new("下一处").min_size(egui::vec2(56.0, control_height)),
                        )
                        .clicked()
                    {
                        navigation = 1;
                    }

                    let status_error = search_current
                        .then(|| self.search_index.error.clone())
                        .flatten();
                    let status = if self.search_pending
                        || self.search_in_progress
                        || (!self.search_query.is_empty() && !search_current)
                    {
                        RichText::new("搜索中…").color(ui.visuals().weak_text_color())
                    } else if status_error.is_some() {
                        RichText::new("表达式错误")
                            .color(ui.visuals().error_fg_color)
                            .underline()
                    } else if self.search_query.is_empty() {
                        RichText::new("—").color(ui.visuals().weak_text_color())
                    } else {
                        let selected = self.search_selected_match.map_or(0, |index| index + 1);
                        let suffix = if self.search_index.truncated { "+" } else { "" };
                        RichText::new(format!(
                            "{selected}/{}{suffix}",
                            self.search_index.matches.len()
                        ))
                    };
                    let status_response = ui.add_sized(
                        [status_width, control_height],
                        egui::Label::new(status).truncate(),
                    );
                    if let Some(error) = status_error {
                        status_response.on_hover_text(error);
                    }
                },
                |ui| {
                    Self::show_highlight_menu(
                        ui,
                        &highlight_rules,
                        highlight_config_error.as_deref(),
                        &mut highlight_action,
                    )
                },
            );

        if search_changed {
            self.request_search(true);
            context.request_repaint_after(SEARCH_DEBOUNCE);
        }
        if navigation != 0 {
            self.navigate_search(navigation);
        }
        self.handle_highlight_action(highlight_action, context);
    }

    fn show_highlight_menu(
        ui: &mut egui::Ui,
        highlight_rules: &HighlightRules,
        highlight_config_error: Option<&str>,
        highlight_action: &mut Option<HighlightConfigAction>,
    ) {
        let path_display = highlight_rules.path().display().to_string();
        let button_text = if highlight_config_error.is_some() {
            "高亮 !".to_owned()
        } else {
            format!("高亮 {}", highlight_rules.len())
        };
        ui.menu_button(button_text, |ui| {
            ui.set_min_width(360.0);
            ui.label(
                RichText::new(&path_display)
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            if let Some(error) = highlight_config_error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }
            ui.separator();
            if ui.button("重新加载").clicked() {
                *highlight_action = Some(HighlightConfigAction::Reload);
                ui.close();
            }
            if ui.button("打开配置文件").clicked() {
                *highlight_action = Some(HighlightConfigAction::Open);
                ui.close();
            }
            if ui.button("复制配置路径").clicked() {
                *highlight_action = Some(HighlightConfigAction::CopyPath);
                ui.close();
            }
        })
        .response
        .on_hover_text("高亮规则由 highlight.toml 管理");
    }

    fn handle_highlight_action(
        &mut self,
        highlight_action: Option<HighlightConfigAction>,
        context: &egui::Context,
    ) {
        match highlight_action {
            Some(HighlightConfigAction::Reload) => self.reload_highlight_rules(),
            Some(HighlightConfigAction::Open) => {
                if let Err(error) = open_config_file(self.highlight_rules.path()) {
                    self.set_notice(error, true);
                }
            }
            Some(HighlightConfigAction::CopyPath) => {
                context.copy_text(self.highlight_rules.path().display().to_string());
                self.set_notice("已复制高亮配置路径", false);
            }
            None => {}
        }
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
        let line_spacing = self.preferences.data_line_spacing;
        let data_font = FontId::new(self.preferences.data_font_size, data_font_family());
        let rows = Arc::clone(&self.display_rows);
        let timestamps = self.preferences.timestamps;
        let search_index = Arc::clone(&self.search_index);
        let highlight_rules = Arc::clone(&self.highlight_rules);
        let search_current = self.search_index_is_current();
        let selected_match = search_current
            .then_some(self.search_selected_match)
            .flatten();
        let filter_active = self.search_filter
            && !self.search_query.is_empty()
            && search_current
            && search_index.error.is_none();
        let visible_row_count = if filter_active {
            search_index.matched_row_count(rows.len())
        } else {
            rows.len()
        };
        if visible_row_count == 0 {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(if self.search_pending || self.search_in_progress {
                        "正在搜索…"
                    } else {
                        "没有匹配的接收数据"
                    })
                    .color(ui.visuals().weak_text_color()),
                );
            });
            return;
        }

        let mut scroll_area = egui::ScrollArea::both()
            .id_salt("receive_output")
            .auto_shrink([false, false])
            .stick_to_bottom(self.preferences.auto_scroll && self.search_query.is_empty());
        if self.force_scroll_bottom {
            scroll_area = scroll_area.vertical_scroll_offset(f32::INFINITY);
            self.force_scroll_bottom = false;
        } else if let Some(visible_row) = self.search_scroll_to_row.take() {
            scroll_area = scroll_area
                .vertical_scroll_offset(visible_row as f32 * (row_height + line_spacing));
        }
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = line_spacing;
            scroll_area.show_rows(ui, row_height, visible_row_count, |ui, range| {
                for visible_row in range {
                    let row_index = if filter_active {
                        search_index.matched_rows[visible_row]
                    } else {
                        visible_row
                    };
                    let Some(row) = rows.get(row_index) else {
                        continue;
                    };
                    let text = display_text(row, timestamps);
                    let line_style = highlight_rules.style_for(&row.text);
                    let (match_offset, row_matches) = if search_current {
                        search_index.matches_for_row(row_index)
                    } else {
                        (0, &[] as &[SearchMatch])
                    };
                    let layout_job = receive_row_layout_job(
                        ui,
                        &text,
                        &data_font,
                        line_style,
                        match_offset,
                        row_matches,
                        selected_match,
                    );
                    ui.add(
                        egui::Label::new(layout_job)
                            .selectable(true)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                }
            });
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
        let root_control_height = toolbar_control_height(root_ui);
        let min_panel_height = (root_control_height * 2.0 + 72.0).max(150.0);
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
            .size_range(min_panel_height..=280.0_f32.max(min_panel_height + 48.0))
            .frame(panel_frame)
            .show(root_ui, |ui| {
                let repeat_running = self.repeat.is_some();
                let mut send_requested = false;
                let control_height = toolbar_control_height(ui);
                ui.spacing_mut().interact_size.y = control_height;
                ui.horizontal_wrapped(|ui| {
                    toolbar_label(ui, "发送", 38.0);
                    ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.add_enabled_ui(!repeat_running, |ui| {
                            for mode in [SendMode::Text, SendMode::Hex] {
                                if ui
                                    .add(
                                        egui::Button::selectable(
                                            self.preferences.send_mode == mode,
                                            mode.label(),
                                        )
                                        .min_size(egui::vec2(50.0, control_height)),
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
                            let ending_label = self.preferences.line_ending.label();
                            egui::ComboBox::from_id_salt("line_ending")
                                .selected_text(ending_label)
                                .width(combo_width(ui, ending_label, 92.0))
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
                            egui::Button::new("清发送").min_size(egui::vec2(72.0, control_height)),
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
                                [92.0, control_height],
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
                    let repeat_available = self.connection.is_connected() || repeat_running;
                    let repeat_response = ui
                        .add_enabled_ui(repeat_available, |ui| {
                            ui.checkbox(&mut repeat_checkbox, "循环发送")
                        })
                        .inner
                        .on_disabled_hover_text("请先打开串口");
                    if repeat_response.changed() {
                        if repeat_checkbox {
                            self.start_repeat(&context);
                        } else {
                            self.repeat = None;
                        }
                    }

                    let can_send = self.connection.is_connected() && !repeat_running;
                    let mut button = egui::Button::new(RichText::new("发送").strong());
                    if can_send {
                        button = button.fill(ui.visuals().selection.bg_fill);
                    }
                    send_requested = ui
                        .add_enabled(can_send, button.min_size(egui::vec2(72.0, control_height)))
                        .clicked();
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
        let control_height = toolbar_control_height(root_ui);
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 210));
        egui::Panel::bottom("status_panel")
            .resizable(false)
            .exact_size(control_height)
            .frame(panel_frame)
            .show(root_ui, |ui| {
                ui.spacing_mut().interact_size.y = control_height;
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
        let viewport =
            context.input(|input| input.viewport_rect().shrink(SETTINGS_VIEWPORT_MARGIN));
        let settings_window_width =
            preferred_settings_window_width(viewport.width(), self.preferences.ui_font_size);
        let max_window_height = viewport.height().max(160.0);
        let default_window_height = SETTINGS_WINDOW_HEIGHT.min(max_window_height);
        let default_position = centered_window_position(
            viewport,
            egui::vec2(settings_window_width, default_window_height),
        );
        let mut window = egui::Window::new("设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(settings_window_width)
            .min_width(settings_window_width)
            .max_width(settings_window_width)
            .max_height(max_window_height)
            .vscroll(true);
        if self.settings_center_on_open {
            window = window.fixed_pos(default_position);
        } else {
            window = window.default_pos(default_position);
        }
        window.show(context, |ui| {
            ui.spacing_mut().interact_size.y = toolbar_control_height(ui);
            ui.set_min_size(egui::vec2(
                (settings_window_width - 32.0).max(240.0),
                (default_window_height - 64.0).max(240.0),
            ));
            self.show_settings_tabs(ui);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            match self.settings_tab {
                SettingsTab::Fonts => self.show_font_settings_tab(ui, context),
                SettingsTab::Background => self.show_background_settings_tab(ui, context),
            }
        });
        self.settings_center_on_open = false;
        self.settings_open = open;
    }

    fn show_settings_tabs(&mut self, ui: &mut egui::Ui) {
        let gap = ui.spacing().item_spacing.x;
        let tab_width = (ui.available_width() - gap) * 0.5;
        let control_height = toolbar_control_height(ui).max(38.0);
        ui.horizontal(|ui| {
            for (tab, label) in [
                (SettingsTab::Fonts, "字体与显示"),
                (SettingsTab::Background, "应用背景"),
            ] {
                if ui
                    .add(
                        egui::Button::selectable(self.settings_tab == tab, label)
                            .min_size(egui::vec2(tab_width, control_height)),
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
                            let family_width =
                                combo_width(ui, &self.preferences.ui_font_family, 292.0);
                            egui::ComboBox::from_id_salt("ui_font")
                                .selected_text(&self.preferences.ui_font_family)
                                .width(family_width)
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
                            let weight_label = self.font_catalog.weight_label(
                                &self.preferences.ui_font_family,
                                self.preferences.ui_font_weight,
                            );
                            let weight_width = combo_width(ui, &weight_label, 118.0);
                            egui::ComboBox::from_id_salt("ui_font_weight")
                                .selected_text(weight_label)
                                .width(weight_width)
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
                            font_changed |= font_size_combo(
                                ui,
                                "ui_font_size",
                                &mut self.preferences.ui_font_size,
                                MAX_UI_FONT_SIZE,
                            );
                        });
                        ui.end_row();

                        settings_row_label(ui, "数据字体");
                        settings_controls(ui, |ui| {
                            let family_width =
                                combo_width(ui, &self.preferences.data_font_family, 292.0);
                            egui::ComboBox::from_id_salt("data_font")
                                .selected_text(&self.preferences.data_font_family)
                                .width(family_width)
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
                            let weight_label = self.font_catalog.weight_label(
                                &self.preferences.data_font_family,
                                self.preferences.data_font_weight,
                            );
                            let weight_width = combo_width(ui, &weight_label, 118.0);
                            egui::ComboBox::from_id_salt("data_font_weight")
                                .selected_text(weight_label)
                                .width(weight_width)
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
                            preferences_changed |= font_size_combo(
                                ui,
                                "data_font_size",
                                &mut self.preferences.data_font_size,
                                MAX_DATA_FONT_SIZE,
                            );
                        });
                        ui.end_row();

                        settings_row_label(ui, "行距");
                        settings_controls(ui, |ui| {
                            let control_height = toolbar_control_height(ui);
                            preferences_changed |= ui
                                .add_sized(
                                    [220.0, control_height],
                                    egui::Slider::new(
                                        &mut self.preferences.data_line_spacing,
                                        MIN_DATA_LINE_SPACING..=MAX_DATA_LINE_SPACING,
                                    )
                                    .integer()
                                    .suffix(" px"),
                                )
                                .on_hover_text("调整接收显示区相邻两行文本之间的间距")
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
            "选择纯色、本地图片或在线图片。",
            |ui| {
                let gap = ui.spacing().item_spacing.x;
                let option_width = (ui.available_width() - gap * 2.0) / 3.0;
                let control_height = toolbar_control_height(ui);
                ui.horizontal_wrapped(|ui| {
                    for source in [
                        AppBackgroundSource::None,
                        AppBackgroundSource::Local,
                        AppBackgroundSource::Online,
                    ] {
                        if ui
                            .add(
                                egui::Button::selectable(
                                    self.preferences.background_source == source,
                                    source.label(),
                                )
                                .min_size(egui::vec2(option_width, control_height)),
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

                ui.add_space(6.0);
                match self.preferences.background_source {
                    AppBackgroundSource::None => {
                        ui.label(
                            RichText::new("当前使用主题的默认纯色背景。")
                                .color(ui.visuals().weak_text_color()),
                        );
                    }
                    AppBackgroundSource::Local => {
                        ui.horizontal_wrapped(|ui| {
                            let control_height = toolbar_control_height(ui);
                            let mut path_display =
                                if self.preferences.background_local_path.trim().is_empty() {
                                    "尚未选择图片".to_owned()
                                } else {
                                    self.preferences.background_local_path.clone()
                                };
                            let field_width = (ui.available_width() - 184.0).max(220.0);
                            ui.add_sized(
                                [field_width, control_height],
                                egui::TextEdit::singleline(&mut path_display).interactive(false),
                            )
                            .on_hover_text(&path_display);
                            if ui
                                .add(
                                    egui::Button::new("选择图片")
                                        .min_size(egui::vec2(88.0, control_height)),
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
                                        .min_size(egui::vec2(80.0, control_height)),
                                )
                                .clicked()
                            {
                                reload_requested = true;
                            }
                        });
                        self.show_background_load_status(ui, false);
                    }
                    AppBackgroundSource::Online => {
                        ui.horizontal_wrapped(|ui| {
                            let control_height = toolbar_control_height(ui);
                            let field_width = (ui.available_width() - 92.0).max(260.0);
                            let response = ui.add_sized(
                                [field_width, control_height],
                                egui::TextEdit::singleline(&mut self.background_url_draft)
                                    .hint_text("https://example.com/background.jpg"),
                            );
                            let enter_pressed = response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter));
                            let apply_clicked = ui
                                .add(
                                    egui::Button::new("应用地址")
                                        .min_size(egui::vec2(84.0, control_height)),
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

        ui.add_space(8.0);
        settings_card(
            ui,
            "背景不透明度",
            "直接输入 0.0–1.0 的小数，亮色与暗色主题分别保存。",
            |ui| {
                egui::Grid::new("background_opacity_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        settings_row_label(ui, "亮色模式");
                        settings_controls(ui, |ui| {
                            preferences_changed |= background_opacity_control(
                                ui,
                                &mut self.background_light_opacity_draft,
                                &mut self.preferences.background_light_opacity,
                            );
                        });
                        ui.end_row();

                        settings_row_label(ui, "暗色模式");
                        settings_controls(ui, |ui| {
                            preferences_changed |= background_opacity_control(
                                ui,
                                &mut self.background_dark_opacity_draft,
                                &mut self.preferences.background_dark_opacity,
                            );
                        });
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    let control_height = toolbar_control_height(ui);
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
                                .min_size(egui::vec2(104.0, control_height)),
                        )
                        .clicked()
                    {
                        self.preferences.background_light_opacity =
                            DEFAULT_BACKGROUND_LIGHT_OPACITY;
                        self.preferences.background_dark_opacity = DEFAULT_BACKGROUND_DARK_OPACITY;
                        self.sync_background_opacity_drafts();
                        preferences_changed = true;
                    }
                });
            },
        );

        ui.add_space(8.0);
        settings_card(ui, "效果预览", "", |ui| {
            self.show_background_previews(ui);
        });

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
        ui.add_space(3.0);
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
        let control_height = toolbar_control_height(ui);
        let button = egui::Button::new(format!("历史 ({})", self.history.len()))
            .min_size(egui::vec2(82.0, control_height));
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
        let baud_rate = match parse_baud_rate(&self.baud_rate_input) {
            Ok(baud_rate) => baud_rate,
            Err(message) => {
                self.set_notice(message, true);
                return;
            }
        };
        self.serial_config.baud_rate = baud_rate;
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
        let generation = if let Ok(mut store) = self.store.lock() {
            store.clear();
            store.generation()
        } else {
            self.display_generation
        };
        self.invalidate_format();
        self.display_rows = Arc::new(Vec::new());
        self.display_generation = generation;
        self.display_formatter = None;
        self.search_token = self.search_token.wrapping_add(1);
        self.search_pending = false;
        self.search_wait_for_debounce = false;
        self.search_reset_selection = false;
        self.search_index = Arc::new(SearchIndex::default());
        self.search_index_generation = Some(generation);
        self.search_selected_match = None;
        self.search_scroll_to_row = None;
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

fn receive_row_layout_job(
    ui: &egui::Ui,
    text: &str,
    font: &FontId,
    line_style: Option<HighlightStyle>,
    match_offset: usize,
    row_matches: &[SearchMatch],
    selected_match: Option<usize>,
) -> egui::text::LayoutJob {
    let base_color = line_style
        .and_then(|style| style.foreground)
        .unwrap_or_else(|| ui.visuals().text_color());
    let mut base_format = egui::TextFormat {
        font_id: font.clone(),
        color: base_color,
        ..Default::default()
    };
    if let Some(style) = line_style {
        base_format.background = style.background.unwrap_or(Color32::TRANSPARENT);
        if style.underline {
            base_format.underline = egui::Stroke::new(1.0, base_color);
        }
    }

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut cursor = 0;
    for (local_index, found) in row_matches.iter().enumerate() {
        let range = &found.byte_range;
        if range.start < cursor
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        if cursor < range.start {
            job.append(&text[cursor..range.start], 0.0, base_format.clone());
        }
        let is_selected = selected_match == Some(match_offset + local_index);
        let mut match_format = base_format.clone();
        match_format.background = if is_selected {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().selection.bg_fill.gamma_multiply(0.48)
        };
        if is_selected {
            match_format.color = ui.visuals().selection.stroke.color;
        }
        job.append(&text[range.clone()], 0.0, match_format);
        cursor = range.end;
    }
    if cursor < text.len() || text.is_empty() {
        job.append(&text[cursor..], 0.0, base_format);
    }
    job
}

#[cfg(target_os = "windows")]
fn open_config_file(path: &Path) -> Result<(), String> {
    Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开高亮配置文件：{error}"))
}

#[cfg(target_os = "macos")]
fn open_config_file(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开高亮配置文件：{error}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_config_file(path: &Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开高亮配置文件：{error}"))
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
        .width(combo_width(ui, selected_text, CONFIG_COMBO_WIDTH))
        .show_ui(ui, contents);
}

fn toolbar_label(ui: &mut egui::Ui, text: &'static str, width: f32) {
    let width = label_width(ui, text, width);
    let control_height = toolbar_control_height(ui);
    ui.allocate_ui_with_layout(
        egui::vec2(width, control_height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.label(text);
        },
    );
}

fn toolbar_separator(ui: &mut egui::Ui) {
    let height = (toolbar_control_height(ui) - 10.0).max(22.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

fn toolbar_control_height(ui: &mut egui::Ui) -> f32 {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let text_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id));
    toolbar_control_height_from_metrics(text_height, ui.spacing().button_padding.y)
}

fn toolbar_control_height_from_metrics(text_height: f32, vertical_padding: f32) -> f32 {
    (text_height + vertical_padding * 2.0)
        .ceil()
        .max(MIN_CONTROL_HEIGHT)
}

fn styled_text_width(ui: &mut egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font_id = style.resolve(ui.style());
    ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font_id, Color32::WHITE)
            .size()
            .x
    })
}

fn label_width(ui: &mut egui::Ui, text: &str, minimum: f32) -> f32 {
    styled_text_width(ui, text, egui::TextStyle::Body)
        .ceil()
        .max(minimum)
}

fn text_field_width(ui: &mut egui::Ui, sample: &str, minimum: f32) -> f32 {
    (styled_text_width(ui, sample, egui::TextStyle::Body) + 20.0)
        .ceil()
        .max(minimum)
}

fn combo_width(ui: &mut egui::Ui, selected_text: &str, minimum: f32) -> f32 {
    (styled_text_width(ui, selected_text, egui::TextStyle::Button)
        + ui.spacing().button_padding.x * 2.0
        + ui.spacing().icon_width
        + ui.spacing().icon_spacing)
        .ceil()
        .max(minimum)
}

fn preferred_settings_window_width(available_width: f32, ui_font_size: f32) -> f32 {
    let large_font_extra = (ui_font_size - 15.0).max(0.0) * 10.0;
    (SETTINGS_WINDOW_WIDTH + large_font_extra).min(available_width.max(0.0))
}

fn centered_window_position(viewport: egui::Rect, window_size: egui::Vec2) -> egui::Pos2 {
    egui::pos2(
        viewport.left() + (viewport.width() - window_size.x).max(0.0) * 0.5,
        viewport.top() + (viewport.height() - window_size.y).max(0.0) * 0.5,
    )
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
        .inner_margin(egui::Margin::same(10));
    frame
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong());
            if !description.is_empty() {
                ui.label(
                    RichText::new(description)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(6.0);
            }
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

fn background_opacity_control(ui: &mut egui::Ui, draft: &mut String, opacity: &mut f32) -> bool {
    let control_height = toolbar_control_height(ui);
    let input_is_valid = parse_background_opacity(draft).is_ok();
    let text_color = if input_is_valid {
        ui.visuals().text_color()
    } else {
        ui.visuals().error_fg_color
    };
    let input_stroke = if input_is_valid {
        ui.visuals().widgets.noninteractive.bg_stroke
    } else {
        egui::Stroke::new(1.0, ui.visuals().error_fg_color)
    };
    let input_frame = egui::Frame::new()
        .fill(ui.visuals().text_edit_bg_color())
        .stroke(input_stroke)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 4));
    let response = ui.add_sized(
        [140.0, control_height],
        egui::TextEdit::singleline(draft)
            .char_limit(6)
            .horizontal_align(Align::Center)
            .vertical_align(Align::Center)
            .text_color(text_color)
            .frame(input_frame),
    );
    let mut changed = false;
    if response.changed()
        && let Ok(value) = parse_background_opacity(draft)
        && *opacity != value
    {
        *opacity = value;
        changed = true;
    }

    if response.lost_focus() {
        *draft = parse_background_opacity(draft)
            .map(format_background_opacity)
            .unwrap_or_else(|_| format_background_opacity(*opacity));
    }

    match parse_background_opacity(draft) {
        Ok(_) => response.on_hover_text("输入 0.0 到 1.0 之间的小数"),
        Err(message) => response.on_hover_text(message),
    };
    changed
}

fn parse_background_opacity(input: &str) -> Result<f32, &'static str> {
    let value = input.trim();
    if value.is_empty() {
        return Err("请输入不透明度小数");
    }
    if value.chars().filter(|character| *character == '.').count() > 1
        || !value
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        return Err("只能输入 0.0 到 1.0 之间的小数");
    }
    let parsed = value
        .parse::<f32>()
        .map_err(|_| "请输入有效的不透明度小数")?;
    if !parsed.is_finite() || !(0.0..=1.0).contains(&parsed) {
        return Err("不透明度必须在 0.0 到 1.0 之间");
    }
    Ok(parsed)
}

fn format_background_opacity(opacity: f32) -> String {
    let mut formatted = format!("{:.4}", opacity.clamp(0.0, 1.0));
    while formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.push('0');
    }
    formatted
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
    response.on_hover_text(format!(
        "背景不透明度 {}",
        format_background_opacity(opacity)
    ));
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
    let control_height = toolbar_control_height(ui);
    let label_width = label_width(ui, text, SETTINGS_LABEL_WIDTH);
    ui.allocate_ui_with_layout(
        egui::vec2(label_width, control_height),
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
    let control_height = toolbar_control_height(ui);
    let controls_width = ui.available_width().max(0.0);
    ui.allocate_ui_with_layout(
        egui::vec2(controls_width, control_height),
        Layout::left_to_right(Align::Center).with_main_wrap(true),
        contents,
    )
    .inner
}

fn font_size_combo(
    ui: &mut egui::Ui,
    id: &'static str,
    font_size: &mut f32,
    max_font_size: f32,
) -> bool {
    let mut changed = false;
    let widest_label = format!("{max_font_size:.0} pt");
    egui::ComboBox::from_id_salt(id)
        .selected_text(format!("{font_size:.0} pt"))
        .width(combo_width(ui, &widest_label, 120.0))
        .show_ui(ui, |ui| {
            for size in MIN_FONT_SIZE as u16..=max_font_size as u16 {
                changed |= ui
                    .selectable_value(font_size, f32::from(size), format!("{size} pt"))
                    .changed();
            }
        })
        .response
        .on_hover_text(format!("选择字号（10–{max_font_size:.0} pt）"));
    changed
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
    fn toolbar_control_height_expands_with_large_text_metrics() {
        assert_eq!(toolbar_control_height_from_metrics(13.0, 3.0), 32.0);
        assert_eq!(toolbar_control_height_from_metrics(50.0, 4.0), 58.0);
    }

    #[test]
    fn settings_window_width_is_font_aware_and_viewport_bounded() {
        assert_eq!(preferred_settings_window_width(1248.0, 15.0), 660.0);
        assert_eq!(preferred_settings_window_width(992.0, 18.0), 690.0);
        assert_eq!(preferred_settings_window_width(608.0, 15.0), 608.0);
    }

    #[test]
    fn settings_window_default_position_is_centered() {
        let viewport = egui::Rect::from_min_size(egui::pos2(16.0, 16.0), egui::vec2(992.0, 608.0));
        assert_eq!(
            centered_window_position(viewport, egui::vec2(660.0, 454.0)),
            egui::pos2(182.0, 93.0)
        );
    }

    #[test]
    fn background_opacity_input_accepts_only_bounded_decimals() {
        assert_eq!(parse_background_opacity("0.22"), Ok(0.22));
        assert_eq!(parse_background_opacity(".5"), Ok(0.5));
        assert_eq!(parse_background_opacity("1.0"), Ok(1.0));
        assert!(parse_background_opacity("").is_err());
        assert!(parse_background_opacity(".").is_err());
        assert!(parse_background_opacity("1.01").is_err());
        assert!(parse_background_opacity("20%").is_err());
    }

    #[test]
    fn background_opacity_is_formatted_as_a_compact_decimal() {
        assert_eq!(format_background_opacity(0.0), "0.0");
        assert_eq!(format_background_opacity(0.22), "0.22");
        assert_eq!(format_background_opacity(0.5), "0.5");
        assert_eq!(format_background_opacity(1.0), "1.0");
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
    fn receive_row_layout_combines_rule_and_search_highlights() {
        egui::__run_test_ui(|ui| {
            let rule_background = Color32::from_rgba_unmultiplied(220, 40, 40, 80);
            let matches = [SearchMatch {
                row_index: 0,
                byte_range: 0..5,
            }];
            let selected_background = ui.visuals().selection.bg_fill;
            let job = receive_row_layout_job(
                ui,
                "ERROR ready",
                &FontId::monospace(15.0),
                Some(HighlightStyle {
                    foreground: None,
                    background: Some(rule_background),
                    underline: true,
                }),
                0,
                &matches,
                Some(0),
            );

            assert_eq!(job.sections.len(), 2);
            assert_eq!(job.sections[0].byte_range.start.0, 0);
            assert_eq!(job.sections[0].byte_range.end.0, 5);
            assert_eq!(job.sections[0].format.background, selected_background);
            assert_eq!(job.sections[1].format.background, rule_background);
            assert_ne!(job.sections[1].format.underline, egui::Stroke::NONE);
        });
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
