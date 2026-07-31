use std::ops::Range;

use regex::RegexBuilder;

use crate::formatting::{FormattedRow, display_text};

pub const MAX_SEARCH_MATCHES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub row_index: usize,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Default)]
pub struct SearchIndex {
    pub matches: Vec<SearchMatch>,
    pub matched_rows: Vec<usize>,
    pub error: Option<String>,
    pub truncated: bool,
}

impl SearchIndex {
    pub fn matches_for_row(&self, row_index: usize) -> (usize, &[SearchMatch]) {
        let start = self
            .matches
            .partition_point(|item| item.row_index < row_index);
        let end = self
            .matches
            .partition_point(|item| item.row_index <= row_index);
        (start, &self.matches[start..end])
    }

    pub fn matched_row_count(&self, total_rows: usize) -> usize {
        self.matched_rows
            .partition_point(|row_index| *row_index < total_rows)
    }
}

pub fn search_rows(
    rows: &[FormattedRow],
    query: &str,
    case_sensitive: bool,
    regex_mode: bool,
    timestamps: bool,
) -> SearchIndex {
    if query.is_empty() {
        return SearchIndex::default();
    }

    let expression = if regex_mode {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let matcher = match RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .build()
    {
        Ok(matcher) => matcher,
        Err(error) => {
            return SearchIndex {
                error: Some(format!("正则表达式无效：{error}")),
                ..Default::default()
            };
        }
    };

    let mut index = SearchIndex::default();
    'rows: for (row_index, row) in rows.iter().enumerate() {
        let text = display_text(row, timestamps);
        let mut row_matched = false;
        for found in matcher.find_iter(&text) {
            if found.is_empty() {
                continue;
            }
            if !row_matched {
                index.matched_rows.push(row_index);
                row_matched = true;
            }
            index.matches.push(SearchMatch {
                row_index,
                byte_range: found.range(),
            });
            if index.matches.len() >= MAX_SEARCH_MATCHES {
                index.truncated = true;
                break 'rows;
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;

    fn row(text: &str) -> FormattedRow {
        FormattedRow {
            received_at: Local::now(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let rows = vec![row("Ready READY"), row("idle"), row("ready")];
        let index = search_rows(&rows, "ready", false, false, false);

        assert_eq!(index.matches.len(), 3);
        assert_eq!(index.matched_rows, [0, 2]);
        let (offset, matches) = index.matches_for_row(0);
        assert_eq!(offset, 0);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].byte_range, 0..5);
    }

    #[test]
    fn regex_search_and_errors_are_reported() {
        let rows = vec![row("value=12"), row("value=x")];
        let index = search_rows(&rows, r"value=\d+", true, true, false);
        assert_eq!(index.matched_rows, [0]);

        let invalid = search_rows(&rows, "(", true, true, false);
        assert!(invalid.error.is_some());
        assert!(invalid.matches.is_empty());
    }
}
