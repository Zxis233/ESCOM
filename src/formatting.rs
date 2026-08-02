use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::Path;

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local};
use encoding_rs::{CoderResult, GBK, UTF_8};

use crate::model::{LineEnding, ReceiveMode, SendMode, TextEncoding};
use crate::store::{ReceiveCursor, ReceiveDelta, ReceiveSnapshot, RxChunk};
use crate::terminal::IncrementalTerminalFormatter;

pub const MAX_DISPLAY_ROWS: usize = 100_000;
pub const MAX_DISPLAY_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DISPLAY_LINE_BYTES: usize = 512 * 1024;
pub const MAX_DISPLAY_INCREMENT_BYTES: usize = 128 * 1024;
pub const DEFAULT_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";
const MAX_TEXT_REBUILD_BYTES: usize = 16 * 1024 * 1024;
const HEX_BYTES_PER_ROW: usize = 16;
const EXPORT_BUFFER_BYTES: usize = 64 * 1024;
const EXPORT_DECODE_INPUT_BYTES: usize = 64 * 1024;
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

fn hex_skipped_bytes_modulo(snapshot: &ReceiveSnapshot) -> usize {
    ((snapshot.dropped_bytes % HEX_BYTES_PER_ROW as u64) as usize
        + snapshot.omitted_bytes % HEX_BYTES_PER_ROW)
        % HEX_BYTES_PER_ROW
}

fn hex_first_row_capacity(skipped_bytes_modulo: usize) -> usize {
    let skipped_bytes_modulo = skipped_bytes_modulo % HEX_BYTES_PER_ROW;
    if skipped_bytes_modulo == 0 {
        HEX_BYTES_PER_ROW
    } else {
        HEX_BYTES_PER_ROW - skipped_bytes_modulo
    }
}

pub const fn display_snapshot_limit(mode: ReceiveMode) -> usize {
    match mode {
        ReceiveMode::Text | ReceiveMode::Terminal => MAX_TEXT_REBUILD_BYTES,
        ReceiveMode::Hex => MAX_DISPLAY_ROWS * HEX_BYTES_PER_ROW,
    }
}

#[derive(Clone, Copy)]
struct DisplayLimits {
    max_rows: usize,
    max_text_bytes: usize,
    max_line_bytes: usize,
}

