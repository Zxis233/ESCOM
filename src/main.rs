#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use escom::app::EscomApp;
use escom::icon;
use escom::settings;

fn main() -> eframe::Result {
    if let Err(error) = settings::prepare_storage() {
        rfd::MessageDialog::new()
            .set_title("ESCOM")
            .set_description(format!("无法初始化配置目录：{error}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
        return Ok(());
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([1024.0, 640.0])
            .with_icon(icon::icon_data()),
        centered: true,
        persist_window: true,
        persistence_path: Some(settings::eframe_persistence_path()),
        ..Default::default()
    };

    eframe::run_native(
        "ESCOM",
        native_options,
        Box::new(|creation_context| Ok(Box::new(EscomApp::new(creation_context)))),
    )
}
