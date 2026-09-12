use crate::app::{App, InputMode};
use crate::i18n::{Key, Message};
use crate::msg;
use escom_core::model::SendMode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::borrow::Cow;
use unicode_width::UnicodeWidthChar;

const ACCENT: Color = Color::Cyan;

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
    let language = app.config.language;
    let tr = |message: Message| message.render(language).into_owned();
    let send_mode = language.text(match app.send_mode {
        SendMode::Text => Key::Text,
        SendMode::Hex => Key::Hex,
    });
    let area = frame.area();
    if area.width < 48 || area.height < 12 {
        frame.render_widget(Paragraph::new(language.text(Key::SmallScreen)), area);
        return;
    }
    let [
        header,
        controls,
        body,
        memory,
        recording,
        notice,
        editor,
        footer,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
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
        .style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    frame.render_widget(
        Paragraph::new(language.text(Key::Toolbar)).style(Style::default().fg(Color::DarkGray)),
        controls,
    );

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
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if app.matches.binary_search(&index).is_ok() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let mut spans = Vec::with_capacity(2);
            if app.config.timestamps {
                spans.push(Span::styled(
                    row.received_at.format("%H:%M:%S%.3f ").to_string(),
                    Style::default().fg(Color::DarkGray),
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
                .border_style(Style::default().fg(ACCENT)),
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
        .style(Style::default().fg(Color::DarkGray)),
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
            Color::Red
        } else {
            Color::Green
        })),
        recording,
    );
    frame.render_widget(
        Paragraph::new(safe_text(&app.notice.render(language)))
            .style(Style::default().fg(Color::Yellow)),
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
        Paragraph::new(tail).block(Block::bordered().title(label).border_style(
            Style::default().fg(if app.input_mode == InputMode::View {
                Color::DarkGray
            } else {
                ACCENT
            }),
        )),
        editor,
    );
    if matches!(
        app.input_mode,
        InputMode::Send | InputMode::Search | InputMode::Command
    ) && !app.help
    {
        frame.set_cursor_position((editor.x + 1 + tail_width as u16, editor.y + 1));
    }
    frame.render_widget(
        Paragraph::new(language.text(Key::Footer)).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
    if app.help {
        let rect = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(language.text(Key::KeyboardHelp)).block(
                Block::bordered()
                    .title(language.text(Key::HelpTitle))
                    .border_style(Style::default().fg(ACCENT)),
            ),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::i18n::Language;
    use chrono::Local;
    use ratatui::{Terminal, backend::TestBackend};
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
