//! A point-in-time snapshot of the reader thread's connection/logging
//! state, shared behind a mutex so both the reader thread (writer) and
//! CLI/MCP callers (readers) can access it without going through the
//! command channel.

use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ReaderStatus {
    pub connected: bool,
    pub port: String,
    pub baud: u32,
    pub file_log_path: Option<String>,
    pub stats_file_path: Option<String>,
    pub last_error: Option<String>,
    pub started_at: Instant,
}

impl ReaderStatus {
    pub fn new(port: String, baud: u32) -> Self {
        Self {
            connected: false,
            port,
            baud,
            file_log_path: None,
            stats_file_path: None,
            last_error: None,
            started_at: Instant::now(),
        }
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

pub type SharedStatus = Arc<Mutex<ReaderStatus>>;