const DEFAULT_DISPLAY_LIMITS: DisplayLimits = DisplayLimits {
    max_rows: MAX_DISPLAY_ROWS,
    max_text_bytes: MAX_DISPLAY_TEXT_BYTES,
    max_line_bytes: MAX_DISPLAY_LINE_BYTES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedRow {
    pub received_at: DateTime<Local>,
    pub text: String,
}

#[derive(Debug)]
pub struct DisplayUpdate {
    pub generation: u64,
    pub remove_prefix: usize,
    pub replace_tail: usize,
    pub rows: Vec<FormattedRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayUpdateError {
    ResetOrGap,
}

pub struct DisplayFormatter {
    mode: ReceiveMode,
    encoding: TextEncoding,
    cursor: ReceiveCursor,
    row_end_sequences: VecDeque<u64>,
    row_text_bytes: VecDeque<usize>,
    text_bytes: usize,
    limited: bool,
    limits: DisplayLimits,
    state: DisplayFormatterState,
}

enum DisplayFormatterState {
    Text(IncrementalTextFormatter),
    Terminal(Box<IncrementalTerminalFormatter>),
    Hex(IncrementalHexFormatter),
}

struct IncrementalTextFormatter {
    decoder: encoding_rs::Decoder,
    current: String,
    line_started_at: Option<DateTime<Local>>,
    current_end_sequence: Option<u64>,
    skip_lf_after_cr: bool,
    partial_visible: bool,
    max_line_bytes: usize,
    line_limited: bool,
}

struct IncrementalHexFormatter {
    current: Vec<u8>,
    current_capacity: usize,
    line_started_at: Option<DateTime<Local>>,
    current_end_sequence: Option<u64>,
    partial_visible: bool,
}

impl DisplayFormatter {
    pub fn rebuild(
        snapshot: &ReceiveSnapshot,
        mode: ReceiveMode,
        encoding: TextEncoding,
    ) -> (Self, Vec<FormattedRow>) {
        Self::rebuild_with_limits(snapshot, mode, encoding, DEFAULT_DISPLAY_LIMITS)
    }

    fn rebuild_with_limits(
        snapshot: &ReceiveSnapshot,
        mode: ReceiveMode,
        encoding: TextEncoding,
        limits: DisplayLimits,
    ) -> (Self, Vec<FormattedRow>) {
        let state = match mode {
            ReceiveMode::Text => DisplayFormatterState::Text(IncrementalTextFormatter::new(
                encoding,
                limits.max_line_bytes,
            )),
            ReceiveMode::Terminal => DisplayFormatterState::Terminal(Box::new(
                IncrementalTerminalFormatter::new(encoding),
            )),
            ReceiveMode::Hex => DisplayFormatterState::Hex(IncrementalHexFormatter::new(
                hex_skipped_bytes_modulo(snapshot),
            )),
        };
        let mut formatter = Self {
            mode,
            encoding,
            cursor: ReceiveCursor {
                stream_id: snapshot.stream_id,
                next_sequence: snapshot.first_sequence,
            },
            row_end_sequences: VecDeque::new(),
            row_text_bytes: VecDeque::new(),
            text_bytes: 0,
            limited: snapshot.omitted_bytes != 0,
            limits,
            state,
        };
        let delta = ReceiveDelta {
            generation: snapshot.generation,
            stream_id: snapshot.stream_id,
            first_sequence: snapshot.first_sequence,
            next_sequence: snapshot.next_sequence,
            chunks: snapshot.chunks.clone(),
            reset_or_gap: false,
        };
        let update = formatter
            .apply_delta(&delta)
            .expect("a snapshot always forms a contiguous display stream");
        (formatter, update.rows)
    }

    pub const fn cursor(&self) -> ReceiveCursor {
        self.cursor
    }

    pub fn is_compatible(&self, mode: ReceiveMode, encoding: TextEncoding) -> bool {
        self.mode == mode && self.encoding == encoding
    }

    pub const fn is_limited(&self) -> bool {
        self.limited
    }

    pub fn terminal_cursor(&self) -> Option<(usize, usize)> {
        match &self.state {
            DisplayFormatterState::Terminal(state) => state.cursor(),
            _ => None,
        }
    }

    pub fn apply_delta(
        &mut self,
        delta: &ReceiveDelta,
    ) -> Result<DisplayUpdate, DisplayUpdateError> {
        if delta.reset_or_gap
            || delta.stream_id != self.cursor.stream_id
            || delta.next_sequence < self.cursor.next_sequence
        {
            return Err(DisplayUpdateError::ResetOrGap);
        }

        if matches!(&self.state, DisplayFormatterState::Terminal(_)) {
            return self.apply_terminal_delta(delta);
        }

        let partial_visible = match &self.state {
            DisplayFormatterState::Text(state) => state.partial_visible,
            DisplayFormatterState::Hex(state) => state.partial_visible,
            DisplayFormatterState::Terminal(_) => false,
        };
        let completed_rows = self
            .row_end_sequences
            .len()
            .saturating_sub(usize::from(partial_visible));
        let mut remove_prefix = 0;
        while remove_prefix < completed_rows
            && self
                .row_end_sequences
                .front()
                .is_some_and(|sequence| *sequence < delta.first_sequence)
        {
            self.row_end_sequences.pop_front();
            if let Some(bytes) = self.row_text_bytes.pop_front() {
                self.text_bytes = self.text_bytes.saturating_sub(bytes);
            }
            remove_prefix += 1;
        }

        let replace_tail = usize::from(partial_visible && !delta.chunks.is_empty());
        if replace_tail != 0 {
            self.row_end_sequences.pop_back();
            if let Some(bytes) = self.row_text_bytes.pop_back() {
                self.text_bytes = self.text_bytes.saturating_sub(bytes);
            }
            match &mut self.state {
                DisplayFormatterState::Text(state) => state.partial_visible = false,
                DisplayFormatterState::Hex(state) => state.partial_visible = false,
                DisplayFormatterState::Terminal(_) => {}
            }
        }

        let mut retained_existing = self.row_end_sequences.len();
        let mut cap_removed_existing = 0;
        let mut removed_new_rows = 0;
        let mut rows = Vec::new();
        if !delta.chunks.is_empty() {
            let Self {
                state,
                row_end_sequences,
                row_text_bytes,
                text_bytes,
                limited,
                limits,
                ..
            } = self;
            match state {
                DisplayFormatterState::Text(state) => {
                    for chunk in &delta.chunks {
                        let new_rows_start = rows.len();
                        state.push_chunk(chunk, &mut rows, row_end_sequences);
                        *limited |= state.line_limited;
                        register_new_rows(&rows[new_rows_start..], row_text_bytes, text_bytes);
                        enforce_display_limits(
                            &mut retained_existing,
                            &mut cap_removed_existing,
                            &mut removed_new_rows,
                            row_end_sequences,
                            row_text_bytes,
                            text_bytes,
                            limited,
                            *limits,
                        );
                    }
                    let new_rows_start = rows.len();
                    state.push_partial_row(&mut rows, row_end_sequences);
                    register_new_rows(&rows[new_rows_start..], row_text_bytes, text_bytes);
                    enforce_display_limits(
                        &mut retained_existing,
                        &mut cap_removed_existing,
                        &mut removed_new_rows,
                        row_end_sequences,
                        row_text_bytes,
                        text_bytes,
                        limited,
                        *limits,
                    );
                }
                DisplayFormatterState::Hex(state) => {
                    for chunk in &delta.chunks {
                        let new_rows_start = rows.len();
                        state.push_chunk(chunk, &mut rows, row_end_sequences);
                        register_new_rows(&rows[new_rows_start..], row_text_bytes, text_bytes);
                        enforce_display_limits(
                            &mut retained_existing,
                            &mut cap_removed_existing,
                            &mut removed_new_rows,
                            row_end_sequences,
                            row_text_bytes,
                            text_bytes,
                            limited,
                            *limits,
                        );
                    }
                    let new_rows_start = rows.len();
                    state.push_partial_row(&mut rows, row_end_sequences);
                    register_new_rows(&rows[new_rows_start..], row_text_bytes, text_bytes);
                    enforce_display_limits(
                        &mut retained_existing,
                        &mut cap_removed_existing,
                        &mut removed_new_rows,
                        row_end_sequences,
                        row_text_bytes,
                        text_bytes,
                        limited,
                        *limits,
                    );
                }
                DisplayFormatterState::Terminal(_) => {
                    unreachable!("terminal deltas are handled before append-only formatting")
                }
            }
        }

        if removed_new_rows != 0 {
            rows.drain(..removed_new_rows.min(rows.len()));
        }
        remove_prefix += cap_removed_existing;

        self.cursor = ReceiveCursor {
            stream_id: delta.stream_id,
            next_sequence: delta.next_sequence,
        };
        Ok(DisplayUpdate {
            generation: delta.generation,
            remove_prefix,
            replace_tail,
            rows,
        })
    }

    fn apply_terminal_delta(
        &mut self,
        delta: &ReceiveDelta,
    ) -> Result<DisplayUpdate, DisplayUpdateError> {
        let DisplayFormatterState::Terminal(state) = &mut self.state else {
            unreachable!("terminal update requires terminal formatter state");
        };
        let update = state.apply_chunks(
            &delta.chunks,
            self.limits.max_rows,
            self.limits.max_text_bytes,
            self.limits.max_line_bytes,
        );
        self.limited |= state.is_limited();
        self.cursor = ReceiveCursor {
            stream_id: delta.stream_id,
            next_sequence: delta.next_sequence,
        };

        Ok(DisplayUpdate {
            generation: delta.generation,
            remove_prefix: update.remove_prefix,
            replace_tail: update.replace_tail,
            rows: update
                .rows
                .into_iter()
                .map(|row| FormattedRow {
                    received_at: row.received_at,
                    text: row.text,
                })
                .collect(),
        })
    }
}

fn register_new_rows(
    rows: &[FormattedRow],
    row_text_bytes: &mut VecDeque<usize>,
    text_bytes: &mut usize,
) {
    for row in rows {
        let bytes = row.text.len();
        row_text_bytes.push_back(bytes);
        *text_bytes = text_bytes.saturating_add(bytes);
    }
}

#[allow(clippy::too_many_arguments)]
fn enforce_display_limits(
    retained_existing: &mut usize,
    removed_existing: &mut usize,
    removed_new: &mut usize,
    row_end_sequences: &mut VecDeque<u64>,
    row_text_bytes: &mut VecDeque<usize>,
    text_bytes: &mut usize,
    limited: &mut bool,
    limits: DisplayLimits,
) {
    while row_text_bytes.len() > limits.max_rows || *text_bytes > limits.max_text_bytes {
        let Some(bytes) = row_text_bytes.pop_front() else {
            break;
        };
        row_end_sequences.pop_front();
        *text_bytes = text_bytes.saturating_sub(bytes);
        if *retained_existing != 0 {
            *retained_existing -= 1;
            *removed_existing += 1;
        } else {
            *removed_new += 1;
        }
        *limited = true;
    }
}

impl IncrementalTextFormatter {
    fn new(encoding: TextEncoding, max_line_bytes: usize) -> Self {
        let selected_encoding = match encoding {
            TextEncoding::Utf8 => UTF_8,
            TextEncoding::Gbk => GBK,
        };
        Self {
            decoder: selected_encoding.new_decoder_without_bom_handling(),
            current: String::new(),
            line_started_at: None,
            current_end_sequence: None,
            skip_lf_after_cr: false,
            partial_visible: false,
            max_line_bytes: max_line_bytes.max(16),
            line_limited: false,
        }
    }

    fn push_chunk(
        &mut self,
        chunk: &RxChunk,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        if !chunk.bytes.is_empty() && self.line_started_at.is_none() && !self.skip_lf_after_cr {
            self.line_started_at = Some(chunk.received_at);
        }

        let mut input = &*chunk.bytes;
        loop {
            let capacity = input.len().saturating_mul(3).max(32);
            let mut decoded = String::with_capacity(capacity);
            let (result, read, _) = self.decoder.decode_to_string(input, &mut decoded, false);
            self.push_decoded(
                &decoded,
                chunk.received_at,
                chunk.sequence,
                rows,
                row_end_sequences,
            );
            input = &input[read..];
            match result {
                CoderResult::InputEmpty => break,
                CoderResult::OutputFull => continue,
            }
        }
    }

    fn push_decoded(
        &mut self,
        text: &str,
        received_at: DateTime<Local>,
        sequence: u64,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        for character in text.chars() {
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if character == '\n' {
                    continue;
                }
            }

            match character {
                '\r' => {
                    self.push_completed_row(received_at, sequence, rows, row_end_sequences);
                    self.skip_lf_after_cr = true;
                }
                '\n' => {
                    self.push_completed_row(received_at, sequence, rows, row_end_sequences);
                }
                _ => {
                    self.line_started_at.get_or_insert(received_at);
                    self.current.push(character);
                    self.current_end_sequence = Some(sequence);
                }
            }
        }
        self.trim_current_line(received_at);
    }

    fn trim_current_line(&mut self, received_at: DateTime<Local>) {
        if self.current.len() <= self.max_line_bytes {
            return;
        }

        let retain_bytes = (self.max_line_bytes / 2).max(1);
        let mut retain_from = self.current.len().saturating_sub(retain_bytes);
        while retain_from < self.current.len() && !self.current.is_char_boundary(retain_from) {
            retain_from += 1;
        }
        self.current.drain(..retain_from);
        self.current.insert(0, '…');
        self.line_started_at = Some(received_at);
        self.line_limited = true;
    }

    fn push_completed_row(
        &mut self,
        fallback_time: DateTime<Local>,
        sequence: u64,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        rows.push(FormattedRow {
            received_at: self.line_started_at.unwrap_or(fallback_time),
            text: std::mem::take(&mut self.current),
        });
        row_end_sequences.push_back(sequence);
        self.line_started_at = None;
        self.current_end_sequence = None;
    }

    fn push_partial_row(
        &mut self,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        if self.current.is_empty() {
            self.partial_visible = false;
            return;
        }

        rows.push(FormattedRow {
            received_at: self.line_started_at.unwrap_or_else(Local::now),
            text: self.current.clone(),
        });
        row_end_sequences.push_back(
            self.current_end_sequence
                .expect("visible text must originate from a receive chunk"),
        );
        self.partial_visible = true;
    }
}

impl IncrementalHexFormatter {
    fn new(skipped_bytes_modulo: usize) -> Self {
        Self {
            current: Vec::with_capacity(HEX_BYTES_PER_ROW),
            current_capacity: hex_first_row_capacity(skipped_bytes_modulo),
            line_started_at: None,
            current_end_sequence: None,
            partial_visible: false,
        }
    }

    fn push_chunk(
        &mut self,
        chunk: &RxChunk,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        let mut bytes = &*chunk.bytes;
        while !bytes.is_empty() {
            if self.current.is_empty() {
                self.line_started_at = Some(chunk.received_at);
            }
            let take = bytes
                .len()
                .min(self.current_capacity.saturating_sub(self.current.len()));
            self.current.extend_from_slice(&bytes[..take]);
            self.current_end_sequence = Some(chunk.sequence);
            bytes = &bytes[take..];

            if self.current.len() == self.current_capacity {
                self.push_completed_row(chunk.received_at, chunk.sequence, rows, row_end_sequences);
            }
        }
    }

    fn push_completed_row(
        &mut self,
        fallback_time: DateTime<Local>,
        sequence: u64,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        rows.push(FormattedRow {
            received_at: self.line_started_at.unwrap_or(fallback_time),
            text: format_hex_row(&self.current),
        });
        row_end_sequences.push_back(sequence);
        self.current.clear();
        self.current_capacity = HEX_BYTES_PER_ROW;
        self.line_started_at = None;
        self.current_end_sequence = None;
    }

    fn push_partial_row(
        &mut self,
        rows: &mut Vec<FormattedRow>,
        row_end_sequences: &mut VecDeque<u64>,
    ) {
        if self.current.is_empty() {
            self.partial_visible = false;
            return;
        }

        rows.push(FormattedRow {
            received_at: self.line_started_at.unwrap_or_else(Local::now),
            text: format_hex_row(&self.current),
        });
        row_end_sequences.push_back(
            self.current_end_sequence
                .expect("visible HEX bytes must originate from a receive chunk"),
        );
        self.partial_visible = true;
    }
}

fn format_hex_row(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(3).saturating_sub(1));
    for (index, byte) in bytes.iter().enumerate() {
        if index != 0 {
            text.push(' ');
        }
        let _ = write!(text, "{byte:02X}");
    }
    text
}

