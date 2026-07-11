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
use regex::Regex;

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
    /// Reset the board, watch its console for a pass/fail pattern, and exit
    /// with a matching status code (for CI/on-device test runners)
    Watch(WatchArgs),
    /// Start the MCP server (stdio transport)
    Mcp(McpArgs),
}

impl Command {
    pub fn verbose(&self) -> u8 {
        match self {
            Command::Console(a) | Command::On(a) | Command::Reset(a) => a.port.verbose,
            Command::Off(a) => a.verbose,
            Command::Watch(a) => a.port.verbose,
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
pub struct WatchArgs {
    #[command(flatten)]
    pub port: PortArgs,
    #[command(flatten)]
    pub reset: ResetTuning,
    /// Exit 0 the moment a line matches this regex
    #[arg(long)]
    pub pass_pattern: String,
    /// Exit with the fail status the moment a line matches this regex
    #[arg(long)]
    pub fail_pattern: String,
    /// Seconds to wait for a match before exiting with the timeout status
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
    /// Do not reset the board before watching
    #[arg(long)]
    pub no_reboot: bool,
    /// Do not print board output to the console while watching
    #[arg(long)]
    pub no_console: bool,
}

/// Exit code used when `--fail-pattern` matches before `--pass-pattern`.
pub const WATCH_EXIT_FAILED: i32 = 1;
/// Exit code used when `--timeout` elapses with no pattern match.
pub const WATCH_EXIT_TIMED_OUT: i32 = 2;

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

/// Result of scanning a batch of log lines for `watch`'s pass/fail patterns.
#[derive(Debug, PartialEq, Eq)]
enum ScanOutcome {
    /// A pattern matched; `seq` is the sequence number of the matching line
    /// (later lines in the same batch are not scanned).
    Matched { exit_code: i32, seq: u64 },
    /// Nothing matched; `newest_seq` is the highest sequence number seen in
    /// this batch, if any, for the caller to advance its polling cursor.
    NoMatch { newest_seq: Option<u64> },
}

/// Scans `entries` in order, returning as soon as `fail_re` or `pass_re`
/// matches a line (fail takes priority when both match the same line).
fn scan_for_match(
    entries: &[esp_monitor::logbuf::LogEntry],
    pass_re: &Regex,
    fail_re: &Regex,
) -> ScanOutcome {
    let mut newest_seq = None;
    for entry in entries {
        newest_seq = Some(entry.seq);
        if fail_re.is_match(&entry.text) {
            return ScanOutcome::Matched {
                exit_code: WATCH_EXIT_FAILED,
                seq: entry.seq,
            };
        }
        if pass_re.is_match(&entry.text) {
            return ScanOutcome::Matched {
                exit_code: 0,
                seq: entry.seq,
            };
        }
    }
    ScanOutcome::NoMatch { newest_seq }
}

/// Resets the board (unless `--no-reboot`) and streams its console, looking
/// for `--pass-pattern`/`--fail-pattern` matches. Returns the process exit
/// code to use: 0 on pass, [`WATCH_EXIT_FAILED`] on fail, or
/// [`WATCH_EXIT_TIMED_OUT`] if `--timeout` elapses with no match.
pub fn run_watch(args: WatchArgs) -> anyhow::Result<i32> {
    let pass_re = Regex::new(&args.pass_pattern)
        .map_err(|e| anyhow::anyhow!("invalid --pass-pattern: {e}"))?;
    let fail_re = Regex::new(&args.fail_pattern)
        .map_err(|e| anyhow::anyhow!("invalid --fail-pattern: {e}"))?;

    let config = ReaderConfig {
        port: args.port.port.clone(),
        baud: args.port.baud,
        reset_opts: args.reset.to_opts(),
        ..ReaderConfig::default()
    };

    let handle = reader::spawn(config);
    wait_for_connection(&handle, Duration::from_secs(5))?;

    if !args.no_reboot {
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

    let deadline = Instant::now() + Duration::from_secs(args.timeout);

    let mut last_seq: Option<u64> = None;
    let mut exit_code = WATCH_EXIT_TIMED_OUT;
    while running.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            break;
        }

        let entries = {
            let log = handle.log.lock().unwrap();
            match last_seq {
                Some(seq) => log.since(seq),
                None => log.tail(usize::MAX),
            }
        };

        if !args.no_console {
            for entry in &entries {
                println!("{}", entry.text);
            }
        }

        match scan_for_match(&entries, &pass_re, &fail_re) {
            ScanOutcome::Matched {
                exit_code: code, ..
            } => {
                exit_code = code;
                break;
            }
            ScanOutcome::NoMatch { newest_seq } => {
                if let Some(seq) = newest_seq {
                    last_seq = Some(seq);
                }
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    handle.shutdown();

    // restore terminal colors in case the board's output left an ANSI
    // color code unclosed
    print!("\x1b[0m");

    Ok(exit_code)
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

#[cfg(test)]
mod tests {
    use super::*;
    use esp_monitor::logbuf::LogEntry;
    use std::time::SystemTime;

    fn entry(seq: u64, text: &str) -> LogEntry {
        LogEntry {
            seq,
            text: text.to_string(),
            at: SystemTime::now(),
        }
    }

    // -- scan_for_match --

    #[test]
    fn scan_returns_no_match_on_empty_entries() {
        let pass = Regex::new("ok").unwrap();
        let fail = Regex::new("FAILED").unwrap();
        assert_eq!(
            scan_for_match(&[], &pass, &fail),
            ScanOutcome::NoMatch { newest_seq: None }
        );
    }

    #[test]
    fn scan_reports_newest_seq_when_nothing_matches() {
        let pass = Regex::new("ok").unwrap();
        let fail = Regex::new("FAILED").unwrap();
        let entries = vec![entry(0, "booting"), entry(1, "still running")];
        assert_eq!(
            scan_for_match(&entries, &pass, &fail),
            ScanOutcome::NoMatch {
                newest_seq: Some(1)
            }
        );
    }

    #[test]
    fn scan_matches_pass_pattern() {
        let pass = Regex::new("test result: ok").unwrap();
        let fail = Regex::new("test result: FAILED").unwrap();
        let entries = vec![entry(0, "booting"), entry(1, "test result: ok. 3 passed")];
        assert_eq!(
            scan_for_match(&entries, &pass, &fail),
            ScanOutcome::Matched {
                exit_code: 0,
                seq: 1
            }
        );
    }

    #[test]
    fn scan_matches_fail_pattern() {
        let pass = Regex::new("test result: ok").unwrap();
        let fail = Regex::new("test result: FAILED").unwrap();
        let entries = vec![
            entry(0, "booting"),
            entry(1, "test result: FAILED. 1 failed"),
        ];
        assert_eq!(
            scan_for_match(&entries, &pass, &fail),
            ScanOutcome::Matched {
                exit_code: WATCH_EXIT_FAILED,
                seq: 1
            }
        );
    }

    #[test]
    fn scan_stops_at_first_matching_line_ignoring_later_entries() {
        let pass = Regex::new("test result: ok").unwrap();
        let fail = Regex::new("test result: FAILED").unwrap();
        let entries = vec![
            entry(0, "test result: ok. 1 passed"),
            entry(1, "test result: FAILED. would be seen if we kept scanning"),
        ];
        assert_eq!(
            scan_for_match(&entries, &pass, &fail),
            ScanOutcome::Matched {
                exit_code: 0,
                seq: 0
            }
        );
    }

    #[test]
    fn scan_prefers_fail_over_pass_on_the_same_line() {
        // A line matching both patterns (e.g. overlapping regexes) should be
        // treated as a failure, not a pass — fail-fast is the safer default
        // for a CI gate.
        let pass = Regex::new("test result").unwrap();
        let fail = Regex::new("test result: FAILED").unwrap();
        let entries = vec![entry(0, "test result: FAILED")];
        assert_eq!(
            scan_for_match(&entries, &pass, &fail),
            ScanOutcome::Matched {
                exit_code: WATCH_EXIT_FAILED,
                seq: 0
            }
        );
    }

    // -- ResetTuning::to_opts --

    #[test]
    fn reset_tuning_converts_to_reset_options() {
        let tuning = ResetTuning {
            reset_retries: 7,
            reset_timeout_ms: 1234,
            reset_pulse_ms: 56,
        };
        let opts = tuning.to_opts();
        assert_eq!(opts.max_retries, 7);
        assert_eq!(opts.confirm_timeout, Duration::from_millis(1234));
        assert_eq!(opts.pulse, Duration::from_millis(56));
    }

    // -- CLI argument parsing --

    #[test]
    fn watch_args_parse_with_defaults() {
        let cli = Cli::parse_from([
            "esp-monitor",
            "watch",
            "--pass-pattern",
            "ok",
            "--fail-pattern",
            "FAILED",
        ]);
        match cli.command {
            Command::Watch(args) => {
                assert_eq!(args.port.port, "/dev/ttyUSB0");
                assert_eq!(args.port.baud, 115_200);
                assert_eq!(args.timeout, 30);
                assert!(!args.no_reboot);
                assert!(!args.no_console);
                assert_eq!(args.pass_pattern, "ok");
                assert_eq!(args.fail_pattern, "FAILED");
            }
            _ => panic!("expected Command::Watch"),
        }
    }

    #[test]
    fn watch_args_parse_with_overrides() {
        let cli = Cli::parse_from([
            "esp-monitor",
            "watch",
            "--port",
            "/dev/ttyACM0",
            "--pass-pattern",
            "ok",
            "--fail-pattern",
            "FAILED",
            "--timeout",
            "10",
            "--no-reboot",
            "--no-console",
        ]);
        match cli.command {
            Command::Watch(args) => {
                assert_eq!(args.port.port, "/dev/ttyACM0");
                assert_eq!(args.timeout, 10);
                assert!(args.no_reboot);
                assert!(args.no_console);
            }
            _ => panic!("expected Command::Watch"),
        }
    }

    #[test]
    fn watch_command_verbose_reads_through_port_args() {
        let cli = Cli::parse_from([
            "esp-monitor",
            "watch",
            "--pass-pattern",
            "ok",
            "--fail-pattern",
            "FAILED",
            "-vv",
        ]);
        assert_eq!(cli.command.verbose(), 2);
    }

    // -- run_watch: fails fast on bad input without touching hardware --

    #[test]
    fn run_watch_rejects_invalid_pass_pattern() {
        let args = WatchArgs {
            port: PortArgs {
                port: "/dev/null-not-a-real-port".to_string(),
                baud: 115_200,
                verbose: 0,
            },
            reset: ResetTuning {
                reset_retries: 1,
                reset_timeout_ms: 1,
                reset_pulse_ms: 1,
            },
            pass_pattern: "(unclosed".to_string(),
            fail_pattern: "FAILED".to_string(),
            timeout: 1,
            no_reboot: true,
            no_console: true,
        };
        let err = run_watch(args).unwrap_err();
        assert!(err.to_string().contains("invalid --pass-pattern"));
    }

    #[test]
    fn run_watch_rejects_invalid_fail_pattern() {
        let args = WatchArgs {
            port: PortArgs {
                port: "/dev/null-not-a-real-port".to_string(),
                baud: 115_200,
                verbose: 0,
            },
            reset: ResetTuning {
                reset_retries: 1,
                reset_timeout_ms: 1,
                reset_pulse_ms: 1,
            },
            pass_pattern: "ok".to_string(),
            fail_pattern: "(unclosed".to_string(),
            timeout: 1,
            no_reboot: true,
            no_console: true,
        };
        let err = run_watch(args).unwrap_err();
        assert!(err.to_string().contains("invalid --fail-pattern"));
    }
}
