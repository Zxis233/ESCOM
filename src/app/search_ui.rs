use std::path::Path;
use std::process::Command;

use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn request_search(&mut self, reset_selection: bool) {
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

    pub(super) fn request_search_for_display(&mut self) {
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

    pub(super) fn maybe_start_search(&mut self, context: &egui::Context) {
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
        let timestamp_format = self.preferences.timestamp_format.clone();
        let sender = self.background_tx.clone();
        let repaint_context = context.clone();
        self.search_pending = false;
        self.search_in_progress = true;
        self.search_wait_for_debounce = false;

        thread::Builder::new()
            .name("escom-search".into())
            .spawn(move || {
                let index = search::search_rows_with_matcher(
                    &rows,
                    &matcher,
                    SearchDisplayOptions::new(timestamps, &timestamp_format),
                );
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

    pub(super) fn search_index_is_current(&self) -> bool {
        self.search_query.is_empty()
            || self.search_index_generation == Some(self.display_generation)
    }

    pub(super) fn clamp_search_selection(&mut self) {
        if self.search_index.matches.is_empty() {
            self.search_selected_match = None;
            self.search_scroll_to_row = None;
        } else if let Some(selected) = self.search_selected_match {
            self.search_selected_match = Some(selected.min(self.search_index.matches.len() - 1));
        }
    }

    pub(super) fn navigate_search(&mut self, direction: isize) {
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

    pub(super) fn queue_selected_search_scroll(&mut self) {
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

    pub(super) fn reload_highlight_rules(&mut self) {
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

    pub(super) fn show_search_bar(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
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

    pub(super) fn show_highlight_menu(
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

    pub(super) fn handle_highlight_action(
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