pub fn parse_send_input(
    input: &str,
    mode: SendMode,
    encoding: TextEncoding,
    line_ending: LineEnding,
) -> Result<Vec<u8>, String> {
    match mode {
        SendMode::Text => encode_text(input, encoding, line_ending),
        SendMode::Hex => parse_hex(input),
    }
}

pub fn parse_hex(input: &str) -> Result<Vec<u8>, String> {
    let compact: String = input
        .split_whitespace()
        .map(|token| {
            token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or(token)
        })
        .collect();

    if compact.is_empty() {
        return Err("请输入要发送的 HEX 数据".into());
    }
    if !compact.len().is_multiple_of(2) {
        return Err("HEX 字符数量必须为偶数".into());
    }
    if !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("HEX 数据只能包含 0-9、A-F 和空格".into());
    }

    compact
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let token = std::str::from_utf8(pair).expect("ASCII hex was validated");
            u8::from_str_radix(token, 16).map_err(|_| "HEX 数据格式无效".to_owned())
        })
        .collect()
}

pub fn encode_text(
    input: &str,
    encoding: TextEncoding,
    line_ending: LineEnding,
) -> Result<Vec<u8>, String> {
    let mut bytes = match encoding {
        TextEncoding::Utf8 => input.as_bytes().to_vec(),
        TextEncoding::Gbk => {
            let (encoded, _, had_errors) = GBK.encode(input);
            if had_errors {
                return Err("文本包含 GBK 无法表示的字符".into());
            }
            encoded.into_owned()
        }
    };
    bytes.extend_from_slice(line_ending.bytes());

    if bytes.is_empty() {
        return Err("请输入要发送的数据".into());
    }
    Ok(bytes)
}

