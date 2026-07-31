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
    pub chunks: Vec<RxChunk>,
    pub bytes_len: usize,
    pub dropped_bytes: u64,
}

#[derive(Debug)]
pub struct ReceiveStore {
    chunks: VecDeque<RxChunk>,
    bytes_len: usize,
    limit_bytes: usize,
    next_sequence: u64,
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
        ReceiveSnapshot {
            generation: self.generation,
            chunks: self.chunks.iter().cloned().collect(),
            bytes_len: self.bytes_len,
            dropped_bytes: self.dropped_bytes,
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
}
