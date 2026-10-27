//! Small, bounded input buffers. No copies of receive history or persistent editor state.

use escom_core::model::SendMode;
use std::collections::VecDeque;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Default)]
pub struct LineEditor {
    text: String,
    cursor: usize,
    view_start: usize,
}

pub struct Viewport {
    pub range: Range<usize>,
    pub cursor_column: usize,
}

impl From<&str> for LineEditor {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            cursor: text.len(),
            view_start: 0,
        }
    }
}

impl LineEditor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        // Returning from a large send must not retain its peak allocation indefinitely.
        *self = Self::default();
    }

    pub fn insert(&mut self, text: &str, limit: usize) -> bool {
        if text.len() > limit.saturating_sub(self.text.len()) {
            return false;
        }
        self.reserve_bounded(self.text.len() + text.len(), limit);
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.normalize_boundaries();
        true
    }

    fn reserve_bounded(&mut self, needed: usize, limit: usize) {
        if needed > self.text.capacity() {
            let capacity = self
                .text
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(limit);
            self.text.reserve_exact(capacity - self.text.len());
        }
    }

    fn normalize_boundaries(&mut self) {
        // Insertion/deletion can join neighboring graphemes (combining marks, ZWJ, flags).
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.text.len()))
            .find(|index| *index >= self.cursor)
            .unwrap_or(self.text.len());
        self.view_start = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .take_while(|index| *index <= self.view_start.min(self.cursor))
            .last()
            .unwrap_or(0);
    }

    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub fn right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn backspace(&mut self) -> bool {
        let end = self.cursor;
        self.left();
        if self.cursor == end {
            return false;
        }
        self.text.drain(self.cursor..end);
        self.normalize_boundaries();
        true
    }

    pub fn delete(&mut self) -> bool {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return false;
        };
        self.text.drain(self.cursor..self.cursor + grapheme.len());
        self.normalize_boundaries();
        true
    }

    /// Returns only a visible slice; control characters are measured as the UI's `·`.
    pub fn viewport(&mut self, width: usize) -> Viewport {
        if width == 0 {
            return Viewport {
                range: self.cursor..self.cursor,
                cursor_column: 0,
            };
        }
        self.view_start = self.view_start.min(self.cursor);
        let mut cursor_column: usize = self.text[self.view_start..self.cursor]
            .graphemes(true)
            .map(display_width)
            .sum();
        let cursor_width = self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(1, display_width)
            .max(1)
            .min(width);
        while cursor_column + cursor_width > width {
            let grapheme = self.text[self.view_start..self.cursor]
                .graphemes(true)
                .next()
                .expect("a nonzero cursor column has a preceding grapheme");
            cursor_column -= display_width(grapheme);
            self.view_start += grapheme.len();
        }
        let mut end = self.view_start;
        let mut columns = 0;
        for grapheme in self.text[self.view_start..].graphemes(true) {
            let next_width = display_width(grapheme);
            if columns + next_width > width {
                break;
            }
            columns += next_width;
            end += grapheme.len();
        }
        Viewport {
            range: self.view_start..end,
            cursor_column,
        }
    }

    /// Complete a command token or a fixed enum value, preserving the rest of the line.
    /// Ambiguous prefixes expand only to their common prefix; callers can show the choices.
    pub fn complete_command(&mut self, limit: usize) -> Result<Vec<&'static str>, ()> {
        let start = self.text[..self.cursor]
            .rfind(char::is_whitespace)
            .map_or(0, |index| {
                index + self.text[index..].chars().next().unwrap().len_utf8()
            });
        let end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |index| self.cursor + index);
        let context = self.text[..start].trim();
        let candidates = command_candidates(context);
        let prefix = &self.text[start..self.cursor];
        let choices: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.starts_with(prefix))
            .collect();
        let Some(first) = choices.first() else {
            return Ok(choices);
        };
        let mut common = first.len();
        for candidate in &choices[1..] {
            common = first
                .bytes()
                .zip(candidate.bytes())
                .take(common)
                .take_while(|(a, b)| a == b)
                .count();
        }
        // Do not erase an existing token suffix when completion is still ambiguous.
        if choices.len() > 1 && (self.cursor < end || common <= prefix.len()) {
            return Ok(choices);
        }
        let add_space = choices.len() == 1
            && context.is_empty()
            && end == self.text.len()
            && !matches!(*first, "connect" | "disconnect" | "ports" | "stop-record");
        let needed = self.text.len() - (end - start) + common + usize::from(add_space);
        if needed > limit {
            return Err(());
        }
        self.reserve_bounded(needed, limit);
        self.text.replace_range(start..end, &first[..common]);
        self.cursor = start + common;
        if add_space {
            self.text.insert(self.cursor, ' ');
            self.cursor += 1;
        }
        self.normalize_boundaries();
        Ok(choices)
    }
}

