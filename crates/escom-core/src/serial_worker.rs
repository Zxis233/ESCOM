use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::Local;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::budget::{ByteBudget, Reservation};
use crate::capture::CaptureSink;
use crate::model::SerialConfig;
use crate::store::ReceiveStore;

const WRITE_QUEUE_CAPACITY: usize = 128;
const WRITE_SLICE_BYTES: usize = 16 * 1024;
const DISCONNECTED_POLL_INTERVAL: Duration = Duration::from_millis(50);
const EMPTY_READ_BACKOFF: Duration = Duration::from_millis(1);

struct WorkerLifecycleLog;

impl Drop for WorkerLifecycleLog {
    fn drop(&mut self) {
        log::info!(target: "escom::serial", "serial worker stopped");
    }
}

pub type UiWake = Arc<dyn Fn() + Send + Sync + 'static>;

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
    SetCapture(Option<CaptureSink>, Sender<()>),
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
    bytes: Box<[u8]>,
    reservation: Reservation,
}

struct PendingWrite {
    id: u64,
    bytes: Box<[u8]>,
    offset: usize,
    _reservation: Reservation,
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Ports(Vec<String>),
    Opened(String),
    Closed { error: Option<String> },
    TxCompleted { id: u64, count: usize },
    TxFailed { id: u64, message: String },
    ControlError(String),
}

struct WorkerNotifier {
    events: Sender<WorkerEvent>,
    wake_ui: UiWake,
}

impl WorkerNotifier {
    fn new(events: Sender<WorkerEvent>, wake_ui: UiWake) -> Self {
        Self { events, wake_ui }
    }

    fn emit(&self, event: WorkerEvent) {
        if self.events.send(event).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        (self.wake_ui.as_ref())();
    }
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

#[derive(Debug, Clone, Copy)]
pub struct WorkerOptions {
    pub max_queued_write_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            max_queued_write_bytes: 4 * 1024 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

pub struct WorkerHandle {
    write_budget: Arc<ByteBudget>,
    max_write_bytes: usize,
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

    pub fn spawn_with_wake(store: Arc<Mutex<ReceiveStore>>, wake_ui: UiWake) -> Self {
        Self::spawn_with_backend_and_wake(store, Arc::new(ProductionBackend), wake_ui)
    }

    pub fn spawn_with_backend(
        store: Arc<Mutex<ReceiveStore>>,
        backend: Arc<dyn SerialBackend>,
    ) -> Self {
        Self::spawn_with_backend_and_wake(store, backend, Arc::new(|| {}))
    }

    pub fn spawn_with_backend_and_wake(
        store: Arc<Mutex<ReceiveStore>>,
        backend: Arc<dyn SerialBackend>,
        wake_ui: UiWake,
    ) -> Self {
        Self::spawn_with_options(store, backend, wake_ui, WorkerOptions::default())
    }

    pub fn spawn_with_options(
        store: Arc<Mutex<ReceiveStore>>,
        backend: Arc<dyn SerialBackend>,
        wake_ui: UiWake,
        options: WorkerOptions,
    ) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (write_tx, write_rx) = bounded(WRITE_QUEUE_CAPACITY);
        let (event_tx, event_rx) = unbounded();
        let stats = Arc::new(SerialStats::default());
        let worker_stats = Arc::clone(&stats);
        let thread = thread::Builder::new()
            .name("escom-serial".into())
            .spawn(move || {
                worker_loop(
                    backend,
                    store,
                    command_rx,
                    write_rx,
                    event_tx,
                    worker_stats,
                    wake_ui,
                )
            })
            .expect("failed to start serial worker");

