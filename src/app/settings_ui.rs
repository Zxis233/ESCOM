use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn background_image_uri(&self) -> Option<String> {
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

    pub(super) fn sync_background_opacity_drafts(&mut self) {
        self.background_light_opacity_draft =
            format_background_opacity(self.preferences.background_light_opacity);
        self.background_dark_opacity_draft =
            format_background_opacity(self.preferences.background_dark_opacity);
    }

    pub(super) fn has_configured_background(&self) -> bool {
        match self.preferences.background_source {
            AppBackgroundSource::None => false,
            AppBackgroundSource::Local => !self.preferences.background_local_path.trim().is_empty(),
            AppBackgroundSource::Online => {
                !self.preferences.background_online_url.trim().is_empty()
            }
        }
    }

    pub(super) fn active_background_opacity(&self, dark_mode: bool) -> f32 {
        if dark_mode {
            self.preferences.background_dark_opacity
        } else {
            self.preferences.background_light_opacity
        }
    }

    pub(super) fn paint_app_background(&mut self, root_ui: &mut egui::Ui) {
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

    pub(super) fn reload_background_image(&mut self, context: &egui::Context) {
        if let Some(uri) = self.background_load_uri.take() {
            context.forget_image(&uri);
        }
        self.background_load_state = BackgroundLoadState::Idle;
        context.request_repaint();
    }

    pub(super) fn show_settings_window(&mut self, context: &egui::Context) {
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

    pub(super) fn show_settings_tabs(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn show_font_settings_tab(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
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

    pub(super) fn show_background_settings_tab(
        &mut self,
        ui: &mut egui::Ui,
        context: &egui::Context,
    ) {
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

    pub(super) fn show_background_load_status(&self, ui: &mut egui::Ui, check_draft: bool) {
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

    pub(super) fn show_background_previews(&self, ui: &mut egui::Ui) {
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
}

pub(super) fn preferred_settings_window_width(available_width: f32, ui_font_size: f32) -> f32 {
    let large_font_extra = (ui_font_size - 15.0).max(0.0) * 10.0;
    (SETTINGS_WINDOW_WIDTH + large_font_extra).min(available_width.max(0.0))
}

pub(super) fn centered_window_position(
    viewport: egui::Rect,
    window_size: egui::Vec2,
) -> egui::Pos2 {
    egui::pos2(
        viewport.left() + (viewport.width() - window_size.x).max(0.0) * 0.5,
        viewport.top() + (viewport.height() - window_size.y).max(0.0) * 0.5,
    )
}

pub(super) fn settings_card<R>(
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

pub(super) fn normalized_online_image_url(input: &str) -> Result<String, &'static str> {
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

pub(super) fn local_background_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(unc_path) = normalized.strip_prefix("//") {
        format!("file://{unc_path}")
    } else {
        format!("file:///{normalized}")
    }
}

pub(super) fn background_opacity_control(
    ui: &mut egui::Ui,
    draft: &mut String,
    opacity: &mut f32,
) -> bool {
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

pub(super) fn parse_background_opacity(input: &str) -> Result<f32, &'static str> {
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

pub(super) fn format_background_opacity(opacity: f32) -> String {
    let mut formatted = format!("{:.4}", opacity.clamp(0.0, 1.0));
    while formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.push('0');
    }
    formatted
}

pub(super) fn background_preview(
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

pub(super) fn paint_texture_cover(
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

pub(super) fn cover_uv(image_size: egui::Vec2, target_size: egui::Vec2) -> egui::Rect {
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

pub(super) fn settings_row_label(ui: &mut egui::Ui, text: &str) {
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

pub(super) fn show_theme_menu(
    ui: &mut egui::Ui,
    theme_preference: &mut egui::ThemePreference,
) -> bool {
    let current = *theme_preference;
    let mut changed = false;
    let response = ui
        .menu_button(
            RichText::new(theme_preference_icon(current)).size(THEME_ICON_SIZE),
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

pub(super) fn theme_preference_icon(preference: egui::ThemePreference) -> &'static str {
    match preference {
        egui::ThemePreference::System => "💻",
        egui::ThemePreference::Light => "☀",
        egui::ThemePreference::Dark => "🌙",
    }
}

pub(super) fn theme_preference_label(preference: egui::ThemePreference) -> &'static str {
    match preference {
        egui::ThemePreference::System => "跟随系统",
        egui::ThemePreference::Light => "亮色",
        egui::ThemePreference::Dark => "暗色",
    }
}

pub(super) fn settings_controls<R>(
    ui: &mut egui::Ui,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let control_height = toolbar_control_height(ui);
    let controls_width = ui.available_width().max(0.0);
    ui.allocate_ui_with_layout(
        egui::vec2(controls_width, control_height),
        Layout::left_to_right(Align::Center).with_main_wrap(true),
        contents,
    )
    .inner
}

pub(super) fn font_size_combo(
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
