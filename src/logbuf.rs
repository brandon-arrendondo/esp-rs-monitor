//! An in-memory, size-bounded ring buffer of log lines, shared between the
//! background serial reader and anything that wants to query recent
//! history (the CLI's console mode, MCP tools) without having "been
//! watching" since the buffer's first line.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Monotonically increasing sequence number, stable across evictions,
    /// so callers can poll "everything since I last checked" via `since`.
    pub seq: u64,
    pub text: String,
    pub at: SystemTime,
}

pub struct RingBuffer {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
    max_lines: usize,
    max_bytes: usize,
    total_bytes: usize,
}

impl RingBuffer {
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 0,
            max_lines,
            max_bytes,
            total_bytes: 0,
        }
    }

    /// Appends a line, evicting the oldest entries if the line-count or
    /// byte-size cap is exceeded. Returns the new entry's sequence number.
    pub fn push(&mut self, text: String) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.total_bytes += text.len();
        self.entries.push_back(LogEntry {
            seq,
            text,
            at: SystemTime::now(),
        });
        self.evict();
        seq
    }

    fn evict(&mut self) {
        while self.entries.len() > self.max_lines || self.total_bytes > self.max_bytes {
            match self.entries.pop_front() {
                Some(e) => self.total_bytes -= e.text.len(),
                None => break,
            }
        }
    }

    /// The most recent `n` entries, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogEntry> {
        let skip = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(skip).cloned().collect()
    }

    /// All entries with `seq` strictly greater than `since_seq`.
    ///
    /// If `since_seq` is older than the oldest entry still buffered, the
    /// gap can't be filled in — the caller should treat the result as
    /// possibly missing history (see `is_truncated`).
    pub fn since(&self, since_seq: u64) -> Vec<LogEntry> {
        self.entries
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect()
    }

    /// True if `since_seq` predates what's still buffered, meaning some
    /// history between it and the oldest buffered entry was evicted.
    pub fn is_truncated(&self, since_seq: u64) -> bool {
        match self.entries.front() {
            Some(oldest) => since_seq + 1 < oldest.seq,
            None => since_seq + 1 < self.next_seq,
        }
    }

    pub fn newest_seq(&self) -> Option<u64> {
        self.entries.back().map(|e| e.seq)
    }

    pub fn oldest_seq(&self) -> Option<u64> {
        self.entries.front().map(|e| e.seq)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity_lines(&self) -> usize {
        self.max_lines
    }

    /// Clears all buffered entries (does not reset the sequence counter,
    /// so `seq` values remain stable/comparable across a clear).
    pub fn clear(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        self.total_bytes = 0;
        n
    }
}

pub type SharedRingBuffer = Arc<Mutex<RingBuffer>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_assigns_increasing_seq() {
        let mut rb = RingBuffer::new(100, 10_000);
        let s0 = rb.push("a".into());
        let s1 = rb.push("b".into());
        let s2 = rb.push("c".into());
        assert_eq!((s0, s1, s2), (0, 1, 2));
    }

    #[test]
    fn evicts_oldest_when_line_cap_exceeded() {
        let mut rb = RingBuffer::new(2, 10_000);
        rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into());
        let tail: Vec<_> = rb.tail(10).into_iter().map(|e| e.text).collect();
        assert_eq!(tail, vec!["b", "c"]);
        assert_eq!(rb.len(), 2);
    }

    #[test]
    fn evicts_oldest_when_byte_cap_exceeded() {
        let mut rb = RingBuffer::new(100, 5);
        rb.push("aaa".into()); // 3 bytes
        rb.push("bbb".into()); // 3 bytes, total 6 > 5, evict "aaa"
        let tail: Vec<_> = rb.tail(10).into_iter().map(|e| e.text).collect();
        assert_eq!(tail, vec!["bbb"]);
    }

    #[test]
    fn tail_returns_oldest_first() {
        let mut rb = RingBuffer::new(100, 10_000);
        for c in ["a", "b", "c", "d"] {
            rb.push(c.into());
        }
        let tail: Vec<_> = rb.tail(2).into_iter().map(|e| e.text).collect();
        assert_eq!(tail, vec!["c", "d"]);
    }

    #[test]
    fn tail_larger_than_buffer_returns_everything() {
        let mut rb = RingBuffer::new(100, 10_000);
        rb.push("a".into());
        rb.push("b".into());
        assert_eq!(rb.tail(50).len(), 2);
    }

    #[test]
    fn since_returns_only_newer_entries() {
        let mut rb = RingBuffer::new(100, 10_000);
        let s0 = rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into());
        let newer: Vec<_> = rb.since(s0).into_iter().map(|e| e.text).collect();
        assert_eq!(newer, vec!["b", "c"]);
    }

    #[test]
    fn since_with_newest_seq_returns_empty() {
        let mut rb = RingBuffer::new(100, 10_000);
        rb.push("a".into());
        let last = rb.push("b".into());
        assert!(rb.since(last).is_empty());
    }

    #[test]
    fn is_truncated_true_when_unread_entry_was_evicted() {
        // Caller has seen up through "a" (seq 0) and hasn't read "b" (seq 1)
        // yet. If "b" gets evicted before the caller polls again, that's a
        // real gap in what since(0) can return.
        let mut rb = RingBuffer::new(2, 10_000);
        let s0 = rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into()); // evicts "a"
        rb.push("d".into()); // evicts "b" — now there's a real gap
        assert!(rb.is_truncated(s0));
    }

    #[test]
    fn is_truncated_false_when_evicted_entry_was_already_seen() {
        // Caller has seen through "a" (seq 0); "a" itself being evicted
        // doesn't lose anything the caller still needed.
        let mut rb = RingBuffer::new(2, 10_000);
        let s0 = rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into()); // evicts "a", but since(0) == [b, c] intact
        assert!(!rb.is_truncated(s0));
    }

    #[test]
    fn is_truncated_false_when_history_intact() {
        let mut rb = RingBuffer::new(100, 10_000);
        let s0 = rb.push("a".into());
        rb.push("b".into());
        assert!(!rb.is_truncated(s0));
    }

    #[test]
    fn is_truncated_false_on_buffer_that_was_never_pushed_to() {
        let rb = RingBuffer::new(100, 10_000);
        assert!(!rb.is_truncated(0));
        assert!(!rb.is_truncated(1000));
    }

    #[test]
    fn clear_empties_buffer_but_keeps_seq_counter_moving() {
        let mut rb = RingBuffer::new(100, 10_000);
        rb.push("a".into());
        rb.push("b".into());
        let cleared = rb.clear();
        assert_eq!(cleared, 2);
        assert!(rb.is_empty());
        let next = rb.push("c".into());
        assert_eq!(next, 2, "seq must not reset/collide after clear");
    }

    #[test]
    fn newest_and_oldest_seq_track_buffer_contents() {
        let mut rb = RingBuffer::new(2, 10_000);
        assert_eq!(rb.newest_seq(), None);
        assert_eq!(rb.oldest_seq(), None);
        rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into()); // evicts "a" (seq 0)
        assert_eq!(rb.oldest_seq(), Some(1));
        assert_eq!(rb.newest_seq(), Some(2));
    }
}
