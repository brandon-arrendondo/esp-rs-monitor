//! Background thread that is the sole owner of the open serial port. It
//! continuously reads lines into a shared ring buffer (and optional file
//! log), while servicing reset/power/file-log commands sent by the CLI or
//! MCP server over a channel — so there is never more than one thing
//! touching the port's RTS/DTR lines or read half at once.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::lineread::LineSplitter;
use crate::logbuf::{RingBuffer, SharedRingBuffer};
use crate::serial::{self, ResetPort};
use crate::stats::StatsExtractor;
use crate::status::{ReaderStatus, SharedStatus};

pub use crate::serial::{ResetOptions, ResetOutcome};

#[derive(Debug, Clone)]
pub struct ReaderConfig {
    pub port: String,
    pub baud: u32,
    pub read_timeout: Duration,
    pub reset_opts: ResetOptions,
    pub max_lines: usize,
    pub max_bytes: usize,
    pub reconnect_backoff: Duration,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            port: "/dev/ttyUSB0".to_string(),
            baud: 115_200,
            read_timeout: Duration::from_millis(500),
            reset_opts: ResetOptions::default(),
            max_lines: 2000,
            max_bytes: 2 * 1024 * 1024,
            reconnect_backoff: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileLogInfo {
    pub path: PathBuf,
    pub stats_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct FileLogSummary {
    pub path: PathBuf,
    pub stats_path: PathBuf,
    pub lines_written: usize,
}

enum ReaderCommand {
    Reset {
        reply: mpsc::Sender<io::Result<ResetOutcome>>,
    },
    PowerOn {
        reply: mpsc::Sender<io::Result<ResetOutcome>>,
    },
    PowerOff {
        reply: mpsc::Sender<io::Result<()>>,
    },
    Close {
        reply: mpsc::Sender<io::Result<()>>,
    },
    Open {
        reply: mpsc::Sender<io::Result<()>>,
    },
    StartFileLog {
        path: PathBuf,
        append: bool,
        reply: mpsc::Sender<io::Result<FileLogInfo>>,
    },
    StopFileLog {
        reply: mpsc::Sender<io::Result<FileLogSummary>>,
    },
    Shutdown,
}

/// A cheaply-cloneable handle to a running reader thread: send it commands,
/// or read `log`/`status` directly (those never touch the port, so they
/// don't need to go through the command channel).
#[derive(Clone)]
pub struct ReaderHandle {
    cmd_tx: mpsc::Sender<ReaderCommand>,
    pub log: SharedRingBuffer,
    pub status: SharedStatus,
    join: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl ReaderHandle {
    pub fn reset(&self) -> anyhow::Result<ResetOutcome> {
        self.call(|reply| ReaderCommand::Reset { reply })
    }

    pub fn power_on(&self) -> anyhow::Result<ResetOutcome> {
        self.call(|reply| ReaderCommand::PowerOn { reply })
    }

    pub fn power_off(&self) -> anyhow::Result<()> {
        self.call(|reply| ReaderCommand::PowerOff { reply })
    }

    /// Releases the serial port so another process can open it, and
    /// suppresses auto-reconnect until [`Self::open`] is called.
    pub fn close(&self) -> anyhow::Result<()> {
        self.call(|reply| ReaderCommand::Close { reply })
    }

    /// Clears the manual-close latch and attempts to reopen the port
    /// immediately. If this attempt fails, normal backoff-based
    /// auto-reconnect resumes in the background.
    pub fn open(&self) -> anyhow::Result<()> {
        self.call(|reply| ReaderCommand::Open { reply })
    }

    pub fn start_file_log(&self, path: PathBuf, append: bool) -> anyhow::Result<FileLogInfo> {
        self.call(|reply| ReaderCommand::StartFileLog {
            path,
            append,
            reply,
        })
    }

    pub fn stop_file_log(&self) -> anyhow::Result<FileLogSummary> {
        self.call(|reply| ReaderCommand::StopFileLog { reply })
    }

    /// Signals the reader thread to shut down (flushing any active file
    /// log) and waits for it to exit. Safe to call more than once.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(ReaderCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn call<T>(
        &self,
        make_cmd: impl FnOnce(mpsc::Sender<io::Result<T>>) -> ReaderCommand,
    ) -> anyhow::Result<T> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(make_cmd(tx))
            .map_err(|_| anyhow::anyhow!("reader thread is not running"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("reader thread dropped without replying"))?
            .map_err(anyhow::Error::from)
    }
}

/// Starts the background reader thread against a real serial port.
pub fn spawn(config: ReaderConfig) -> ReaderHandle {
    let port_cfg = config.clone();
    let opener = move || -> io::Result<Box<dyn ResetPort>> {
        serial::open(&port_cfg.port, port_cfg.baud, port_cfg.read_timeout)
            .map(|p| Box::new(p) as Box<dyn ResetPort>)
            .map_err(|e| io::Error::other(e.to_string()))
    };
    spawn_with_opener(config, opener)
}

fn spawn_with_opener(
    config: ReaderConfig,
    opener: impl FnMut() -> io::Result<Box<dyn ResetPort>> + Send + 'static,
) -> ReaderHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let log: SharedRingBuffer = Arc::new(Mutex::new(RingBuffer::new(
        config.max_lines,
        config.max_bytes,
    )));
    let status: SharedStatus = Arc::new(Mutex::new(ReaderStatus::new(
        config.port.clone(),
        config.baud,
    )));

    let log_for_thread = log.clone();
    let status_for_thread = status.clone();
    let backoff = config.reconnect_backoff;
    let reset_opts = config.reset_opts;

    let join = thread::spawn(move || {
        run_loop(
            opener,
            backoff,
            reset_opts,
            cmd_rx,
            log_for_thread,
            status_for_thread,
        )
    });

    ReaderHandle {
        cmd_tx,
        log,
        status,
        join: Arc::new(Mutex::new(Some(join))),
    }
}

fn run_loop(
    mut opener: impl FnMut() -> io::Result<Box<dyn ResetPort>>,
    backoff: Duration,
    reset_opts: ResetOptions,
    cmd_rx: mpsc::Receiver<ReaderCommand>,
    log: SharedRingBuffer,
    status: SharedStatus,
) {
    let mut port: Option<Box<dyn ResetPort>> = None;
    let mut splitter = LineSplitter::new();
    let stats = StatsExtractor::new();
    let mut file_log: Option<FileLog> = None;
    let mut last_attempt = Instant::now()
        .checked_sub(backoff)
        .unwrap_or_else(Instant::now);
    let mut buf = [0u8; 4096];

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ReaderCommand::Shutdown => {
                    if let Some(fl) = file_log.take() {
                        fl.close();
                    }
                    return;
                }
                ReaderCommand::Reset { reply } => {
                    let result = match port.as_deref_mut() {
                        Some(p) => serial::reset_sequence(p, reset_opts),
                        None => Err(not_connected()),
                    };
                    let _ = reply.send(result);
                }
                ReaderCommand::PowerOn { reply } => {
                    let result = match port.as_deref_mut() {
                        Some(p) => serial::power_on(p, reset_opts),
                        None => Err(not_connected()),
                    };
                    let _ = reply.send(result);
                }
                ReaderCommand::PowerOff { reply } => {
                    let result = match port.as_deref_mut() {
                        Some(p) => serial::power_off(p),
                        None => Err(not_connected()),
                    };
                    let _ = reply.send(result);
                }
                ReaderCommand::Close { reply } => {
                    port = None;
                    let mut st = status.lock().unwrap();
                    st.connected = false;
                    st.closed = true;
                    st.last_error = None;
                    drop(st);
                    let _ = reply.send(Ok(()));
                }
                ReaderCommand::Open { reply } => {
                    if port.is_some() {
                        // already connected (Open with no prior Close) — just
                        // clear the latch, no need to reopen anything.
                        status.lock().unwrap().closed = false;
                        let _ = reply.send(Ok(()));
                    } else {
                        last_attempt = Instant::now();
                        let _ = reply.send(try_open(&mut port, &status, &mut opener));
                    }
                }
                ReaderCommand::StartFileLog {
                    path,
                    append,
                    reply,
                } => {
                    let result = FileLog::open(&path, append).map(|fl| {
                        let info = FileLogInfo {
                            path: fl.log_path.clone(),
                            stats_path: fl.stats_path.clone(),
                        };
                        let mut st = status.lock().unwrap();
                        st.file_log_path = Some(info.path.display().to_string());
                        st.stats_file_path = Some(info.stats_path.display().to_string());
                        drop(st);
                        file_log = Some(fl);
                        info
                    });
                    let _ = reply.send(result);
                }
                ReaderCommand::StopFileLog { reply } => {
                    let result = match file_log.take() {
                        Some(fl) => {
                            let summary = fl.close();
                            let mut st = status.lock().unwrap();
                            st.file_log_path = None;
                            st.stats_file_path = None;
                            drop(st);
                            Ok(summary)
                        }
                        None => Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "no active file log",
                        )),
                    };
                    let _ = reply.send(result);
                }
            }
        }

        if port.is_none() {
            if status.lock().unwrap().closed {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            if last_attempt.elapsed() >= backoff {
                last_attempt = Instant::now();
                let _ = try_open(&mut port, &status, &mut opener);
            } else {
                thread::sleep(Duration::from_millis(20));
            }
            continue;
        }

        match port.as_deref_mut().unwrap().read(&mut buf) {
            Ok(0) => thread::sleep(Duration::from_millis(1)),
            Ok(n) => {
                for line in splitter.feed(&buf[..n]) {
                    if let Some(stat) = stats.feed_line(&line) {
                        if let Some(fl) = file_log.as_mut() {
                            fl.write_stat(&stat);
                        }
                    } else {
                        log.lock().unwrap().push(line.clone());
                        if let Some(fl) = file_log.as_mut() {
                            fl.write_line(&line);
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => {
                let mut st = status.lock().unwrap();
                st.connected = false;
                st.last_error = Some(e.to_string());
                drop(st);
                port = None;
            }
        }
    }
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "serial port not open")
}

/// Attempts one open, updating `port`/`status` to reflect the outcome.
/// Shared by the auto-reconnect loop and the `Open` command handler.
fn try_open(
    port: &mut Option<Box<dyn ResetPort>>,
    status: &SharedStatus,
    opener: &mut impl FnMut() -> io::Result<Box<dyn ResetPort>>,
) -> io::Result<()> {
    match opener() {
        Ok(p) => {
            *port = Some(p);
            let mut st = status.lock().unwrap();
            st.connected = true;
            st.closed = false;
            st.last_error = None;
            Ok(())
        }
        Err(e) => {
            let mut st = status.lock().unwrap();
            st.connected = false;
            st.closed = false;
            st.last_error = Some(e.to_string());
            Err(e)
        }
    }
}

/// Splits captured serial output into a main log file and a separate
/// stats file for `/* ... */`-delimited packets, mirroring how the board's
/// regular console output and periodic system-stat packets are meant to be
/// consumed separately.
struct FileLog {
    file: std::fs::File,
    stats_file: std::fs::File,
    log_path: PathBuf,
    stats_path: PathBuf,
    lines_written: usize,
    stats_lines_written: usize,
}

impl FileLog {
    fn open(path: &Path, append: bool) -> io::Result<Self> {
        let stats_path = stats_path_for(path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)?;
        let stats_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stats_path)?;
        Ok(Self {
            file,
            stats_file,
            log_path: path.to_path_buf(),
            stats_path,
            lines_written: 0,
            stats_lines_written: 0,
        })
    }

    fn write_line(&mut self, line: &str) {
        let _ = writeln!(self.file, "{line}");
        self.lines_written += 1;
    }

    fn write_stat(&mut self, stat: &str) {
        let _ = writeln!(self.stats_file, "{stat}");
        self.stats_lines_written += 1;
    }

    /// Flushes both files and removes the stats file if nothing was ever
    /// written to it, so an empty `*.stats.*` file doesn't linger for a
    /// session that never emitted a stat packet.
    fn close(mut self) -> FileLogSummary {
        let _ = self.file.flush();
        let _ = self.stats_file.flush();
        if self.stats_lines_written == 0 {
            let _ = std::fs::remove_file(&self.stats_path);
        }
        FileLogSummary {
            path: self.log_path,
            stats_path: self.stats_path,
            lines_written: self.lines_written,
        }
    }
}

fn stats_path_for(log_path: &Path) -> PathBuf {
    let stem = log_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match log_path.extension() {
        Some(ext) => format!("{stem}.stats.{}", ext.to_string_lossy()),
        None => format!("{stem}.stats"),
    };
    log_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Read;

    fn wait_until_connected(handle: &ReaderHandle) {
        for _ in 0..200 {
            if handle.status.lock().unwrap().connected {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("reader never connected");
    }

    fn fast_config() -> ReaderConfig {
        ReaderConfig {
            reconnect_backoff: Duration::from_millis(5),
            reset_opts: ResetOptions {
                pulse: Duration::from_millis(1),
                confirm_timeout: Duration::from_millis(50),
                max_retries: 3,
            },
            ..ReaderConfig::default()
        }
    }

    // -- stats_path_for --

    #[test]
    fn stats_path_inserts_before_extension() {
        assert_eq!(
            stats_path_for(Path::new("/tmp/session.log")),
            PathBuf::from("/tmp/session.stats.log")
        );
    }

    #[test]
    fn stats_path_handles_no_extension() {
        assert_eq!(
            stats_path_for(Path::new("/tmp/session")),
            PathBuf::from("/tmp/session.stats")
        );
    }

    // -- FileLog --

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "esp-monitor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn file_log_writes_lines_and_stats_separately() {
        let path = temp_path("session.log");
        let mut fl = FileLog::open(&path, false).unwrap();
        fl.write_line("boot line 1");
        fl.write_stat(" heap=100 ");
        fl.write_line("boot line 2");
        let summary = fl.close();

        assert_eq!(summary.lines_written, 2);
        let contents = std::fs::read_to_string(&summary.path).unwrap();
        assert_eq!(contents, "boot line 1\nboot line 2\n");
        let stats = std::fs::read_to_string(&summary.stats_path).unwrap();
        assert_eq!(stats, " heap=100 \n");
    }

    #[test]
    fn file_log_removes_empty_stats_file_on_close() {
        let path = temp_path("session.log");
        let mut fl = FileLog::open(&path, false).unwrap();
        fl.write_line("just a log line");
        let summary = fl.close();

        assert_eq!(summary.lines_written, 1);
        assert!(summary.path.exists());
        assert!(
            !summary.stats_path.exists(),
            "stats file should be removed when no stats were written"
        );
    }

    #[test]
    fn file_log_append_preserves_existing_content() {
        let path = temp_path("session.log");
        FileLog::open(&path, false).unwrap().write_line("first");
        let mut fl = FileLog::open(&path, true).unwrap();
        fl.write_line("second");
        fl.close();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first\nsecond\n");
    }

    // -- threaded reader loop --

    /// Always returns a scripted list of lines then times out forever.
    struct FiniteScriptPort {
        lines: VecDeque<Vec<u8>>,
    }

    impl Read for FiniteScriptPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.lines.pop_front() {
                Some(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                None => Err(io::Error::new(io::ErrorKind::TimedOut, "exhausted")),
            }
        }
    }

    impl ResetPort for FiniteScriptPort {
        fn write_request_to_send(&mut self, _level: bool) -> io::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _level: bool) -> io::Result<()> {
            Ok(())
        }
    }

    fn opener_for(port: FiniteScriptPort) -> impl FnMut() -> io::Result<Box<dyn ResetPort>> {
        let mut slot = Some(port);
        move || {
            slot.take()
                .map(|p| Box::new(p) as Box<dyn ResetPort>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "already opened"))
        }
    }

    #[test]
    fn connects_and_buffers_read_lines() {
        let port = FiniteScriptPort {
            lines: VecDeque::from(vec![b"hello\n".to_vec(), b"world\n".to_vec()]),
        };
        let handle = spawn_with_opener(fast_config(), opener_for(port));
        wait_until_connected(&handle);

        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = handle
                .log
                .lock()
                .unwrap()
                .tail(10)
                .into_iter()
                .map(|e| e.text)
                .collect::<Vec<_>>();
            if seen.len() >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen, vec!["hello", "world"]);

        handle.shutdown();
    }

    #[test]
    fn reconnects_after_initial_open_failure() {
        let mut attempt = 0;
        let opener = move || -> io::Result<Box<dyn ResetPort>> {
            attempt += 1;
            if attempt == 1 {
                Err(io::Error::new(io::ErrorKind::NotFound, "no such device"))
            } else {
                Ok(Box::new(FiniteScriptPort {
                    lines: VecDeque::new(),
                }) as Box<dyn ResetPort>)
            }
        };
        let handle = spawn_with_opener(fast_config(), opener);
        wait_until_connected(&handle);
        assert!(handle.status.lock().unwrap().last_error.is_none());

        handle.shutdown();
    }

    /// A port whose read() only yields data after a reset pulse has been
    /// observed, so the reset-confirmation path is deterministic instead
    /// of racing a background stream of fake data.
    struct ResetTriggeredPort {
        toggles: Arc<Mutex<Vec<(&'static str, bool)>>>,
        reset_seen: bool,
        replied: bool,
    }

    impl Read for ResetTriggeredPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.reset_seen && !self.replied {
                self.replied = true;
                let data = b"boot\n";
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(data);
                return Ok(n);
            }
            Err(io::Error::new(io::ErrorKind::TimedOut, "waiting for reset"))
        }
    }

    impl ResetPort for ResetTriggeredPort {
        fn write_request_to_send(&mut self, level: bool) -> io::Result<()> {
            self.toggles.lock().unwrap().push(("rts", level));
            // Only a completed reset pulse (RTS asserted, then released)
            // should wake the board — `power_off` asserts RTS and leaves
            // it there, so it must not trigger this by itself, or a
            // power_off immediately followed by power_on would race the
            // reply against the main loop's own idle read.
            if !level {
                self.reset_seen = true;
            }
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, level: bool) -> io::Result<()> {
            self.toggles.lock().unwrap().push(("dtr", level));
            Ok(())
        }
    }

    #[test]
    fn reset_command_pulses_lines_and_confirms() {
        let toggles = Arc::new(Mutex::new(Vec::new()));
        let toggles_for_port = toggles.clone();
        let port = ResetTriggeredPort {
            toggles: toggles_for_port,
            reset_seen: false,
            replied: false,
        };
        let mut slot = Some(port);
        let opener = move || -> io::Result<Box<dyn ResetPort>> {
            slot.take()
                .map(|p| Box::new(p) as Box<dyn ResetPort>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "already opened"))
        };

        let handle = spawn_with_opener(fast_config(), opener);
        wait_until_connected(&handle);

        let outcome = handle.reset().unwrap();
        assert!(outcome.confirmed);
        assert_eq!(outcome.attempts, 1);

        let recorded = toggles.lock().unwrap().clone();
        assert!(recorded.contains(&("rts", true)));
        assert!(recorded.contains(&("rts", false)));
        assert!(recorded.contains(&("dtr", false)));

        handle.shutdown();
    }

    #[test]
    fn power_off_and_power_on_round_trip() {
        let toggles = Arc::new(Mutex::new(Vec::new()));
        let port = ResetTriggeredPort {
            toggles: toggles.clone(),
            reset_seen: false,
            replied: false,
        };
        let mut slot = Some(port);
        let opener = move || -> io::Result<Box<dyn ResetPort>> {
            slot.take()
                .map(|p| Box::new(p) as Box<dyn ResetPort>)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "already opened"))
        };
        let handle = spawn_with_opener(fast_config(), opener);
        wait_until_connected(&handle);

        handle.power_off().unwrap();
        assert_eq!(
            toggles.lock().unwrap().as_slice(),
            &[("rts", true), ("dtr", false)]
        );

        let outcome = handle.power_on().unwrap();
        assert!(outcome.confirmed);

        handle.shutdown();
    }

    #[test]
    fn commands_fail_cleanly_when_never_connected() {
        // opener always fails, backoff kept short but > test duration so
        // the reader stays disconnected for the life of this test.
        let opener = || -> io::Result<Box<dyn ResetPort>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "never plugged in"))
        };
        let config = ReaderConfig {
            reconnect_backoff: Duration::from_secs(60),
            ..fast_config()
        };
        let handle = spawn_with_opener(config, opener);
        thread::sleep(Duration::from_millis(20));

        assert!(!handle.status.lock().unwrap().connected);
        assert!(handle.reset().is_err());
        assert!(handle.power_off().is_err());

        handle.shutdown();
    }

    #[test]
    fn close_releases_port_and_suppresses_auto_reconnect() {
        let opens = Arc::new(Mutex::new(0u32));
        let opens_for_opener = opens.clone();
        let opener = move || -> io::Result<Box<dyn ResetPort>> {
            *opens_for_opener.lock().unwrap() += 1;
            Ok(Box::new(FiniteScriptPort {
                lines: VecDeque::new(),
            }) as Box<dyn ResetPort>)
        };
        let handle = spawn_with_opener(fast_config(), opener);
        wait_until_connected(&handle);
        assert_eq!(*opens.lock().unwrap(), 1);

        handle.close().unwrap();
        assert!(!handle.status.lock().unwrap().connected);
        assert!(handle.status.lock().unwrap().closed);

        // Give the reader loop several backoff windows to (incorrectly)
        // reconnect if the `closed` latch weren't respected.
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            *opens.lock().unwrap(),
            1,
            "closed port must not be auto-reopened"
        );

        handle.shutdown();
    }

    #[test]
    fn open_after_close_reconnects() {
        let opens = Arc::new(Mutex::new(0u32));
        let opens_for_opener = opens.clone();
        let opener = move || -> io::Result<Box<dyn ResetPort>> {
            *opens_for_opener.lock().unwrap() += 1;
            Ok(Box::new(FiniteScriptPort {
                lines: VecDeque::new(),
            }) as Box<dyn ResetPort>)
        };
        let handle = spawn_with_opener(fast_config(), opener);
        wait_until_connected(&handle);

        handle.close().unwrap();
        assert!(!handle.status.lock().unwrap().connected);

        handle.open().unwrap();
        assert!(handle.status.lock().unwrap().connected);
        assert!(!handle.status.lock().unwrap().closed);
        assert_eq!(*opens.lock().unwrap(), 2);

        handle.shutdown();
    }

    #[test]
    fn open_without_prior_close_is_a_harmless_noop() {
        // `opener_for` only has one port in its slot — if `open` tried to
        // reopen an already-connected port, this would fail.
        let handle = spawn_with_opener(
            fast_config(),
            opener_for(FiniteScriptPort {
                lines: VecDeque::new(),
            }),
        );
        wait_until_connected(&handle);

        assert!(handle.open().is_ok());
        assert!(handle.status.lock().unwrap().connected);
        assert!(!handle.status.lock().unwrap().closed);

        handle.shutdown();
    }

    #[test]
    fn file_log_start_stop_round_trip_through_reader_handle() {
        let port = FiniteScriptPort {
            lines: VecDeque::new(),
        };
        let handle = spawn_with_opener(fast_config(), opener_for(port));
        wait_until_connected(&handle);

        let path = temp_path("live-session.log");
        let info = handle.start_file_log(path.clone(), false).unwrap();
        assert_eq!(info.path, path);
        assert_eq!(
            handle.status.lock().unwrap().file_log_path.as_deref(),
            Some(path.display().to_string().as_str())
        );

        let summary = handle.stop_file_log().unwrap();
        assert_eq!(summary.path, path);
        assert_eq!(summary.lines_written, 0);
        assert!(!summary.stats_path.exists());
        assert!(handle.status.lock().unwrap().file_log_path.is_none());

        // stopping again with nothing active is a clean error, not a panic
        assert!(handle.stop_file_log().is_err());

        handle.shutdown();
    }

    #[test]
    fn shutdown_is_idempotent() {
        let handle = spawn_with_opener(
            fast_config(),
            opener_for(FiniteScriptPort {
                lines: VecDeque::new(),
            }),
        );
        handle.shutdown();
        handle.shutdown(); // must not hang or panic
    }
}
