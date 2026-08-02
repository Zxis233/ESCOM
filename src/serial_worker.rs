use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::Local;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::model::SerialConfig;
use crate::store::ReceiveStore;

const WRITE_QUEUE_CAPACITY: usize = 128;
const WRITE_SLICE_BYTES: usize = 16 * 1024;
const DISCONNECTED_POLL_INTERVAL: Duration = Duration::from_millis(50);
const EMPTY_READ_BACKOFF: Duration = Duration::from_millis(1);

pub trait PortIo: Read + Write + Send {
    fn set_dtr(&mut self, level: bool) -> Result<(), String>;
    fn set_rts(&mut self, level: bool) -> Result<(), String>;
}

pub trait SerialBackend: Send + Sync + 'static {
    fn list_ports(&self) -> Result<Vec<String>, String>;
    fn open(&self, config: &SerialConfig) -> Result<Box<dyn PortIo>, String>;
}

#[derive(Default)]
pub struct ProductionBackend;

impl SerialBackend for ProductionBackend {
    fn list_ports(&self) -> Result<Vec<String>, String> {
        let mut ports: Vec<_> = serialport::available_ports()
            .map_err(|error| format!("无法枚举串口：{error}"))?
            .into_iter()
            .map(|port| port.port_name)
            .collect();
        ports.sort_by_key(|name| port_sort_key(name));
        Ok(ports)
    }

    fn open(&self, config: &SerialConfig) -> Result<Box<dyn PortIo>, String> {
        let port = serialport::new(&config.port_name, config.baud_rate)
            .data_bits(config.data_bits)
            .stop_bits(config.stop_bits)
            .parity(config.parity)
            .flow_control(config.flow_control)
            .timeout(Duration::from_millis(20))
            .dtr_on_open(config.dtr)
            .open()
            .map_err(|error| format!("打开 {} 失败：{error}", config.port_name))?;

        let mut port = NativePort(port);
        port.set_dtr(config.dtr)?;
        if config.flow_control != serialport::FlowControl::Hardware {
            port.set_rts(config.rts)?;
        }
        Ok(Box::new(port))
    }
}

struct NativePort(Box<dyn serialport::SerialPort>);

impl Read for NativePort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for NativePort {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl PortIo for NativePort {
    fn set_dtr(&mut self, level: bool) -> Result<(), String> {
        self.0
            .write_data_terminal_ready(level)
            .map_err(|error| format!("设置 DTR 失败：{error}"))
    }

    fn set_rts(&mut self, level: bool) -> Result<(), String> {
        self.0
            .write_request_to_send(level)
            .map_err(|error| format!("设置 RTS 失败：{error}"))
    }
}

#[derive(Debug)]
enum WorkerCommand {
    RefreshPorts,
    Open(SerialConfig),
    Close,
    SetDtr(bool),
    SetRts(bool),
    Shutdown,
}

#[derive(Debug)]
struct WriteRequest {
    id: u64,
    bytes: Vec<u8>,
}

struct PendingWrite {
    id: u64,
    bytes: Vec<u8>,
    offset: usize,
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Ports(Vec<String>),
    Opened(String),
    Closed,
    TxCompleted { id: u64, count: usize },
    TxFailed { id: u64, message: String },
    PortError(String),
    ControlError(String),
}

#[derive(Default)]
pub struct SerialStats {
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
}

impl SerialStats {
    pub fn reset(&self) {
        self.rx_bytes.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
    }

    pub fn rx_bytes(&self) -> u64 {
        self.rx_bytes.load(Ordering::Relaxed)
    }

    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes.load(Ordering::Relaxed)
    }
}

pub struct WorkerHandle {
    commands: Sender<WorkerCommand>,
    write_requests: Sender<WriteRequest>,
    pub events: Receiver<WorkerEvent>,
    pub stats: Arc<SerialStats>,
    thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn(store: Arc<Mutex<ReceiveStore>>) -> Self {
        Self::spawn_with_backend(store, Arc::new(ProductionBackend))
    }

