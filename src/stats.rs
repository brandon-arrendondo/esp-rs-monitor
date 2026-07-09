//! Pulls `/* ... */`-delimited "system stat" packets out of the log line
//! stream so they can be routed to a separate stats file/stream instead of
//! the regular console log.

pub struct StatsExtractor;

impl StatsExtractor {
    pub fn new() -> Self {
        Self
    }

    /// If `line` contains a `/* ... */` block, returns its inner content
    /// (the line should then be treated as a stats packet, not a normal
    /// log line). Otherwise returns `None` and `line` is an ordinary line.
    pub fn feed_line(&self, line: &str) -> Option<String> {
        let start = line.find("/*")?;
        let after_open = start + 2;
        let end = line[after_open..].find("*/")?;
        Some(line[after_open..after_open + end].to_string())
    }
}

impl Default for StatsExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_stat_block() {
        let e = StatsExtractor::new();
        assert_eq!(
            e.feed_line("/* heap=1234 uptime=56 */"),
            Some(" heap=1234 uptime=56 ".to_string())
        );
    }

    #[test]
    fn extracts_stat_block_with_surrounding_text() {
        let e = StatsExtractor::new();
        assert_eq!(
            e.feed_line("prefix /*stat*/ suffix"),
            Some("stat".to_string())
        );
    }

    #[test]
    fn ordinary_line_returns_none() {
        let e = StatsExtractor::new();
        assert_eq!(e.feed_line("just a normal boot log line"), None);
    }

    #[test]
    fn unterminated_block_returns_none() {
        let e = StatsExtractor::new();
        assert_eq!(e.feed_line("/* never closed"), None);
    }

    #[test]
    fn only_close_marker_returns_none() {
        let e = StatsExtractor::new();
        assert_eq!(e.feed_line("no open marker */"), None);
    }
}
