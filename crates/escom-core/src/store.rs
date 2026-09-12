use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Local};

#[derive(Debug, Clone)]
pub struct RxChunk {
    pub sequence: u64,
    pub received_at: DateTime<Local>,
    pub session_offset: u64,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub struct ReceiveBoundary {
    pub sequence: u64,
    pub received_at: DateTime<Local>,
}

#[derive(Debug, Clone)]
pub enum ReceiveRecord {
    Data(RxChunk),
    Boundary(ReceiveBoundary),
}

impl ReceiveRecord {
    pub const fn sequence(&self) -> u64 {
        match self {
            Self::Data(chunk) => chunk.sequence,
            Self::Boundary(boundary) => boundary.sequence,
        }
    }

    pub const fn received_at(&self) -> DateTime<Local> {
        match self {
            Self::Data(chunk) => chunk.received_at,
            Self::Boundary(boundary) => boundary.received_at,
        }
    }

    pub fn bytes_len(&self) -> usize {
        match self {
            Self::Data(chunk) => chunk.bytes.len(),
            Self::Boundary(_) => 0,
        }
    }

    pub const fn is_boundary(&self) -> bool {
        matches!(self, Self::Boundary(_))
    }

    pub const fn as_data(&self) -> Option<&RxChunk> {
        match self {
            Self::Data(chunk) => Some(chunk),
            Self::Boundary(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceiveSnapshot {
    pub generation: u64,
    pub stream_id: u64,
    pub first_sequence: u64,
    pub next_sequence: u64,
    pub records: Vec<ReceiveRecord>,
    pub bytes_len: usize,
    pub omitted_bytes: usize,
    pub dropped_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveCursor {
    pub stream_id: u64,
    pub next_sequence: u64,
}

#[derive(Debug, Clone)]
pub struct ReceiveDelta {
    pub generation: u64,
    pub stream_id: u64,
    pub first_sequence: u64,
    pub next_sequence: u64,
    pub records: Vec<ReceiveRecord>,
    pub reset_or_gap: bool,
}

#[derive(Debug)]
pub struct ReceiveStore {
    records: VecDeque<ReceiveRecord>,
    bytes_len: usize,
    limit_bytes: usize,
    limit_records: usize,
    next_sequence: u64,
    session_bytes: u64,
    stream_id: u64,
    generation: u64,
    dropped_bytes: u64,
}

impl ReceiveStore {
    pub fn new(limit_bytes: usize) -> Self {
        Self::with_limits(limit_bytes, usize::MAX)
    }

    /// Both payload bytes and record metadata are bounded. Empty data is ignored.
    pub fn with_limits(limit_bytes: usize, limit_records: usize) -> Self {
        Self {
            records: VecDeque::new(),
            bytes_len: 0,
            limit_bytes,
            limit_records: limit_records.max(1),
            next_sequence: 0,
            session_bytes: 0,
            stream_id: 0,
            generation: 0,
            dropped_bytes: 0,
        }
    }

    pub fn append(&mut self, received_at: DateTime<Local>, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }

        let bytes_len = bytes.len();
        self.bytes_len = self.bytes_len.saturating_add(bytes_len);
        self.records.push_back(ReceiveRecord::Data(RxChunk {
            sequence: self.next_sequence,
            received_at,
            session_offset: self.session_bytes,
            bytes: bytes.into(),
        }));
        self.session_bytes = self.session_bytes.saturating_add(bytes_len as u64);
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        self.trim_to_limit();
    }

    pub fn mark_stream_boundary(&mut self, received_at: DateTime<Local>) -> bool {
        self.session_bytes = 0;
        if self.records.is_empty() || self.records.back().is_some_and(ReceiveRecord::is_boundary) {
            return false;
        }

        self.records
            .push_back(ReceiveRecord::Boundary(ReceiveBoundary {
                sequence: self.next_sequence,
                received_at,
            }));
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        self.trim_to_limit();
        true
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.bytes_len = 0;
        self.session_bytes = 0;
        self.dropped_bytes = 0;
        self.stream_id = self.stream_id.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn set_limit(&mut self, limit_bytes: usize) {
        self.limit_bytes = limit_bytes;
        self.trim_to_limit();
        self.generation = self.generation.wrapping_add(1);
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn bytes_len(&self) -> usize {
        self.bytes_len
    }

    pub fn records_len(&self) -> usize {
        self.records.len()
    }

    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    pub fn snapshot(&self) -> ReceiveSnapshot {
        let first_sequence = self
            .records
            .front()
            .map_or(self.next_sequence, ReceiveRecord::sequence);
        ReceiveSnapshot {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            records: self.records.iter().cloned().collect(),
            bytes_len: self.bytes_len,
            omitted_bytes: 0,
            dropped_bytes: self.dropped_bytes,
        }
    }

    pub fn tail_snapshot(&self, max_bytes: usize) -> ReceiveSnapshot {
        if self.bytes_len <= max_bytes {
            return self.snapshot();
        }

        let mut remaining = max_bytes;
        let mut records = Vec::new();
        for record in self.records.iter().rev() {
            if remaining == 0 {
                break;
            }
            match record {
                ReceiveRecord::Boundary(_) => records.push(record.clone()),
                ReceiveRecord::Data(chunk) => {
                    let take = chunk.bytes.len().min(remaining);
                    let skipped = chunk.bytes.len() - take;
                    let bytes = if skipped == 0 {
                        Arc::clone(&chunk.bytes)
                    } else {
                        Arc::from(&chunk.bytes[skipped..])
                    };
                    records.push(ReceiveRecord::Data(RxChunk {
                        sequence: chunk.sequence,
                        received_at: chunk.received_at,
                        session_offset: chunk.session_offset.saturating_add(skipped as u64),
                        bytes,
                    }));
                    remaining -= take;
                }
            }
        }
        records.reverse();

        let bytes_len = max_bytes - remaining;
        let first_sequence = records
            .first()
            .map_or(self.next_sequence, ReceiveRecord::sequence);
        ReceiveSnapshot {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            records,
            bytes_len,
            omitted_bytes: self.bytes_len.saturating_sub(bytes_len),
            dropped_bytes: self.dropped_bytes,
        }
    }

    pub fn delta_since(&self, cursor: ReceiveCursor) -> ReceiveDelta {
        self.delta_since_bounded(cursor, usize::MAX)
    }

    pub fn delta_since_bounded(&self, cursor: ReceiveCursor, max_bytes: usize) -> ReceiveDelta {
        let first_sequence = self
            .records
            .front()
            .map_or(self.next_sequence, ReceiveRecord::sequence);
        let mut reset_or_gap = cursor.stream_id != self.stream_id
            || cursor.next_sequence < first_sequence
            || cursor.next_sequence > self.next_sequence;
        let records = if reset_or_gap {
            Vec::new()
        } else {
            let start = usize::try_from(cursor.next_sequence - first_sequence)
                .unwrap_or(self.records.len());
            let start = start.min(self.records.len());
            let mut bytes_len = 0_usize;
            for record in self.records.range(start..) {
                bytes_len = bytes_len.saturating_add(record.bytes_len());
                if bytes_len > max_bytes {
                    reset_or_gap = true;
                    break;
                }
            }
            if reset_or_gap {
                Vec::new()
            } else {
                self.records.range(start..).cloned().collect()
            }
        };

        ReceiveDelta {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            records,
            reset_or_gap,
        }
    }

    fn trim_to_limit(&mut self) {
        while self.bytes_len > self.limit_bytes || self.records.len() > self.limit_records {
            let Some(oldest) = self.records.pop_front() else {
                break;
            };
            let dropped = oldest.bytes_len();
            self.bytes_len = self.bytes_len.saturating_sub(dropped);
            self.dropped_bytes = self.dropped_bytes.saturating_add(dropped as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;

    #[test]
    fn record_budget_bounds_tiny_reads_and_session_metadata() {
        let mut store = ReceiveStore::with_limits(1024, 8);
        for _ in 0..1000 {
            store.append(Local::now(), vec![1]);
            store.mark_stream_boundary(Local::now());
            assert!(store.records_len() <= 8);
        }
        assert!(store.bytes_len() <= 4);
        assert!(store.dropped_bytes() >= 996);
        assert!(
            store
                .delta_since(ReceiveCursor {
                    stream_id: 0,
                    next_sequence: 0
                })
                .reset_or_gap
        );
    }

    fn data(record: &ReceiveRecord) -> &RxChunk {
        record.as_data().expect("expected data record")
    }

    #[test]
    fn bounded_store_drops_oldest_complete_chunks() {
        let mut store = ReceiveStore::new(5);
        store.append(Local::now(), vec![1, 2, 3]);
        store.append(Local::now(), vec![4, 5, 6]);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.bytes_len, 3);
        assert_eq!(snapshot.dropped_bytes, 3);
        assert_eq!(&*data(&snapshot.records[0]).bytes, &[4, 5, 6]);
    }

    #[test]
    fn clear_resets_session_eviction_notice() {
        let mut store = ReceiveStore::new(1);
        store.append(Local::now(), vec![1, 2]);
        assert_eq!(store.dropped_bytes(), 2);
        store.clear();
        assert_eq!(store.dropped_bytes(), 0);
        assert_eq!(store.bytes_len(), 0);
    }

    #[test]
    fn delta_only_contains_chunks_after_cursor() {
        let mut store = ReceiveStore::new(1024);
        store.append(Local::now(), vec![1]);
        let snapshot = store.snapshot();
        store.append(Local::now(), vec![2]);
        store.append(Local::now(), vec![3]);

        let delta = store.delta_since(ReceiveCursor {
            stream_id: snapshot.stream_id,
            next_sequence: snapshot.next_sequence,
        });
        assert!(!delta.reset_or_gap);
        assert_eq!(delta.records.len(), 2);
        assert_eq!(&*data(&delta.records[0]).bytes, &[2]);
        assert_eq!(&*data(&delta.records[1]).bytes, &[3]);
    }

    #[test]
    fn delta_detects_evicted_unread_chunks() {
        let mut store = ReceiveStore::new(2);
        let cursor = ReceiveCursor {
            stream_id: 0,
            next_sequence: 0,
        };
        store.append(Local::now(), vec![1, 2]);
        store.append(Local::now(), vec![3, 4]);

        let delta = store.delta_since(cursor);
        assert!(delta.reset_or_gap);
        assert!(delta.records.is_empty());
    }

    #[test]
    fn delta_detects_clear_even_when_sequence_matches() {
        let mut store = ReceiveStore::new(1024);
        store.append(Local::now(), vec![1]);
        let snapshot = store.snapshot();
        store.clear();

        let delta = store.delta_since(ReceiveCursor {
            stream_id: snapshot.stream_id,
            next_sequence: snapshot.next_sequence,
        });
        assert!(delta.reset_or_gap);
        assert_ne!(delta.stream_id, snapshot.stream_id);
    }

    #[test]
    fn tail_snapshot_retains_only_the_latest_bytes() {
        let mut store = ReceiveStore::new(1024);
        store.append(Local::now(), vec![1, 2, 3]);
        store.append(Local::now(), vec![4, 5, 6]);

        let snapshot = store.tail_snapshot(4);

        assert_eq!(snapshot.bytes_len, 4);
        assert_eq!(snapshot.omitted_bytes, 2);
        assert_eq!(snapshot.first_sequence, 0);
        assert_eq!(snapshot.next_sequence, 2);
        assert_eq!(&*data(&snapshot.records[0]).bytes, &[3]);
        assert_eq!(&*data(&snapshot.records[1]).bytes, &[4, 5, 6]);
        assert_eq!(data(&snapshot.records[0]).session_offset, 2);
    }

    #[test]
    fn bounded_delta_rejects_an_excessive_backlog_without_cloning_it() {
        let mut store = ReceiveStore::new(1024);
        let cursor = ReceiveCursor {
            stream_id: 0,
            next_sequence: 0,
        };
        store.append(Local::now(), vec![1, 2, 3]);
        store.append(Local::now(), vec![4, 5, 6]);

        let delta = store.delta_since_bounded(cursor, 5);

        assert!(delta.reset_or_gap);
        assert!(delta.records.is_empty());
        assert_eq!(delta.next_sequence, 2);
    }

    #[test]
    fn stream_boundary_is_ordered_without_counting_as_received_bytes() {
        let mut store = ReceiveStore::new(1024);
        store.append(Local::now(), vec![1, 2, 3]);
        let cursor = store.snapshot();

        assert!(store.mark_stream_boundary(Local::now()));
        assert!(!store.mark_stream_boundary(Local::now()));
        assert_eq!(store.bytes_len(), 3);

        let delta = store.delta_since(ReceiveCursor {
            stream_id: cursor.stream_id,
            next_sequence: cursor.next_sequence,
        });
        assert_eq!(delta.records.len(), 1);
        assert!(delta.records[0].is_boundary());
        assert_eq!(delta.next_sequence, cursor.next_sequence + 1);

        store.append(Local::now(), vec![4, 5]);
        let snapshot = store.snapshot();
        assert_eq!(snapshot.records.len(), 3);
        assert!(snapshot.records[1].is_boundary());
        assert_eq!(data(&snapshot.records[2]).session_offset, 0);
    }

    #[test]
    fn tail_snapshot_keeps_boundaries_between_retained_sessions() {
        let mut store = ReceiveStore::new(1024);
        store.append(Local::now(), vec![1, 2, 3]);
        store.mark_stream_boundary(Local::now());
        store.append(Local::now(), vec![4, 5, 6]);

        let snapshot = store.tail_snapshot(4);

        assert_eq!(snapshot.records.len(), 3);
        assert_eq!(&*data(&snapshot.records[0]).bytes, &[3]);
        assert_eq!(data(&snapshot.records[0]).session_offset, 2);
        assert!(snapshot.records[1].is_boundary());
        assert_eq!(&*data(&snapshot.records[2]).bytes, &[4, 5, 6]);
        assert_eq!(data(&snapshot.records[2]).session_offset, 0);
    }
}
