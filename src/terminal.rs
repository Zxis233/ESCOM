use std::collections::VecDeque;

use chrono::{DateTime, Local};
use encoding_rs::{CoderResult, GBK, UTF_8};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::model::TextEncoding;
use crate::store::{ReceiveBoundary, ReceiveRecord, RxChunk};

const TERMINAL_DECODE_INPUT_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_ROWS: usize = 100_000;
const MAX_TERMINAL_COLUMNS: usize = 512 * 1024;
// Bound fresh storage requested by one CSI dispatch without restricting access to existing cells.
const MAX_CSI_CELL_GROWTH: usize = 4 * 1024;
// Bound fresh row changes requested by one CSI dispatch. L/M/S/T also share an update budget.
const MAX_CSI_LINE_CHANGE_PER_DISPATCH: usize = 1024;
const MAX_CSI_LINE_WORK_PER_UPDATE: usize = 4 * MAX_TERMINAL_ROWS;
const MIN_CSI_LINE_DISPATCH_WORK: usize = 64;

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
    encoding: TextEncoding,
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
            encoding,
            decoder: selected_encoding.new_decoder_without_bom_handling(),
            parser: Parser::new(),
            screen: TerminalScreen::default(),
            displayed_len: 0,
            limited: false,
        }
    }

    pub(crate) fn apply_records(
        &mut self,
        records: &[ReceiveRecord],
        max_rows: usize,
        max_text_bytes: usize,
        max_line_bytes: usize,
    ) -> TerminalUpdate {
        let max_rows = max_rows.clamp(1, MAX_TERMINAL_ROWS);
        let max_text_bytes = max_text_bytes.max(1);
        let max_line_bytes = max_line_bytes.clamp(16, MAX_TERMINAL_COLUMNS);
        self.screen
            .begin_update(max_rows, max_text_bytes, max_line_bytes);
        for record in records {
            match record {
                ReceiveRecord::Data(chunk) => {
                    self.push_chunk(chunk, max_rows, max_text_bytes, max_line_bytes)
                }
                ReceiveRecord::Boundary(boundary) => {
                    self.finish_session(boundary, max_rows, max_text_bytes, max_line_bytes)
                }
            }
        }
        self.limited |= self
            .screen
            .enforce_limits(max_rows, max_text_bytes, max_line_bytes);
        self.limited |= self.screen.csi_limited;

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
            self.decode_bytes(piece, false);
            self.limited |= self
                .screen
                .enforce_limits(max_rows, max_text_bytes, max_line_bytes);
        }
    }

    fn decode_bytes(&mut self, mut input: &[u8], last: bool) {
        loop {
            let capacity = input.len().saturating_mul(3).max(32);
            let mut decoded = String::with_capacity(capacity);
            let (result, read, _) = self.decoder.decode_to_string(input, &mut decoded, last);
            self.parser.advance(&mut self.screen, decoded.as_bytes());
            input = &input[read..];
            match result {
                CoderResult::InputEmpty => break,
                CoderResult::OutputFull => continue,
            }
        }
    }

    fn finish_session(
        &mut self,
        boundary: &ReceiveBoundary,
        max_rows: usize,
        max_text_bytes: usize,
        max_line_bytes: usize,
    ) {
        self.screen.received_at = boundary.received_at;
        self.decode_bytes(&[], true);
        self.parser = Parser::new();
        let selected_encoding = match self.encoding {
            TextEncoding::Utf8 => UTF_8,
            TextEncoding::Gbk => GBK,
        };
        self.decoder = selected_encoding.new_decoder_without_bom_handling();
        self.screen.finish_session();
        self.limited |= self
            .screen
            .enforce_limits(max_rows, max_text_bytes, max_line_bytes);
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
    session_origin: usize,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    received_at: DateTime<Local>,
    dirty_from: Option<usize>,
    byte_dirty_from: Option<usize>,
    removed_front: usize,
    full_replace: bool,
    text_bytes: usize,
    max_rows: usize,
    max_line_cells: usize,
    csi_limited: bool,
    csi_line_work_remaining: usize,
}