pub fn format_snapshot(
    snapshot: &ReceiveSnapshot,
    mode: ReceiveMode,
    encoding: TextEncoding,
) -> Vec<FormattedRow> {
    match mode {
        ReceiveMode::Text => format_text(snapshot, encoding),
        ReceiveMode::Terminal => DisplayFormatter::rebuild(snapshot, mode, encoding).1,
        ReceiveMode::Hex => format_hex(snapshot),
    }
}

pub fn export_snapshot_to_file(
    path: &Path,
    snapshot: ReceiveSnapshot,
    mode: ReceiveMode,
    encoding: TextEncoding,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(EXPORT_BUFFER_BYTES, file);
    write_export(
        &mut writer,
        snapshot,
        mode,
        encoding,
        timestamps,
        timestamp_format,
    )?;
    writer.flush()
}

pub fn write_export<W: io::Write + ?Sized>(
    writer: &mut W,
    snapshot: ReceiveSnapshot,
    mode: ReceiveMode,
    encoding: TextEncoding,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    writer.write_all(&UTF8_BOM)?;
    match mode {
        ReceiveMode::Text => {
            write_text_export(writer, snapshot, encoding, timestamps, timestamp_format)
        }
        ReceiveMode::Terminal => {
            write_terminal_export(writer, snapshot, encoding, timestamps, timestamp_format)
        }
        ReceiveMode::Hex => write_hex_export(writer, snapshot, timestamps, timestamp_format),
    }
}

pub fn is_valid_timestamp_format(timestamp_format: &str) -> bool {
    !timestamp_format.is_empty()
        && !StrftimeItems::new(timestamp_format).any(|item| matches!(item, Item::Error))
}

pub fn format_timestamp(received_at: DateTime<Local>, timestamp_format: &str) -> String {
    let timestamp_format = if is_valid_timestamp_format(timestamp_format) {
        timestamp_format
    } else {
        DEFAULT_TIMESTAMP_FORMAT
    };
    received_at.format(timestamp_format).to_string()
}

pub fn timestamp_prefix(received_at: DateTime<Local>, timestamp_format: &str) -> String {
    format!("[{}] ", format_timestamp(received_at, timestamp_format))
}

pub fn display_text(row: &FormattedRow, timestamps: bool, timestamp_format: &str) -> String {
    if timestamps {
        format!(
            "{}{}",
            timestamp_prefix(row.received_at, timestamp_format),
            row.text
        )
    } else {
        row.text.clone()
    }
}

fn format_hex(snapshot: &ReceiveSnapshot) -> Vec<FormattedRow> {
    let mut state = IncrementalHexFormatter::new(hex_skipped_bytes_modulo(snapshot));
    let mut rows = Vec::new();
    let mut row_end_sequences = VecDeque::new();
    for chunk in &snapshot.chunks {
        state.push_chunk(chunk, &mut rows, &mut row_end_sequences);
    }
    state.push_partial_row(&mut rows, &mut row_end_sequences);
    rows
}

fn format_text(snapshot: &ReceiveSnapshot, encoding: TextEncoding) -> Vec<FormattedRow> {
    let selected_encoding = match encoding {
        TextEncoding::Utf8 => UTF_8,
        TextEncoding::Gbk => GBK,
    };
    let mut decoder = selected_encoding.new_decoder_without_bom_handling();
    let mut builder = TextRowsBuilder::default();

    for chunk in &snapshot.chunks {
        if !chunk.bytes.is_empty() && builder.line_started_at.is_none() && !builder.skip_lf_after_cr
        {
            builder.line_started_at = Some(chunk.received_at);
        }
        decode_piece(
            &mut decoder,
            &chunk.bytes,
            false,
            chunk.received_at,
            &mut builder,
        );
    }

    let flush_time = snapshot
        .chunks
        .last()
        .map(|chunk| chunk.received_at)
        .unwrap_or_else(Local::now);
    decode_piece(&mut decoder, &[], true, flush_time, &mut builder);
    builder.finish()
}

fn write_text_export<W: io::Write + ?Sized>(
    writer: &mut W,
    snapshot: ReceiveSnapshot,
    encoding: TextEncoding,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    let selected_encoding = match encoding {
        TextEncoding::Utf8 => UTF_8,
        TextEncoding::Gbk => GBK,
    };
    let mut decoder = selected_encoding.new_decoder_without_bom_handling();
    let mut decoded = String::with_capacity(EXPORT_DECODE_INPUT_BYTES * 3);
    let mut exporter = StreamingTextExporter::new(writer, timestamps, timestamp_format);

    let flush_time = snapshot
        .chunks
        .last()
        .map(|chunk| chunk.received_at)
        .unwrap_or_else(Local::now);
    for chunk in snapshot.chunks {
        if !chunk.bytes.is_empty() {
            exporter.note_input(chunk.received_at);
        }
        for input in chunk.bytes.chunks(EXPORT_DECODE_INPUT_BYTES) {
            decode_export_piece(
                &mut decoder,
                input,
                false,
                chunk.received_at,
                &mut decoded,
                &mut exporter,
            )?;
        }
    }

    decode_export_piece(
        &mut decoder,
        &[],
        true,
        flush_time,
        &mut decoded,
        &mut exporter,
    )?;
    exporter.finish()
}

