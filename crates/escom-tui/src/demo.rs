use escom_core::error::CoreError;
use escom_core::{
    model::SerialConfig,
    serial_worker::{PortIo, SerialBackend},
};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub struct DemoBackend;
impl SerialBackend for DemoBackend {
    fn list_ports(&self) -> Result<Vec<String>, CoreError> {
        Ok(vec!["DEMO".into()])
    }
    fn open(&self, _: &SerialConfig) -> Result<Box<dyn PortIo>, CoreError> {
        Ok(Box::new(DemoPort {
            next: Instant::now(),
            sequence: 0,
            echo: Vec::new(),
        }))
    }
}
struct DemoPort {
    next: Instant,
    sequence: u64,
    echo: Vec<u8>,
}
impl Read for DemoPort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.echo.is_empty() {
            let count = buffer.len().min(self.echo.len());
            buffer[..count].copy_from_slice(&self.echo[..count]);
            self.echo.drain(..count);
            return Ok(count);
        }
        if Instant::now() < self.next {
            std::thread::sleep(Duration::from_millis(10));
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.next = Instant::now() + Duration::from_millis(100);
        self.sequence += 1;
        let line = format!(
            "[{:06}] {} sensor={}.{} C | serial demo\r\n",
            self.sequence,
            if self.sequence.is_multiple_of(20) {
                "WARN"
            } else {
                "INFO"
            },
            24 + self.sequence % 4,
            self.sequence % 10
        );
        let count = buffer.len().min(line.len());
        buffer[..count].copy_from_slice(&line.as_bytes()[..count]);
        Ok(count)
    }
}
impl Write for DemoPort {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.echo.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl PortIo for DemoPort {
    fn set_dtr(&mut self, _: bool) -> Result<(), CoreError> {
        Ok(())
    }
    fn set_rts(&mut self, _: bool) -> Result<(), CoreError> {
        Ok(())
    }
}
