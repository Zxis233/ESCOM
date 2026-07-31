include!("../icon_pixels.rs");

pub fn icon_data() -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: escom_icon_rgba(),
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}
