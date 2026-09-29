use crate::app::{App, ClickAction, InputMode, Picker};
use crate::i18n::{Key, Message};
use crate::msg;
use crossterm::event::KeyCode;
use escom_core::model::SendMode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::borrow::Cow;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Serial bytes must never be interpreted as host terminal escape sequences.
pub fn safe_text(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_control) {
        Cow::Owned(
            text.chars()
                .map(|ch| if ch.is_control() { '·' } else { ch })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.click_targets.clear();
    app.receive_area = Rect::default();
    app.picker_area = Rect::default();
    let language = app.config.language;
    let palette = app.config.theme.palette();
    let tr = |message: Message| message.render(language).into_owned();
    let send_mode = language.text(match app.send_mode {
        SendMode::Text => Key::Text,
        SendMode::Hex => Key::Hex,
    });
    let area = frame.area();
    frame.render_widget(Block::default().style(palette.base()), area);
    if area.width < 48 || area.height < 12 {
        frame.render_widget(Paragraph::new(language.text(Key::SmallScreen)), area);
        return;
    }
    let buttons = toolbar_buttons(app);
    let quit_label = language.text(Key::ButtonQuit);
    let quit_width = quit_label.width() as u16 + 2;
    let button_layout = layout_buttons(&buttons, area.width, quit_width + 1);
    let toolbar_height = button_layout
        .last()
        .map_or(1, |rect| rect.y + 1)
        .min(area.height.saturating_sub(11).max(1));
    // One fifth of the window, with a minimum of three text rows plus borders.
    let editor_height = (area.height / 6).max(5);
    let [header, controls, body, memory, recording, notice, editor] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(toolbar_height),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(editor_height),
    ])
    .areas(area);
    let [header_status, header_clock] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(19)]).areas(header);
    let header_style = Style::default()
        .fg(palette.header_foreground)
        .bg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let connection = if app.connected {
        language.text(Key::Connected)
    } else if app.connecting {
        language.text(Key::Connecting)
    } else {
        language.text(Key::Offline)
    };
    frame.render_widget(
        Paragraph::new(tr(msg!(
            Header,
            app.config.port,
            app.config.baud,
            connection,
            language.text(match app.config.mode.as_str() {
                "text" => Key::Text,
                "hex" => Key::Hex,
                _ => Key::Terminal,
            }),
            app.config.encoding.to_uppercase()
        )))
        .style(header_style),
        header_status,
    );
    frame.render_widget(
        Paragraph::new(format!(
            " {} ",
            chrono::Local::now().format("%y/%m/%d %H:%M:%S")
        ))
        .style(header_style),
        header_clock,
    );
    for ((label, action), rect) in buttons.into_iter().zip(button_layout) {
        if rect.y >= controls.height {
            break;
        }
        let rect = Rect::new(controls.x + rect.x, controls.y + rect.y, rect.width, 1);
        frame.render_widget(
            Paragraph::new(format!("[{label}]")).style(palette.button()),
            rect,
        );
        app.click_targets.push((rect, action));
    }
    let quit_rect = Rect::new(controls.right() - quit_width, controls.y, quit_width, 1);
    frame.render_widget(
        Paragraph::new(format!("[{quit_label}]")).style(palette.button()),
        quit_rect,
    );
    app.click_targets
        .push((quit_rect, ClickAction::Shortcut(KeyCode::Char('q'))));
    app.receive_area = body;

    app.page_rows = usize::from(body.height.saturating_sub(2)).max(1);
    if !app.paused {
        app.offset = app.display.rows.len().saturating_sub(app.page_rows);
    }
    app.offset = app.offset.min(app.display.rows.len().saturating_sub(1));
    let selected = app.matches.get(app.selected_match).copied();
    let rows: Vec<Line<'_>> = app
        .display
        .rows
        .iter()
        .enumerate()
        .skip(app.offset)
        .take(app.page_rows)
        .map(|(index, row)| {
            let style = if Some(index) == selected {
                Style::default()
                    .fg(palette.search_selected_foreground)
                    .bg(palette.search_selected_background)
            } else if app.matches.binary_search(&index).is_ok() {
                Style::default().fg(palette.search_foreground)
            } else {
                Style::default()
            };
            let mut spans = Vec::with_capacity(2);
            if app.config.timestamps {
                spans.push(Span::styled(
                    row.received_at.format("%H:%M:%S%.3f ").to_string(),
                    Style::default().fg(palette.muted),
                ));
            }
            spans.push(Span::styled(safe_text(&row.text), style));
            Line::from(spans)
        })
        .collect();
    let title = tr(msg!(
        RxTitle,
        language.text(if app.paused { Key::Paused } else { Key::Live }),
        if app.display.rows.is_empty() {
            0
        } else {
            app.offset + 1
        },
        (app.offset + app.page_rows).min(app.display.rows.len()),
        app.display.rows.len(),
        if app.display.limited {
            language.text(Key::Clipped)
        } else {
            ""
        }
    ));
    frame.render_widget(
        Paragraph::new(rows).scroll((0, app.horizontal)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(palette.border)),
        ),
        body,
    );

    let (raw, records, dropped) = app
        .store
        .lock()
        .map(|s| (s.bytes_len(), s.records_len(), s.dropped_bytes()))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(tr(msg!(
            Memory,
            app.worker.stats.rx_bytes(),
            app.worker.stats.tx_bytes(),
            raw / 1024,
            app.config.history_kib,
            records,
            dropped,
            app.worker.queued_write_bytes().div_ceil(1024),
            app.config.tx_kib
        )))
        .style(Style::default().fg(palette.muted)),
        memory,
    );
    let (record_text, failed) = if let Some(capture) = &app.capture {
        let progress = capture.progress();
        match progress.error {
            Some(error) => (
                tr(msg!(RecordingError, Message::Core(error).render(language))),
                true,
            ),
            None => (
                tr(msg!(
                    RecordingProgress,
                    progress.written_bytes,
                    progress.queued_bytes,
                    app.capture_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )),
                false,
            ),
        }
    } else {
        (
            format!(
                " {}{}",
                app.capture_result.render(language),
                app.capture_path
                    .as_ref()
                    .map(|p| format!(" | {}", p.display()))
                    .unwrap_or_default()
            ),
            app.capture_failed,
        )
    };
    frame.render_widget(
        Paragraph::new(safe_text(&record_text)).style(Style::default().fg(if failed {
            palette.error
        } else {
            palette.success
        })),
        recording,
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", safe_text(&app.notice.render(language))))
            .style(Style::default().fg(palette.warning)),
        notice,
    );
    let last_tx = app.last_tx.render(language);
    let (label, text) = match app.input_mode {
        InputMode::View => (
            tr(msg!(TxTitle, send_mode, app.config.line_ending)),
            safe_text(&last_tx),
        ),
        InputMode::Send => (tr(msg!(SendTitle, send_mode)), safe_text(&app.input)),
        InputMode::Search => (
            tr(msg!(
                SearchTitle,
                language.text(if app.search_regex {
                    Key::Regex
                } else {
                    Key::Literal
                })
            )),
            safe_text(&app.input),
        ),
        InputMode::Command => (
            language.text(Key::CommandTitle).into(),
            safe_text(&app.input),
        ),
        InputMode::Direct => (
            language.text(Key::DirectTitle).into(),
            Cow::Borrowed(language.text(Key::DirectHint)),
        ),
    };
    let width = editor.width.saturating_sub(2) as usize;
    // Show only the tail of a long editor, avoiding a large per-frame layout.
    let mut tail_start = text.len();
    let mut tail_width = 0;
    for (index, ch) in text.char_indices().rev() {
        let cells = ch.width().unwrap_or(0);
        if tail_width + cells > width.saturating_sub(1) {
            break;
        }
        tail_width += cells;
        tail_start = index;
    }
    let tail = &text[tail_start..];
    frame.render_widget(
        Paragraph::new(tail)
            .style(
                Style::default()
                    .fg(palette.foreground)
                    .bg(palette.editor_background),
            )
            .block(
                Block::bordered()
                    .title(label)
                    .border_style(Style::default().fg(if app.input_mode == InputMode::View {
                        palette.inactive_border
                    } else {
                        palette.accent
                    })),
            ),
        editor,
    );
    app.click_targets.push((editor, ClickAction::Editor));
    if matches!(
        app.input_mode,
        InputMode::Send | InputMode::Search | InputMode::Command
    ) && !app.help
        && app.picker.is_none()
    {
        frame.set_cursor_position((editor.x + 1 + tail_width as u16, editor.y + 1));
    }
    if app.help {
        app.click_targets.clear();
        let rect = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(language.text(Key::KeyboardHelp))
                .style(palette.popup())
                .block(
                    Block::bordered()
                        .title(language.text(Key::HelpTitle))
                        .border_style(Style::default().fg(palette.border)),
                ),
            rect,
        );
    } else if let Some(picker) = app.picker {
        draw_picker(frame, app, picker);
    }
}

