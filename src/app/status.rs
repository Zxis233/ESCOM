use super::settings_ui::show_theme_menu;
use super::widgets::*;
use super::*;

impl EscomApp {
    pub(super) fn show_status_panel(&mut self, root_ui: &mut egui::Ui) {
        let context = root_ui.ctx().clone();
        let mut theme_preference = self.preferences.theme_preference;
        let mut theme_changed = false;
        let control_height = status_control_height(root_ui);
        let panel_frame = egui::Frame::side_top_panel(root_ui.style())
            .fill(self.surface_fill(root_ui.visuals().panel_fill, 210));
        let panel_height = framed_control_height(control_height, &panel_frame);
        egui::Panel::bottom("status_panel")
            .resizable(false)
            .exact_size(panel_height)
            .frame(panel_frame)
            .show(root_ui, |ui| {
                ui.spacing_mut().interact_size.y = control_height;
                egui::containers::Sides::new()
                    .height(control_height)
                    .shrink_left()
                    .truncate()
                    .show(
                        ui,
                        |ui| {
                            let (status, color) = match &self.connection {
                                ConnectionState::Disconnected => {
                                    ("未连接".to_owned(), Color32::GRAY)
                                }
                                ConnectionState::Connecting => {
                                    ("正在连接".to_owned(), Color32::YELLOW)
                                }
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

    pub(super) fn store_status(&self) -> (usize, u64) {
        self.store
            .lock()
            .map(|store| (store.bytes_len(), store.dropped_bytes()))
            .unwrap_or((0, 0))
    }
}

pub(super) fn human_bytes(bytes: u64) -> String {
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