fn write_terminal_export<W: io::Write + ?Sized>(
    writer: &mut W,
    snapshot: ReceiveSnapshot,
    encoding: TextEncoding,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    // Reuse the display formatter so cursor movement, erasing, overwriting, encoding and display
    // limits have exactly the same semantics as the terminal surface. Release the raw snapshot
    // before writing the bounded rendered rows.
    let (_, rows) = DisplayFormatter::rebuild(&snapshot, ReceiveMode::Terminal, encoding);
    drop(snapshot);

    for row in rows {
        write_export_timestamp(writer, row.received_at, timestamps, timestamp_format)?;
        for bytes in row.text.as_bytes().chunks(EXPORT_DECODE_INPUT_BYTES) {
            writer.write_all(bytes)?;
        }
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

fn decode_export_piece<W: io::Write + ?Sized>(
    decoder: &mut encoding_rs::Decoder,
    mut input: &[u8],
    last: bool,
    received_at: DateTime<Local>,
    decoded: &mut String,
    exporter: &mut StreamingTextExporter<'_, W>,
) -> io::Result<()> {
    loop {
        let capacity = input.len().saturating_mul(3).max(32);
        decoded.clear();
        if decoded.capacity() < capacity {
            decoded.reserve(capacity);
        }
        let (result, read, _) = decoder.decode_to_string(input, decoded, last);
        exporter.push_decoded(decoded, received_at)?;
        input = &input[read..];

        match result {
            CoderResult::InputEmpty => return Ok(()),
            CoderResult::OutputFull => {}
        }
    }
}

struct StreamingTextExporter<'a, W: io::Write + ?Sized> {
    writer: &'a mut W,
    timestamps: bool,
    timestamp_format: &'a str,
    line_open: bool,
    line_started_at: Option<DateTime<Local>>,
    skip_lf_after_cr: bool,
}

impl<'a, W: io::Write + ?Sized> StreamingTextExporter<'a, W> {
    fn new(writer: &'a mut W, timestamps: bool, timestamp_format: &'a str) -> Self {
        Self {
            writer,
            timestamps,
            timestamp_format,
            line_open: false,
            line_started_at: None,
            skip_lf_after_cr: false,
        }
    }

    fn note_input(&mut self, received_at: DateTime<Local>) {
        if !self.line_open && self.line_started_at.is_none() && !self.skip_lf_after_cr {
            self.line_started_at = Some(received_at);
        }
    }

    fn push_decoded(&mut self, text: &str, received_at: DateTime<Local>) -> io::Result<()> {
        let bytes = text.as_bytes();
        let mut cursor = 0;
        if self.skip_lf_after_cr && !bytes.is_empty() {
            self.skip_lf_after_cr = false;
            if bytes[0] == b'\n' {
                cursor = 1;
            }
        }

        let mut segment_start = cursor;
        while cursor < bytes.len() {
            let newline = bytes[cursor];
            if newline != b'\r' && newline != b'\n' {
                cursor += 1;
                continue;
            }

            self.write_segment(&text[segment_start..cursor], received_at)?;
            self.finish_row(received_at)?;
            cursor += 1;
            if newline == b'\r' {
                if cursor == bytes.len() {
                    self.skip_lf_after_cr = true;
                } else if bytes[cursor] == b'\n' {
                    cursor += 1;
                }
            }
            segment_start = cursor;
        }

        self.write_segment(&text[segment_start..], received_at)
    }

    fn write_segment(&mut self, text: &str, received_at: DateTime<Local>) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.start_row(received_at)?;
        self.writer.write_all(text.as_bytes())
    }

    fn start_row(&mut self, received_at: DateTime<Local>) -> io::Result<()> {
        if self.line_open {
            return Ok(());
        }
        if self.timestamps {
            let timestamp = self.line_started_at.unwrap_or(received_at);
            self.writer
                .write_all(timestamp_prefix(timestamp, self.timestamp_format).as_bytes())?;
        }
        self.line_open = true;
        Ok(())
    }

    fn finish_row(&mut self, received_at: DateTime<Local>) -> io::Result<()> {
        self.start_row(received_at)?;
        self.writer.write_all(b"\r\n")?;
        self.line_open = false;
        self.line_started_at = None;
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if self.line_open {
            self.writer.write_all(b"\r\n")?;
            self.line_open = false;
        }
        Ok(())
    }
}

fn write_hex_export<W: io::Write + ?Sized>(
    writer: &mut W,
    snapshot: ReceiveSnapshot,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    let mut row_capacity = hex_first_row_capacity(hex_skipped_bytes_modulo(&snapshot));
    let mut row = [0_u8; HEX_BYTES_PER_ROW];
    let mut row_len = 0;
    let mut row_started_at = None;

    for chunk in snapshot.chunks {
        for byte in chunk.bytes.iter().copied() {
            if row_len == 0 {
                row_started_at = Some(chunk.received_at);
            }
            row[row_len] = byte;
            row_len += 1;
            if row_len == row_capacity {
                write_hex_export_row(
                    writer,
                    &row[..row_len],
                    row_started_at.unwrap_or(chunk.received_at),
                    timestamps,
                    timestamp_format,
                )?;
                row_len = 0;
                row_capacity = HEX_BYTES_PER_ROW;
                row_started_at = None;
            }
        }
    }
    if row_len != 0 {
        write_hex_export_row(
            writer,
            &row[..row_len],
            row_started_at.unwrap_or_else(Local::now),
            timestamps,
            timestamp_format,
        )?;
    }
    Ok(())
}

fn write_hex_export_row<W: io::Write + ?Sized>(
    writer: &mut W,
    bytes: &[u8],
    received_at: DateTime<Local>,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    write_export_timestamp(writer, received_at, timestamps, timestamp_format)?;
    let mut encoded = [0_u8; HEX_BYTES_PER_ROW * 3 - 1];
    for (index, byte) in bytes.iter().copied().enumerate() {
        let offset = index * 3;
        if index != 0 {
            encoded[offset - 1] = b' ';
        }
        encoded[offset] = HEX_DIGITS[usize::from(byte >> 4)];
        encoded[offset + 1] = HEX_DIGITS[usize::from(byte & 0x0F)];
    }
    writer.write_all(&encoded[..bytes.len() * 3 - 1])?;
    writer.write_all(b"\r\n")
}

fn write_export_timestamp<W: io::Write + ?Sized>(
    writer: &mut W,
    received_at: DateTime<Local>,
    timestamps: bool,
    timestamp_format: &str,
) -> io::Result<()> {
    if timestamps {
        writer.write_all(timestamp_prefix(received_at, timestamp_format).as_bytes())?;
    }
    Ok(())
}

fn decode_piece(
    decoder: &mut encoding_rs::Decoder,
    mut input: &[u8],
    last: bool,
    received_at: DateTime<Local>,
    builder: &mut TextRowsBuilder,
) {
    loop {
        let capacity = input.len().saturating_mul(3).max(32);
        let mut decoded = String::with_capacity(capacity);
        let (result, read, _) = decoder.decode_to_string(input, &mut decoded, last);
        builder.push_decoded(&decoded, received_at);
        input = &input[read..];

        match result {
            CoderResult::InputEmpty => break,
            CoderResult::OutputFull => continue,
        }
    }
}

#[derive(Default)]
struct TextRowsBuilder {
    rows: Vec<FormattedRow>,
    current: String,
    line_started_at: Option<DateTime<Local>>,
    skip_lf_after_cr: bool,
}

