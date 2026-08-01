use std::collections::VecDeque;

use chrono::{DateTime, Local};
use encoding_rs::{CoderResult, GBK, UTF_8};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::model::TextEncoding;
use crate::store::RxChunk;

const TERMINAL_DECODE_INPUT_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_ROWS: usize = 100_000;
const MAX_TERMINAL_COLUMNS: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalRow {
    pub received_at: DateTime<Local>,
    pub text: String,
}

#[derive(Debug)]
pub(crate) struct TerminalUpdate {
    pub remove_prefix: usize,
    pub replace_tail: usize,
    pub rows: Vec<TerminalRow>,
}

pub(crate) struct IncrementalTerminalFormatter {
    decoder: encoding_rs::Decoder,
    parser: Parser,
    screen: TerminalScreen,
    displayed_len: usize,
    limited: bool,
}

impl IncrementalTerminalFormatter {
    pub(crate) fn new(encoding: TextEncoding) -> Self {
        let selected_encoding = match encoding {
            TextEncoding::Utf8 => UTF_8,
            TextEncoding::Gbk => GBK,
        };
        Self {
            decoder: selected_encoding.new_decoder_without_bom_handling(),
            parser: Parser::new(),
            screen: TerminalScreen::default(),
            displayed_len: 0,
            limited: false,
        }
    }

    pub(crate) fn apply_chunks(
        &mut self,
        chunks: &[RxChunk],
        max_rows: usize,
        max_text_bytes: usize,
        max_line_bytes: usize,
    ) -> TerminalUpdate {
        self.screen.begin_update();
        for chunk in chunks {
            self.push_chunk(
                chunk,
                max_rows.max(1),
                max_text_bytes.max(1),
                max_line_bytes.max(16),
            );
        }
        self.limited |= self.screen.enforce_limits(
            max_rows.max(1),
            max_text_bytes.max(1),
            max_line_bytes.max(16),
        );

        let new_len = self.screen.rendered_len();
        let (remove_prefix, replace_tail, rows_start) = if self.screen.full_replace {
            (0, self.displayed_len, 0)
        } else {
            let remove_prefix = self.screen.removed_front.min(self.displayed_len);
            let retained = self.displayed_len.saturating_sub(remove_prefix);
            let rows_start = self.screen.dirty_from.unwrap_or(retained).min(new_len);
            let replace_tail = retained.saturating_sub(rows_start.min(retained));
            (remove_prefix, replace_tail, rows_start)
        };
        let rows = self.screen.rows_from(rows_start);
        self.displayed_len = new_len;

        TerminalUpdate {
            remove_prefix,
            replace_tail,
            rows,
        }
    }

    pub(crate) fn cursor(&self) -> Option<(usize, usize)> {
        (self.screen.cursor_row < self.screen.rendered_len())
            .then_some((self.screen.cursor_row, self.screen.cursor_col))
    }

    pub(crate) const fn is_limited(&self) -> bool {
        self.limited
    }

    fn push_chunk(
        &mut self,
        chunk: &RxChunk,
        max_rows: usize,
        max_text_bytes: usize,
        max_line_bytes: usize,
    ) {
        if chunk.bytes.is_empty() {
            return;
        }
        self.screen.received_at = chunk.received_at;

        for piece in chunk.bytes.chunks(TERMINAL_DECODE_INPUT_BYTES) {
            let mut input = piece;
            loop {
                let capacity = input.len().saturating_mul(3).max(32);
                let mut decoded = String::with_capacity(capacity);
                let (result, read, _) = self.decoder.decode_to_string(input, &mut decoded, false);
                self.parser.advance(&mut self.screen, decoded.as_bytes());
                input = &input[read..];
                match result {
                    CoderResult::InputEmpty => break,
                    CoderResult::OutputFull => continue,
                }
            }
            self.limited |= self
                .screen
                .enforce_limits(max_rows, max_text_bytes, max_line_bytes);
        }
    }
}

#[derive(Default)]
struct TerminalLine {
    cells: Vec<char>,
    received_at: Option<DateTime<Local>>,
    completed: bool,
    byte_len: usize,
}

