//! Argument parsing and synchronous hardware subcommands (`console`, `on`,
//! `off`, `reset`). These never touch tokio — they drive `esp_monitor::reader`
//! directly from a plain `fn main`. The `mcp` subcommand is dispatched by
//! `main.rs` separately since it's the only thing that needs an async runtime.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use esp_monitor::reader::{self, ReaderConfig, ReaderHandle, ResetOptions};

#[derive(Parser)]
#[command(
    name = "esp-monitor",
    version,
    about = "Serial monitor and MCP server for ESP8266/ESP32 dev boards"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Reset the board (unless --no-reboot) and stream its serial console
    Console(ConsoleArgs),
    /// Reset the board and stream its serial console
    On(ConsoleArgs),
    /// Hold the board in reset/power-off and exit
    Off(PortArgs),
    /// Reset the board and stream its serial console (same as `on`)
    Reset(ConsoleArgs),
    /// Start the MCP server (stdio transport)
    Mcp(McpArgs),
}

impl Command {
    pub fn verbose(&self) -> u8 {
        match self {
            Command::Console(a) | Command::On(a) | Command::Reset(a) => a.port.verbose,
            Command::Off(a) => a.verbose,
            Command::Mcp(a) => a.port.verbose,
        }
    }
}

#[derive(Args, Clone)]
pub struct PortArgs {
    #[arg(long, default_value = "/dev/ttyUSB0")]
    pub port: String,
    #[arg(long, default_value_t = 115_200)]
    pub baud: u32,
    /// Increase output verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Args, Clone)]
pub struct ResetTuning {
    #[arg(long, default_value_t = 5)]
    pub reset_retries: u32,
    #[arg(long, default_value_t = 2000)]
    pub reset_timeout_ms: u64,
    #[arg(long, default_value_t = 100)]
    pub reset_pulse_ms: u64,
}

impl ResetTuning {
    pub(crate) fn to_opts(&self) -> ResetOptions {
        ResetOptions {
            pulse: Duration::from_millis(self.reset_pulse_ms),
            confirm_timeout: Duration::from_millis(self.reset_timeout_ms),
            max_retries: self.reset_retries,
        }
    }
}

#[derive(Args, Clone)]
pub struct ConsoleArgs {
    #[command(flatten)]
    pub port: PortArgs,
    #[command(flatten)]
    pub reset: ResetTuning,
    /// Persist the session to this file (a companion `*.stats.*` file
    /// captures any `/* ... */` system-stat packets separately)
    #[arg(long)]
    pub log_path: Option<PathBuf>,
    /// Seconds to log before exiting (-1 = run until Ctrl-C)
    #[arg(long, default_value_t = -1, allow_negative_numbers = true)]
    pub log_time: i64,
    /// Do not print board output to the console
    #[arg(long)]
    pub no_console: bool,
    /// Do not reset the board before streaming (console/reset only; on/reset
    /// resetting is the point, but console can skip it to just attach)
    #[arg(long)]
    pub no_reboot: bool,
}

#[derive(Args, Clone)]
pub struct McpArgs {
    #[command(flatten)]
    pub port: PortArgs,
    #[command(flatten)]
    pub reset: ResetTuning,
    /// Optionally start file logging immediately when the server connects
    #[arg(long)]
    pub log_path: Option<PathBuf>,
    #[arg(long, default_value_t = 2000)]
    pub ring_buffer_lines: usize,
    #[arg(long, default_value_t = 2 * 1024 * 1024)]
    pub ring_buffer_bytes: usize,
}

fn wait_for_connection(handle: &ReaderHandle, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.status.lock().unwrap().connected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let err = handle.status.lock().unwrap().last_error.clone();
            handle.shutdown();
            anyhow::bail!(
                "timed out waiting to connect: {}",
                err.unwrap_or_else(|| "unknown error".to_string())
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn run_console(args: ConsoleArgs, reset_capable: bool) -> anyhow::Result<()> {
    let config = ReaderConfig {
        port: args.port.port.clone(),
        baud: args.port.baud,
        reset_opts: args.reset.to_opts(),
        ..ReaderConfig::default()
    };

    let handle = reader::spawn(config);
    wait_for_connection(&handle, Duration::from_secs(5))?;

    if let Some(path) = &args.log_path {
        handle.start_file_log(path.clone(), false)?;
    }

    if reset_capable && !args.no_reboot {
        match handle.reset() {
            Ok(outcome) if outcome.confirmed => {
                tracing::info!(attempts = outcome.attempts, "board reset confirmed");
            }
            Ok(outcome) => {
                tracing::warn!(
                    attempts = outcome.attempts,
                    "reset sent but board did not respond"
                );
            }
            Err(e) => tracing::warn!(error = %e, "reset failed"),
        }
    }

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))?;
    }

    let deadline =
        (args.log_time >= 0).then(|| Instant::now() + Duration::from_secs(args.log_time as u64));

    let mut last_seq: Option<u64> = None;
    while running.load(Ordering::SeqCst) {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        let entries = {
            let log = handle.log.lock().unwrap();
            match last_seq {
                Some(seq) => log.since(seq),
                None => log.tail(usize::MAX),
            }
        };
        for entry in &entries {
            last_seq = Some(entry.seq);
            if !args.no_console {
                println!("{}", entry.text);
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    if let Ok(summary) = handle.stop_file_log() {
        tracing::info!(
            lines = summary.lines_written,
            path = %summary.path.display(),
            "file log closed"
        );
    }
    handle.shutdown();

    // restore terminal colors in case the board's output left an ANSI
    // color code unclosed
    print!("\x1b[0m");

    Ok(())
}

pub fn run_off(args: PortArgs) -> anyhow::Result<()> {
    let config = ReaderConfig {
        port: args.port.clone(),
        baud: args.baud,
        ..ReaderConfig::default()
    };

    let handle = reader::spawn(config);
    wait_for_connection(&handle, Duration::from_secs(5))?;
    handle.power_off()?;
    println!("{}: held in power-off (RTS asserted, DTR clear)", args.port);
    handle.shutdown();
    Ok(())
}
