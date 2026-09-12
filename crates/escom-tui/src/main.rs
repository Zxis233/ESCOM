mod app;
mod config;
mod demo;
mod display;
mod text;
mod ui;

use app::App;
use config::{Action, Config, io_error};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste},
    execute,
};
use escom_core::serial_worker::{ProductionBackend, SerialBackend};
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

fn main() {
    if let Err(error) = run() {
        eprintln!("ESCOM TUI: {}", text::english_error(&error.to_string()));
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let (config, action) = Config::parse(std::env::args().skip(1)).map_err(io_error)?;
    match action {
        Action::Help => {
            print!("{}", config::HELP);
            return Ok(());
        }
        Action::Version => {
            println!("escom-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Action::PrintConfig => {
            println!(
                "{}",
                toml::to_string_pretty(&config).map_err(|e| io::Error::other(e.to_string()))?
            );
            return Ok(());
        }
        Action::List => {
            let ports = ProductionBackend.list_ports().map_err(io_error)?;
            if ports.is_empty() {
                println!("No serial ports found");
            }
            for port in ports {
                println!("{port}");
            }
            return Ok(());
        }
        Action::Run => {}
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "An interactive terminal is required. Try Windows Terminal / PowerShell, or --help.",
        ));
    }
    let mut app = App::new(config).map_err(io_error)?;
    let result = (|| {
        let mut terminal = ratatui::try_init()?;
        let _restore = RestoreTerminal;
        execute!(io::stdout(), EnableBracketedPaste)?;
        let mut next_tick = Instant::now();
        let mut dirty = true;
        loop {
            if Instant::now() >= next_tick {
                dirty |= app.tick();
                next_tick = Instant::now() + Duration::from_millis(50);
            }
            if dirty {
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
                dirty = false;
            }
            if event::poll(next_tick.saturating_duration_since(Instant::now()))? {
                if app.handle_event(event::read()?) {
                    break;
                }
                dirty = true;
            }
        }
        Ok(())
    })();
    // Serial producer stops first, then capture drains and syncs accepted bytes.
    let shutdown = app.shutdown().map_err(io_error);
    result.and(shutdown)
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}
