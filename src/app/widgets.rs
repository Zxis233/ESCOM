use super::*;

pub(super) fn config_combo(
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

pub(super) fn toolbar_label(ui: &mut egui::Ui, text: &'static str, width: f32) {
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

pub(super) fn toolbar_separator(ui: &mut egui::Ui) {
    let height = (toolbar_control_height(ui) - 10.0).max(22.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

pub(super) fn toolbar_control_height(ui: &mut egui::Ui) -> f32 {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let text_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id));
    toolbar_control_height_from_metrics(text_height, ui.spacing().button_padding.y)
}

pub(super) fn toolbar_control_height_from_metrics(text_height: f32, vertical_padding: f32) -> f32 {
    (text_height + vertical_padding * 2.0)
        .ceil()
        .max(MIN_CONTROL_HEIGHT)
}

pub(super) fn toolbar_button_width(ui: &mut egui::Ui, text: &str, minimum: f32) -> f32 {
    (styled_text_width(ui, text, egui::TextStyle::Button) + ui.spacing().button_padding.x * 2.0)
        .ceil()
        .max(minimum)
}

pub(super) fn toolbar_button(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    minimum: f32,
) -> egui::Button<'static> {
    let text = text.into();
    let width = toolbar_button_width(ui, &text, minimum);
    egui::Button::new(text)
        .wrap_mode(egui::TextWrapMode::Extend)
        .min_size(egui::vec2(width, toolbar_control_height(ui)))
}

pub(super) fn selectable_toolbar_button(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<String>,
    minimum: f32,
) -> egui::Button<'static> {
    let text = text.into();
    let width = toolbar_button_width(ui, &text, minimum);
    egui::Button::selectable(selected, text)
        .wrap_mode(egui::TextWrapMode::Extend)
        .min_size(egui::vec2(width, toolbar_control_height(ui)))
}

pub(super) fn toolbar_checkbox_width(ui: &mut egui::Ui, text: &str) -> f32 {
    (ui.spacing().icon_width
        + ui.spacing().icon_spacing
        + styled_text_width(ui, text, egui::TextStyle::Button))
    .ceil()
}

pub(super) fn responsive_toolbar_breakpoint(ui: &egui::Ui, base_width: f32) -> f32 {
    let font_size = egui::TextStyle::Button.resolve(ui.style()).size;
    base_width * (font_size / 15.0).max(1.0)
}

pub(super) fn status_control_height(ui: &mut egui::Ui) -> f32 {
    let text_font = egui::TextStyle::Button.resolve(ui.style());
    let icon_font = FontId::proportional(THEME_ICON_SIZE);
    let (text_height, icon_height) =
        ui.fonts_mut(|fonts| (fonts.row_height(&text_font), fonts.row_height(&icon_font)));
    status_control_height_from_metrics(text_height, icon_height, ui.spacing().button_padding.y)
}

pub(super) fn status_control_height_from_metrics(
    text_height: f32,
    icon_height: f32,
    vertical_padding: f32,
) -> f32 {
    toolbar_control_height_from_metrics(text_height.max(icon_height), vertical_padding)
}

pub(super) fn framed_control_height(control_height: f32, frame: &egui::Frame) -> f32 {
    (control_height + frame.total_margin().sum().y).ceil()
}

pub(super) fn styled_text_width(ui: &mut egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font_id = style.resolve(ui.style());
    ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font_id, Color32::WHITE)
            .size()
            .x
    })
}

pub(super) fn label_width(ui: &mut egui::Ui, text: &str, minimum: f32) -> f32 {
    styled_text_width(ui, text, egui::TextStyle::Body)
        .ceil()
        .max(minimum)
}

pub(super) fn text_field_width(ui: &mut egui::Ui, sample: &str, minimum: f32) -> f32 {
    (styled_text_width(ui, sample, egui::TextStyle::Body) + 20.0)
        .ceil()
        .max(minimum)
}

pub(super) fn combo_width(ui: &mut egui::Ui, selected_text: &str, minimum: f32) -> f32 {
    (styled_text_width(ui, selected_text, egui::TextStyle::Button)
        + ui.spacing().button_padding.x * 2.0
        + ui.spacing().icon_width
        + ui.spacing().icon_spacing)
        .ceil()
        .max(minimum)
}
