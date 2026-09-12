use crate::app::{App, InputMode};
use crate::text::send_mode_label;
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
pub const HELP_TEXT: &str = "ESCOM TUI / KEYBOARD

F2       Cycle detected ports       F3       Connect / disconnect
F4       Text / HEX / Terminal      F5       UTF-8 / GBK
F6       Start / stop raw recording F7       Send text / HEX
F8       Direct terminal input (Esc returns to viewer)
s        Compose a send            Enter    Submit editor
/        Search frozen history     Ctrl+R   Toggle regex in search editor
n / N    Next / previous hit row   Esc      Close editor
Space    Pause / resume display    End      Follow live output
Arrows   Scroll rows / columns     PgUp/Dn  Scroll a page
Home     Oldest displayed row      t        Toggle timestamps
c        Clear memory history      q        Quit (Ctrl+Q from any mode)
Ctrl+U   Clear editor              :        Command editor

COMMANDS (type : then a command; changes last for this run)
port COM3    baud 115200    data 8    stop 1    parity none|odd|even
flow none|software|hardware    dtr on|off    rts on|off
mode text|hex|terminal    encoding utf8|gbk    eol none|cr|lf|crlf
ports    connect    disconnect    record path with spaces.bin    stop-record

Search: case-insensitive, current formatted history only; no background index.
Pausing freezes the view, not reception or recording. Resume clears search hits.
Long lines are clipped: scroll horizontally. History eviction is shown below.
Recording: exact RX bytes, new file only; queue/disk failure marks it incomplete.
Direct mode: Enter sends CR, Ctrl+C sends 0x03; Esc returns, Ctrl+Q exits.

Press any key to close help.";

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
    let area = frame.area();
    if area.width < 48 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "ESCOM TUI\nResize to at least 48 x 12.\nCtrl+Q exits; RX / recording continue.",
            ),
            area,
        );
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
        "CONNECTED"
    } else if app.connecting {
        "CONNECTING"
    } else {
        "OFFLINE"
    };
    frame.render_widget(
        Paragraph::new(format!(
            " ESCOM TUI  |  {} {} baud  |  {}  |  {} / {}",
            app.config.port,
            app.config.baud,
            connection,
            app.config.mode.to_uppercase(),
            app.config.encoding.to_uppercase()
        ))
        .style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    frame.render_widget(
        Paragraph::new(" F2 Port  F3 Connect  F4 Mode  F5 Encoding  F6 Record  F8 Direct  ? Help")
            .style(Style::default().fg(Color::DarkGray)),
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
    let title = format!(
        " RX / {} / rows {}-{} of {}{} ",
        if app.paused { "PAUSED" } else { "LIVE" },
        if app.display.rows.is_empty() {
            0
        } else {
            app.offset + 1
        },
        (app.offset + app.page_rows).min(app.display.rows.len()),
        app.display.rows.len(),
        if app.display.limited {
            " / clipped"
        } else {
            ""
        }
    );
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
        Paragraph::new(format!(
            " RX {} B  TX {} B | Raw {}/{} KiB ({} records) | evicted {} B | TX queue {}/{} KiB",
            app.worker.stats.rx_bytes(),
            app.worker.stats.tx_bytes(),
            raw / 1024,
            app.config.history_kib,
            records,
            dropped,
            app.worker.queued_write_bytes().div_ceil(1024),
            app.config.tx_kib
        ))
        .style(Style::default().fg(Color::DarkGray)),
        memory,
    );
    let (record_text, failed) = if let Some(capture) = &app.capture {
        let progress = capture.progress();
        match progress.error {
            Some(error) => (format!(" REC ERROR: {error}"), true),
            None => (
                format!(
                    " REC {} B | queued {} B | {}",
                    progress.written_bytes,
                    progress.queued_bytes,
                    app.capture_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                false,
            ),
        }
    } else {
        (
            format!(
                " {}{}",
                app.capture_result,
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
        Paragraph::new(safe_text(&app.notice)).style(Style::default().fg(Color::Yellow)),
        notice,
    );
    let (label, text) = match app.input_mode {
        InputMode::View => (
            format!(
                " TX {} / {} / s compose ",
                send_mode_label(app.send_mode),
                app.config.line_ending
            ),
            safe_text(&app.last_tx),
        ),
        InputMode::Send => (
            format!(
                " SEND {} / Enter submit / Esc cancel ",
                send_mode_label(app.send_mode)
            ),
            safe_text(&app.input),
        ),
        InputMode::Search => (
            format!(
                " SEARCH {} / Enter scan / Ctrl+R toggle ",
                if app.search_regex { "REGEX" } else { "LITERAL" }
            ),
            safe_text(&app.input),
        ),
        InputMode::Command => (
            " COMMAND / Enter apply / Esc cancel ".into(),
            safe_text(&app.input),
        ),
        InputMode::Direct => (
            " DIRECT INPUT / Esc viewer / Ctrl+Q quit ".into(),
            Cow::Borrowed("Keys go to the serial port. Enter = CR; Ctrl+C = interrupt."),
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
        Paragraph::new(" s Send   / Search   : Command   Space Pause   PgUp/PgDn Scroll   q Quit")
            .style(Style::default().fg(Color::DarkGray)),
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
            Paragraph::new(HELP_TEXT).block(
                Block::bordered()
                    .title(" Help ")
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
    use chrono::Local;
    use ratatui::{Terminal, backend::TestBackend};
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