impl TerminalLine {
    fn text(&self) -> String {
        let mut end = self.cells.len();
        while end != 0 && self.cells[end - 1] == ' ' {
            end -= 1;
        }
        self.cells[..end]
            .iter()
            .filter(|character| **character != '\0')
            .collect()
    }
}

struct TerminalScreen {
    lines: VecDeque<TerminalLine>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    received_at: DateTime<Local>,
    dirty_from: Option<usize>,
    byte_dirty_from: Option<usize>,
    removed_front: usize,
    full_replace: bool,
    text_bytes: usize,
}

impl Default for TerminalScreen {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            received_at: Local::now(),
            dirty_from: None,
            byte_dirty_from: None,
            removed_front: 0,
            full_replace: false,
            text_bytes: 0,
        }
    }
}

impl TerminalScreen {
    fn begin_update(&mut self) {
        self.dirty_from = None;
        self.byte_dirty_from = None;
        self.removed_front = 0;
        self.full_replace = false;
    }

    fn rendered_len(&self) -> usize {
        match self.lines.back() {
            Some(line) if line.cells.is_empty() && !line.completed => self.lines.len() - 1,
            _ => self.lines.len(),
        }
    }

    fn rows_from(&self, start: usize) -> Vec<TerminalRow> {
        let end = self.rendered_len();
        self.lines
            .iter()
            .take(end)
            .skip(start.min(end))
            .map(|line| TerminalRow {
                received_at: line.received_at.unwrap_or(self.received_at),
                text: line.text(),
            })
            .collect()
    }