impl Default for TerminalScreen {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            session_origin: 0,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            received_at: Local::now(),
            dirty_from: None,
            byte_dirty_from: None,
            removed_front: 0,
            full_replace: false,
            text_bytes: 0,
            max_rows: MAX_TERMINAL_ROWS,
            max_line_cells: MAX_TERMINAL_COLUMNS,
            csi_limited: false,
            csi_line_work_remaining: MAX_CSI_LINE_WORK_PER_UPDATE,
        }
    }
}

impl TerminalScreen {
    fn begin_update(&mut self, max_rows: usize, max_text_bytes: usize, max_line_bytes: usize) {
        self.dirty_from = None;
        self.byte_dirty_from = None;
        self.removed_front = 0;
        self.full_replace = false;
        self.max_rows = max_rows;
        self.max_line_cells = max_line_bytes.min(max_text_bytes).max(1);
        self.csi_limited = false;
        self.csi_line_work_remaining = MAX_CSI_LINE_WORK_PER_UPDATE;
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

    fn finish_session(&mut self) {
        self.saved_cursor = None;
        self.cursor_col = 0;
        let Some(last_index) = self.lines.len().checked_sub(1) else {
            self.session_origin = 0;
            self.cursor_row = 0;
            return;
        };

        let reuse_trailing_line =
            self.lines[last_index].cells.is_empty() && !self.lines[last_index].completed;
        if reuse_trailing_line {
            for line in self.lines.iter_mut().take(last_index) {
                line.completed = true;
            }
            self.cursor_row = last_index;
        } else {
            for line in &mut self.lines {
                line.completed = true;
            }
            self.cursor_row = self.lines.len();
            self.ensure_line(self.cursor_row);
        }
        while self.cursor_row >= self.max_rows {
            self.remove_front();
        }
        self.session_origin = self.cursor_row;
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
        self.cursor_row = self
            .cursor_row
            .saturating_sub(count)
            .max(self.session_origin);
    }

    fn move_down(&mut self, count: usize) {
        let row = self.cursor_row.saturating_add(count);
        self.set_csi_absolute_row(row);
    }

    fn move_forward(&mut self, count: usize) {
        let col = self.cursor_col.saturating_add(count);
        self.set_csi_column(col);
    }

    fn move_back(&mut self, count: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    fn set_position(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(MAX_TERMINAL_ROWS - 1);
        self.cursor_col = col.min(MAX_TERMINAL_COLUMNS);
        self.ensure_line(self.cursor_row);
    }

    fn set_csi_position(&mut self, row: usize, col: usize) {
        self.set_csi_row(row);
        self.set_csi_column(col);
    }

    fn set_csi_row(&mut self, row: usize) {
        self.set_csi_absolute_row(self.session_origin.saturating_add(row));
    }

    fn set_csi_absolute_row(&mut self, row: usize) {
        let allocation_limit = self
            .lines
            .len()
            .saturating_add(MAX_CSI_LINE_CHANGE_PER_DISPATCH)
            .saturating_sub(1);
        let target = row
            .min(self.max_rows.saturating_sub(1))
            .min(allocation_limit);
        self.csi_limited |= target != row;
        self.lines
            .reserve_exact(target.saturating_add(1).saturating_sub(self.lines.len()));
        self.cursor_row = target;
        self.ensure_line(self.cursor_row);
    }

    fn set_csi_column(&mut self, col: usize) {
        let line_len = self
            .lines
            .get(self.cursor_row)
            .map_or(0, |line| line.cells.len());
        let allocation_limit = line_len
            .saturating_add(MAX_CSI_CELL_GROWTH)
            .saturating_sub(1);
        let target = col
            .min(self.max_line_cells.saturating_sub(1))
            .min(allocation_limit);
        self.csi_limited |= target != col;
        self.cursor_col = target;
    }

    fn csi_cell_growth_limit(&self, current_len: usize) -> usize {
        current_len
            .saturating_add(MAX_CSI_CELL_GROWTH)
            .min(self.max_line_cells)
    }

    fn limit_csi_line_count(&mut self, requested: usize) -> usize {
        let count = requested.min(MAX_CSI_LINE_CHANGE_PER_DISPATCH);
        self.csi_limited |= count != requested;
        count
    }

    fn claim_csi_line_work(&mut self, estimated_work: usize) -> bool {
        let estimated_work = estimated_work.max(MIN_CSI_LINE_DISPATCH_WORK);
        let Some(remaining) = self.csi_line_work_remaining.checked_sub(estimated_work) else {
            self.csi_limited = true;
            return false;
        };
        self.csi_line_work_remaining = remaining;
        true
    }

    fn truncate_lines(&mut self, len: usize) -> bool {
        if len >= self.lines.len() {
            return false;
        }
        let removed_bytes = self.lines.drain(len..).map(|line| line.byte_len).sum();
        self.text_bytes = self.text_bytes.saturating_sub(removed_bytes);
        true
    }

    fn erase_line(&mut self, mode: usize) {
        self.ensure_line(self.cursor_row);
        self.mark_dirty(self.cursor_row);
        match mode {
            1 => {
                let current_len = self.lines[self.cursor_row].cells.len();
                let requested_end = self.cursor_col.saturating_add(1);
                let end = requested_end
                    .min(current_len.saturating_add(MAX_CSI_CELL_GROWTH))
                    .min(self.max_line_cells);
                self.csi_limited |= end != requested_end;
                let line = &mut self.lines[self.cursor_row];
                line.cells.reserve_exact(end.saturating_sub(current_len));
                if current_len < end {
                    line.cells.resize(end, ' ');
                }
                for cell in line.cells.iter_mut().take(end) {
                    *cell = ' ';
                }
            }
            2 | 3 => {
                let line = &mut self.lines[self.cursor_row];
                line.cells.clear();
                line.received_at = None;
                line.completed = false;
            }
            _ => {
                let line = &mut self.lines[self.cursor_row];
                line.cells.truncate(self.cursor_col.min(line.cells.len()));
            }
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            1 => {
                self.ensure_line(self.cursor_row);
                self.mark_dirty(self.session_origin);
                for row in self.session_origin..self.cursor_row {
                    self.lines[row].cells.clear();
                    self.lines[row].received_at = None;
                }
                self.erase_line(1);
            }
            2 | 3 => self.clear_session(),
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
        let current_len = self.lines[self.cursor_row].cells.len();
        let growth_limit = self.csi_cell_growth_limit(current_len);
        let insert_at = self.cursor_col.min(growth_limit);
        let base_len = current_len.max(insert_at);
        let inserted = count.min(growth_limit.saturating_sub(base_len));
        let final_len = base_len.saturating_add(inserted);
        self.csi_limited |= insert_at != self.cursor_col || inserted != count;

        let line = &mut self.lines[self.cursor_row];
        line.cells
            .reserve_exact(final_len.saturating_sub(current_len));
        if insert_at > current_len {
            line.cells.resize(insert_at, ' ');
        }
        if inserted == 0 {
            return;
        }
        let old_len = line.cells.len();
        line.cells.resize(final_len, ' ');
        line.cells
            .copy_within(insert_at..old_len, insert_at + inserted);
        line.cells[insert_at..insert_at + inserted].fill(' ');
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
        let current_len = self.lines[self.cursor_row].cells.len();
        let growth_limit = self.csi_cell_growth_limit(current_len);
        let start = self.cursor_col.min(self.max_line_cells);
        let requested_end = start.saturating_add(count);
        let end = requested_end.min(growth_limit);
        self.csi_limited |= start != self.cursor_col || end != requested_end;
        if start >= end {
            return;
        }

        let line = &mut self.lines[self.cursor_row];
        line.cells.reserve_exact(end.saturating_sub(current_len));
        if current_len < end {
            line.cells.resize(end, ' ');
        }
        for cell in &mut line.cells[start..end] {
            *cell = ' ';
        }
    }

    fn insert_lines(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        let limited_count = self.limit_csi_line_count(count);
        let count = limited_count.min(self.max_rows.saturating_sub(self.cursor_row));
        self.csi_limited |= count != limited_count;
        if count == 0 {
            return;
        }

        let retained_len = self.lines.len().min(self.max_rows.saturating_sub(count));
        let rows_to_relocate = self
            .lines
            .len()
            .saturating_add(retained_len.saturating_sub(self.cursor_row))
            .saturating_add(count);
        if !self.claim_csi_line_work(rows_to_relocate) {
            return;
        }

        self.mark_dirty(self.cursor_row);
        if self.truncate_lines(retained_len) {
            self.csi_limited = true;
        }
        let old_len = self.lines.len();
        self.lines
            .resize_with(old_len.saturating_add(count), TerminalLine::default);
        if old_len > self.cursor_row {
            self.lines.make_contiguous()[self.cursor_row..].rotate_right(count);
        }
    }

    fn delete_lines(&mut self, count: usize) {
        self.ensure_line(self.cursor_row);
        let count = self
            .limit_csi_line_count(count)
            .min(self.lines.len().saturating_sub(self.cursor_row));
        if count == 0 {
            return;
        }

        let estimated_work = self.lines.len().saturating_add(count);
        if !self.claim_csi_line_work(estimated_work) {
            return;
        }

        self.mark_dirty(self.cursor_row);
        let end = self.cursor_row.saturating_add(count);
        let removed_bytes = self
            .lines
            .drain(self.cursor_row..end)
            .map(|line| line.byte_len)
            .sum();
        self.text_bytes = self.text_bytes.saturating_sub(removed_bytes);
        if self.lines.is_empty() {
            self.lines.push_back(TerminalLine::default());
        }
        self.cursor_row = self.cursor_row.min(self.lines.len() - 1);
    }

    fn scroll_up(&mut self, count: usize) {
        let count = self
            .limit_csi_line_count(count)
            .min(self.lines.len().saturating_sub(self.session_origin));
        if count != 0 && self.claim_csi_line_work(count) {
            if self.session_origin == 0 {
                self.remove_front_lines(count);
            } else {
                self.mark_dirty(self.session_origin);
                let end = self.session_origin.saturating_add(count);
                let removed_bytes = self
                    .lines
                    .drain(self.session_origin..end)
                    .map(|line| line.byte_len)
                    .sum();
                self.text_bytes = self.text_bytes.saturating_sub(removed_bytes);
                self.cursor_row = self
                    .cursor_row
                    .saturating_sub(count)
                    .max(self.session_origin);
                if let Some((row, col)) = self.saved_cursor {
                    self.saved_cursor =
                        Some((row.saturating_sub(count).max(self.session_origin), col));
                }
            }
        }
        self.ensure_line(self.cursor_row);
    }

    fn scroll_down(&mut self, count: usize) {
        let limited_count = self.limit_csi_line_count(count);
        let available_rows = self.max_rows.saturating_sub(self.session_origin);
        let count = limited_count.min(available_rows);
        self.csi_limited |= count != limited_count;
        if count == 0 {
            return;
        }

        let session_len = self.lines.len().saturating_sub(self.session_origin);
        let retained_len = session_len.min(available_rows.saturating_sub(count));
        let rows_to_relocate = self
            .lines
            .len()
            .saturating_add(retained_len)
            .saturating_add(count);
        if !self.claim_csi_line_work(rows_to_relocate) {
            return;
        }

        self.mark_dirty(self.session_origin);
        if self.truncate_lines(self.session_origin.saturating_add(retained_len)) {
            self.csi_limited = true;
        }
        let old_len = self.lines.len();
        self.lines
            .resize_with(old_len.saturating_add(count), TerminalLine::default);
        if old_len > self.session_origin {
            self.lines.make_contiguous()[self.session_origin..].rotate_right(count);
        }
        self.cursor_row = self.cursor_row.saturating_add(count).min(self.max_rows - 1);
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.session_origin {
            self.lines
                .insert(self.session_origin, TerminalLine::default());
            self.mark_dirty(self.session_origin);
        } else {
            self.cursor_row -= 1;
        }
    }

    fn clear_session(&mut self) {
        let origin = self.session_origin.min(self.lines.len());
        let removed_bytes = self.lines.drain(origin..).map(|line| line.byte_len).sum();
        self.text_bytes = self.text_bytes.saturating_sub(removed_bytes);
        self.cursor_row = origin;
        self.cursor_col = 0;
        self.saved_cursor = None;
        self.mark_dirty(origin);
        self.full_replace |= origin == 0;
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
        self.remove_front_lines(1);
    }

    fn remove_front_lines(&mut self, count: usize) {
        let count = count.min(self.lines.len());
        if count == 0 {
            return;
        }

        let rendered_removed = count.min(self.rendered_len());
        let removed_bytes = self.lines.drain(..count).map(|line| line.byte_len).sum();
        self.text_bytes = self.text_bytes.saturating_sub(removed_bytes);
        self.removed_front = self.removed_front.saturating_add(rendered_removed);
        self.session_origin = self.session_origin.saturating_sub(count);
        self.cursor_row = self.cursor_row.saturating_sub(count);
        if let Some((row, col)) = self.saved_cursor {
            self.saved_cursor = Some((row.saturating_sub(count), col));
        }
        self.dirty_from = self.dirty_from.map(|row| row.saturating_sub(count));
        self.byte_dirty_from = self.byte_dirty_from.map(|row| row.saturating_sub(count));
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
                self.set_csi_column(parameter(params, 0, 1).saturating_sub(1));
            }
            'H' | 'f' => self.set_csi_position(
                parameter(params, 0, 1).saturating_sub(1),
                parameter(params, 1, 1).saturating_sub(1),
            ),
            'd' => {
                self.set_csi_row(parameter(params, 0, 1).saturating_sub(1));
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
            b'c' => self.clear_session(),
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

    fn chunk(sequence: u64, second: u32, bytes: &[u8]) -> ReceiveRecord {
        ReceiveRecord::Data(RxChunk {
            sequence,
            received_at: timestamp(second),
            session_offset: 0,
            bytes: bytes.to_vec().into(),
        })
    }

    fn boundary(sequence: u64, second: u32) -> ReceiveRecord {
        ReceiveRecord::Boundary(ReceiveBoundary {
            sequence,
            received_at: timestamp(second),
        })
    }

    fn apply(
        formatter: &mut IncrementalTerminalFormatter,
        records: &[ReceiveRecord],
    ) -> TerminalUpdate {
        formatter.apply_records(records, 100, 4096, 1024)
    }

    fn terminal_line(text: &str) -> TerminalLine {
        TerminalLine {
            cells: text.chars().collect(),
            completed: true,
            byte_len: text.len(),
            ..TerminalLine::default()
        }
    }

    fn screen_with_lines(lines: &[&str], max_rows: usize, cursor_row: usize) -> TerminalScreen {
        let mut screen = TerminalScreen::default();
        screen.begin_update(max_rows, 4096, 1024);
        screen.lines = lines.iter().map(|line| terminal_line(line)).collect();
        screen.text_bytes = lines.iter().map(|line| line.len()).sum();
        screen.cursor_row = cursor_row;
        screen
    }

    fn screen_texts(screen: &TerminalScreen) -> Vec<String> {
        screen.lines.iter().map(TerminalLine::text).collect()
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
    fn stream_boundary_resets_parser_and_starts_a_new_history_row() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(
            &mut formatter,
            &[
                chunk(0, 1, b"old\x1b["),
                boundary(1, 2),
                chunk(2, 3, b"2Jnew"),
            ],
        );

        assert_eq!(
            update
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["old", "2Jnew"]
        );
        assert_eq!(formatter.cursor(), Some((1, 5)));
    }

    #[test]
    fn stream_boundary_reuses_the_hidden_line_after_a_completed_newline() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(
            &mut formatter,
            &[chunk(0, 1, b"old\r\n"), boundary(1, 2), chunk(2, 3, b"new")],
        );

        assert_eq!(
            update
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["old", "new"]
        );
    }

    #[test]
    fn new_session_clear_and_home_sequences_cannot_modify_archived_rows() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let update = apply(
            &mut formatter,
            &[
                chunk(0, 1, b"archived"),
                boundary(1, 2),
                chunk(2, 3, b"first\r\nsecond\x1b[2Jnew\x1b[Hhome"),
            ],
        );

        assert_eq!(
            update
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["archived", "home"]
        );
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
    fn csi_character_operations_limit_growth_per_dispatch() {
        let mut parser = Parser::new();
        let mut screen = TerminalScreen::default();
        screen.begin_update(
            MAX_TERMINAL_ROWS,
            MAX_TERMINAL_COLUMNS,
            MAX_TERMINAL_COLUMNS,
        );

        parser.advance(&mut screen, b"a\x1b[65535@");
        assert_eq!(screen.lines[0].cells.len(), 1 + MAX_CSI_CELL_GROWTH);

        let previous_len = screen.lines[0].cells.len();
        parser.advance(&mut screen, b"\x1b[65535X");
        assert_eq!(
            screen.lines[0].cells.len() - previous_len,
            MAX_CSI_CELL_GROWTH
        );
    }

    #[test]
    fn csi_coordinates_and_line_operations_respect_allocation_limits() {
        let mut parser = Parser::new();
        let mut screen = TerminalScreen::default();
        screen.begin_update(10, 64, 64);

        parser.advance(&mut screen, b"\x1b[65535;65535HX");
        assert_eq!(screen.lines.len(), 10);
        assert_eq!(screen.cursor_row, 9);
        assert_eq!(screen.cursor_col, 64);
        assert_eq!(screen.lines[9].cells.len(), 64);

        parser.advance(&mut screen, b"\x1b[65535L\x1b[65535T");
        assert_eq!(screen.lines.len(), 10);
        assert!(screen.csi_limited);

        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        formatter.apply_records(&[chunk(0, 1, b"\x1b[65535;65535HX")], 10, 64, 64);
        assert!(formatter.is_limited());
    }

    #[test]
    fn csi_line_operations_batch_changes_and_preserve_screen_accounting() {
        let mut screen = screen_with_lines(&["a", "bb", "ccc", "dddd"], 5, 1);
        screen.saved_cursor = Some((3, 2));

        screen.insert_lines(2);
        assert_eq!(screen_texts(&screen), ["a", "", "", "bb", "ccc"]);
        assert_eq!(screen.text_bytes, 6);
        assert_eq!(screen.cursor_row, 1);
        assert_eq!(screen.saved_cursor, Some((3, 2)));

        screen.delete_lines(2);
        assert_eq!(screen_texts(&screen), ["a", "bb", "ccc"]);
        assert_eq!(screen.text_bytes, 6);

        screen.scroll_down(1);
        assert_eq!(screen_texts(&screen), ["", "a", "bb", "ccc"]);
        assert_eq!(screen.text_bytes, 6);
        assert_eq!(screen.cursor_row, 2);

        screen.scroll_up(2);
        assert_eq!(screen_texts(&screen), ["bb", "ccc"]);
        assert_eq!(screen.text_bytes, 5);
        assert_eq!(screen.cursor_row, 0);
        assert_eq!(screen.saved_cursor, Some((1, 2)));
        assert_eq!(screen.removed_front, 2);
    }

    #[test]
    fn large_csi_line_counts_are_capped_per_dispatch() {
        let mut insert_screen = TerminalScreen::default();
        insert_screen.begin_update(2_000, 4096, 1024);
        insert_screen.lines.push_back(TerminalLine::default());
        insert_screen.insert_lines(usize::MAX);
        assert_eq!(
            insert_screen.lines.len(),
            1 + MAX_CSI_LINE_CHANGE_PER_DISPATCH
        );
        assert!(insert_screen.csi_limited);

        let mut delete_screen = TerminalScreen::default();
        delete_screen.begin_update(2_000, 4096, 1024);
        delete_screen
            .lines
            .resize_with(1_500, TerminalLine::default);
        delete_screen.delete_lines(usize::MAX);
        assert_eq!(
            delete_screen.lines.len(),
            1_500 - MAX_CSI_LINE_CHANGE_PER_DISPATCH
        );
        assert!(delete_screen.csi_limited);

        let mut scroll_screen = TerminalScreen::default();
        scroll_screen.begin_update(2_000, 4096, 1024);
        scroll_screen
            .lines
            .resize_with(1_500, TerminalLine::default);
        scroll_screen.scroll_up(usize::MAX);
        assert_eq!(
            scroll_screen.lines.len(),
            1_500 - MAX_CSI_LINE_CHANGE_PER_DISPATCH
        );
        assert!(scroll_screen.csi_limited);

        let mut scroll_down_screen = TerminalScreen::default();
        scroll_down_screen.begin_update(2_000, 4096, 1024);
        scroll_down_screen.lines.push_back(TerminalLine::default());
        scroll_down_screen.scroll_down(usize::MAX);
        assert_eq!(
            scroll_down_screen.lines.len(),
            1 + MAX_CSI_LINE_CHANGE_PER_DISPATCH
        );
        assert!(scroll_down_screen.csi_limited);
    }

    #[test]
    fn repeated_short_csi_line_edits_stop_at_the_update_work_budget() {
        let mut parser = Parser::new();
        let mut screen = screen_with_lines(&["a", "b", "c"], 10, 1);
        screen.csi_line_work_remaining = MIN_CSI_LINE_DISPATCH_WORK;

        parser.advance(&mut screen, b"\x1b[L\x1b[L");

        assert_eq!(screen_texts(&screen), ["a", "", "b", "c"]);
        assert_eq!(screen.csi_line_work_remaining, 0);
        assert!(screen.csi_limited);

        screen.begin_update(10, 4096, 1024);
        parser.advance(&mut screen, b"\x1b[L");
        assert_eq!(screen_texts(&screen), ["a", "", "", "b", "c"]);
    }

    #[test]
    fn batched_csi_line_edits_keep_incremental_updates_consistent() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let mut rows = apply(&mut formatter, &[chunk(0, 1, b"a\r\nb\r\nc")]).rows;

        let inserted = apply(&mut formatter, &[chunk(1, 2, b"\x1b[1A\x1b[L\rX")]);
        assert_eq!(inserted.remove_prefix, 0);
        assert_eq!(inserted.replace_tail, 2);
        rows.truncate(rows.len() - inserted.replace_tail);
        rows.extend(inserted.rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["a", "X", "b", "c"]
        );

        let deleted = apply(&mut formatter, &[chunk(2, 3, b"\x1b[M")]);
        assert_eq!(deleted.remove_prefix, 0);
        assert_eq!(deleted.replace_tail, 3);
        rows.truncate(rows.len() - deleted.replace_tail);
        rows.extend(deleted.rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn row_limit_prunes_the_prefix_and_keeps_incremental_updates_valid() {
        let mut formatter = IncrementalTerminalFormatter::new(TextEncoding::Utf8);
        let mut rows = Vec::new();
        let first = formatter.apply_records(&[chunk(0, 1, b"a\r\nb\r\nc\r\nd")], 3, 4096, 1024);
        rows.extend(first.rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );

        let second = formatter.apply_records(&[chunk(1, 2, b"\r\ne")], 3, 4096, 1024);
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
