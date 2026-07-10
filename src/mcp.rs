//! MCP server (stdio transport): exposes the board's reset/power/log
//! controls as tools so an agent can drive board bring-up/debugging loops
//! directly. The only file in this crate allowed to depend on tokio.

use std::path::PathBuf;
use std::time::SystemTime;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use esp_monitor::logbuf::LogEntry;
use esp_monitor::reader::{self, ReaderConfig, ReaderHandle};

use crate::cli::McpArgs;

pub async fn run_server(args: McpArgs) -> anyhow::Result<()> {
    let config = ReaderConfig {
        port: args.port.port.clone(),
        baud: args.port.baud,
        reset_opts: args.reset.to_opts(),
        max_lines: args.ring_buffer_lines,
        max_bytes: args.ring_buffer_bytes,
        ..ReaderConfig::default()
    };
    let handle = reader::spawn(config);

    if let Some(path) = args.log_path.clone() {
        let h = handle.clone();
        tokio::task::spawn_blocking(move || h.start_file_log(path, false)).await??;
    }

    let server = EspMonitorServer::new(handle.clone());
    let running = server.serve(stdio()).await?;
    running.waiting().await?;

    handle.shutdown();
    Ok(())
}

#[derive(Clone)]
struct EspMonitorServer {
    handle: ReaderHandle,
    tool_router: ToolRouter<Self>,
}

impl EspMonitorServer {
    fn new(handle: ReaderHandle) -> Self {
        Self {
            handle,
            tool_router: Self::tool_router(),
        }
    }
}

fn blocking_err(e: tokio::task::JoinError) -> McpError {
    McpError::internal_error(format!("reader task panicked: {e}"), None)
}

