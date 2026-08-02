use eframe::egui::{self, Color32, FontId};

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

pub fn show_title_bar(root_ui: &mut egui::Ui, title_icon: &egui::TextureHandle, fill: Color32) {
    let context = root_ui.ctx().clone();
    let (maximized, focused) = context.input(|input| {
        (
            input.viewport().maximized.unwrap_or(false),
            input.viewport().focused.unwrap_or(true),
        )
    });

    egui::Panel::top("custom_title_bar")
        .resizable(false)
        .exact_size(TITLE_BAR_HEIGHT)
        .frame(egui::Frame::new().fill(fill).inner_margin(0.0))
        .show(root_ui, |ui| {
            let rect = ui.max_rect();
            let drag_response = ui.interact(
                title_bar_drag_rect(rect, maximized),
                ui.id().with("window_drag_area"),
                egui::Sense::click_and_drag(),
            );
            if drag_response.double_clicked_by(egui::PointerButton::Primary) {
                context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if drag_response.drag_started_by(egui::PointerButton::Primary) {
                context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            show_title_bar_menu(&drag_response, &context, maximized);

            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 18.0, rect.center().y),
                egui::vec2(18.0, 18.0),
            );
            ui.painter().image(
                title_icon.id(),
                icon_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::from_white_alpha(if focused { 255 } else { 160 }),
            );
            let title_color = if focused {
                ui.visuals().text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            ui.painter().text(
                egui::pos2(icon_rect.right() + 8.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                "ESCOM",
                FontId::new(14.0, egui::FontFamily::Proportional),
                title_color,
            );

            let close_rect = title_bar_control_rect(rect, 0);
            let maximize_rect = title_bar_control_rect(rect, 1);
            let minimize_rect = title_bar_control_rect(rect, 2);

            if title_bar_control(
                ui,
                minimize_rect,
                TitleBarControl::Minimize,
                maximized,
                focused,
            )
            .clicked()
            {
                context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            let maximize_control = if maximized {
                TitleBarControl::Restore
            } else {
                TitleBarControl::Maximize
            };
            if title_bar_control(ui, maximize_rect, maximize_control, maximized, focused).clicked()
            {
                context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if title_bar_control(ui, close_rect, TitleBarControl::Close, maximized, focused)
                .clicked()
            {
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }

            ui.painter().hline(
                rect.x_range(),
                rect.bottom() - 0.5,
                ui.visuals().widgets.noninteractive.bg_stroke,
            );
        });
}

pub fn handle_window_resize(context: &egui::Context) {
    let (maximized, fullscreen, pointer_position, primary_pressed, viewport_rect) =
        context.input(|input| {
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

fn show_title_bar_menu(response: &egui::Response, context: &egui::Context, maximized: bool) {
    response.context_menu(|ui| {
        ui.set_min_width(132.0);
        if ui
            .add_enabled(maximized, egui::Button::new("还原"))
            .clicked()
        {
            context.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ui.close();
        }
        if ui.button("最小化").clicked() {
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            ui.close();
        }
        if ui
            .add_enabled(!maximized, egui::Button::new("最大化"))
            .clicked()
        {
            context.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ui.close();
        }
        ui.separator();
        if ui.button("关闭").clicked() {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            ui.close();
        }
    });
}

fn title_bar_drag_rect(title_rect: egui::Rect, maximized: bool) -> egui::Rect {
    let border = if maximized { 0.0 } else { WINDOW_RESIZE_BORDER };
    egui::Rect::from_min_max(
        title_rect.min + egui::vec2(border, border),
        egui::pos2(
            title_rect.right() - TITLE_BAR_CONTROLS_WIDTH,
            title_rect.bottom(),
        ),
    )
}

fn title_bar_control_rect(title_rect: egui::Rect, index_from_right: usize) -> egui::Rect {
    let right = title_rect.right() - index_from_right as f32 * TITLE_BAR_BUTTON_WIDTH;
    egui::Rect::from_min_max(
        egui::pos2(right - TITLE_BAR_BUTTON_WIDTH, title_rect.top()),
        egui::pos2(right, title_rect.bottom()),
    )
}

fn title_bar_control_hit_rect(
    visual_rect: egui::Rect,
    control: TitleBarControl,
    maximized: bool,
) -> egui::Rect {
    if maximized {
        return visual_rect;
    }
    let mut hit_rect = visual_rect;
    hit_rect.min.y += WINDOW_RESIZE_BORDER;
    if matches!(control, TitleBarControl::Close) {
        hit_rect.max.x -= WINDOW_RESIZE_BORDER;
    }
    hit_rect
}

fn title_bar_control(
    ui: &mut egui::Ui,
    visual_rect: egui::Rect,
    control: TitleBarControl,
    maximized: bool,
    window_focused: bool,
) -> egui::Response {
    let label = match control {
        TitleBarControl::Minimize => "最小化",
        TitleBarControl::Maximize => "最大化",
        TitleBarControl::Restore => "还原",
        TitleBarControl::Close => "关闭",
    };
    let response = ui.interact(
        title_bar_control_hit_rect(visual_rect, control, maximized),
        ui.id().with(label),
        egui::Sense::click(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));

    let is_close = matches!(control, TitleBarControl::Close);
    let icon_color = if is_close && response.hovered() {
        Color32::WHITE
    } else if window_focused {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
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
        ui.painter().rect_filled(visual_rect, 0.0, fill);
    }

    let center = visual_rect.center();
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
    let left = pointer_position.x <= viewport_rect.left() + WINDOW_RESIZE_BORDER;
    let right = pointer_position.x >= viewport_rect.right() - WINDOW_RESIZE_BORDER;
    let top = pointer_position.y <= viewport_rect.top() + WINDOW_RESIZE_BORDER;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_hit_zones_map_to_every_edge_and_corner() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0));

        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1.0, 1.0)),
            Some(egui::ResizeDirection::NorthWest)
        );
        assert_eq!(
            window_resize_direction(viewport, egui::pos2(1279.0, 1.0)),
            Some(egui::ResizeDirection::NorthEast)
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
    fn normal_title_bar_drag_area_does_not_overlap_resize_border_or_controls() {
        let title = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 36.0));
        let drag = title_bar_drag_rect(title, false);

        assert_eq!(
            drag.min,
            egui::pos2(WINDOW_RESIZE_BORDER, WINDOW_RESIZE_BORDER)
        );
        assert_eq!(
            drag.max,
            egui::pos2(1280.0 - TITLE_BAR_CONTROLS_WIDTH, 36.0)
        );
        assert!(!drag.contains(egui::pos2(200.0, 2.0)));
        assert!(drag.contains(egui::pos2(200.0, 12.0)));
        assert!(!drag.contains(egui::pos2(1270.0, 20.0)));
    }

    #[test]
    fn normal_close_button_does_not_overlap_top_or_right_resize_borders() {
        let title = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 36.0));
        let visual = title_bar_control_rect(title, 0);
        let hit = title_bar_control_hit_rect(visual, TitleBarControl::Close, false);

        assert_eq!(hit.min.y, WINDOW_RESIZE_BORDER);
        assert_eq!(hit.max.x, 1280.0 - WINDOW_RESIZE_BORDER);
        assert!(!hit.contains(egui::pos2(1260.0, 2.0)));
        assert!(!hit.contains(egui::pos2(1279.0, 20.0)));
        assert!(hit.contains(egui::pos2(1260.0, 20.0)));
    }
}
