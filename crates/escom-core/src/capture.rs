//! Exact RX bytes, streamed independently of the evicting display history.
//! A slow/full disk stops capture visibly; serial reception is never blocked by the queue.
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::budget::{ByteBudget, Reservation};
use crossbeam_channel::{Receiver, Sender, bounded};

const BLOCK_BYTES: usize = 8192;

#[derive(Debug)]
struct Block {
    bytes: Box<[u8]>,
    _reservation: Reservation,
}

#[derive(Debug, Default)]
struct Status {
    stopped: AtomicBool,
    failed: AtomicBool,
    finished: AtomicBool,
    written: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Status {
    fn fail(&self, message: String) {
        if let Ok(mut error) = self.error.lock() {
            if error.is_none() {
                *error = Some(message);
            }
            self.failed.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureSink {
    sender: Sender<Block>,
    budget: Arc<ByteBudget>,
    status: Arc<Status>,
}

impl CaptureSink {
    /// Nonblocking. On overflow the recording is permanently marked incomplete.
    pub fn record(&self, bytes: &[u8]) {
        for bytes in bytes.chunks(BLOCK_BYTES) {
            if self.status.stopped.load(Ordering::Acquire)
                || self.status.failed.load(Ordering::Acquire)
            {
                return;
            }
            let Some(reservation) = self.budget.reserve(bytes.len()) else {
                self.status.fail(
                    "Recording stopped: disk queue byte limit reached; file is incomplete".into(),
                );
                return;
            };
            let block = Block {
                bytes: bytes.into(),
                _reservation: reservation,
            };
            if self.sender.try_send(block).is_err() {
                self.status.fail(
                    "Recording stopped: disk queue unavailable/full; file is incomplete".into(),
                );
                return;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureProgress {
    pub written_bytes: u64,
    pub queued_bytes: usize,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct CaptureHandle {
    sink: CaptureSink,
    thread: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    /// Never overwrites an existing recording. Buffer budget includes the active block.
    pub fn start(path: &Path, queue_bytes: usize) -> io::Result<Self> {
        if !(BLOCK_BYTES..=64 * 1024 * 1024).contains(&queue_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capture queue must be 8 KiB..64 MiB",
            ));
        }
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        // Also bound metadata, while allowing bursts of small serial reads.
        let (sender, receiver) = bounded(queue_bytes.div_ceil(256).min(4096));
        let status = Arc::new(Status::default());
        let sink = CaptureSink {
            sender,
            budget: ByteBudget::new(queue_bytes),
            status: Arc::clone(&status),
        };
        let thread = thread::Builder::new()
            .name("escom-capture".into())
            .spawn(move || {
                write_capture(file, receiver, &status);
                status.finished.store(true, Ordering::Release);
            })?;
        Ok(Self {
            sink,
            thread: Some(thread),
        })
    }

    pub fn sink(&self) -> CaptureSink {
        self.sink.clone()
    }

    pub fn progress(&self) -> CaptureProgress {
        CaptureProgress {
            written_bytes: self.sink.status.written.load(Ordering::Acquire),
            queued_bytes: self.sink.budget.used(),
            finished: self.sink.status.finished.load(Ordering::Acquire),
            error: self
                .sink
                .status
                .error
                .lock()
                .ok()
                .and_then(|error| error.clone()),
        }
    }

    /// Call after detaching the sink / shutting down the producer. Drains accepted blocks.
    pub fn finish(&mut self) -> io::Result<()> {
        self.sink.status.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("capture thread panicked"))?;
        }
        match self.progress().error {
            Some(error) => Err(io::Error::other(error)),
            None => Ok(()),
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn write_capture(file: File, receiver: Receiver<Block>, status: &Status) {
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    let mut last_flush = Instant::now();
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(block) => {
                if let Err(error) = writer.write_all(&block.bytes) {
                    status.fail(format!(
                        "Recording write failed: {error}; file is incomplete"
                    ));
                    break;
                }
                status
                    .written
                    .fetch_add(block.bytes.len() as u64, Ordering::Release);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if status.stopped.load(Ordering::Acquire) || status.failed.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if last_flush.elapsed() >= Duration::from_secs(1) {
            if let Err(error) = writer.flush() {
                status.fail(format!(
                    "Recording flush failed: {error}; file is incomplete"
                ));
                break;
            }
            last_flush = Instant::now();
        }
    }
    if let Err(error) = writer.flush().and_then(|()| writer.get_ref().sync_all()) {
        status.fail(format!(
            "Recording final flush failed: {error}; file may be incomplete"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drains_exact_binary_data_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.bin");
        let mut capture = CaptureHandle::start(&path, 32768).unwrap();
        let bytes = [0, 255, 27, 13, 10, 128];
        capture.sink().record(&bytes);
        capture.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(capture.progress().queued_bytes, 0);
        assert!(CaptureHandle::start(&path, 8192).is_err());
    }

    #[test]
    fn saturated_queue_fails_visibly_without_blocking_or_growing() {
        let (sender, receiver) = bounded(1);
        let sink = CaptureSink {
            sender,
            budget: ByteBudget::new(8192),
            status: Arc::new(Status::default()),
        };
        sink.record(&vec![0; 8192]);
        sink.record(b"overflow");
        assert!(sink.status.failed.load(Ordering::Acquire));
        assert!(sink.status.error.lock().unwrap().is_some());
        assert_eq!(sink.budget.used(), 8192);
        drop(receiver.recv().unwrap());
        assert_eq!(sink.budget.used(), 0);
        sink.record(b"must not resume silently");
        assert!(receiver.try_recv().is_err());
    }
}