fn display_width(grapheme: &str) -> usize {
    if grapheme.chars().any(char::is_control) {
        grapheme
            .chars()
            .map(|ch| {
                if ch.is_control() {
                    1
                } else {
                    ch.width().unwrap_or(0)
                }
            })
            .sum()
    } else {
        grapheme.width()
    }
}

fn command_candidates(context: &str) -> &'static [&'static str] {
    match context {
        "" => &[
            "port",
            "ports",
            "baud",
            "data",
            "stop",
            "parity",
            "flow",
            "dtr",
            "rts",
            "mode",
            "encoding",
            "eol",
            "lang",
            "connect",
            "disconnect",
            "record",
            "stop-record",
        ],
        "data" => &["5", "6", "7", "8"],
        "stop" => &["1", "2"],
        "parity" => &["none", "odd", "even"],
        "flow" => &["none", "software", "hardware"],
        "dtr" | "rts" => &["on", "off"],
        "mode" => &["text", "hex", "terminal"],
        "encoding" => &["utf8", "gbk"],
        "eol" => &["none", "cr", "lf", "crlf"],
        "lang" => &["en", "zh-CN"],
        _ => &[],
    }
}

pub const HISTORY_ENTRIES: usize = 32;
pub const HISTORY_BYTES: usize = 16 * 1024;

struct HistoryEntry {
    text: Box<str>,
    mode: SendMode,
}

#[derive(Default)]
pub struct SendHistory {
    entries: VecDeque<HistoryEntry>,
    bytes: usize,
    selected: Option<usize>,
    draft: Option<LineEditor>,
}

impl SendHistory {
    /// History records queue acceptance, not delivery; never truncate a recalled command.
    pub fn record(&mut self, text: &str, mode: SendMode) {
        self.reset_navigation();
        if text.is_empty()
            || text.len() > HISTORY_BYTES
            || self
                .entries
                .back()
                .is_some_and(|entry| entry.mode == mode && &*entry.text == text)
        {
            return;
        }
        while self.entries.len() >= HISTORY_ENTRIES || self.bytes + text.len() > HISTORY_BYTES {
            self.bytes -= self.entries.pop_front().unwrap().text.len();
        }
        self.bytes += text.len();
        self.entries.push_back(HistoryEntry {
            text: text.into(),
            mode,
        });
    }

    pub fn reset_navigation(&mut self) {
        self.selected = None;
        self.draft = None;
    }