    pub fn spawn_with_backend(
        store: Arc<Mutex<ReceiveStore>>,
        backend: Arc<dyn SerialBackend>,
    ) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (write_tx, write_rx) = bounded(WRITE_QUEUE_CAPACITY);
        let (event_tx, event_rx) = unbounded();
        let stats = Arc::new(SerialStats::default());
        let worker_stats = Arc::clone(&stats);
        let thread = thread::Builder::new()
            .name("escom-serial".into())
            .spawn(move || {
                worker_loop(backend, store, command_rx, write_rx, event_tx, worker_stats)
            })
            .expect("failed to start serial worker");

        Self {
            commands: command_tx,
            write_requests: write_tx,
            events: event_rx,
            stats,
            thread: Some(thread),
        }
    }

    pub fn refresh_ports(&self) -> Result<(), String> {
        self.send_command(WorkerCommand::RefreshPorts)
    }

    pub fn open(&self, config: SerialConfig) -> Result<(), String> {
        self.send_command(WorkerCommand::Open(config))
    }

    pub fn close(&self) -> Result<(), String> {
        self.send_command(WorkerCommand::Close)
    }

    pub fn send(&self, id: u64, bytes: Vec<u8>) -> Result<(), String> {
        self.write_requests
            .try_send(WriteRequest { id, bytes })
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(_) => "串口发送队列已满，请稍后重试".into(),
                crossbeam_channel::TrySendError::Disconnected(_) => "串口任务已停止".into(),
            })
    }

    pub fn set_dtr(&self, level: bool) -> Result<(), String> {
        self.send_command(WorkerCommand::SetDtr(level))
    }

    pub fn set_rts(&self, level: bool) -> Result<(), String> {
        self.send_command(WorkerCommand::SetRts(level))
    }

    fn send_command(&self, command: WorkerCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "串口任务已停止".into())
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    backend: Arc<dyn SerialBackend>,
    store: Arc<Mutex<ReceiveStore>>,
    commands: Receiver<WorkerCommand>,
    write_requests: Receiver<WriteRequest>,
    events: Sender<WorkerEvent>,
    stats: Arc<SerialStats>,
) {
    let mut port: Option<Box<dyn PortIo>> = None;
    let mut pending_write: Option<PendingWrite> = None;
    let mut read_buffer = vec![0_u8; 8192];

    loop {
        if port.is_none() {
            reject_queued_writes(&write_requests, &events, "串口尚未连接");
            match commands.recv_timeout(DISCONNECTED_POLL_INTERVAL) {
                Ok(command) => {
                    if handle_command(
                        command,
                        &backend,
                        &events,
                        &stats,
                        &mut port,
                        &mut pending_write,
                        &write_requests,
                    ) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            continue;
        }

        loop {
            match commands.try_recv() {
                Ok(command) => {
                    if handle_command(
                        command,
                        &backend,
                        &events,
                        &stats,
                        &mut port,
                        &mut pending_write,
                        &write_requests,
                    ) {
                        return;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return,
            }
        }

        if port.is_none() {
            continue;
        }

        if pending_write.is_none()
            && let Ok(request) = write_requests.try_recv()
        {
            pending_write = Some(PendingWrite {
                id: request.id,
                bytes: request.bytes,
                offset: 0,
            });
        }

        if let Some(active_port) = port.as_mut()
            && let Err(error) =
                write_next_slice(active_port.as_mut(), &mut pending_write, &events, &stats)
        {
            let _ = events.send(WorkerEvent::PortError(format!("串口写入失败：{error}")));
            port = None;
            discard_writes(&mut pending_write, &write_requests);
            let _ = events.send(WorkerEvent::Closed);
            continue;
        }

        let Some(active_port) = port.as_mut() else {
            continue;
        };

        match active_port.read(&mut read_buffer) {
            Ok(0) => thread::sleep(EMPTY_READ_BACKOFF),
            Ok(count) => {
                stats.rx_bytes.fetch_add(count as u64, Ordering::Relaxed);
                if let Ok(mut receive_store) = store.lock() {
                    receive_store.append(Local::now(), read_buffer[..count].to_vec());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                let _ = events.send(WorkerEvent::PortError(format!("串口读取失败：{error}")));
                port = None;
                discard_writes(&mut pending_write, &write_requests);
                let _ = events.send(WorkerEvent::Closed);
            }
        }
    }
}

fn handle_command(
    command: WorkerCommand,
    backend: &Arc<dyn SerialBackend>,
    events: &Sender<WorkerEvent>,
    stats: &Arc<SerialStats>,
    port: &mut Option<Box<dyn PortIo>>,
    pending_write: &mut Option<PendingWrite>,
    write_requests: &Receiver<WriteRequest>,
) -> bool {
    match command {
        WorkerCommand::RefreshPorts => match backend.list_ports() {
            Ok(ports) => {
                let _ = events.send(WorkerEvent::Ports(ports));
            }
            Err(error) => {
                let _ = events.send(WorkerEvent::ControlError(error));
            }
        },
        WorkerCommand::Open(config) => {
            discard_writes(pending_write, write_requests);
            *port = None;
            match backend.open(&config) {
                Ok(opened_port) => {
                    stats.reset();
                    *port = Some(opened_port);
                    let _ = events.send(WorkerEvent::Opened(config.port_name));
                }
                Err(error) => {
                    let _ = events.send(WorkerEvent::PortError(error));
                    let _ = events.send(WorkerEvent::Closed);
                }
            }
        }
        WorkerCommand::Close => {
            discard_writes(pending_write, write_requests);
            let was_open = port.take().is_some();
            if was_open {
                let _ = events.send(WorkerEvent::Closed);
            }
        }
        WorkerCommand::SetDtr(level) => {
            if let Some(active_port) = port.as_mut()
                && let Err(error) = active_port.set_dtr(level)
            {
                let _ = events.send(WorkerEvent::ControlError(error));
            }
        }
        WorkerCommand::SetRts(level) => {
            if let Some(active_port) = port.as_mut()
                && let Err(error) = active_port.set_rts(level)
            {
                let _ = events.send(WorkerEvent::ControlError(error));
            }
        }
        WorkerCommand::Shutdown => return true,
    }
    false
}

fn write_next_slice(
    port: &mut dyn PortIo,
    pending_write: &mut Option<PendingWrite>,
    events: &Sender<WorkerEvent>,
    stats: &Arc<SerialStats>,
) -> io::Result<()> {
    let Some(pending) = pending_write.as_mut() else {
        return Ok(());
    };

    if pending.offset == pending.bytes.len() {
        let id = pending.id;
        let count = pending.bytes.len();
        *pending_write = None;
        let _ = events.send(WorkerEvent::TxCompleted { id, count });
        return Ok(());
    }

    let slice_end = pending
        .offset
        .saturating_add(WRITE_SLICE_BYTES)
        .min(pending.bytes.len());
    let slice = &pending.bytes[pending.offset..slice_end];
    let written = match port.write(slice) {
        Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
        Ok(count) if count <= slice.len() => count,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "串口驱动返回了无效的写入字节数",
            ));
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    pending.offset += written;
    stats.tx_bytes.fetch_add(written as u64, Ordering::Relaxed);
    if pending.offset == pending.bytes.len() {
        let id = pending.id;
        let count = pending.bytes.len();
        *pending_write = None;
        let _ = events.send(WorkerEvent::TxCompleted { id, count });
    }
    Ok(())
}

fn discard_writes(
    pending_write: &mut Option<PendingWrite>,
    write_requests: &Receiver<WriteRequest>,
) {
    *pending_write = None;
    while write_requests.try_recv().is_ok() {}
}

fn reject_queued_writes(
    write_requests: &Receiver<WriteRequest>,
    events: &Sender<WorkerEvent>,
    message: &str,
) {
    while let Ok(request) = write_requests.try_recv() {
        let _ = events.send(WorkerEvent::TxFailed {
            id: request.id,
            message: message.into(),
        });
    }
}

fn port_sort_key(name: &str) -> (u32, String) {
    let uppercase = name.to_ascii_uppercase();
    let number = uppercase
        .strip_prefix("COM")
        .and_then(|value| value.parse().ok())
        .unwrap_or(u32::MAX);
    (number, uppercase)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    struct MockBackend {
        opened: AtomicBool,
        reads: Receiver<Vec<u8>>,
        writes: Arc<Mutex<Vec<u8>>>,
    }

    impl SerialBackend for MockBackend {
        fn list_ports(&self) -> Result<Vec<String>, String> {
            Ok(vec!["COM12".into(), "COM3".into()])
        }

        fn open(&self, _config: &SerialConfig) -> Result<Box<dyn PortIo>, String> {
            self.opened.store(true, Ordering::Relaxed);
            Ok(Box::new(MockPort {
                reads: self.reads.clone(),
                pending: VecDeque::new(),
                writes: Arc::clone(&self.writes),
            }))
        }
    }

    struct MockPort {
        reads: Receiver<Vec<u8>>,
        pending: VecDeque<u8>,
        writes: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for MockPort {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.pending.is_empty()
                && let Ok(bytes) = self.reads.recv_timeout(Duration::from_millis(5))
            {
                self.pending.extend(bytes);
            }
            if self.pending.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "idle"));
            }
            let count = buffer.len().min(self.pending.len());
            for target in &mut buffer[..count] {
                *target = self.pending.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for MockPort {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PortIo for MockPort {
        fn set_dtr(&mut self, _level: bool) -> Result<(), String> {
            Ok(())
        }

        fn set_rts(&mut self, _level: bool) -> Result<(), String> {
            Ok(())
        }
    }

    struct SchedulingBackend {
        total_bytes: usize,
        max_write_bytes: usize,
        read_delay: Duration,
        inject_read_between_writes: bool,
        written_bytes: Arc<AtomicUsize>,
        write_calls: Arc<Mutex<Vec<usize>>>,
        read_observed_at: Arc<AtomicUsize>,
    }

    impl SerialBackend for SchedulingBackend {
        fn list_ports(&self) -> Result<Vec<String>, String> {
            Ok(vec!["COM3".into()])
        }

        fn open(&self, _config: &SerialConfig) -> Result<Box<dyn PortIo>, String> {
            Ok(Box::new(SchedulingPort {
                total_bytes: self.total_bytes,
                max_write_bytes: self.max_write_bytes,
                read_delay: self.read_delay,
                inject_read_between_writes: self.inject_read_between_writes,
                written_bytes: Arc::clone(&self.written_bytes),
                write_calls: Arc::clone(&self.write_calls),
                read_observed_at: Arc::clone(&self.read_observed_at),
            }))
        }
    }

    struct SchedulingPort {
        total_bytes: usize,
        max_write_bytes: usize,
        read_delay: Duration,
        inject_read_between_writes: bool,
        written_bytes: Arc<AtomicUsize>,
        write_calls: Arc<Mutex<Vec<usize>>>,
        read_observed_at: Arc<AtomicUsize>,
    }

    impl Read for SchedulingPort {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let written = self.written_bytes.load(Ordering::SeqCst);
            if self.inject_read_between_writes
                && written > 0
                && written < self.total_bytes
                && self
                    .read_observed_at
                    .compare_exchange(usize::MAX, written, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                buffer[..2].copy_from_slice(b"rx");
                return Ok(2);
            }

            thread::sleep(self.read_delay);
            Err(io::Error::new(io::ErrorKind::TimedOut, "idle"))
        }
    }

    impl Write for SchedulingPort {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.max_write_bytes);
            self.write_calls.lock().unwrap().push(count);
            self.written_bytes.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl PortIo for SchedulingPort {
        fn set_dtr(&mut self, _level: bool) -> Result<(), String> {
            Ok(())
        }

        fn set_rts(&mut self, _level: bool) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn worker_moves_data_both_directions() {
        let (read_tx, read_rx) = unbounded();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(MockBackend {
            opened: AtomicBool::new(false),
            reads: read_rx,
            writes: Arc::clone(&writes),
        });
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let mut worker = WorkerHandle::spawn_with_backend(Arc::clone(&store), backend);
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        read_tx.send(b"incoming".to_vec()).unwrap();
        worker.send(7, b"outgoing".to_vec()).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::TxCompleted { id: 7, .. })
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while worker.stats.rx_bytes() == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(worker.stats.rx_bytes(), 8);
        assert_eq!(worker.stats.tx_bytes(), 8);
        assert_eq!(&*writes.lock().unwrap(), b"outgoing");
        assert_eq!(store.lock().unwrap().bytes_len(), 8);
        worker.shutdown();
    }

    #[test]
    fn worker_fragments_writes_and_reads_between_slices() {
        let total_bytes = WRITE_SLICE_BYTES * 2 + 23;
        let written_bytes = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(Mutex::new(Vec::new()));
        let read_observed_at = Arc::new(AtomicUsize::new(usize::MAX));
        let backend = Arc::new(SchedulingBackend {
            total_bytes,
            max_write_bytes: usize::MAX,
            read_delay: Duration::from_millis(1),
            inject_read_between_writes: true,
            written_bytes: Arc::clone(&written_bytes),
            write_calls: Arc::clone(&write_calls),
            read_observed_at: Arc::clone(&read_observed_at),
        });
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let mut worker = WorkerHandle::spawn_with_backend(Arc::clone(&store), backend);
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        worker.send(9, vec![0xA5; total_bytes]).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(
                event,
                WorkerEvent::TxCompleted {
                    id: 9,
                    count
                } if *count == total_bytes
            )
        });

        assert_eq!(written_bytes.load(Ordering::SeqCst), total_bytes);
        assert_eq!(
            &*write_calls.lock().unwrap(),
            &[WRITE_SLICE_BYTES, WRITE_SLICE_BYTES, 23]
        );
        assert_eq!(read_observed_at.load(Ordering::SeqCst), WRITE_SLICE_BYTES);
        assert_eq!(worker.stats.tx_bytes(), total_bytes as u64);
        assert_eq!(worker.stats.rx_bytes(), 2);
        assert_eq!(store.lock().unwrap().bytes_len(), 2);
        worker.shutdown();
    }

    #[test]
    fn close_interrupts_an_in_progress_write() {
        let total_bytes = 4096;
        let written_bytes = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(SchedulingBackend {
            total_bytes,
            max_write_bytes: 1,
            read_delay: Duration::from_millis(2),
            inject_read_between_writes: false,
            written_bytes: Arc::clone(&written_bytes),
            write_calls,
            read_observed_at: Arc::new(AtomicUsize::new(usize::MAX)),
        });
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let mut worker = WorkerHandle::spawn_with_backend(store, backend);
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        worker.send(10, vec![0x5A; total_bytes]).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while written_bytes.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(written_bytes.load(Ordering::SeqCst) > 0);

        worker.close().unwrap();
        wait_for_event(&worker.events, |event| matches!(event, WorkerEvent::Closed));
        assert!(written_bytes.load(Ordering::SeqCst) < total_bytes);
        worker.shutdown();
    }

    fn wait_for_event(events: &Receiver<WorkerEvent>, predicate: impl Fn(&WorkerEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(remaining).expect("worker event");
            if predicate(&event) {
                return;
            }
        }
    }
}
