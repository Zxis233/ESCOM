use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn maybe_start_format(&mut self, context: &egui::Context) {
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
                .map(|store| store.delta_since_bounded(cursor, MAX_DISPLAY_INCREMENT_BYTES));
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
                    self.terminal_cursor = self
                        .display_formatter
                        .as_ref()
                        .and_then(DisplayFormatter::terminal_cursor);
                    self.apply_display_update(update);
                    self.last_format_started = Instant::now();
                    return;
                }
            }
            self.force_format = true;
        }

        let snapshot = self
            .store
            .lock()
            .ok()
            .map(|store| store.tail_snapshot(display_snapshot_limit(mode)));
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
                    formatter: Box::new(formatter),
                });
                repaint_context.request_repaint();
            })
            .expect("failed to start formatting task");
    }

    pub(super) fn apply_display_update(&mut self, update: DisplayUpdate) {
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
                    SearchDisplayOptions::new(
                        self.preferences.timestamps,
                        &self.preferences.timestamp_format,
                    ),
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

    pub(super) fn show_output_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        let toolbar_frame = egui::Frame::central_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, RECEIVE_SEND_PANEL_ALPHA));
        egui::Panel::top("receive_toolbar_panel")
            .resizable(false)
            .frame(toolbar_frame)
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
                        .add_enabled_ui(
                            self.receive_bytes_len() > 0 && !self.export_in_progress,
                            |ui| {
                                ui.add(
                                    egui::Button::new("导出 TXT")
                                        .min_size(egui::vec2(80.0, control_height)),
                                )
                            },
                        )
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
            });

        let content_frame = egui::Frame::central_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, RECEIVE_CONTENT_ALPHA));
        egui::CentralPanel::default()
            .frame(content_frame)
            .show(root_ui, |ui| {
                if self.preferences.receive_mode == ReceiveMode::Terminal {
                    self.show_terminal_surface(ui);
                } else {
                    self.show_receive_content(ui);
                }
            });
    }

    pub(super) fn show_receive_content(&mut self, ui: &mut egui::Ui) {
        if self.display_rows.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(if self.paused {
                        "显示被暂停啦 >_<"
                    } else {
                        "什么都没有哦 ˘o˘ ᶻᶻᶻ"
                    })
                    .size(self.preferences.ui_font_size * 2.0)
                    .color(ui.visuals().weak_text_color()),
                );
            });
            return;
        }

        let line_spacing = self.preferences.data_line_spacing;
        let data_font = FontId::new(self.preferences.data_font_size, data_font_family());
        let row_height = ui.fonts_mut(|fonts| fonts.row_height(&data_font)).max(1.0);
        let rows = Arc::clone(&self.display_rows);
        let timestamps = self.preferences.timestamps;
        let timestamp_format = self.preferences.timestamp_format.clone();
        let terminal_cursor = (self.preferences.receive_mode == ReceiveMode::Terminal)
            .then_some(self.terminal_cursor)
            .flatten();
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
            scroll_area = scroll_area.vertical_scroll_offset(virtual_rows_content_height(
                row_height,
                line_spacing,
                visible_row_count,
            ));
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
                    let text = display_text(row, timestamps, &timestamp_format);
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
                    let response = ui.add(
                        egui::Label::new(layout_job)
                            .selectable(true)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                    if let Some((cursor_row, cursor_col)) = terminal_cursor
                        && cursor_row == row_index
                    {
                        paint_terminal_cursor(
                            ui,
                            &response,
                            row,
                            &data_font,
                            cursor_col,
                            timestamps,
                            &timestamp_format,
                        );
                    }
                }
            });
        });
    }

    pub(super) fn clear_receive(&mut self) {
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
        self.terminal_cursor = None;
        self.search_token = self.search_token.wrapping_add(1);
        self.search_pending = false;
        self.search_wait_for_debounce = false;
        self.search_reset_selection = false;
        self.search_index = Arc::new(SearchIndex::default());
        self.search_index_generation = Some(generation);
        self.search_selected_match = None;
        self.search_scroll_to_row = None;
    }

    pub(super) fn export_snapshot(&mut self, context: &egui::Context) {
        let file_name = format!("ESCOM_{}.txt", Local::now().format("%Y%m%d_%H%M%S"));
        let Some(path) = rfd::FileDialog::new()
            .set_title("导出接收数据")
            .add_filter("文本文件", &["txt"])
            .set_file_name(&file_name)
            .save_file()
        else {
            return;
        };

        // Include bytes received while the modal file dialog was open, and avoid retaining an
        // obsolete snapshot if the user cancels the dialog.
        let snapshot = self.store.lock().ok().map(|store| store.snapshot());
        let Some(snapshot) = snapshot else {
            self.set_notice("无法读取接收缓存", true);
            return;
        };
        if snapshot.bytes_len == 0 {
            self.set_notice("接收区没有可导出的数据", true);
            return;
        }

        let mode = self.preferences.receive_mode;
        let encoding = self.preferences.text_encoding;
        let timestamps = self.preferences.timestamps;
        let timestamp_format = self.preferences.timestamp_format.clone();
        let sender = self.background_tx.clone();
        let repaint_context = context.clone();
        self.export_in_progress = true;
        let spawn_result = thread::Builder::new()
            .name("escom-export".into())
            .spawn(move || {
                let result = export_snapshot_to_file(
                    &path,
                    snapshot,
                    mode,
                    encoding,
                    timestamps,
                    &timestamp_format,
                )
                .map(|()| path)
                .map_err(|error| format!("导出失败：{error}"));
                let _ = sender.send(BackgroundEvent::Exported(result));
                repaint_context.request_repaint();
            });
        if let Err(error) = spawn_result {
            self.export_in_progress = false;
            self.set_notice(format!("无法启动导出任务：{error}"), true);
        }
    }

    pub(super) fn receive_bytes_len(&self) -> usize {
        self.store
            .lock()
            .map(|store| store.bytes_len())
            .unwrap_or(0)
    }
}

pub(super) fn virtual_rows_content_height(
    row_height: f32,
    line_spacing: f32,
    row_count: usize,
) -> f32 {
    if row_count == 0 || !row_height.is_finite() || !line_spacing.is_finite() {
        return 0.0;
    }

    let row_height = row_height.max(0.0);
    let line_spacing = line_spacing.max(0.0);
    let height = (row_height + line_spacing).mul_add(row_count as f32, -line_spacing);
    if height.is_finite() {
        height.max(0.0)
    } else {
        0.0
    }
}

fn paint_terminal_cursor(
    ui: &mut egui::Ui,
    response: &egui::Response,
    row: &FormattedRow,
    font: &FontId,
    column: usize,
    timestamps: bool,
    timestamp_format: &str,
) {
    let timestamp_prefix = timestamps.then(|| timestamp_prefix(row.received_at, timestamp_format));
    let (prefix_width, cell_width) = ui.fonts_mut(|fonts| {
        let prefix_width = timestamp_prefix.as_ref().map_or(0.0, |prefix| {
            fonts
                .layout_no_wrap(prefix.clone(), font.clone(), Color32::WHITE)
                .size()
                .x
        });
        (prefix_width, fonts.glyph_width(font, 'M').max(1.0))
    });
    let x = response.rect.left() + prefix_width + column as f32 * cell_width;
    let cursor_height = ui.fonts_mut(|fonts| fonts.row_height(font)).max(1.0);
    let top = response.rect.center().y - cursor_height * 0.5;
    ui.painter().vline(
        x,
        top..=top + cursor_height,
        egui::Stroke::new(1.5, ui.visuals().strong_text_color()),
    );
}

pub(super) fn receive_row_layout_job(
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
