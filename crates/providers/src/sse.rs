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
    /// Bytes of the current incomplete line, retained until a terminator.
    buffer: Vec<u8>,
    /// Complete fields awaiting the blank line that dispatches their event.
    event: SseEvent,
    /// A CR terminates a line immediately; swallow its optional LF even when
    /// the pair crosses a network chunk boundary.
    skip_lf: bool,
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
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.finish_line(&mut events);
                    self.buffer.clear();
                    self.skip_lf = byte == b'\r';
                }
                _ => self.buffer.push(byte),
            }
        }
        events
    }

    /// Consume a complete line without discarding fields from earlier chunks.
    fn finish_line(&mut self, events: &mut Vec<SseEvent>) {
        // Decode only complete lines: split UTF-8 sequences stay buffered,
        // and malformed bytes affect only the line that contains them.
        let line = String::from_utf8_lossy(&self.buffer);
        if line.is_empty() {
            if let Some(data) = &mut self.event.data {
                while data.ends_with('\n') {
                    data.pop();
                }
            }
            events.push(std::mem::take(&mut self.event));
        } else if line.starts_with(':') {
            // Keepalives are immediate activity signals, independent of the
            // data event currently being assembled.
            events.push(SseEvent { keepalive: true, ..SseEvent::default() });
        } else if let Some(value) = line.strip_prefix("event: ") {
            self.event.event = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("data: ") {
            let data = self.event.data.get_or_insert_with(String::new);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value);
        } else if let Some(value) = line.strip_prefix("id: ") {
            self.event.id = Some(value.to_string());
        }
        // Other field lines (no `:`, unknown fields) are ignored, as before.
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
    fn complete_fields_survive_until_the_blank_line_arrives() {
        let mut parser = BufferedSseParser::new();
        assert!(parser.push_bytes(b"id: request-1\nevent: delta\n").is_empty());
        assert!(parser.push_bytes(b"data: hello\n").is_empty());
        assert!(parser.push_bytes(b"").is_empty());
        assert_eq!(
            parser.push_bytes(b"\n"),
            vec![SseEvent {
                id: Some("request-1".into()),
                event: Some("delta".into()),
                data: Some("hello".into()),
                keepalive: false,
            }]
        );
    }

    #[test]
    fn split_crlf_does_not_dispatch_an_event_early() {
        let mut parser = BufferedSseParser::new();
        assert!(parser.push_bytes(b"data: first\r").is_empty());
        assert!(parser.push_bytes(b"").is_empty());
        assert!(parser.push_bytes(b"\ndata: second\r").is_empty());
        assert!(parser.push_bytes(b"\n").is_empty());
        assert_eq!(
            parser.push_bytes(b"\r"),
            vec![SseEvent { data: Some("first\nsecond".into()), ..SseEvent::default() }]
        );
        assert!(parser.push_bytes(b"\n").is_empty());
    }

    /// Exact expected events, rather than a one-chunk parse as the oracle:
    /// fragmentation must preserve fields, order, UTF-8 and keepalives.
    fn fragmented_fixture(newline: &str) -> (Vec<u8>, Vec<SseEvent>) {
        let wire = [
            "id: first",
            "event: delta",
            "data: hello €",
            ": ping",
            "data: world",
            "",
            "event: done",
            "data: [DONE]",
            "",
            "",
        ]
        .join(newline)
        .into_bytes();
        let expected = vec![
            SseEvent { keepalive: true, ..SseEvent::default() },
            SseEvent {
                id: Some("first".into()),
                event: Some("delta".into()),
                data: Some("hello €\nworld".into()),
                keepalive: false,
            },
            SseEvent {
                event: Some("done".into()),
                data: Some("[DONE]".into()),
                ..SseEvent::default()
            },
        ];
        (wire, expected)
    }

    #[test]
    fn every_two_chunk_split_preserves_the_event_sequence() {
        for newline in ["\n", "\r", "\r\n"] {
            let (wire, expected) = fragmented_fixture(newline);
            for split in 0..=wire.len() {
                let mut parser = BufferedSseParser::new();
                let mut actual = parser.push_bytes(&wire[..split]);
                actual.extend(parser.push_bytes(&wire[split..]));
                assert_eq!(actual, expected, "newline={newline:?}, split={split}");
            }
        }
    }

    #[test]
    fn repeated_fragmentation_preserves_the_event_sequence() {
        for newline in ["\n", "\r", "\r\n"] {
            let (wire, expected) = fragmented_fixture(newline);
            for chunk_size in 1..=wire.len() {
                let mut parser = BufferedSseParser::new();
                let actual: Vec<_> =
                    wire.chunks(chunk_size).flat_map(|chunk| parser.push_bytes(chunk)).collect();
                assert_eq!(actual, expected, "newline={newline:?}, chunk_size={chunk_size}");
            }
        }
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
