use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Local};

#[derive(Debug, Clone)]
pub struct RxChunk {
    pub sequence: u64,
    pub received_at: DateTime<Local>,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub struct ReceiveSnapshot {
    pub generation: u64,
    pub stream_id: u64,
    pub first_sequence: u64,
    pub next_sequence: u64,
    pub chunks: Vec<RxChunk>,
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
    pub chunks: Vec<RxChunk>,
    pub reset_or_gap: bool,
}

#[derive(Debug)]
pub struct ReceiveStore {
    chunks: VecDeque<RxChunk>,
    bytes_len: usize,
    limit_bytes: usize,
    next_sequence: u64,
    stream_id: u64,
    generation: u64,
    dropped_bytes: u64,
}

impl ReceiveStore {
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes_len: 0,
            limit_bytes,
            next_sequence: 0,
            stream_id: 0,
            generation: 0,
            dropped_bytes: 0,
        }
    }

    pub fn append(&mut self, received_at: DateTime<Local>, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }

        self.bytes_len = self.bytes_len.saturating_add(bytes.len());
        self.chunks.push_back(RxChunk {
            sequence: self.next_sequence,
            received_at,
            bytes: bytes.into(),
        });
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        self.trim_to_limit();
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.bytes_len = 0;
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

    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    pub fn snapshot(&self) -> ReceiveSnapshot {
        let first_sequence = self
            .chunks
            .front()
            .map_or(self.next_sequence, |chunk| chunk.sequence);
        ReceiveSnapshot {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            chunks: self.chunks.iter().cloned().collect(),
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
        let mut chunks = Vec::new();
        for chunk in self.chunks.iter().rev() {
            if remaining == 0 {
                break;
            }
            let take = chunk.bytes.len().min(remaining);
            let bytes = if take == chunk.bytes.len() {
                Arc::clone(&chunk.bytes)
            } else {
                Arc::from(&chunk.bytes[chunk.bytes.len() - take..])
            };
            chunks.push(RxChunk {
                sequence: chunk.sequence,
                received_at: chunk.received_at,
                bytes,
            });
            remaining -= take;
        }
        chunks.reverse();

        let bytes_len = max_bytes - remaining;
        let first_sequence = chunks
            .first()
            .map_or(self.next_sequence, |chunk| chunk.sequence);
        ReceiveSnapshot {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            chunks,
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
            .chunks
            .front()
            .map_or(self.next_sequence, |chunk| chunk.sequence);
        let mut reset_or_gap = cursor.stream_id != self.stream_id
            || cursor.next_sequence < first_sequence
            || cursor.next_sequence > self.next_sequence;
        let chunks = if reset_or_gap {
            Vec::new()
        } else {
            let start =
                usize::try_from(cursor.next_sequence - first_sequence).unwrap_or(self.chunks.len());
            let start = start.min(self.chunks.len());
            let mut bytes_len = 0_usize;
            for chunk in self.chunks.range(start..) {
                bytes_len = bytes_len.saturating_add(chunk.bytes.len());
                if bytes_len > max_bytes {
                    reset_or_gap = true;
                    break;
                }
            }
            if reset_or_gap {
                Vec::new()
            } else {
                self.chunks.range(start..).cloned().collect()
            }
        };

        ReceiveDelta {
            generation: self.generation,
            stream_id: self.stream_id,
            first_sequence,
            next_sequence: self.next_sequence,
            chunks,
            reset_or_gap,
        }
    }

    fn trim_to_limit(&mut self) {
        while self.bytes_len > self.limit_bytes {
            let Some(oldest) = self.chunks.pop_front() else {
                break;
            };
            self.bytes_len = self.bytes_len.saturating_sub(oldest.bytes.len());
            self.dropped_bytes = self.dropped_bytes.saturating_add(oldest.bytes.len() as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;

    #[test]
    fn bounded_store_drops_oldest_complete_chunks() {
        let mut store = ReceiveStore::new(5);
        store.append(Local::now(), vec![1, 2, 3]);
        store.append(Local::now(), vec![4, 5, 6]);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.bytes_len, 3);
        assert_eq!(snapshot.dropped_bytes, 3);
        assert_eq!(&*snapshot.chunks[0].bytes, &[4, 5, 6]);
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
        assert_eq!(delta.chunks.len(), 2);
        assert_eq!(&*delta.chunks[0].bytes, &[2]);
        assert_eq!(&*delta.chunks[1].bytes, &[3]);
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
        assert!(delta.chunks.is_empty());
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
        assert_eq!(&*snapshot.chunks[0].bytes, &[3]);
        assert_eq!(&*snapshot.chunks[1].bytes, &[4, 5, 6]);
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
        assert!(delta.chunks.is_empty());
        assert_eq!(delta.next_sequence, 2);
    }
}