        Self {
            write_budget: ByteBudget::new(options.max_queued_write_bytes),
            max_write_bytes: options.max_write_bytes,
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
        if bytes.len() > self.max_write_bytes {
            return Err(format!("单次发送超过 {} 字节限制", self.max_write_bytes));
        }
        let reservation = self
            .write_budget
            .reserve(bytes.len())
            .ok_or_else(|| "串口发送积压已达到字节上限，请稍后重试".to_owned())?;
        self.write_requests
            .try_send(WriteRequest {
                id,
                bytes: bytes.into_boxed_slice(),
                reservation,
            })
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(_) => "串口发送队列已满，请稍后重试".into(),
                crossbeam_channel::TrySendError::Disconnected(_) => "串口任务已停止".into(),
            })
    }

    pub fn queued_write_bytes(&self) -> usize {
        self.write_budget.used()
    }

    pub fn set_capture(&self, sink: Option<CaptureSink>) -> Result<(), String> {
        let (sender, receiver) = bounded(1);
        self.send_command(WorkerCommand::SetCapture(sink, sender))?;
        receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "串口任务未确认记录切换".to_owned())
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
    wake_ui: UiWake,
) {
    log::info!(target: "escom::serial", "serial worker started");
    let _lifecycle_log = WorkerLifecycleLog;
    let events = WorkerNotifier::new(events, wake_ui);
    let mut port: Option<Box<dyn PortIo>> = None;
    let mut pending_write: Option<PendingWrite> = None;
    let mut capture: Option<CaptureSink> = None;
    let mut read_buffer = vec![0_u8; 8192];
    let command_context = CommandContext {
        backend: backend.as_ref(),
        events: &events,
        stats: stats.as_ref(),
        store: &store,
        write_requests: &write_requests,
    };

    'worker: loop {
        if port.is_none() {
            reject_queued_writes(&write_requests, &events, "串口尚未连接");
            match commands.recv_timeout(DISCONNECTED_POLL_INTERVAL) {
                Ok(command) => {
                    if handle_command(
                        command,
                        &command_context,
                        &mut port,
                        &mut pending_write,
                        &mut capture,
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
                        &command_context,
                        &mut port,
                        &mut pending_write,
                        &mut capture,
                    ) {
                        break 'worker;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break 'worker,
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
                _reservation: request.reservation,
            });
        }

        if let Some(active_port) = port.as_mut()
            && let Err(error) =
                write_next_slice(active_port.as_mut(), &mut pending_write, &events, &stats)
        {
            log::error!(target: "escom::serial", "serial write failed: {error}");
            port = None;
            mark_receive_boundary(&store);
            discard_writes(&mut pending_write, &write_requests, &events);
            events.emit(WorkerEvent::Closed {
                error: Some(format!("串口写入失败：{error}")),
            });
            continue;
        }

        let Some(active_port) = port.as_mut() else {
            continue;
        };

        match active_port.read(&mut read_buffer) {
            Ok(0) => thread::sleep(EMPTY_READ_BACKOFF),
            Ok(count) => {
                if let Some(sink) = &capture {
                    sink.record(&read_buffer[..count]);
                }
                stats.rx_bytes.fetch_add(count as u64, Ordering::Relaxed);
                let appended = if let Ok(mut receive_store) = store.lock() {
                    receive_store.append(Local::now(), read_buffer[..count].to_vec());
                    true
                } else {
                    false
                };
                if appended {
                    events.wake();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                log::error!(target: "escom::serial", "serial read failed: {error}");
                port = None;
                mark_receive_boundary(&store);
                discard_writes(&mut pending_write, &write_requests, &events);
                events.emit(WorkerEvent::Closed {
                    error: Some(format!("串口读取失败：{error}")),
                });
            }
        }
    }

    discard_writes(&mut pending_write, &write_requests, &events);
    if port.take().is_some() {
        mark_receive_boundary(&store);
    }
}

struct CommandContext<'a> {
    backend: &'a dyn SerialBackend,
    events: &'a WorkerNotifier,
    stats: &'a SerialStats,
    store: &'a Arc<Mutex<ReceiveStore>>,
    write_requests: &'a Receiver<WriteRequest>,
}