fn toolbar_buttons(app: &App) -> Vec<(&'static str, ClickAction)> {
    let tr = |key| app.config.language.text(key);
    let mut buttons = vec![
        (tr(Key::ButtonPort), ClickAction::OpenPicker(Picker::Port)),
        (
            tr(if app.connected || app.connecting {
                Key::ButtonDisconnect
            } else {
                Key::ButtonConnect
            }),
            ClickAction::Shortcut(KeyCode::F(3)),
        ),
        (tr(Key::ButtonMode), ClickAction::OpenPicker(Picker::Mode)),
        (
            tr(Key::ButtonEncoding),
            ClickAction::Shortcut(KeyCode::F(5)),
        ),
        (
            tr(if app.capture.is_some() {
                Key::ButtonStopRecord
            } else {
                Key::ButtonRecord
            }),
            ClickAction::Shortcut(KeyCode::F(6)),
        ),
        (
            tr(Key::ButtonSendMode),
            ClickAction::Shortcut(KeyCode::F(7)),
        ),
        (tr(Key::ButtonDirect), ClickAction::Shortcut(KeyCode::F(8))),
        (tr(Key::ButtonTheme), ClickAction::Shortcut(KeyCode::F(10))),
        (
            tr(if app.input_mode == InputMode::Send {
                Key::ButtonSubmitSend
            } else {
                Key::ButtonSend
            }),
            ClickAction::Shortcut(KeyCode::Char('s')),
        ),
        (
            tr(Key::ButtonClear),
            ClickAction::Shortcut(KeyCode::Char('c')),
        ),
        (
            tr(if app.paused {
                Key::ButtonResume
            } else {
                Key::ButtonPause
            }),
            ClickAction::Shortcut(KeyCode::Char(' ')),
        ),
        (
            tr(Key::ButtonSearch),
            ClickAction::Shortcut(KeyCode::Char('/')),
        ),
        (
            tr(Key::ButtonCommand),
            ClickAction::Shortcut(KeyCode::Char(':')),
        ),
        (
            tr(Key::ButtonHelp),
            ClickAction::Shortcut(KeyCode::Char('?')),
        ),
    ];
    if app.input_mode != InputMode::View {
        buttons.push((tr(Key::ButtonCancel), ClickAction::Cancel));
    }
    buttons
}

