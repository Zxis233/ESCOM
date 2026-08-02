use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::formatting::{FormattedRow, display_text};

pub const MAX_SEARCH_MATCHES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub row_index: usize,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchIndex {
    pub matches: Vec<SearchMatch>,
    pub matched_rows: Vec<usize>,
    pub error: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    matcher: Regex,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchDisplayOptions<'a> {
    pub timestamps: bool,
    pub timestamp_format: &'a str,
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchUpdateOutcome {
    Applied,
    RequiresFullSearch,
}

impl<'a> SearchDisplayOptions<'a> {
    pub const fn new(timestamps: bool, timestamp_format: &'a str) -> Self {
        Self {
            timestamps,
            timestamp_format,
        }
    }
}

impl SearchMatcher {
    pub fn new(
        query: &str,
        case_sensitive: bool,
        regex_mode: bool,
    ) -> Result<Option<Self>, String> {
        if query.is_empty() {
            return Ok(None);
        }

        let expression = if regex_mode {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        RegexBuilder::new(&expression)
            .case_insensitive(!case_sensitive)
            .build()
            .map(|matcher| Some(Self { matcher }))
            .map_err(|error| format!("正则表达式无效：{error}"))
    }
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

    pub fn apply_display_update(
        &mut self,
        old_row_count: usize,
        remove_prefix: usize,
        replace_tail: usize,
        rows: &[FormattedRow],
        matcher: &SearchMatcher,
        display_options: SearchDisplayOptions<'_>,
    ) -> SearchUpdateOutcome {
        debug_assert!(remove_prefix <= old_row_count);
        let rows_after_prefix = old_row_count.saturating_sub(remove_prefix);
        debug_assert!(replace_tail <= rows_after_prefix);

        let removed_matches = self
            .matches
            .partition_point(|item| item.row_index < remove_prefix);
        let replaced_tail_start = old_row_count.saturating_sub(replace_tail);
        let replacement_touches_indexed_prefix = replace_tail != 0
            && self
                .matches
                .last()
                .is_some_and(|item| replaced_tail_start <= item.row_index);
        if self.truncated && (removed_matches != 0 || replacement_touches_indexed_prefix) {
            return SearchUpdateOutcome::RequiresFullSearch;
        }

        if remove_prefix != 0 {
            self.matches.drain(..removed_matches);
            for item in &mut self.matches {
                item.row_index -= remove_prefix;
            }

            let first_row = self
                .matched_rows
                .partition_point(|row_index| *row_index < remove_prefix);
            self.matched_rows.drain(..first_row);
            for row_index in &mut self.matched_rows {
                *row_index -= remove_prefix;
            }
        }

        let append_start = rows_after_prefix.saturating_sub(replace_tail);
        if replace_tail != 0 {
            let match_end = self
                .matches
                .partition_point(|item| item.row_index < append_start);
            self.matches.truncate(match_end);
            let row_end = self
                .matched_rows
                .partition_point(|row_index| *row_index < append_start);
            self.matched_rows.truncate(row_end);
        }

        if !self.truncated {
            append_matches(self, rows, append_start, matcher, display_options);
        }
        SearchUpdateOutcome::Applied
    }
}

pub fn search_rows(
    rows: &[FormattedRow],
    query: &str,
    case_sensitive: bool,
    regex_mode: bool,
    display_options: SearchDisplayOptions<'_>,
) -> SearchIndex {
    let matcher = match SearchMatcher::new(query, case_sensitive, regex_mode) {
        Ok(Some(matcher)) => matcher,
        Ok(None) => return SearchIndex::default(),
        Err(error) => {
            return SearchIndex {
                error: Some(error),
                ..Default::default()
            };
        }
    };

    search_rows_with_matcher(rows, &matcher, display_options)
}

pub fn search_rows_with_matcher(
    rows: &[FormattedRow],
    matcher: &SearchMatcher,
    display_options: SearchDisplayOptions<'_>,
) -> SearchIndex {
    let mut index = SearchIndex::default();
    append_matches(&mut index, rows, 0, matcher, display_options);
    index
}

fn append_matches(
    index: &mut SearchIndex,
    rows: &[FormattedRow],
    row_offset: usize,
    matcher: &SearchMatcher,
    display_options: SearchDisplayOptions<'_>,
) {
    'rows: for (local_row_index, row) in rows.iter().enumerate() {
        let row_index = row_offset + local_row_index;
        let text = display_text(
            row,
            display_options.timestamps,
            display_options.timestamp_format,
        );
        let mut row_matched = false;
        for found in matcher.matcher.find_iter(&text) {
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
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;
    use crate::formatting::DEFAULT_TIMESTAMP_FORMAT;

    const DISPLAY_OPTIONS: SearchDisplayOptions<'static> =
        SearchDisplayOptions::new(false, DEFAULT_TIMESTAMP_FORMAT);

    fn row(text: &str) -> FormattedRow {
        FormattedRow {
            received_at: Local::now(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let rows = vec![row("Ready READY"), row("idle"), row("ready")];
        let index = search_rows(&rows, "ready", false, false, DISPLAY_OPTIONS);

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
        let index = search_rows(&rows, r"value=\d+", true, true, DISPLAY_OPTIONS);
        assert_eq!(index.matched_rows, [0]);

        let invalid = search_rows(&rows, "(", true, true, DISPLAY_OPTIONS);
        assert!(invalid.error.is_some());
        assert!(invalid.matches.is_empty());
    }

    #[test]
    fn incremental_update_shifts_rows_and_indexes_appended_matches() {
        let matcher = SearchMatcher::new("ready", false, false).unwrap().unwrap();
        let initial = vec![row("ready 0"), row("idle"), row("ready 2")];
        let mut index = search_rows_with_matcher(&initial, &matcher, DISPLAY_OPTIONS);

        let outcome =
            index.apply_display_update(3, 1, 0, &[row("ready 3")], &matcher, DISPLAY_OPTIONS);
        assert_eq!(outcome, SearchUpdateOutcome::Applied);
        assert_eq!(index.matched_rows, [1, 2]);
        assert_eq!(
            index
                .matches
                .iter()
                .map(|item| item.row_index)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn incremental_update_replaces_tail_matches() {
        let matcher = SearchMatcher::new("ok", false, false).unwrap().unwrap();
        let initial = vec![row("first"), row("not yet")];
        let mut index = search_rows_with_matcher(&initial, &matcher, DISPLAY_OPTIONS);

        let outcome =
            index.apply_display_update(2, 0, 1, &[row("now ok")], &matcher, DISPLAY_OPTIONS);
        assert_eq!(outcome, SearchUpdateOutcome::Applied);
        assert_eq!(index.matched_rows, [1]);
        assert_eq!(index.matches[0].byte_range, 4..6);
    }

    #[test]
    fn truncated_update_requests_full_search_after_indexed_matches_are_evicted() {
        let matcher = SearchMatcher::new("hit", true, false).unwrap().unwrap();
        let mut index = SearchIndex {
            matches: vec![
                SearchMatch {
                    row_index: 0,
                    byte_range: 0..3,
                },
                SearchMatch {
                    row_index: 2,
                    byte_range: 0..3,
                },
            ],
            matched_rows: vec![0, 2],
            error: None,
            truncated: true,
        };

        let outcome =
            index.apply_display_update(3, 1, 0, &[row("hit new")], &matcher, DISPLAY_OPTIONS);

        assert_eq!(outcome, SearchUpdateOutcome::RequiresFullSearch);
        assert_eq!(
            index
                .matches
                .iter()
                .map(|item| item.row_index)
                .collect::<Vec<_>>(),
            [0, 2]
        );
    }

    #[test]
    fn truncated_update_keeps_valid_prefix_when_eviction_removes_no_matches() {
        let matcher = SearchMatcher::new("hit", true, false).unwrap().unwrap();
        let mut index = SearchIndex {
            matches: vec![
                SearchMatch {
                    row_index: 1,
                    byte_range: 0..3,
                },
                SearchMatch {
                    row_index: 2,
                    byte_range: 0..3,
                },
            ],
            matched_rows: vec![1, 2],
            error: None,
            truncated: true,
        };

        let outcome =
            index.apply_display_update(3, 1, 0, &[row("hit new")], &matcher, DISPLAY_OPTIONS);

        assert_eq!(outcome, SearchUpdateOutcome::Applied);
        assert_eq!(
            index
                .matches
                .iter()
                .map(|item| item.row_index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(index.matched_rows, [0, 1]);
        assert!(index.truncated);
    }

    #[test]
    fn truncated_update_rebuilds_only_when_tail_replacement_touches_indexed_prefix() {
        let matcher = SearchMatcher::new("hit", true, false).unwrap().unwrap();
        let make_index = || SearchIndex {
            matches: vec![
                SearchMatch {
                    row_index: 0,
                    byte_range: 0..3,
                },
                SearchMatch {
                    row_index: 1,
                    byte_range: 0..3,
                },
            ],
            matched_rows: vec![0, 1],
            error: None,
            truncated: true,
        };

        let mut after_prefix = make_index();
        let outcome = after_prefix.apply_display_update(
            4,
            0,
            1,
            &[row("hit tail")],
            &matcher,
            DISPLAY_OPTIONS,
        );
        assert_eq!(outcome, SearchUpdateOutcome::Applied);

        let mut touching_prefix = make_index();
        let outcome = touching_prefix.apply_display_update(
            2,
            0,
            1,
            &[row("hit replacement")],
            &matcher,
            DISPLAY_OPTIONS,
        );
        assert_eq!(outcome, SearchUpdateOutcome::RequiresFullSearch);
    }
}