fn handle_command(
    command: WorkerCommand,
    context: &CommandContext<'_>,
    port: &mut Option<Box<dyn PortIo>>,
    pending_write: &mut Option<PendingWrite>,
    capture: &mut Option<CaptureSink>,
) -> bool {
    match command {
        WorkerCommand::SetCapture(sink, acknowledged) => {
            *capture = sink;
            let _ = acknowledged.send(());
        }
        WorkerCommand::RefreshPorts => match context.backend.list_ports() {
            Ok(ports) => {
                context.events.emit(WorkerEvent::Ports(ports));
            }
            Err(error) => {
                log::warn!(target: "escom::serial", "port enumeration failed: {error}");
                context.events.emit(WorkerEvent::ControlError(error));
            }
        },
        WorkerCommand::Open(config) => {
            log::info!(
                target: "escom::serial",
                "opening port={} baud={}",
                config.port_name,
                config.baud_rate
            );
            discard_writes(pending_write, context.write_requests, context.events);
            if port.take().is_some() {
                mark_receive_boundary(context.store);
            }
            match context.backend.open(&config) {
                Ok(opened_port) => {
                    context.stats.reset();
                    *port = Some(opened_port);
                    log::info!(target: "escom::serial", "port opened: {}", config.port_name);
                    context.events.emit(WorkerEvent::Opened(config.port_name));
                }
                Err(error) => {
                    log::error!(target: "escom::serial", "port open failed: {error}");
                    context
                        .events
                        .emit(WorkerEvent::Closed { error: Some(error) });
                }
            }
        }
        WorkerCommand::Close => {
            discard_writes(pending_write, context.write_requests, context.events);
            let was_open = port.take().is_some();
            if was_open {
                mark_receive_boundary(context.store);
                log::info!(target: "escom::serial", "port closed by user");
                context.events.emit(WorkerEvent::Closed { error: None });
            }
        }
        WorkerCommand::SetDtr(level) => {
            if let Some(active_port) = port.as_mut()
                && let Err(error) = active_port.set_dtr(level)
            {
                log::warn!(target: "escom::serial", "DTR update failed: {error}");
                context.events.emit(WorkerEvent::ControlError(error));
            }
        }
        WorkerCommand::SetRts(level) => {
            if let Some(active_port) = port.as_mut()
                && let Err(error) = active_port.set_rts(level)
            {
                log::warn!(target: "escom::serial", "RTS update failed: {error}");
                context.events.emit(WorkerEvent::ControlError(error));
            }
        }
        WorkerCommand::Shutdown => return true,
    }
    false
}

fn mark_receive_boundary(store: &Arc<Mutex<ReceiveStore>>) {
    match store.lock() {
        Ok(mut receive_store) => {
            receive_store.mark_stream_boundary(Local::now());
        }
        Err(_) => {
            log::error!(target: "escom::serial", "receive store lock poisoned while marking a stream boundary");
        }
    }
}

fn write_next_slice(
    port: &mut dyn PortIo,
    pending_write: &mut Option<PendingWrite>,
    events: &WorkerNotifier,
    stats: &Arc<SerialStats>,
) -> io::Result<()> {
    let Some(pending) = pending_write.as_mut() else {
        return Ok(());
    };

    if pending.offset == pending.bytes.len() {
        let id = pending.id;
        let count = pending.bytes.len();
        *pending_write = None;
        events.emit(WorkerEvent::TxCompleted { id, count });
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
        events.emit(WorkerEvent::TxCompleted { id, count });
    }
    Ok(())
}

fn discard_writes(
    pending_write: &mut Option<PendingWrite>,
    write_requests: &Receiver<WriteRequest>,
    events: &WorkerNotifier,
) {
    if let Some(pending) = pending_write.take() {
        events.emit(WorkerEvent::TxFailed {
            id: pending.id,
            message: format!(
                "发送 #{} 已取消：已写入 {}/{} 字节",
                pending.id,
                pending.offset,
                pending.bytes.len()
            ),
        });
    }
    reject_queued_writes(
        write_requests,
        events,
        "发送已取消：串口关闭或重新连接，已写入 0 字节",
    );
}