impl TextRowsBuilder {
    fn push_decoded(&mut self, text: &str, received_at: DateTime<Local>) {
        for character in text.chars() {
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if character == '\n' {
                    continue;
                }
            }

            match character {
                '\r' => {
                    self.push_row(received_at);
                    self.skip_lf_after_cr = true;
                }
                '\n' => self.push_row(received_at),
                _ => {
                    self.line_started_at.get_or_insert(received_at);
                    self.current.push(character);
                }
            }
        }
    }

    fn push_row(&mut self, fallback_time: DateTime<Local>) {
        self.rows.push(FormattedRow {
            received_at: self.line_started_at.unwrap_or(fallback_time),
            text: std::mem::take(&mut self.current),
        });
        self.line_started_at = None;
    }

    fn finish(mut self) -> Vec<FormattedRow> {
        if !self.current.is_empty() {
            self.rows.push(FormattedRow {
                received_at: self.line_started_at.unwrap_or_else(Local::now),
                text: self.current,
            });
        }
        self.rows
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::store::ReceiveStore;

    fn timestamp(second: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 31, 12, 0, second)
            .single()
            .unwrap()
    }

    fn apply_display_update(rows: &mut Vec<FormattedRow>, update: DisplayUpdate) {
        rows.drain(..update.remove_prefix);
        rows.truncate(rows.len().saturating_sub(update.replace_tail));
        rows.extend(update.rows);
    }

    #[test]
    fn parses_contiguous_spaced_and_prefixed_hex() {
        assert_eq!(parse_hex("0A10ff").unwrap(), [0x0A, 0x10, 0xFF]);
        assert_eq!(parse_hex("0x0A 10 FF").unwrap(), [0x0A, 0x10, 0xFF]);
        assert!(parse_hex("ABC").is_err());
        assert!(parse_hex("GG").is_err());
    }

    #[test]
    fn text_formatter_handles_split_utf8_and_crlf() {
        let mut store = ReceiveStore::new(1024);
        let bytes = "第一行\r\n第二行".as_bytes();
        store.append(timestamp(1), bytes[..4].to_vec());
        store.append(timestamp(2), bytes[4..10].to_vec());
        store.append(timestamp(3), bytes[10..].to_vec());

        let rows = format_snapshot(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "第一行");
        assert_eq!(rows[1].text, "第二行");
        assert_eq!(rows[0].received_at, timestamp(1));
    }

    #[test]
    fn split_crlf_does_not_timestamp_next_line_from_lf_chunk() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"first\r".to_vec());
        store.append(timestamp(2), b"\n".to_vec());
        store.append(timestamp(3), b"second".to_vec());

        let rows = format_snapshot(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].text, "second");
        assert_eq!(rows[1].received_at, timestamp(3));
    }

    #[test]
    fn text_formatter_handles_split_gbk_character() {
        let (encoded, _, _) = GBK.encode("中文\n完成");
        let bytes = encoded.into_owned();
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), bytes[..1].to_vec());
        store.append(timestamp(2), bytes[1..3].to_vec());
        store.append(timestamp(3), bytes[3..].to_vec());

        let rows = format_snapshot(&store.snapshot(), ReceiveMode::Text, TextEncoding::Gbk);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["中文", "完成"]
        );
        assert_eq!(rows[0].received_at, timestamp(1));
    }

    #[test]
    fn terminal_mode_uses_streaming_text_formatter() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"ready> ".to_vec());
        store.append(timestamp(2), b"ok\r\n".to_vec());

        let rows = format_snapshot(&store.snapshot(), ReceiveMode::Terminal, TextEncoding::Utf8);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "ready> ok");
        assert_eq!(rows[0].received_at, timestamp(1));
    }

    #[test]
    fn incremental_formatter_matches_complete_utf8_snapshot() {
        let mut store = ReceiveStore::new(1024);
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);
        let bytes = "第一行\r\n第二行".as_bytes();
        for (index, part) in [&bytes[..4], &bytes[4..10], &bytes[10..]]
            .into_iter()
            .enumerate()
        {
            store.append(timestamp(index as u32 + 1), part.to_vec());
            let delta = store.delta_since(formatter.cursor());
            apply_display_update(&mut rows, formatter.apply_delta(&delta).unwrap());
        }

        assert_eq!(
            rows,
            format_snapshot(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8)
        );
    }

    #[test]
    fn incremental_formatter_replaces_visible_partial_row() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"ready".to_vec());
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Terminal, TextEncoding::Utf8);
        assert_eq!(rows[0].text, "ready");

        store.append(timestamp(2), b"> ok\r\n".to_vec());
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.replace_tail, 1);
        apply_display_update(&mut rows, update);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "ready> ok");
    }

    #[test]
    fn incremental_formatter_handles_split_gbk_character() {
        let (encoded, _, _) = GBK.encode("中文\n完成");
        let bytes = encoded.into_owned();
        let mut store = ReceiveStore::new(1024);
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Text, TextEncoding::Gbk);
        for (index, part) in [&bytes[..1], &bytes[1..3], &bytes[3..]]
            .into_iter()
            .enumerate()
        {
            store.append(timestamp(index as u32 + 1), part.to_vec());
            let delta = store.delta_since(formatter.cursor());
            apply_display_update(&mut rows, formatter.apply_delta(&delta).unwrap());
        }

        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["中文", "完成"]
        );
    }

    #[test]
    fn hex_rebuild_is_independent_of_receive_chunk_boundaries() {
        let bytes: Vec<u8> = (0..=32).collect();
        let mut single_chunk = ReceiveStore::new(1024);
        single_chunk.append(timestamp(1), bytes.clone());
        let mut fragmented = ReceiveStore::new(1024);
        for part in [&bytes[..1], &bytes[1..8], &bytes[8..17], &bytes[17..]] {
            fragmented.append(timestamp(1), part.to_vec());
        }

        let single_rows = format_snapshot(
            &single_chunk.snapshot(),
            ReceiveMode::Hex,
            TextEncoding::Utf8,
        );
        let fragmented_rows =
            format_snapshot(&fragmented.snapshot(), ReceiveMode::Hex, TextEncoding::Utf8);

        assert_eq!(fragmented_rows, single_rows);
        assert_eq!(
            single_rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            [
                "00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F",
                "10 11 12 13 14 15 16 17 18 19 1A 1B 1C 1D 1E 1F",
                "20",
            ]
        );
    }

    #[test]
    fn incremental_hex_formatter_replaces_and_completes_partial_rows() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), (0..5).collect());
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Hex, TextEncoding::Utf8);
        assert_eq!(rows[0].text, "00 01 02 03 04");

        store.append(timestamp(2), (5..16).collect());
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.replace_tail, 1);
        assert_eq!(update.rows[0].received_at, timestamp(1));
        apply_display_update(&mut rows, update);
        assert_eq!(
            rows[0].text,
            "00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F"
        );

        store.append(timestamp(3), vec![0x10, 0x11]);
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.replace_tail, 0);
        apply_display_update(&mut rows, update);
        assert_eq!(rows[1].text, "10 11");

        store.append(timestamp(4), (0x12..0x20).collect());
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.replace_tail, 1);
        assert_eq!(update.rows[0].received_at, timestamp(3));
        apply_display_update(&mut rows, update);

        assert_eq!(
            rows,
            format_snapshot(&store.snapshot(), ReceiveMode::Hex, TextEncoding::Utf8)
        );
    }

    #[test]
    fn hex_tail_snapshot_preserves_the_original_sixteen_byte_alignment() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), (0..20).collect());
        let snapshot = store.tail_snapshot(18);
        let rows = format_snapshot(&snapshot, ReceiveMode::Hex, TextEncoding::Utf8);

        let mut evicted_store = ReceiveStore::new(18);
        evicted_store.append(timestamp(1), vec![0, 1]);
        evicted_store.append(timestamp(1), (2..20).collect());
        let evicted_snapshot = evicted_store.snapshot();
        let evicted_rows = format_snapshot(&evicted_snapshot, ReceiveMode::Hex, TextEncoding::Utf8);

        assert_eq!(snapshot.omitted_bytes, 2);
        assert_eq!(evicted_snapshot.dropped_bytes, 2);
        assert_eq!(evicted_rows, rows);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F", "10 11 12 13",]
        );
    }

    #[test]
    fn incremental_formatter_prunes_rows_from_evicted_chunks() {
        let mut store = ReceiveStore::new(6);
        store.append(timestamp(1), b"a\n".to_vec());
        store.append(timestamp(2), b"b\n".to_vec());
        store.append(timestamp(3), b"c\n".to_vec());
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);

        store.set_limit(4);
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.remove_prefix, 1);
        apply_display_update(&mut rows, update);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn eviction_without_new_chunks_does_not_duplicate_partial_row() {
        let mut store = ReceiveStore::new(6);
        store.append(timestamp(1), b"a\n".to_vec());
        store.append(timestamp(2), b"tail".to_vec());
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);

        store.set_limit(4);
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.remove_prefix, 1);
        assert_eq!(update.replace_tail, 0);
        assert!(update.rows.is_empty());
        apply_display_update(&mut rows, update);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "tail");
    }

    #[test]
    fn incremental_formatter_waits_for_incomplete_multibyte_character() {
        let bytes = "中".as_bytes();
        let mut store = ReceiveStore::new(1024);
        let (mut formatter, mut rows) =
            DisplayFormatter::rebuild(&store.snapshot(), ReceiveMode::Text, TextEncoding::Utf8);
        store.append(timestamp(1), bytes[..1].to_vec());
        let delta = store.delta_since(formatter.cursor());
        apply_display_update(&mut rows, formatter.apply_delta(&delta).unwrap());
        assert!(rows.is_empty());

        store.append(timestamp(2), bytes[1..].to_vec());
        let delta = store.delta_since(formatter.cursor());
        apply_display_update(&mut rows, formatter.apply_delta(&delta).unwrap());
        assert_eq!(rows[0].text, "中");
        assert_eq!(rows[0].received_at, timestamp(1));
    }

    #[test]
    fn display_formatter_limits_rows_during_rebuild() {
        let mut store = ReceiveStore::new(1024);
        for (index, row) in [b"a\n", b"b\n", b"c\n", b"d\n"].into_iter().enumerate() {
            store.append(timestamp(index as u32 + 1), row.to_vec());
        }
        let limits = DisplayLimits {
            max_rows: 3,
            max_text_bytes: 1024,
            max_line_bytes: 128,
        };

        let (formatter, rows) = DisplayFormatter::rebuild_with_limits(
            &store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            limits,
        );

        assert!(formatter.is_limited());
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );
    }

    #[test]
    fn display_formatter_limits_total_text_bytes() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"aaaa\nbbbb\n".to_vec());
        let limits = DisplayLimits {
            max_rows: 10,
            max_text_bytes: 6,
            max_line_bytes: 128,
        };

        let (formatter, rows) = DisplayFormatter::rebuild_with_limits(
            &store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            limits,
        );

        assert!(formatter.is_limited());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "bbbb");
    }

    #[test]
    fn display_formatter_bounds_a_line_without_newlines() {
        let mut store = ReceiveStore::new(1024);
        store.append(
            timestamp(1),
            b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec(),
        );
        let limits = DisplayLimits {
            max_rows: 10,
            max_text_bytes: 1024,
            max_line_bytes: 16,
        };

        let (formatter, rows) = DisplayFormatter::rebuild_with_limits(
            &store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            limits,
        );

        assert!(formatter.is_limited());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].text.starts_with('…'));
        assert!(rows[0].text.len() <= 16);
        assert!(rows[0].text.ends_with("stuvwxyz"));
    }

    #[test]
    fn incremental_display_limit_prunes_existing_rows() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"a\nb\nc\n".to_vec());
        let limits = DisplayLimits {
            max_rows: 3,
            max_text_bytes: 1024,
            max_line_bytes: 128,
        };
        let (mut formatter, mut rows) = DisplayFormatter::rebuild_with_limits(
            &store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            limits,
        );

        store.append(timestamp(2), b"d\n".to_vec());
        let update = formatter
            .apply_delta(&store.delta_since(formatter.cursor()))
            .unwrap();
        assert_eq!(update.remove_prefix, 1);
        apply_display_update(&mut rows, update);

        assert!(formatter.is_limited());
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );
    }

    #[test]
    fn default_hex_display_window_never_exceeds_the_row_limit() {
        let mut store = ReceiveStore::new(4 * 1024 * 1024);
        store.append(
            timestamp(1),
            vec![0xA5; (MAX_DISPLAY_ROWS + 128) * HEX_BYTES_PER_ROW],
        );
        let snapshot = store.tail_snapshot(display_snapshot_limit(ReceiveMode::Hex));

        let (formatter, rows) =
            DisplayFormatter::rebuild(&snapshot, ReceiveMode::Hex, TextEncoding::Utf8);

        assert!(formatter.is_limited());
        assert_eq!(rows.len(), MAX_DISPLAY_ROWS);
        assert!(rows.iter().map(|row| row.text.len()).sum::<usize>() <= MAX_DISPLAY_TEXT_BYTES);
    }

    #[test]
    fn export_is_utf8_bom_and_crlf() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), "数据".as_bytes().to_vec());
        let mut bytes = Vec::new();

        write_export(
            &mut bytes,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert_eq!(std::str::from_utf8(&bytes[3..]).unwrap(), "数据\r\n");
    }

    #[test]
    fn timestamp_format_is_validated_and_applied_to_display_text() {
        let row = FormattedRow {
            received_at: timestamp(1),
            text: "ready".to_owned(),
        };

        assert!(is_valid_timestamp_format(DEFAULT_TIMESTAMP_FORMAT));
        assert!(is_valid_timestamp_format("%H:%M:%S"));
        assert!(!is_valid_timestamp_format(""));
        assert!(!is_valid_timestamp_format("%Q"));
        assert_eq!(display_text(&row, true, "%H:%M:%S"), "[12:00:01] ready");
        assert_eq!(
            format_timestamp(row.received_at, "%Q"),
            "2026-07-31 12:00:01.000"
        );
    }

    #[test]
    fn streaming_text_export_handles_split_utf8_and_crlf() {
        let mut store = ReceiveStore::new(1024);
        let input = "第一行\r\n第二行".as_bytes();
        store.append(timestamp(1), input[..4].to_vec());
        store.append(timestamp(2), input[4..10].to_vec());
        store.append(timestamp(3), input[10..].to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "第一行\r\n第二行\r\n"
        );
    }

    #[test]
    fn streaming_text_export_timestamps_a_split_character_from_its_first_byte() {
        let mut store = ReceiveStore::new(1024);
        let input = "中".as_bytes();
        store.append(timestamp(1), input[..1].to_vec());
        store.append(timestamp(2), input[1..].to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            true,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "[2026-07-31 12:00:01.000] 中\r\n"
        );
    }

    #[test]
    fn streaming_text_export_preserves_line_and_blank_line_timestamps() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"first".to_vec());
        store.append(timestamp(2), b" line\r".to_vec());
        store.append(timestamp(3), b"\n\nlast".to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            true,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            concat!(
                "[2026-07-31 12:00:01.000] first line\r\n",
                "[2026-07-31 12:00:03.000] \r\n",
                "[2026-07-31 12:00:03.000] last\r\n"
            )
        );
    }

    #[test]
    fn streaming_text_export_decodes_split_gbk() {
        let (encoded, _, _) = GBK.encode("中文\r\n完成");
        let input = encoded.into_owned();
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), input[..1].to_vec());
        store.append(timestamp(2), input[1..3].to_vec());
        store.append(timestamp(3), input[3..].to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Gbk,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "中文\r\n完成\r\n"
        );
    }

    #[test]
    fn terminal_export_writes_the_interpreted_screen_instead_of_ansi_input() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"msh >list".to_vec());
        store.append(timestamp(2), b"\x1b[2K\rmsh >thread\r\n".to_vec());
        store.append(timestamp(3), b"normal \x1b[31mred\x1b[0m text".to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Terminal,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "msh >thread\r\nnormal red text\r\n"
        );
        assert!(!output.contains(&0x1B));
    }

    #[test]
    fn terminal_export_handles_split_csi_and_split_gbk() {
        let (encoded, _, _) = GBK.encode("中文");
        let encoded = encoded.into_owned();
        let mut clear_and_first_byte = b"2J".to_vec();
        clear_and_first_byte.extend_from_slice(&encoded[..1]);
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"obsolete\r\n\x1b[".to_vec());
        store.append(timestamp(2), clear_and_first_byte);
        store.append(timestamp(3), encoded[1..].to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Terminal,
            TextEncoding::Gbk,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "中文\r\n"
        );
    }

    #[test]
    fn terminal_export_uses_rendered_row_timestamps() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"first\r\n".to_vec());
        store.append(timestamp(2), b"second".to_vec());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Terminal,
            TextEncoding::Utf8,
            true,
            "%H:%M:%S",
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "[12:00:01] first\r\n[12:00:02] second\r\n"
        );
    }

    #[test]
    fn streaming_hex_export_joins_chunks_and_uses_the_first_byte_timestamp() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), vec![0x00, 0x01, 0xAB]);
        store.append(timestamp(2), vec![0xFF]);
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.snapshot(),
            ReceiveMode::Hex,
            TextEncoding::Utf8,
            true,
            "%H:%M:%S",
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            "[12:00:01] 00 01 AB FF\r\n"
        );
    }

    #[test]
    fn streaming_hex_export_preserves_alignment_after_an_omitted_prefix() {
        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), (0..20).collect());
        let mut output = Vec::new();

        write_export(
            &mut output,
            store.tail_snapshot(18),
            ReceiveMode::Hex,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(&output[UTF8_BOM.len()..]).unwrap(),
            concat!(
                "02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F\r\n",
                "10 11 12 13\r\n"
            )
        );
    }

    #[test]
    fn streaming_export_propagates_write_failures() {
        struct FailingWriter {
            remaining: usize,
        }

        impl io::Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::other("disk full"));
                }
                let written = bytes.len().min(self.remaining);
                self.remaining -= written;
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut store = ReceiveStore::new(1024);
        store.append(timestamp(1), b"payload".to_vec());
        let error = write_export(
            &mut FailingWriter { remaining: 5 },
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn streaming_export_releases_chunks_after_writing_them() {
        struct ReleaseProbe {
            first_chunk: std::sync::Weak<[u8]>,
            observed_release: bool,
        }

        impl io::Write for ReleaseProbe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.observed_release |= self.first_chunk.upgrade().is_none();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let first_bytes: std::sync::Arc<[u8]> = vec![0xAA].into();
        let first_chunk = std::sync::Arc::downgrade(&first_bytes);
        let snapshot = ReceiveSnapshot {
            generation: 0,
            stream_id: 0,
            first_sequence: 0,
            next_sequence: 2,
            chunks: vec![
                RxChunk {
                    sequence: 0,
                    received_at: timestamp(1),
                    bytes: first_bytes,
                },
                RxChunk {
                    sequence: 1,
                    received_at: timestamp(2),
                    bytes: vec![0xBB].into(),
                },
            ],
            bytes_len: 2,
            omitted_bytes: 0,
            dropped_bytes: 0,
        };
        let mut probe = ReleaseProbe {
            first_chunk,
            observed_release: false,
        };

        write_export(
            &mut probe,
            snapshot,
            ReceiveMode::Hex,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert!(probe.observed_release);
    }

    #[test]
    fn streaming_text_export_bounds_each_write_for_a_long_line() {
        #[derive(Default)]
        struct WriteProbe {
            largest_write: usize,
            total: usize,
        }

        impl io::Write for WriteProbe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.largest_write = self.largest_write.max(bytes.len());
                self.total += bytes.len();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let input_len = 1024 * 1024;
        let mut store = ReceiveStore::new(input_len);
        store.append(timestamp(1), vec![b'x'; input_len]);
        let mut probe = WriteProbe::default();

        write_export(
            &mut probe,
            store.snapshot(),
            ReceiveMode::Text,
            TextEncoding::Utf8,
            false,
            DEFAULT_TIMESTAMP_FORMAT,
        )
        .unwrap();

        assert!(probe.largest_write <= EXPORT_DECODE_INPUT_BYTES);
        assert_eq!(probe.total, UTF8_BOM.len() + input_len + 2);
    }

    #[test]
    fn gbk_send_rejects_unrepresentable_characters() {
        assert!(encode_text("😀", TextEncoding::Gbk, LineEnding::None).is_err());
    }
}