    pub fn navigate(&mut self, older: bool, mode: SendMode, editor: &mut LineEditor, limit: usize) {
        let eligible = |entry: &HistoryEntry| entry.mode == mode && entry.text.len() <= limit;
        let index = if older {
            let before = self.selected.unwrap_or(self.entries.len());
            (0..before)
                .rev()
                .find(|&index| eligible(&self.entries[index]))
        } else if let Some(selected) = self.selected {
            (selected + 1..self.entries.len()).find(|&index| eligible(&self.entries[index]))
        } else {
            return;
        };
        if let Some(index) = index {
            if self.selected.is_none() {
                self.draft = Some(std::mem::take(editor));
            }
            *editor = LineEditor::from(self.entries[index].text.as_ref());
            self.selected = Some(index);
        } else if !older {
            if let Some(draft) = self.draft.take() {
                *editor = draft;
            }
            self.selected = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_chinese_combining_marks_and_emoji_without_splitting_graphemes() {
        let mut editor = LineEditor::from("A中e\u{301}👩‍💻Z");
        editor.left();
        assert!(editor.backspace());
        assert_eq!(editor.text(), "A中e\u{301}Z");
        assert!(editor.delete());
        editor.left();
        assert!(editor.insert("!", 64));
        assert_eq!(editor.text(), "A中!e\u{301}");
        editor.home();
        assert!(!editor.backspace());
        assert!(editor.delete());
        editor.right();
        assert!(editor.backspace());
        assert_eq!(editor.text(), "!e\u{301}");
        editor.end();
        assert!(!editor.delete());
        assert!(editor.backspace());
        assert_eq!(editor.text(), "!");
    }

    #[test]
    fn insertion_is_atomic_at_the_byte_limit_and_keeps_payload_controls() {
        let mut editor = LineEditor::from("中B");
        editor.home();
        editor.right();
        assert!(editor.insert("\r\n", 6));
        assert_eq!(editor.text(), "中\r\nB");
        let cursor = editor.cursor;
        assert!(!editor.insert("X", 6));
        assert_eq!(editor.cursor, cursor);
        assert_eq!(editor.text(), "中\r\nB");
        assert!(editor.text.capacity() <= 6);
        editor.clear();
        assert_eq!(editor.text.capacity(), 0);
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn insertion_can_join_graphemes_and_cursor_remains_at_a_boundary() {
        let mut editor = LineEditor::from("👩💻");
        editor.home();
        editor.right();
        editor.insert("\u{200d}", 64);
        assert_eq!(editor.text(), "👩‍💻");
        assert_eq!(editor.cursor, editor.text.len());
        editor.backspace();
        assert!(editor.text().is_empty());
    }

    #[test]
    fn viewport_tracks_cursor_and_resizes_with_wide_and_control_characters() {
        let mut editor = LineEditor::from("prefix-中e\u{301}👩‍💻\r\n-tail");
        for width in [8, 4, 1, 0, 20] {
            for _ in 0..=editor.text.len() {
                let view = editor.viewport(width);
                assert!(view.cursor_column < width.max(1));
                let visible = &editor.text()[view.range.clone()];
                assert!(visible.graphemes(true).map(display_width).sum::<usize>() <= width);
                assert!(view.range.start <= editor.cursor);
                assert!(view.range.end >= editor.cursor);
                editor.left();
            }
            editor.end();
        }
        editor.home();
        let view = editor.viewport(8);
        assert_eq!(&editor.text()[view.range], "prefix-");
        assert_eq!(view.cursor_column, 0);
        editor.end();
        let view = editor.viewport(8);
        assert!(editor.text()[view.range].ends_with("-tail"));
    }

    #[test]
    fn completion_preserves_arguments_and_handles_ambiguity_limits_and_unknown_tokens() {
        let mut editor = LineEditor::from("mo hex");
        editor.home();
        editor.right();
        editor.right();
        assert_eq!(editor.complete_command(1024).unwrap(), vec!["mode"]);
        assert_eq!(editor.text(), "mode hex");
        let mut editor = LineEditor::from("enc");
        editor.complete_command(1024).unwrap();
        assert_eq!(editor.text(), "encoding ");
        editor.insert("g", 1024);
        editor.complete_command(1024).unwrap();
        assert_eq!(editor.text(), "encoding gbk");
        let mut editor = LineEditor::from("mode te");
        assert_eq!(
            editor.complete_command(1024).unwrap(),
            vec!["text", "terminal"]
        );
        assert_eq!(editor.text(), "mode te");
        editor.insert("x", 1024);
        editor.complete_command(1024).unwrap();
        assert_eq!(editor.text(), "mode text");
        let mut editor = LineEditor::from("record D:\\日志.bin");
        assert!(editor.complete_command(1024).unwrap().is_empty());
        assert_eq!(editor.text(), "record D:\\日志.bin");
        let mut editor = LineEditor::from("enc");
        assert!(editor.complete_command(4).is_err());
        assert_eq!(editor.text(), "enc");
        assert_eq!(editor.cursor, 3);
    }

    #[test]
    fn ambiguous_completion_inside_a_token_preserves_its_suffix_and_cursor() {
        let mut editor = LineEditor::from("mode terminal");
        editor.home();
        for _ in 0..6 {
            editor.right();
        }
        assert_eq!(
            editor.complete_command(1024).unwrap(),
            vec!["text", "terminal"]
        );
        assert_eq!(editor.text(), "mode terminal");
        assert_eq!(editor.cursor, 6);
    }

    #[test]
    fn history_filters_send_mode_and_restores_the_unsubmitted_draft_and_cursor() {
        let mut history = SendHistory::default();
        history.record("AT", SendMode::Text);
        history.record("41 54", SendMode::Hex);
        history.record("AT+GMR", SendMode::Text);
        let mut editor = LineEditor::from("草稿");
        editor.home();
        history.navigate(true, SendMode::Text, &mut editor, 1024);
        assert_eq!(editor.text(), "AT+GMR");
        history.navigate(true, SendMode::Text, &mut editor, 1024);
        assert_eq!(editor.text(), "AT");
        history.navigate(true, SendMode::Text, &mut editor, 1024);
        assert_eq!(editor.text(), "AT");
        history.navigate(false, SendMode::Text, &mut editor, 1024);
        assert_eq!(editor.text(), "AT+GMR");
        history.navigate(false, SendMode::Text, &mut editor, 1024);
        editor.insert("原", 1024);
        assert_eq!(editor.text(), "原草稿");
    }

    #[test]
    fn history_has_item_and_byte_caps_and_never_truncates_large_entries() {
        let mut history = SendHistory::default();
        for index in 0..100 {
            history.record(&format!("AT+{index}"), SendMode::Text);
        }
        assert_eq!(history.entries.len(), HISTORY_ENTRIES);
        assert_eq!(&*history.entries.front().unwrap().text, "AT+68");
        history.record("AT+99", SendMode::Text);
        assert_eq!(history.entries.len(), HISTORY_ENTRIES);
        for index in 0..20 {
            history.record(&format!("{index}{}", "x".repeat(2047)), SendMode::Text);
            assert!(history.bytes <= HISTORY_BYTES);
        }
        let count = history.entries.len();
        history.record(&"x".repeat(HISTORY_BYTES + 1), SendMode::Text);
        assert_eq!(history.entries.len(), count);
        let mut editor = LineEditor::from("draft");
        history.navigate(true, SendMode::Text, &mut editor, 1024);
        assert_eq!(editor.text(), "draft");
    }
}
