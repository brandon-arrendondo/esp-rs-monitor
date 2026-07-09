//! Splits a stream of raw byte chunks (as read from a serial port) into
//! complete lines, buffering any partial trailing line across calls.

#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes and get back any complete lines it
    /// produced. Bytes after the last `\n` are held until the next call.
    /// A trailing `\r` before `\n` is stripped (CRLF line endings).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);

        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
            line_bytes.pop(); // trailing '\n'
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            lines.push(String::from_utf8_lossy(&line_bytes).into_owned());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_multiple_lines() {
        let mut s = LineSplitter::new();
        let lines = s.feed(b"hello\nworld\n");
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn partial_line_buffered_across_calls() {
        let mut s = LineSplitter::new();
        assert!(s.feed(b"hel").is_empty());
        assert!(s.feed(b"lo wor").is_empty());
        let lines = s.feed(b"ld\n");
        assert_eq!(lines, vec!["hello world"]);
    }

    #[test]
    fn crlf_line_endings_stripped() {
        let mut s = LineSplitter::new();
        let lines = s.feed(b"hello\r\nworld\r\n");
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn empty_lines_preserved() {
        let mut s = LineSplitter::new();
        let lines = s.feed(b"a\n\nb\n");
        assert_eq!(lines, vec!["a", "", "b"]);
    }

    #[test]
    fn no_newline_yields_nothing() {
        let mut s = LineSplitter::new();
        assert!(s.feed(b"no newline here").is_empty());
    }

    #[test]
    fn invalid_utf8_is_lossily_decoded_not_dropped() {
        let mut s = LineSplitter::new();
        let mut chunk = b"before ".to_vec();
        chunk.push(0xFF); // invalid UTF-8 byte
        chunk.extend_from_slice(b" after\n");
        let lines = s.feed(&chunk);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("before"));
        assert!(lines[0].contains("after"));
    }

    #[test]
    fn byte_at_a_time_feed_matches_bulk_feed() {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        for b in b"one\ntwo\nthree\n" {
            out.extend(s.feed(&[*b]));
        }
        assert_eq!(out, vec!["one", "two", "three"]);
    }
}
