use super::receive::receive_row_layout_job;
use super::send::{history_preview, terminal_bytes_from_events};
use super::settings_ui::{
    centered_window_position, cover_uv, format_background_opacity, local_background_uri,
    normalized_online_image_url, parse_background_opacity, preferred_settings_window_width,
};
use super::status::human_bytes;
use super::widgets::{
    framed_control_height, status_control_height_from_metrics, toolbar_control_height_from_metrics,
};
use super::*;
use crate::formatting::format_snapshot;

fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

#[test]
fn history_preview_is_single_line_and_bounded() {
    let preview = history_preview(&HistoryItem {
        mode: SendMode::Text,
        input: "line one\r\nline two with a very long suffix that is clipped".into(),
    });
    assert!(!preview.contains('\n'));
    assert!(preview.ends_with('…'));
}

#[test]
fn byte_counts_are_readable() {
    assert_eq!(human_bytes(7), "7 B");
    assert_eq!(human_bytes(2048), "2.0 KiB");
    assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MiB");
}

#[test]
fn toolbar_control_height_expands_with_large_text_metrics() {
    assert_eq!(toolbar_control_height_from_metrics(13.0, 3.0), 32.0);
    assert_eq!(toolbar_control_height_from_metrics(50.0, 4.0), 58.0);
}

#[test]
fn status_panel_height_covers_icon_and_frame_margins() {
    let control_height = status_control_height_from_metrics(13.0, 21.0, 6.0);
    let frame = egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 2));

    assert_eq!(control_height, 33.0);
    assert_eq!(framed_control_height(control_height, &frame), 37.0);
}

#[test]
fn settings_window_width_is_font_aware_and_viewport_bounded() {
    assert_eq!(preferred_settings_window_width(1248.0, 15.0), 660.0);
    assert_eq!(preferred_settings_window_width(992.0, 18.0), 690.0);
    assert_eq!(preferred_settings_window_width(608.0, 15.0), 608.0);
}

#[test]
fn settings_window_default_position_is_centered() {
    let viewport = egui::Rect::from_min_size(egui::pos2(16.0, 16.0), egui::vec2(992.0, 608.0));
    assert_eq!(
        centered_window_position(viewport, egui::vec2(660.0, 454.0)),
        egui::pos2(182.0, 93.0)
    );
}

#[test]
fn background_opacity_input_accepts_only_bounded_decimals() {
    assert_eq!(parse_background_opacity("0.22"), Ok(0.22));
    assert_eq!(parse_background_opacity(".5"), Ok(0.5));
    assert_eq!(parse_background_opacity("1.0"), Ok(1.0));
    assert!(parse_background_opacity("").is_err());
    assert!(parse_background_opacity(".").is_err());
    assert!(parse_background_opacity("1.01").is_err());
    assert!(parse_background_opacity("20%").is_err());
}

#[test]
fn background_opacity_is_formatted_as_a_compact_decimal() {
    assert_eq!(format_background_opacity(0.0), "0.0");
    assert_eq!(format_background_opacity(0.22), "0.22");
    assert_eq!(format_background_opacity(0.5), "0.5");
    assert_eq!(format_background_opacity(1.0), "1.0");
}

#[test]
fn cover_uv_center_crops_wide_and_tall_images() {
    let wide = cover_uv(egui::vec2(200.0, 100.0), egui::vec2(100.0, 100.0));
    assert_eq!(wide.min, egui::pos2(0.25, 0.0));
    assert_eq!(wide.max, egui::pos2(0.75, 1.0));

    let tall = cover_uv(egui::vec2(100.0, 200.0), egui::vec2(100.0, 100.0));
    assert_eq!(tall.min, egui::pos2(0.0, 0.25));
    assert_eq!(tall.max, egui::pos2(1.0, 0.75));
}

#[test]
fn receive_row_layout_combines_rule_and_search_highlights() {
    egui::__run_test_ui(|ui| {
        let rule_background = Color32::from_rgba_unmultiplied(220, 40, 40, 80);
        let matches = [SearchMatch {
            row_index: 0,
            byte_range: 0..5,
        }];
        let selected_background = ui.visuals().selection.bg_fill;
        let job = receive_row_layout_job(
            ui,
            "ERROR ready",
            &FontId::monospace(15.0),
            Some(HighlightStyle {
                foreground: None,
                background: Some(rule_background),
                underline: true,
            }),
            0,
            &matches,
            Some(0),
        );

        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].byte_range.start.0, 0);
        assert_eq!(job.sections[0].byte_range.end.0, 5);
        assert_eq!(job.sections[0].format.background, selected_background);
        assert_eq!(job.sections[1].format.background, rule_background);
        assert_ne!(job.sections[1].format.underline, egui::Stroke::NONE);
    });
}

