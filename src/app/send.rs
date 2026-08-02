use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn process_repeat(&mut self, context: &egui::Context) {
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

    pub(super) fn queue_current_input(&mut self) -> Result<(), String> {
        let bytes = self.current_payload()?;
        let history = HistoryItem {
            mode: self.preferences.send_mode,
            input: self.send_input.clone(),
        };
        self.queue_payload(bytes, history)
    }

    pub(super) fn current_payload(&self) -> Result<Vec<u8>, String> {
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

    pub(super) fn queue_payload(
        &mut self,
        bytes: Vec<u8>,
        history: HistoryItem,
    ) -> Result<(), String> {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);
        self.worker.send(id, bytes)?;
        self.pending_history.insert(id, history);
        self.send_error = None;
        Ok(())
    }

    pub(super) fn queue_terminal_payload(&mut self, bytes: Vec<u8>) -> Result<(), String> {
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

    pub(super) fn push_history(&mut self, item: HistoryItem) {
        if self.history.front() == Some(&item) {
            return;
        }
        self.history.retain(|existing| existing != &item);
        self.history.push_front(item);
        self.history.truncate(MAX_HISTORY);
    }

    pub(super) fn start_repeat(&mut self, context: &egui::Context) {
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

    pub(super) fn show_terminal_surface(&mut self, ui: &mut egui::Ui) {
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

        let (events, modifiers) = ui.input(|input| (input.events.clone(), input.modifiers));
        match terminal_bytes_from_events(&events, self.preferences.text_encoding, modifiers) {
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

    pub(super) fn show_send_panel(&mut self, root_ui: &mut egui::Ui) {
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
            .fill(self.surface_fill(root_ui.visuals().panel_fill, RECEIVE_SEND_PANEL_ALPHA));
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
                    if ui
                        .add_enabled_ui(!repeat_running, |ui| {
                            ui.checkbox(&mut self.preferences.clear_after_send, "发送成功后清空")
                        })
                        .inner
                        .changed()
                    {
                        self.mark_preferences_dirty();
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
                if send_requested || shortcut_requested {
                    match self.queue_current_input() {
                        Ok(()) if self.preferences.clear_after_send => self.send_input.clear(),
                        Ok(()) => {}
                        Err(message) => self.send_error = Some(message),
                    }
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

    pub(super) fn history_menu(&mut self, ui: &mut egui::Ui) -> Option<HistoryItem> {
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
}

pub(super) fn terminal_bytes_from_events(
    events: &[egui::Event],
    encoding: TextEncoding,
    frame_modifiers: egui::Modifiers,
) -> Result<Vec<u8>, String> {
    let has_paste = events
        .iter()
        .any(|event| matches!(event, egui::Event::Paste(_)));
    let has_ctrl_c_key = has_pressed_control_key(events, egui::Key::C);
    let has_ctrl_x_key = has_pressed_control_key(events, egui::Key::X);
    let clipboard_event_is_control =
        frame_modifiers.ctrl && !frame_modifiers.alt && !frame_modifiers.shift;
    let text_contains_tab = events
        .iter()
        .any(|event| matches!(event, egui::Event::Text(text) if text.contains('\t')));
    let mut bytes = Vec::new();

    for event in events {
        match event {
            // egui-winit translates Ctrl+C/Ctrl+X into Copy/Cut before emitting a Key event.
            // Treat those events as terminal controls while the unmodified Ctrl key is held.
            egui::Event::Copy if clipboard_event_is_control && !has_ctrl_c_key => {
                bytes.push(0x03);
            }
            egui::Event::Cut if clipboard_event_is_control && !has_ctrl_x_key => {
                bytes.push(0x18);
            }
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
                    if (*key == egui::Key::C || *key == egui::Key::X) && modifiers.shift {
                        continue;
                    }
                    if *key == egui::Key::V && has_paste {
                        continue;
                    }
                    if let Some(byte) = terminal_control_byte(*key) {
                        bytes.push(byte);
                    }
                    continue;
                }

                match key {
                    // A terminal Return key transmits CR. Text-mode line-ending settings do not
                    // apply here; sending CRLF can execute an empty second command in FinSH.
                    egui::Key::Enter => bytes.push(b'\r'),
                    egui::Key::Tab if !text_contains_tab => bytes.push(b'\t'),
                    egui::Key::Backspace => bytes.push(0x08),
                    egui::Key::Escape => bytes.push(0x1B),
                    egui::Key::Delete => bytes.extend_from_slice(b"\x1B[3~"),
                    egui::Key::ArrowUp => bytes.extend_from_slice(b"\x1B[A"),
                    egui::Key::ArrowDown => bytes.extend_from_slice(b"\x1B[B"),
                    egui::Key::ArrowRight => bytes.extend_from_slice(b"\x1B[C"),
                    egui::Key::ArrowLeft => bytes.extend_from_slice(b"\x1B[D"),
                    egui::Key::Home => bytes.extend_from_slice(b"\x1B[1~"),
                    egui::Key::End => bytes.extend_from_slice(b"\x1B[4~"),
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

fn has_pressed_control_key(events: &[egui::Event], expected: egui::Key) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if *key == expected && modifiers.ctrl && !modifiers.alt && !modifiers.shift
        )
    })
}

pub(super) fn append_terminal_text(
    output: &mut Vec<u8>,
    text: &str,
    encoding: TextEncoding,
) -> Result<(), String> {
    if !text.is_empty() {
        output.extend(encode_text(text, encoding, LineEnding::None)?);
    }
    Ok(())
}

pub(super) fn terminal_control_byte(key: egui::Key) -> Option<u8> {
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

pub(super) fn history_preview(item: &HistoryItem) -> String {
    let normalized = item.input.replace(['\r', '\n'], " ");
    let mut preview: String = normalized.chars().take(36).collect();
    if normalized.chars().count() > 36 {
        preview.push('…');
    }
    format!("{}  {preview}", item.mode.label())
}