/// The same cell rectangles drive rendering and hit testing, including wide CJK labels.
fn layout_buttons(
    buttons: &[(&str, ClickAction)],
    width: u16,
    first_row_reserved: u16,
) -> Vec<Rect> {
    let mut x = 0;
    let mut y = 0;
    buttons
        .iter()
        .map(|(label, _)| {
            let button_width = (label.width() as u16 + 2).min(width);
            let row_width = if y == 0 {
                width.saturating_sub(first_row_reserved)
            } else {
                width
            };
            if x + button_width > row_width {
                x = 0;
                y += 1;
            }
            let rect = Rect::new(x, y, button_width, 1);
            x += button_width + 1;
            rect
        })
        .collect()
}

fn draw_picker(frame: &mut Frame, app: &mut App, picker: Picker) {
    let language = app.config.language;
    let palette = app.config.theme.palette();
    let count = match picker {
        Picker::Port => app.ports.len(),
        Picker::Mode => 3,
    };
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(64);
    let height = count.max(1).min(usize::from(area.height.saturating_sub(6))) as u16 + 2;
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    app.picker_area = rect;
    app.picker_rows = usize::from(height - 2);
    app.picker_index = app.picker_index.min(count.saturating_sub(1));
    app.picker_offset = app.picker_offset.min(app.picker_index);
    if app.picker_index >= app.picker_offset + app.picker_rows {
        app.picker_offset = app.picker_index + 1 - app.picker_rows;
    }
    app.picker_offset = app.picker_offset.min(count.saturating_sub(app.picker_rows));
    // A modal replaces all underlying targets, so even its blank cells cannot click through.
    app.click_targets.clear();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::bordered()
            .style(palette.popup())
            .title(language.text(match picker {
                Picker::Port => Key::SelectPort,
                Picker::Mode => Key::SelectMode,
            }))
            .title_bottom(language.text(Key::PickerHint))
            .border_style(Style::default().fg(palette.border)),
        rect,
    );
    let close = Rect::new(rect.right() - 4, rect.y, 3, 1);
    frame.render_widget(
        Paragraph::new("[x]").style(Style::default().fg(palette.accent)),
        close,
    );
    app.click_targets.push((close, ClickAction::ClosePicker));
    if count == 0 {
        frame.render_widget(
            Paragraph::new(language.text(Key::NoPorts)),
            Rect::new(rect.x + 1, rect.y + 1, width - 2, 1),
        );
        return;
    }
    for (row, index) in (app.picker_offset..count).take(app.picker_rows).enumerate() {
        let (label, current, action) = match picker {
            Picker::Port => {
                let port = &app.ports[index];
                (
                    safe_text(port).into_owned(),
                    *port == app.config.port,
                    ClickAction::Port(port.clone()),
                )
            }
            Picker::Mode => {
                let (mode, key) = [
                    ("text", Key::Text),
                    ("hex", Key::Hex),
                    ("terminal", Key::Terminal),
                ][index];
                (
                    language.text(key).to_owned(),
                    mode == app.config.mode,
                    ClickAction::Mode(mode),
                )
            }
        };
        let row_rect = Rect::new(rect.x + 1, rect.y + 1 + row as u16, width - 2, 1);
        let style = if index == app.picker_index {
            Style::default()
                .fg(palette.selection_foreground)
                .bg(palette.selection_background)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(format!("{} {label}", if current { '*' } else { ' ' })).style(style),
            row_rect,
        );
        app.click_targets.push((row_rect, action));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::i18n::Language;
    use chrono::Local;
    use crossterm::event::{
        Event, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn themes_style_entire_frame_editors_popups_and_search_and_reset_cleanly() {
        use crate::theme::Preset;
        use ratatui::style::Color;
        let mut app = App::new(Config {
            timestamps: false,
            ..Config::default()
        })
        .unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"first match\nsecond match\n".to_vec());
        app.tick();
        app.paused = true;
        app.matches = vec![0, 1];
        app.selected_match = 0;
        let mut terminal = Terminal::new(TestBackend::new(120, 38)).unwrap();
        for preset in [
            Preset::Classic,
            Preset::Pink,
            Preset::Midnight,
            Preset::Custom,
            Preset::Classic,
        ] {
            app.config.theme.preset = preset;
            app.input_mode = InputMode::Send;
            app.input = "draft".into();
            app.help = false;
            app.picker = None;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let palette = app.config.theme.palette();
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(0, 0)].bg, palette.accent);
            assert_eq!(buffer[(0, 0)].fg, palette.header_foreground);
            let editor = target(&app, &ClickAction::Editor);
            assert_eq!(
                buffer[(editor.x + 1, editor.y + 1)].bg,
                palette.editor_background
            );
            assert_eq!(buffer[(editor.x + 1, editor.y + 1)].fg, palette.foreground);
            let body = app.receive_area;
            assert_eq!(buffer[(body.x, body.y)].fg, palette.border);
            assert_eq!(
                buffer[(body.x + 1, body.y + 1)].bg,
                palette.search_selected_background
            );
            assert_eq!(
                buffer[(body.x + 1, body.y + 2)].fg,
                palette.search_foreground
            );
            assert_eq!(
                buffer[(body.x + 1, body.bottom() - 2)].bg,
                palette.background
            );
            let button = target(&app, &ClickAction::Shortcut(KeyCode::F(10)));
            assert_eq!(buffer[(button.x, button.y)].fg, palette.accent);
            assert_eq!(buffer[(button.x, button.y)].bg, palette.button_background);
            app.picker = Some(Picker::Mode);
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let rect = app.picker_area;
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(rect.x + 1, rect.y + 1)].bg,
                palette.selection_background
            );
            assert_eq!(
                buffer[(rect.x + 1, rect.y + 2)].bg,
                palette.popup_background
            );
            app.picker = None;
            app.help = true;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            assert_eq!(
                terminal.backend().buffer()[(2, 2)].bg,
                palette.popup_background
            );
        }
        app.help = false;
        app.config.theme.preset = Preset::Custom;
        app.config.theme.colors.accent = Some("#ff80ac".to_owned().try_into().unwrap());
        app.config.theme.colors.background = Some("#010203".to_owned().try_into().unwrap());
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            Color::Rgb(255, 128, 172)
        );
        click(&mut app, ClickAction::Shortcut(KeyCode::F(10)));
        assert_eq!(app.config.theme.preset, Preset::Classic);
        assert_eq!(app.config.theme.palette().background, Color::Reset);
        assert_eq!(app.config.theme.palette().accent, Color::Cyan);
        app.shutdown().unwrap();
    }

    fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
        assert!(!app.handle_event(Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })));
    }

    fn target(app: &App, action: &ClickAction) -> Rect {
        app.click_targets
            .iter()
            .find(|(_, candidate)| candidate == action)
            .unwrap_or_else(|| panic!("missing visible button: {action:?}"))
            .0
    }

    fn click(app: &mut App, action: ClickAction) {
        let rect = target(app, &action);
        mouse(
            app,
            MouseEventKind::Down(MouseButton::Left),
            rect.x + rect.width / 2,
            rect.y,
        );
    }

    fn wait_for(app: &mut App, condition: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            app.tick();
            if condition(app) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "demo worker did not reach expected state"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn mouse_selects_ports_modes_and_clears_in_both_languages_and_compact_layouts() {
        for language in [Language::En, Language::ZhCn] {
            for (width, height) in [(100, 30), (48, 17)] {
                let mut app = App::new(Config {
                    language,
                    ..Config::default()
                })
                .unwrap();
                app.ports = vec!["COM3".into(), "设备串口".into()];
                app.store
                    .lock()
                    .unwrap()
                    .append(Local::now(), b"keep until clear\n".to_vec());
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                for action in [
                    ClickAction::OpenPicker(Picker::Port),
                    ClickAction::Shortcut(KeyCode::F(3)),
                    ClickAction::OpenPicker(Picker::Mode),
                    ClickAction::Shortcut(KeyCode::Char('c')),
                ] {
                    let rect = target(&app, &action);
                    assert!(rect.right() <= width);
                    assert_eq!(terminal.backend().buffer()[(rect.x, rect.y)].symbol(), "[");
                    assert_eq!(
                        terminal.backend().buffer()[(rect.right() - 1, rect.y)].symbol(),
                        "]"
                    );
                }
                click(&mut app, ClickAction::OpenPicker(Picker::Port));
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                click(&mut app, ClickAction::Port("设备串口".into()));
                assert_eq!(app.config.port, "设备串口");
                assert_eq!(app.picker, None);
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                click(&mut app, ClickAction::OpenPicker(Picker::Mode));
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                click(&mut app, ClickAction::Mode("hex"));
                assert_eq!(app.config.mode, "hex");
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                click(&mut app, ClickAction::Shortcut(KeyCode::Char('c')));
                assert_eq!(app.store.lock().unwrap().bytes_len(), 0);
                app.shutdown().unwrap();
            }
        }
    }

    #[test]
    fn quit_is_right_aligned_and_exits_from_view_and_editing_without_click_through() {
        for language in [Language::En, Language::ZhCn] {
            let mut app = App::new(Config {
                language,
                ..Config::default()
            })
            .unwrap();
            for (width, height) in [(100, 30), (48, 12)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                for mode in [InputMode::View, InputMode::Send, InputMode::Direct] {
                    app.input_mode = mode;
                    terminal.draw(|f| draw(f, &mut app)).unwrap();
                    let quit = target(&app, &ClickAction::Shortcut(KeyCode::Char('q')));
                    assert_eq!(quit.right(), width);
                    assert_eq!(quit.y, 1);
                    for (rect, action) in &app.click_targets {
                        if *action != ClickAction::Shortcut(KeyCode::Char('q')) {
                            assert!(!rect.intersects(quit));
                        }
                    }
                    assert!(app.handle_event(Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: quit.x,
                        row: quit.y,
                        modifiers: KeyModifiers::NONE,
                    })));
                    app.help = true;
                    terminal.draw(|f| draw(f, &mut app)).unwrap();
                    mouse(
                        &mut app,
                        MouseEventKind::Down(MouseButton::Left),
                        quit.x,
                        quit.y,
                    );
                    assert!(!app.help);
                }
            }
            app.shutdown().unwrap();
        }
    }

    #[test]
    fn overlays_resize_and_non_left_clicks_never_activate_background_buttons() {
        let mut app = App::new(Config::default()).unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"preserve me\n".to_vec());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let clear = target(&app, &ClickAction::Shortcut(KeyCode::Char('c')));
        for kind in [
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            mouse(&mut app, kind, clear.x, clear.y);
        }
        click(&mut app, ClickAction::OpenPicker(Picker::Mode));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            clear.x,
            clear.y,
        );
        assert_eq!(app.picker, None);
        assert!(app.store.lock().unwrap().bytes_len() > 0);
        app.help = true;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            clear.x,
            clear.y,
        );
        assert!(!app.help);
        assert!(app.store.lock().unwrap().bytes_len() > 0);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        app.handle_event(Event::Resize(20, 5));
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            clear.x,
            clear.y,
        );
        let mut tiny = Terminal::new(TestBackend::new(20, 5)).unwrap();
        tiny.draw(|f| draw(f, &mut app)).unwrap();
        assert!(app.click_targets.is_empty());
        assert_eq!(app.receive_area, Rect::default());
        assert!(app.store.lock().unwrap().bytes_len() > 0);
        app.shutdown().unwrap();
    }

    #[test]
    fn long_port_picker_scrolls_and_supports_keyboard_without_losing_editor_text() {
        let mut app = App::new(Config::default()).unwrap();
        app.ports = (1..=30).map(|i| format!("COM{i}")).collect();
        app.config.port = "COM30".into();
        app.input_mode = InputMode::Send;
        app.input = "AT+串口".into();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::OpenPicker(Picker::Port));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(app.picker_offset > 0);
        let last = target(&app, &ClickAction::Port("COM30".into()));
        mouse(&mut app, MouseEventKind::ScrollUp, last.x, last.y);
        assert_eq!(app.picker_index, 28);
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.config.port, "COM28");
        assert_eq!(app.input, "AT+串口");
        assert_eq!(app.input_mode, InputMode::Send);
        app.shutdown().unwrap();
    }

    #[test]
    fn mouse_demo_connects_sends_and_disconnects_and_local_clicks_do_not_send_keys() {
        let mut app = App::new(Config {
            demo: true,
            ..Config::default()
        })
        .unwrap();
        wait_for(&mut app, |app| app.connected);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::OpenPicker(Picker::Port));
        assert_eq!(app.picker, None);
        assert!(app.notice.render(Language::En).contains("Disconnect"));
        click(&mut app, ClickAction::Shortcut(KeyCode::F(8)));
        assert_eq!(app.input_mode, InputMode::Direct);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Shortcut(KeyCode::Char('c')));
        assert_eq!(app.worker.stats.tx_bytes(), 0);
        assert_eq!(app.input_mode, InputMode::Direct);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Cancel);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Editor);
        app.append_input("AT");
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Shortcut(KeyCode::F(5)));
        assert_eq!(app.input, "AT");
        click(&mut app, ClickAction::Shortcut(KeyCode::Char('s')));
        wait_for(&mut app, |app| app.worker.stats.tx_bytes() == 4);
        assert_eq!(app.input_mode, InputMode::View);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Shortcut(KeyCode::F(3)));
        wait_for(&mut app, |app| !app.connected && !app.connecting);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Shortcut(KeyCode::F(3)));
        wait_for(&mut app, |app| app.connected);
        app.shutdown().unwrap();
    }

    #[test]
    fn mouse_wheel_scrolls_rx_and_clear_keeps_recording_active() {
        let mut app = App::new(Config::default()).unwrap();
        app.store
            .lock()
            .unwrap()
            .append(Local::now(), b"line\n".repeat(60));
        app.tick();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let offset = app.offset;
        let body = app.receive_area;
        mouse(&mut app, MouseEventKind::ScrollUp, body.x + 1, body.y + 1);
        assert!(app.paused);
        assert_eq!(app.offset, offset - 3);
        mouse(&mut app, MouseEventKind::ScrollDown, body.x + 1, body.y + 1);
        assert_eq!(app.offset, offset);
        let dir = tempfile::tempdir().unwrap();
        app.start_capture(&dir.path().join("mouse.bin")).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        click(&mut app, ClickAction::Shortcut(KeyCode::Char('c')));
        assert!(app.capture.is_some());
        assert!(!app.paused);
        assert_eq!(app.store.lock().unwrap().bytes_len(), 0);
        app.shutdown().unwrap();
    }
    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut result = String::new();
        for row in buffer.content().chunks(usize::from(buffer.area.width)) {
            let mut continuation = 0;
            for cell in row {
                if continuation > 0 {
                    continuation -= 1;
                    continue;
                }
                result.push_str(cell.symbol());
                continuation =
                    unicode_width::UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
            }
            result.push('\n');
        }
        result
    }
    #[test]
    fn bilingual_layouts_preserve_device_text_and_display_localized_errors() {
        for language in [Language::En, Language::ZhCn] {
            let mut app = App::new(Config {
                language,
                ..Config::default()
            })
            .unwrap();
            app.store.lock().unwrap().append(
                Local::now(),
                "设备返回：中文 / RAW DATA\n".as_bytes().to_vec(),
            );
            app.tick();
            app.notice = escom_core::error::ErrorKind::HexOdd.into();
            let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let screen = screen_text(&terminal);
            assert!(screen.contains("设备返回：中文 / RAW DATA"));
            assert!(screen.contains(if language == Language::En {
                "TX Text"
            } else {
                "TX 文本"
            }));
            assert!(screen.contains(if language == Language::En {
                "even number"
            } else {
                "偶数"
            }));
            app.help = true;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let screen = screen_text(&terminal);
            assert!(screen.contains("lang en|zh-CN"));
            for (width, height) in [(48, 12), (20, 5)] {
                let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
                small.draw(|frame| draw(frame, &mut app)).unwrap();
            }
            app.shutdown().unwrap();
        }
    }
    #[test]
    fn renders_live_paused_search_editor_and_small_terminals() {
        let mut app = App::new(Config::default()).unwrap();
        app.store.lock().unwrap().append(
            Local::now(),
            "hello 串口\nWARN voltage\n".as_bytes().to_vec(),
        );
        app.tick();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("ESCOM TUI"));
        assert!(screen.contains("WARN voltage"));
        app.paused = true;
        app.input = "WARN".into();
        app.search().unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("PAUSED"));
        assert_eq!(app.matches.len(), 1);
        app.input_mode = InputMode::Send;
        app.input = "AT+GMR".into();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("AT+GMR"));
        let mut tiny = Terminal::new(TestBackend::new(20, 5)).unwrap();
        tiny.draw(|f| draw(f, &mut app)).unwrap();
        app.shutdown().unwrap();
    }
    #[test]
    fn serial_controls_are_never_forwarded_to_host_terminal() {
        assert_eq!(safe_text("\x1b[2J\r\t\x07"), "·[2J···");
        assert_eq!(safe_text("中文"), "中文");
    }
}