    fn mark_dirty(&mut self, row: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(row, |dirty| dirty.min(row)));
        self.byte_dirty_from = Some(self.byte_dirty_from.map_or(row, |dirty| dirty.min(row)));
    }

    fn ensure_line(&mut self, row: usize) {
        while self.lines.len() <= row {
            let index = self.lines.len();
            self.lines.push_back(TerminalLine::default());
            self.mark_dirty(index);
        }
    }

    fn print(&mut self, character: char) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);

        let width = UnicodeWidthChar::width(character).unwrap_or(1).max(1);
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > line.cells.len() {
            line.cells.resize(self.cursor_col, ' ');
        }

        if self.cursor_col < line.cells.len()
            && line.cells[self.cursor_col] == '\0'
            && self.cursor_col != 0
        {
            line.cells[self.cursor_col - 1] = ' ';
        }
        if self.cursor_col < line.cells.len()
            && UnicodeWidthChar::width(line.cells[self.cursor_col]) == Some(2)
            && self.cursor_col + 1 < line.cells.len()
        {
            line.cells[self.cursor_col + 1] = ' ';
        }

        if self.cursor_col == line.cells.len() {
            line.cells.push(character);
        } else {
            line.cells[self.cursor_col] = character;
        }
        if width == 2 {
            if self.cursor_col + 1 == line.cells.len() {
                line.cells.push('\0');
            } else {
                line.cells[self.cursor_col + 1] = '\0';
            }
        }
        line.received_at.get_or_insert(self.received_at);
        line.completed = false;
        self.cursor_col = self
            .cursor_col
            .saturating_add(width)
            .min(MAX_TERMINAL_COLUMNS);
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn line_feed(&mut self) {
        self.ensure_line(self.cursor_row);
        let current_becomes_visible = self.cursor_row + 1 == self.lines.len()
            && self.lines[self.cursor_row].cells.is_empty()
            && !self.lines[self.cursor_row].completed;
        if current_becomes_visible {
            self.mark_dirty(self.cursor_row);
        }
        self.lines[self.cursor_row].completed = true;
        if self.cursor_row >= MAX_TERMINAL_ROWS - 1 {
            self.remove_front();
        }
        self.cursor_row = self.cursor_row.saturating_add(1).min(MAX_TERMINAL_ROWS - 1);
        self.ensure_line(self.cursor_row);
    }

    fn next_line(&mut self) {
        self.carriage_return();
        self.line_feed();
    }

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn horizontal_tab(&mut self) {
        self.cursor_col = self
            .cursor_col
            .saturating_add(8 - self.cursor_col % 8)
            .min(MAX_TERMINAL_COLUMNS);
    }

    fn move_up(&mut self, count: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(count);
    }

    fn move_down(&mut self, count: usize) {
        self.cursor_row = self
            .cursor_row
            .saturating_add(count)
            .min(MAX_TERMINAL_ROWS - 1);
        self.ensure_line(self.cursor_row);
    }

    fn move_forward(&mut self, count: usize) {
        self.cursor_col = self
            .cursor_col
            .saturating_add(count)
            .min(MAX_TERMINAL_COLUMNS);
    }

    fn move_back(&mut self, count: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    fn set_position(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(MAX_TERMINAL_ROWS - 1);
        self.cursor_col = col.min(MAX_TERMINAL_COLUMNS);
        self.ensure_line(self.cursor_row);
    }

    fn erase_line(&mut self, mode: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        match mode {
            1 => {
                if line.cells.len() <= self.cursor_col {
                    line.cells.resize(self.cursor_col.saturating_add(1), ' ');
                }
                for cell in line
                    .cells
                    .iter_mut()
                    .take(self.cursor_col.saturating_add(1))
                {
                    *cell = ' ';
                }
            }
            2 | 3 => {
                line.cells.clear();
                line.received_at = None;
                line.completed = false;
            }
            _ => line.cells.truncate(self.cursor_col.min(line.cells.len())),
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            1 => {
                self.ensure_line(self.cursor_row);
                self.mark_dirty(0);
                for row in 0..self.cursor_row {
                    self.lines[row].cells.clear();
                    self.lines[row].received_at = None;
                }
                self.erase_line(1);
            }
            2 | 3 => self.clear(),
            _ => {
                self.ensure_line(self.cursor_row);
                self.mark_dirty(self.cursor_row);
                self.erase_line(0);
                for row in self.cursor_row + 1..self.lines.len() {
                    self.lines[row].cells.clear();
                    self.lines[row].received_at = None;
                }
            }
        }
    }

    fn insert_blank_characters(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > line.cells.len() {
            line.cells.resize(self.cursor_col, ' ');
        }
        for _ in 0..count {
            line.cells.insert(self.cursor_col, ' ');
        }
    }

    fn delete_characters(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        let start = self.cursor_col.min(line.cells.len());
        let end = start.saturating_add(count).min(line.cells.len());
        line.cells.drain(start..end);
    }

    fn erase_characters(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        let line = &mut self.lines[self.cursor_row];
        let end = self.cursor_col.saturating_add(count);
        if line.cells.len() < end {
            line.cells.resize(end, ' ');
        }
        for cell in &mut line.cells[self.cursor_col..end] {
            *cell = ' ';
        }
    }

    fn insert_lines(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        for _ in 0..count {
            self.lines.insert(self.cursor_row, TerminalLine::default());
        }
    }

    fn delete_lines(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        for _ in 0..count {
            if self.cursor_row < self.lines.len()
                && let Some(removed) = self.lines.remove(self.cursor_row)
            {
                self.text_bytes = self.text_bytes.saturating_sub(removed.byte_len);
            }
        }
        if self.lines.is_empty() {
            self.lines.push_back(TerminalLine::default());
        }
        self.cursor_row = self.cursor_row.min(self.lines.len() - 1);
    }

    fn scroll_up(&mut self, count: usize) {
        for _ in 0..count.min(self.lines.len()) {
            self.remove_front();
        }
        self.ensure_line(self.cursor_row);
    }

    fn scroll_down(&mut self, count: usize) {
        self.mark_dirty(0);
        for _ in 0..count.min(MAX_TERMINAL_ROWS) {
            self.lines.push_front(TerminalLine::default());
            self.cursor_row = self.cursor_row.saturating_add(1).min(MAX_TERMINAL_ROWS - 1);
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == 0 {
            self.lines.push_front(TerminalLine::default());
            self.mark_dirty(0);
        } else {
            self.cursor_row -= 1;
        }
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.text_bytes = 0;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.saved_cursor = None;
        self.dirty_from = Some(0);
        self.byte_dirty_from = Some(0);
        self.full_replace = true;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.set_position(row, col);
        }
    }

    fn remove_front(&mut self) {
        let rendered_before = self.rendered_len();
        let Some(removed) = self.lines.pop_front() else {
            return;
        };
        self.text_bytes = self.text_bytes.saturating_sub(removed.byte_len);
        if rendered_before != 0 {
            self.removed_front = self.removed_front.saturating_add(1);
        }
        self.cursor_row = self.cursor_row.saturating_sub(1);
        if let Some((row, col)) = self.saved_cursor {
            self.saved_cursor = Some((row.saturating_sub(1), col));
        }
        self.dirty_from = self.dirty_from.map(|row| row.saturating_sub(1));
        self.byte_dirty_from = self.byte_dirty_from.map(|row| row.saturating_sub(1));
    }

    fn enforce_limits(
        &mut self,
        max_rows: usize,
        max_text_bytes: usize,
        max_line_bytes: usize,
    ) -> bool {
        let mut limited = false;
        let dirty_from = self.byte_dirty_from.unwrap_or(self.lines.len());
        for row in dirty_from..self.lines.len() {
            self.refresh_line_byte_count(row);
        }
        for row in dirty_from..self.lines.len() {
            let line_bytes = self.lines[row].byte_len;
            if line_bytes <= max_line_bytes {
                continue;
            }

            let retain_bytes = (max_line_bytes / 2).max(1);
            let mut retained = 0_usize;
            let mut retain_from = self.lines[row].cells.len();
            while retain_from != 0 {
                let next = self.lines[row].cells[retain_from - 1].len_utf8();
                if retained.saturating_add(next) > retain_bytes {
                    break;
                }
                retain_from -= 1;
                retained += next;
            }
            self.lines[row].cells.drain(..retain_from);
            self.lines[row].cells.insert(0, '…');
            self.refresh_line_byte_count(row);
            if self.cursor_row == row {
                self.cursor_col = self
                    .cursor_col
                    .saturating_sub(retain_from)
                    .saturating_add(1);
            }
            if let Some((saved_row, saved_col)) = self.saved_cursor
                && saved_row == row
            {
                self.saved_cursor = Some((saved_row, saved_col.saturating_sub(retain_from) + 1));
            }
            self.mark_dirty(row);
            limited = true;
        }

        while self.rendered_len() > max_rows || self.text_bytes > max_text_bytes {
            self.remove_front();
            limited = true;
        }
        self.byte_dirty_from = None;
        limited
    }

    fn refresh_line_byte_count(&mut self, row: usize) {
        let old_bytes = self.lines[row].byte_len;
        let new_bytes = self.lines[row]
            .cells
            .iter()
            .filter(|character| **character != '\0')
            .map(|character| character.len_utf8())
            .sum();
        self.lines[row].byte_len = new_bytes;
        self.text_bytes = self
            .text_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
    }
}

