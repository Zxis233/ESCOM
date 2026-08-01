use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn maybe_refresh_ports(&mut self) {
        if self.connection.is_disconnected()
            && self.last_port_refresh.elapsed() >= PORT_REFRESH_INTERVAL
        {
            if let Err(message) = self.worker.refresh_ports() {
                self.set_notice(message, true);
            }
            self.last_port_refresh = Instant::now();
        }
    }

    pub(super) fn show_baud_rate_control(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn show_connection_panel(&mut self, root_ui: &mut egui::Ui) {
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

    pub(super) fn open_selected_port(&mut self) {
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

    pub(super) fn port_display_name(&self) -> String {
        if self.serial_config.port_name.is_empty() {
            return "选择串口".into();
        }
        if self.ports.contains(&self.serial_config.port_name) {
            self.serial_config.port_name.clone()
        } else {
            format!("{}（不可用）", self.serial_config.port_name)
        }
    }
}