#[test]
fn online_background_url_requires_http_and_a_host() {
    assert_eq!(
        normalized_online_image_url(" https://example.com/image.png ").unwrap(),
        "https://example.com/image.png"
    );
    assert!(normalized_online_image_url("file:///tmp/image.png").is_err());
    assert!(normalized_online_image_url("https://").is_err());
    assert!(normalized_online_image_url("https://example.com/a b.png").is_err());
}

#[test]
fn local_background_paths_use_windows_file_uri_shape() {
    assert_eq!(
        local_background_uri(r"D:\Pictures\background image.png"),
        "file:///D:/Pictures/background image.png"
    );
    assert_eq!(
        local_background_uri(r"\\server\share\background.png"),
        "file://server/share/background.png"
    );
}

#[test]
fn terminal_printable_key_is_sent_once_without_local_echo() {
    let events = [
        key_event(egui::Key::H, egui::Modifiers::NONE),
        egui::Event::Text("H".into()),
    ];
    let bytes =
        terminal_bytes_from_events(&events, TextEncoding::Utf8, egui::Modifiers::NONE).unwrap();
    assert_eq!(bytes, b"H");

    let store = ReceiveStore::new(1024);
    let rows = format_snapshot(&store.snapshot(), ReceiveMode::Terminal, TextEncoding::Utf8);
    assert!(rows.is_empty());
}

#[test]
fn terminal_special_and_control_keys_map_to_serial_bytes() {
    let events = [
        key_event(egui::Key::Enter, egui::Modifiers::NONE),
        key_event(egui::Key::Backspace, egui::Modifiers::NONE),
        key_event(egui::Key::ArrowUp, egui::Modifiers::NONE),
        key_event(egui::Key::C, egui::Modifiers::CTRL),
    ];
    let bytes =
        terminal_bytes_from_events(&events, TextEncoding::Utf8, egui::Modifiers::NONE).unwrap();
    assert_eq!(bytes, b"\r\x08\x1B[A\x03");
}

#[test]
fn terminal_copy_and_cut_events_send_real_control_bytes() {
    let bytes = terminal_bytes_from_events(
        &[egui::Event::Copy, egui::Event::Cut],
        TextEncoding::Utf8,
        egui::Modifiers::CTRL,
    )
    .unwrap();
    assert_eq!(bytes, [0x03, 0x18]);

    let duplicate_events = [
        egui::Event::Copy,
        key_event(egui::Key::C, egui::Modifiers::CTRL),
        egui::Event::Cut,
        key_event(egui::Key::X, egui::Modifiers::CTRL),
    ];
    let bytes =
        terminal_bytes_from_events(&duplicate_events, TextEncoding::Utf8, egui::Modifiers::CTRL)
            .unwrap();
    assert_eq!(bytes, [0x03, 0x18]);
}

#[test]
fn terminal_shifted_copy_and_cut_remain_clipboard_shortcuts() {
    let modifiers = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
    let events = [egui::Event::Copy, egui::Event::Cut];

    let bytes = terminal_bytes_from_events(&events, TextEncoding::Utf8, modifiers).unwrap();
    assert!(bytes.is_empty());

    let bytes =
        terminal_bytes_from_events(&events, TextEncoding::Utf8, egui::Modifiers::NONE).unwrap();
    assert!(bytes.is_empty());
}

#[test]
fn terminal_navigation_keys_use_rt_thread_compatible_sequences() {
    let events = [
        key_event(egui::Key::Home, egui::Modifiers::NONE),
        key_event(egui::Key::End, egui::Modifiers::NONE),
        key_event(egui::Key::Delete, egui::Modifiers::NONE),
    ];

    let bytes =
        terminal_bytes_from_events(&events, TextEncoding::Utf8, egui::Modifiers::NONE).unwrap();
    assert_eq!(bytes, b"\x1B[1~\x1B[4~\x1B[3~");
}

#[test]
fn terminal_text_respects_selected_encoding() {
    let bytes = terminal_bytes_from_events(
        &[egui::Event::Text("中".into())],
        TextEncoding::Gbk,
        egui::Modifiers::NONE,
    )
    .unwrap();
    assert_eq!(bytes, [0xD6, 0xD0]);

    assert!(
        terminal_bytes_from_events(
            &[egui::Event::Text("🙂".into())],
            TextEncoding::Gbk,
            egui::Modifiers::NONE,
        )
        .is_err()
    );
}