fn parameter(params: &Params, index: usize, default: usize) -> usize {
    params
        .iter()
        .nth(index)
        .and_then(|parameter| parameter.first())
        .copied()
        .map(usize::from)
        .filter(|value| *value != 0)
        .unwrap_or(default)
}

fn mode_parameter(params: &Params) -> usize {
    params
        .iter()
        .next()
        .and_then(|parameter| parameter.first())
        .copied()
        .map(usize::from)
        .unwrap_or(0)
}

impl Perform for TerminalScreen {
    fn print(&mut self, character: char) {
        TerminalScreen::print(self, character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.backspace(),
            0x09 => self.horizontal_tab(),
            0x0A..=0x0C => self.line_feed(),
            0x0D => self.carriage_return(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        match action {
            'A' => self.move_up(parameter(params, 0, 1)),
            'B' | 'e' => self.move_down(parameter(params, 0, 1)),
            'C' | 'a' => self.move_forward(parameter(params, 0, 1)),
            'D' => self.move_back(parameter(params, 0, 1)),
            'E' => {
                self.move_down(parameter(params, 0, 1));
                self.carriage_return();
            }
            'F' => {
                self.move_up(parameter(params, 0, 1));
                self.carriage_return();
            }
            'G' | '`' => {
                self.cursor_col = parameter(params, 0, 1)
                    .saturating_sub(1)
                    .min(MAX_TERMINAL_COLUMNS);
            }
            'H' | 'f' => self.set_position(
                parameter(params, 0, 1).saturating_sub(1),
                parameter(params, 1, 1).saturating_sub(1),
            ),
            'd' => {
                self.cursor_row = parameter(params, 0, 1)
                    .saturating_sub(1)
                    .min(MAX_TERMINAL_ROWS - 1);
                self.ensure_line(self.cursor_row);
            }
            'J' => self.erase_display(mode_parameter(params)),
            'K' => self.erase_line(mode_parameter(params)),
            '@' => self.insert_blank_characters(parameter(params, 0, 1)),
            'P' => self.delete_characters(parameter(params, 0, 1)),
            'X' => self.erase_characters(parameter(params, 0, 1)),
            'L' => self.insert_lines(parameter(params, 0, 1)),
            'M' => self.delete_lines(parameter(params, 0, 1)),
            'S' => self.scroll_up(parameter(params, 0, 1)),
            'T' => self.scroll_down(parameter(params, 0, 1)),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            // SGR, modes, status reports, scroll regions and cursor styles do not alter text.
            'm' | 'h' | 'l' | 'n' | 'r' | 'q' | 't' | '~' => {}
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.line_feed(),
            b'E' => self.next_line(),
            b'M' => self.reverse_index(),
            b'c' => self.clear(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;

    fn timestamp(second: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 31, 12, 0, second)
            .single()
            .unwrap()
    }

    fn chunk(sequence: u64, second: u32, bytes: &[u8]) -> RxChunk {
        RxChunk {
            sequence,
            received_at: timestamp(second),
            bytes: bytes.to_vec().into(),
        }
    }

    fn apply(formatter: &mut IncrementalTerminalFormatter, chunks: &[RxChunk]) -> TerminalUpdate {
        formatter.apply_chunks(chunks, 100, 4096, 1024)
    }

    #[test]
    fn rt_thread_history_redraw_replaces_the_current_line() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(
            &mut formatter,
            &[chunk(0, 1, b"msh >list\x1b[2K\rmsh >thread")],
        );

        assert_eq!(update.rows[0].text, "msh >thread");
        assert_eq!(formatter.cursor(), Some((0, 11)));
    }

    #[test]
    fn backspace_space_backspace_erases_the_echoed_character() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(&mut formatter, &[chunk(0, 1, b"msh >abc\x08 \x08")]);

        assert_eq!(update.rows[0].text, "msh >ab");
        assert_eq!(formatter.cursor(), Some((0, 7)));
    }

    #[test]
    fn split_csi_and_split_gbk_are_incremental() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Gbk);
        let (encoded, _, _) = GBK.encode("中文");
        let encoded = encoded.into_owned();
        let first = chunk(0, 1, &encoded[..1]);
        let second = chunk(1, 2, &encoded[1..]);
        let escape = chunk(2, 3, b"\x1b[");
        let redraw = chunk(3, 4, b"2K\rready");

        let first_update = apply(&mut formatter, &[first, second, escape]);
        assert_eq!(first_update.rows[0].text, "中文");
        let second_update = apply(&mut formatter, &[redraw]);
        assert_eq!(second_update.replace_tail, 1);
        assert_eq!(second_update.rows[0].text, "ready");
    }

    #[test]
    fn cursor_movement_overwrites_instead_of_inserting() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(&mut formatter, &[chunk(0, 1, b"abcd\x08\x08X")]);

        assert_eq!(update.rows[0].text, "abXd");
        assert_eq!(formatter.cursor(), Some((0, 3)));
    }

    #[test]
    fn sgr_sequences_are_consumed_without_becoming_visible_text() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(
            &mut formatter,
            &[chunk(0, 1, b"normal \x1b[31mred\x1b[0m text")],
        );

        assert_eq!(update.rows[0].text, "normal red text");
    }

    #[test]
    fn row_limit_prunes_the_prefix_and_keeps_incremental_updates_valid() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let mut rows = Vec::new();
        let first = formatter.apply_chunks(&[chunk(0, 1, b"a\r\nb\r\nc\r\nd")], 3, 4096, 1024);
        rows.extend(first.rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );

        let second = formatter.apply_chunks(&[chunk(1, 2, b"\r\ne")], 3, 4096, 1024);
        assert_eq!(second.remove_prefix, 1);
        assert_eq!(second.replace_tail, 0);
        rows.drain(..second.remove_prefix);
        rows.extend(second.rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["c", "d", "e"]
        );
        assert!(formatter.is_limited());
    }
}