fn anyhow_err(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

#[derive(Debug, Serialize, JsonSchema)]
struct ResetResult {
    confirmed: bool,
    attempts: u32,
    bytes_seen: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PowerParams {
    /// Desired power state: "on" or "off"
    state: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PowerResult {
    state: String,
    confirmed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CloseResult {
    closed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct OpenResult {
    connected: bool,
    error: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadLogsParams {
    /// Number of most recent lines to return (ignored if since_seq is set). Defaults to 200.
    lines: Option<usize>,
    /// Return only entries with seq strictly greater than this value.
    since_seq: Option<u64>,
    /// Only include lines matching this pattern (substring, or regex if `regex` is true).
    search: Option<String>,
    /// Treat `search` as a regular expression instead of a plain substring.
    regex: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LogEntryOut {
    seq: u64,
    text: String,
    at: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReadLogsResult {
    entries: Vec<LogEntryOut>,
    newest_seq: Option<u64>,
    total_buffered: usize,
    truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct StatusResult {
    connected: bool,
    /// True while the port has been intentionally released via `close` and
    /// not yet reopened via `open`.
    closed: bool,
    port: String,
    baud: u32,
    buffered_lines: usize,
    file_logging: bool,
    file_log_path: Option<String>,
    last_error: Option<String>,
    uptime_seconds: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FileLogParams {
    /// "start" or "stop"
    action: String,
    /// Required for "start": where to write the session log.
    path: Option<PathBuf>,
    /// For "start": append to an existing file instead of truncating it. Defaults to false.
    append: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FileLogResult {
    path: String,
    stats_path: String,
    lines_written: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ClearLogsResult {
    cleared_lines: usize,
}

fn entry_timestamp(at: SystemTime) -> f64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn filter_entries(
    entries: Vec<LogEntry>,
    search: Option<&str>,
    regex: bool,
) -> Result<Vec<LogEntry>, McpError> {
    let Some(pattern) = search else {
        return Ok(entries);
    };
    if regex {
        let re = regex::Regex::new(pattern)
            .map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;
        Ok(entries
            .into_iter()
            .filter(|e| re.is_match(&e.text))
            .collect())
    } else {
        Ok(entries
            .into_iter()
            .filter(|e| e.text.contains(pattern))
            .collect())
    }
}

#[tool_router]
impl EspMonitorServer {
    #[tool(description = "Reset the board via an RTS/DTR pulse and confirm it came back up.")]
    async fn reset(&self) -> Result<Json<ResetResult>, McpError> {
        let handle = self.handle.clone();
        let outcome = tokio::task::spawn_blocking(move || handle.reset())
            .await
            .map_err(blocking_err)?
            .map_err(anyhow_err)?;
        Ok(Json(ResetResult {
            confirmed: outcome.confirmed,
            attempts: outcome.attempts,
            bytes_seen: outcome.bytes_seen,
        }))
    }

    #[tool(description = "Power the board on (reset) or off (hold in reset) via RTS/DTR.")]
    async fn power(
        &self,
        Parameters(PowerParams { state }): Parameters<PowerParams>,
    ) -> Result<Json<PowerResult>, McpError> {
        let handle = self.handle.clone();
        match state.as_str() {
            "on" => {
                let outcome = tokio::task::spawn_blocking(move || handle.power_on())
                    .await
                    .map_err(blocking_err)?
                    .map_err(anyhow_err)?;
                Ok(Json(PowerResult {
                    state: "on".to_string(),
                    confirmed: outcome.confirmed,
                }))
            }
            "off" => {
                tokio::task::spawn_blocking(move || handle.power_off())
                    .await
                    .map_err(blocking_err)?
                    .map_err(anyhow_err)?;
                Ok(Json(PowerResult {
                    state: "off".to_string(),
                    confirmed: true,
                }))
            }
            other => Err(McpError::invalid_params(
                format!("state must be \"on\" or \"off\", got {other:?}"),
                None,
            )),
        }
    }

    #[tool(
        description = "Release the serial port so another process (e.g. `cargo test`'s flash \
                        step) can open it. Auto-reconnect is suppressed until `open` is called."
    )]
    async fn close(&self) -> Result<Json<CloseResult>, McpError> {
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || handle.close())
            .await
            .map_err(blocking_err)?
            .map_err(anyhow_err)?;
        Ok(Json(CloseResult { closed: true }))
    }

    #[tool(
        description = "Reopen the serial port after `close`, resuming the log stream. If the \
                        immediate attempt fails (e.g. the port is still held by another \
                        process), normal auto-reconnect resumes in the background."
    )]
    async fn open(&self) -> Result<Json<OpenResult>, McpError> {
        let handle = self.handle.clone();
        let result = tokio::task::spawn_blocking(move || handle.open())
            .await
            .map_err(blocking_err)?;
        match result {
            Ok(()) => Ok(Json(OpenResult {
                connected: true,
                error: None,
            })),
            Err(e) => Ok(Json(OpenResult {
                connected: false,
                error: Some(e.to_string()),
            })),
        }
    }

    #[tool(
        description = "Read buffered serial log lines, optionally filtering by substring or regex."
    )]
    fn read_logs(
        &self,
        Parameters(params): Parameters<ReadLogsParams>,
    ) -> Result<Json<ReadLogsResult>, McpError> {
        let log = self.handle.log.lock().unwrap();
        let entries = match params.since_seq {
            Some(seq) => log.since(seq),
            None => log.tail(params.lines.unwrap_or(200)),
        };
        let truncated = params.since_seq.is_some_and(|seq| log.is_truncated(seq));
        let newest_seq = log.newest_seq();
        let total_buffered = log.len();
        drop(log);

        let filtered = filter_entries(
            entries,
            params.search.as_deref(),
            params.regex.unwrap_or(false),
        )?;

        Ok(Json(ReadLogsResult {
            entries: filtered
                .into_iter()
                .map(|e| LogEntryOut {
                    seq: e.seq,
                    text: e.text,
                    at: entry_timestamp(e.at),
                })
                .collect(),
            newest_seq,
            total_buffered,
            truncated,
        }))
    }

    #[tool(description = "Get the current connection and logging status.")]
    fn status(&self) -> Json<StatusResult> {
        let st = self.handle.status.lock().unwrap();
        let buffered_lines = self.handle.log.lock().unwrap().len();
        Json(StatusResult {
            connected: st.connected,
            closed: st.closed,
            port: st.port.clone(),
            baud: st.baud,
            buffered_lines,
            file_logging: st.file_log_path.is_some(),
            file_log_path: st.file_log_path.clone(),
            last_error: st.last_error.clone(),
            uptime_seconds: st.uptime_seconds(),
        })
    }

    #[tool(description = "Start or stop persisting the live session to a file on disk.")]
    async fn file_log(
        &self,
        Parameters(params): Parameters<FileLogParams>,
    ) -> Result<Json<FileLogResult>, McpError> {
        let handle = self.handle.clone();
        match params.action.as_str() {
            "start" => {
                let path = params.path.ok_or_else(|| {
                    McpError::invalid_params("path is required to start file logging", None)
                })?;
                let append = params.append.unwrap_or(false);
                let info = tokio::task::spawn_blocking(move || handle.start_file_log(path, append))
                    .await
                    .map_err(blocking_err)?
                    .map_err(anyhow_err)?;
                Ok(Json(FileLogResult {
                    path: info.path.display().to_string(),
                    stats_path: info.stats_path.display().to_string(),
                    lines_written: None,
                }))
            }
            "stop" => {
                let summary = tokio::task::spawn_blocking(move || handle.stop_file_log())
                    .await
                    .map_err(blocking_err)?
                    .map_err(anyhow_err)?;
                Ok(Json(FileLogResult {
                    path: summary.path.display().to_string(),
                    stats_path: summary.stats_path.display().to_string(),
                    lines_written: Some(summary.lines_written),
                }))
            }
            other => Err(McpError::invalid_params(
                format!("action must be \"start\" or \"stop\", got {other:?}"),
                None,
            )),
        }
    }

    #[tool(description = "Clear the in-memory log ring buffer.")]
    fn clear_logs(&self) -> Json<ClearLogsResult> {
        let cleared_lines = self.handle.log.lock().unwrap().clear();
        Json(ClearLogsResult { cleared_lines })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EspMonitorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls an ESP8266/ESP32 dev board over serial: reset/power-cycle it and read its \
             buffered console output. Call `status` first to check whether the board is \
             connected before resetting or reading logs. If another process needs exclusive \
             access to the serial port (e.g. flashing firmware, running on-device tests), call \
             `close` first to release it, then `open` afterward to resume monitoring.",
        )
    }
}
