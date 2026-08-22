// Shared buffered SSE parser for handling streaming events

use std::collections::VecDeque;

/// Represents a single Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: Option<String>,
    pub id: Option<String>,
    /// True when this event is a liveness signal emitted for a comment line
    /// (`: ...`), not a real server event. Consumers should treat it as stream
    /// activity (e.g. to keep an idle timeout from firing) and otherwise
    /// ignore it.
    pub keepalive: bool,
}

/// Buffered parser that can handle arbitrary chunk boundaries.
#[derive(Debug, Default)]
pub struct BufferedSseParser {
    buffer: Vec<u8>,
}

impl BufferedSseParser {
    /// Create a new parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new chunk of raw bytes into the parser.
    /// Returns any complete events that could be parsed.
    ///
    /// Lines are terminated by `\r\n`, `\r`, or `\n` per the WHATWG SSE spec,
    /// and the terminating `\r` of a `\r\n` pair is excluded from the line
    /// content. An event is dispatched on the first blank line.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut event = SseEvent::default();
        loop {
            // Scan for the next line terminator at byte level. A `\r` is treated
            // as a terminator immediately (even at the end of the buffer): a
            // `\r\n` pair split across chunks merely yields an extra blank line,
            // which is harmless to consumers.
            let mut content_end = 0;
            let mut term_end = None;
            let mut i = 0;
            while i < self.buffer.len() {
                match self.buffer[i] {
                    b'\n' => {
                        // The `\r` of a `\r\n` pair is part of the terminator.
                        content_end = if i > 0 && self.buffer[i - 1] == b'\r' { i - 1 } else { i };
                        term_end = Some(i + 1);
                        break;
                    }
                    b'\r' => {
                        if matches!(self.buffer.get(i + 1), Some(b'\n')) {
                            // CRLF: move past the `\r`; the `\n` finishes the
                            // line and excludes the `\r` from the content.
                            i += 1;
                            continue;
                        }
                        content_end = i;
                        term_end = Some(i + 1);
                        break;
                    }
                    _ => i += 1,
                }
            }
            let Some(term_end) = term_end else {
                break;
            };

            // Extract the complete line (content only, terminator excluded).
            // `from_utf8` failure can only mean the line itself is malformed:
            // newline bytes are ASCII, so a multi-byte UTF-8 sequence can never
            // span lines. A multi-byte character split across chunks stays raw
            // in the byte buffer until its line completes, so it survives
            // intact. The lossy fallback is therefore scoped to THIS line only
            // and can never corrupt adjacent lines.
            let line_bytes = self.buffer.drain(..term_end).collect::<Vec<u8>>();
            let line: std::borrow::Cow<'_, str> =
                match std::str::from_utf8(&line_bytes[..content_end]) {
                    Ok(s) => std::borrow::Cow::Borrowed(s),
                    Err(_) => std::borrow::Cow::Owned(
                        String::from_utf8_lossy(&line_bytes[..content_end]).into_owned(),
                    ),
                };

            if line.is_empty() {
                // Blank line: dispatch the accumulated event.
                if let Some(d) = &mut event.data {
                    while d.ends_with('\n') {
                        d.pop();
                    }
                }
                events.push(event);
                event = SseEvent::default();
            } else if line.starts_with(':') {
                // Comment line: emit a liveness signal immediately so long
                // keep-alive-only periods still count as stream activity. The
                // comment is not part of the event being accumulated.
                events.push(SseEvent { event: None, data: None, id: None, keepalive: true });
            } else if let Some(v) = line.strip_prefix("event: ") {
                event.event = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("data: ") {
                let data = event.data.get_or_insert_with(String::new);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v);
            } else if let Some(v) = line.strip_prefix("id: ") {
                event.id = Some(v.to_string());
            }
            // Other field lines (no `:`, unknown fields) are ignored, as before.
        }
        events
    }
}

// Helper to convert a vector of SseEvent into a FIFO queue for streaming.
pub fn events_to_queue(events: Vec<SseEvent>) -> VecDeque<SseEvent> {
    events.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_complete_event_in_one_chunk() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"event: delta\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data.as_deref(), Some("hello"));
        assert!(!events[0].keepalive);
    }

    #[test]
    fn parse_event_split_across_chunks() {
        let mut parser = BufferedSseParser::new();
        let events1 = parser.push_bytes(b"event: delta\ndata: hel");
        assert!(events1.is_empty());

        let events2 = parser.push_bytes(b"lo\n\nevent: done\ndata: [DONE]\n\n");
        assert_eq!(events2.len(), 2);
        assert_eq!(events2[0].data.as_deref(), Some("hello"));
        assert_eq!(events2[1].event.as_deref(), Some("done"));
        assert_eq!(events2[1].data.as_deref(), Some("[DONE]"));
    }

    #[test]
    fn multi_data_lines_concatenate() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn trailing_newline_is_trimmed() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"data: payload\n\n");
        assert_eq!(events[0].data.as_deref(), Some("payload"));
    }

    #[test]
    fn crlf_terminated_event_dispatches() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"event: delta\r\ndata: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data.as_deref(), Some("hello"));
    }

    #[test]
    fn lone_cr_lines_dispatch_event() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"data: line1\rdata: line2\r\r");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn mixed_terminators_in_one_chunk() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"data: a\ndata: b\r\ndata: c\r\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.as_deref(), Some("a\nb\nc"));
    }

    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        let mut parser = BufferedSseParser::new();
        // "€" is U+20AC = bytes E2 82 AC; the first push ends mid-character.
        let events1 = parser.push_bytes(b"data: hello \xE2\x82");
        assert!(events1.is_empty());
        let events2 = parser.push_bytes(b"\xAC world\n\n");
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].data.as_deref(), Some("hello \u{20AC} world"));
    }

    #[test]
    fn comment_line_emits_keepalive_and_does_not_disturb_event() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b": keep-alive\ndata: hello\n\n");
        assert_eq!(events.len(), 2);
        assert!(events[0].keepalive);
        assert!(events[0].event.is_none() && events[0].data.is_none());
        assert!(!events[1].keepalive);
        assert_eq!(events[1].data.as_deref(), Some("hello"));
    }

    #[test]
    fn malformed_line_decodes_lossy_only_for_that_line() {
        let mut parser = BufferedSseParser::new();
        let events = parser.push_bytes(b"data: bad\xFFline\n\ndata: clean\n\n");
        assert_eq!(events.len(), 2);
        let first = events[0].data.as_deref().unwrap();
        assert!(first.contains('\u{FFFD}'));
        assert!(!first.contains("clean"));
        assert_eq!(events[1].data.as_deref(), Some("clean"));
    }
}