fn reject_queued_writes(
    write_requests: &Receiver<WriteRequest>,
    events: &WorkerNotifier,
    message: &str,
) {
    while let Ok(request) = write_requests.try_recv() {
        events.emit(WorkerEvent::TxFailed {
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
    use crate::store::ReceiveRecord;

    #[test]
    fn send_budget_includes_active_request_and_refunds_cancellation() {
        let (commands, _) = unbounded();
        let (writes, requests) = bounded(1);
        let (events, receiver) = unbounded();
        let budget = ByteBudget::new(8);
        let worker = WorkerHandle {
            commands,
            write_requests: writes,
            events: receiver.clone(),
            stats: Arc::new(SerialStats::default()),
            thread: None,
            write_budget: Arc::clone(&budget),
            max_write_bytes: 8,
        };
        assert!(worker.send(0, vec![0; 9]).is_err());
        worker.send(1, vec![0; 8]).unwrap();
        assert_eq!(worker.queued_write_bytes(), 8);
        let request = requests.recv().unwrap();
        assert!(worker.send(2, vec![0; 1]).is_err());
        let mut active = Some(PendingWrite {
            id: request.id,
            bytes: request.bytes,
            offset: 2,
            _reservation: request.reservation,
        });
        let notifier = WorkerNotifier::new(events, Arc::new(|| {}));
        discard_writes(&mut active, &requests, &notifier);
        assert_eq!(worker.queued_write_bytes(), 0);
        assert!(
            matches!(receiver.recv().unwrap(), WorkerEvent::TxFailed { id: 1, message } if message.contains("2/8"))
        );
        worker.send(3, vec![0; 4]).unwrap();
        assert!(worker.send(4, vec![0; 4]).is_err()); // item limit also refunds reservation
        assert_eq!(worker.queued_write_bytes(), 4);
        drop(requests.recv().unwrap());
        assert_eq!(worker.queued_write_bytes(), 0);
    }

    #[test]
    fn capture_keeps_bytes_already_evicted_from_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rx.bin");
        let mut capture = crate::capture::CaptureHandle::start(&path, 65536).unwrap();
        let (read_tx, read_rx) = unbounded();
        let backend = Arc::new(MockBackend {
            opened: AtomicBool::new(false),
            reads: read_rx,
            writes: Arc::new(Mutex::new(Vec::new())),
        });
        let store = Arc::new(Mutex::new(ReceiveStore::with_limits(64, 8)));
        let mut worker = WorkerHandle::spawn_with_backend(Arc::clone(&store), backend);
        worker.set_capture(Some(capture.sink())).unwrap();
        worker
            .open(SerialConfig {
                port_name: "COM3".into(),
                ..Default::default()
            })
            .unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        let bytes: Vec<u8> = (0..16384).map(|i| (i % 256) as u8).collect();
        read_tx.send(bytes.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.stats.rx_bytes() < bytes.len() as u64 && Instant::now() < deadline {
            thread::yield_now();
        }
        worker.set_capture(None).unwrap();
        capture.finish().unwrap();
        worker.shutdown();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(store.lock().unwrap().bytes_len() <= 64);
        assert_eq!(store.lock().unwrap().dropped_bytes(), 16384);
    }

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

    struct FailingOpenBackend;

    impl SerialBackend for FailingOpenBackend {
        fn list_ports(&self) -> Result<Vec<String>, String> {
            Ok(vec!["COM3".into()])
        }

        fn open(&self, config: &SerialConfig) -> Result<Box<dyn PortIo>, String> {
            Err(format!("打开 {} 失败：端口被占用", config.port_name))
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
    fn close_and_reopen_insert_an_ordered_receive_boundary() {
        let (read_tx, read_rx) = unbounded();
        let backend = Arc::new(MockBackend {
            opened: AtomicBool::new(false),
            reads: read_rx,
            writes: Arc::new(Mutex::new(Vec::new())),
        });
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let mut worker = WorkerHandle::spawn_with_backend(Arc::clone(&store), backend);
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config.clone()).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        read_tx.send(b"old".to_vec()).unwrap();
        wait_for_store_bytes(&store, 3);

        worker.close().unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Closed { error: None })
        });
        let closed_snapshot = store.lock().unwrap().snapshot();
        assert_eq!(closed_snapshot.records.len(), 2);
        assert!(matches!(
            closed_snapshot.records[1],
            ReceiveRecord::Boundary(_)
        ));

        worker.open(config).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        read_tx.send(b"new".to_vec()).unwrap();
        wait_for_store_bytes(&store, 6);

        let snapshot = store.lock().unwrap().snapshot();
        assert_eq!(snapshot.records.len(), 3);
        let ReceiveRecord::Data(new_chunk) = &snapshot.records[2] else {
            panic!("new session data record expected");
        };
        assert_eq!(&*new_chunk.bytes, b"new");
        assert_eq!(new_chunk.session_offset, 0);
        worker.shutdown();
    }

    #[test]
    fn open_failure_is_reported_by_one_error_close_event() {
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let mut worker = WorkerHandle::spawn_with_backend(store, Arc::new(FailingOpenBackend));
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config).unwrap();
        let event = worker
            .events
            .recv_timeout(Duration::from_secs(1))
            .expect("open failure event");
        match event {
            WorkerEvent::Closed { error: Some(error) } => {
                assert_eq!(error, "打开 COM3 失败：端口被占用");
            }
            other => panic!("unexpected worker event: {other:?}"),
        }
        assert!(matches!(
            worker.events.recv_timeout(Duration::from_millis(20)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
        worker.shutdown();
    }

    #[test]
    fn worker_wakes_ui_for_events_and_received_data_but_not_idle_reads() {
        let (read_tx, read_rx) = unbounded();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(MockBackend {
            opened: AtomicBool::new(false),
            reads: read_rx,
            writes,
        });
        let store = Arc::new(Mutex::new(ReceiveStore::new(1024)));
        let wake_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&wake_count);
        let mut worker = WorkerHandle::spawn_with_backend_and_wake(
            Arc::clone(&store),
            backend,
            Arc::new(move || {
                callback_count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let config = SerialConfig {
            port_name: "COM3".into(),
            ..Default::default()
        };

        worker.open(config).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Opened(_))
        });
        wait_for_counter_to_exceed(&wake_count, 0);

        let after_open = wake_count.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(wake_count.load(Ordering::SeqCst), after_open);

        read_tx.send(b"incoming".to_vec()).unwrap();
        wait_for_counter_to_exceed(&wake_count, after_open);
        assert_eq!(store.lock().unwrap().bytes_len(), 8);

        let after_receive = wake_count.load(Ordering::SeqCst);
        worker.send(11, b"outgoing".to_vec()).unwrap();
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::TxCompleted { id: 11, .. })
        });
        wait_for_counter_to_exceed(&wake_count, after_receive);
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
        wait_for_event(&worker.events, |event| {
            matches!(event, WorkerEvent::Closed { error: None })
        });
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

    fn wait_for_counter_to_exceed(counter: &AtomicUsize, previous: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while counter.load(Ordering::SeqCst) <= previous && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(counter.load(Ordering::SeqCst) > previous);
    }

    fn wait_for_store_bytes(store: &Arc<Mutex<ReceiveStore>>, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while store.lock().unwrap().bytes_len() < expected && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(store.lock().unwrap().bytes_len(), expected);
    }
}
