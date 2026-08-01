use std::collections::VecDeque;
use std::fmt::Write as _;

use chrono::{DateTime, Local};
use encoding_rs::{CoderResult, GBK, UTF_8};

use crate::model::{LineEnding, ReceiveMode, SendMode, TextEncoding};
use crate::store::{ReceiveCursor, ReceiveDelta, ReceiveSnapshot, RxChunk};

pub const MAX_DISPLAY_ROWS: usize = 100_000;
pub const MAX_DISPLAY_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DISPLAY_LINE_BYTES: usize = 512 * 1024;
pub const MAX_DISPLAY_INCREMENT_BYTES: usize = 128 * 1024;
const MAX_TEXT_REBUILD_BYTES: usize = 16 * 1024 * 1024;
const HEX_BYTES_PER_ROW: usize = 16;

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
    Hex,
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
            ReceiveMode::Text | ReceiveMode::Terminal => DisplayFormatterState::Text(
                IncrementalTextFormatter::new(encoding, limits.max_line_bytes),
            ),
            ReceiveMode::Hex => DisplayFormatterState::Hex,
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

        let partial_visible = matches!(
            &self.state,
            DisplayFormatterState::Text(state) if state.partial_visible
        );
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
            if let DisplayFormatterState::Text(state) = &mut self.state {
                state.partial_visible = false;
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
                DisplayFormatterState::Hex => {
                    for chunk in &delta.chunks {
                        let new_rows_start = rows.len();
                        push_hex_chunk(chunk, &mut rows, row_end_sequences);
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

fn push_hex_chunk(
    chunk: &RxChunk,
    rows: &mut Vec<FormattedRow>,
    row_end_sequences: &mut VecDeque<u64>,
) {
    for bytes in chunk.bytes.chunks(16) {
        let mut text = String::with_capacity(bytes.len() * 3);
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            let _ = write!(text, "{byte:02X}");
        }
        rows.push(FormattedRow {
            received_at: chunk.received_at,
            text,
        });
        row_end_sequences.push_back(chunk.sequence);
    }
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
        ReceiveMode::Text | ReceiveMode::Terminal => format_text(snapshot, encoding),
        ReceiveMode::Hex => format_hex(snapshot),
    }
}

pub fn render_export(rows: &[FormattedRow], timestamps: bool) -> Vec<u8> {
    let estimated_len = rows.iter().map(|row| row.text.len() + 32).sum();
    let mut output = String::with_capacity(estimated_len);
    for row in rows {
        if timestamps {
            let _ = write!(
                output,
                "[{}] ",
                row.received_at.format("%Y-%m-%d %H:%M:%S%.3f")
            );
        }
        output.push_str(&row.text);
        output.push_str("\r\n");
    }

    let mut bytes = Vec::with_capacity(output.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(output.as_bytes());
    bytes
}

pub fn display_text(row: &FormattedRow, timestamps: bool) -> String {
    if timestamps {
        format!(
            "[{}] {}",
            row.received_at.format("%Y-%m-%d %H:%M:%S%.3f"),
            row.text
        )
    } else {
        row.text.clone()
    }
}

fn format_hex(snapshot: &ReceiveSnapshot) -> Vec<FormattedRow> {
    let mut rows = Vec::new();
    for chunk in &snapshot.chunks {
        for bytes in chunk.bytes.chunks(16) {
            let mut text = String::with_capacity(bytes.len() * 3);
            for (index, byte) in bytes.iter().enumerate() {
                if index > 0 {
                    text.push(' ');
                }
                let _ = write!(text, "{byte:02X}");
            }
            rows.push(FormattedRow {
                received_at: chunk.received_at,
                text,
            });
        }
    }
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
        let rows = vec![FormattedRow {
            received_at: timestamp(1),
            text: "数据".into(),
        }];
        let bytes = render_export(&rows, false);
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert_eq!(std::str::from_utf8(&bytes[3..]).unwrap(), "数据\r\n");
    }

    #[test]
    fn gbk_send_rejects_unrepresentable_characters() {
        assert!(encode_text("😀", TextEncoding::Gbk, LineEnding::None).is_err());
    }
}
