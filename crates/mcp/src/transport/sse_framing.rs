//! Event-stream framing shared by the HTTP transports.
//!
//! Two consumers: the streamable-HTTP transport parses a POST reply that the
//! server frames as `text/event-stream`, and the legacy SSE transport bounds
//! the size of one event on its long-lived stream. Both need the same answer
//! to "where does an event end", and both must get it right when a line
//! terminator is split across two network chunks.
//!
//! Framing rules: a line ends at LF, CR, or CRLF. An event ends at an empty
//! line. The size cap applies to one event, not to the stream, so a
//! long-lived stream of small events is never refused.

/// One event exceeded the per-event byte cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventTooLarge {
    /// The cap that was exceeded, in bytes.
    pub cap: usize,
}

/// What one byte means for the framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A byte of the current line.
    LineByte,
    /// The end of a line that has content.
    LineEnd,
    /// The end of an empty line: the event is complete.
    EventEnd,
    /// The LF half of a CRLF whose CR already ended the line.
    Skip,
}

/// Counts the bytes of the current event and finds event boundaries.
///
/// The state survives between chunks, so a CRLF (or a blank line) that is
/// split across two chunks is seen exactly as if it arrived in one.
#[derive(Debug)]
pub(crate) struct EventSizeGuard {
    cap: usize,
    event_bytes: usize,
    line_is_empty: bool,
    last_was_cr: bool,
}

impl EventSizeGuard {
    /// A guard that allows at most `cap` bytes per event.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            event_bytes: 0,
            line_is_empty: true,
            last_was_cr: false,
        }
    }

    /// Account for one chunk. Fails as soon as the current event is larger
    /// than the cap, so the caller can stop the read before it buffers more.
    #[cfg(test)]
    fn observe(&mut self, chunk: &[u8]) -> Result<(), EventTooLarge> {
        for &byte in chunk {
            self.step(byte)?;
        }
        Ok(())
    }

    fn step(&mut self, byte: u8) -> Result<Step, EventTooLarge> {
        let after_cr = std::mem::replace(&mut self.last_was_cr, byte == b'\r');
        if byte == b'\n' && after_cr {
            return Ok(Step::Skip);
        }
        if byte == b'\n' || byte == b'\r' {
            if self.line_is_empty {
                self.event_bytes = 0;
                return Ok(Step::EventEnd);
            }
            self.line_is_empty = true;
            self.count_byte()?;
            return Ok(Step::LineEnd);
        }
        self.line_is_empty = false;
        self.count_byte()?;
        Ok(Step::LineByte)
    }

    fn count_byte(&mut self) -> Result<(), EventTooLarge> {
        self.event_bytes += 1;
        if self.event_bytes > self.cap {
            return Err(EventTooLarge { cap: self.cap });
        }
        Ok(())
    }
}

/// One complete event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field. Empty when the server did not name the event.
    pub name: String,
    /// The `data:` lines, joined with `\n`.
    pub data: String,
}

impl SseEvent {
    /// True when the event carries a JSON-RPC message: the name is `message`,
    /// or the server gave no name.
    pub(crate) fn is_message(&self) -> bool {
        self.name.is_empty() || self.name == "message"
    }
}

/// Incremental event-stream parser with a per-event byte cap.
#[derive(Debug)]
pub(crate) struct SseParser {
    guard: EventSizeGuard,
    line: Vec<u8>,
    name: String,
    data: Vec<String>,
}

impl SseParser {
    /// A parser that refuses an event larger than `cap` bytes.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            guard: EventSizeGuard::new(cap),
            line: Vec::new(),
            name: String::new(),
            data: Vec::new(),
        }
    }

    /// Feed one chunk and return the events that it completed.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, EventTooLarge> {
        let mut events = Vec::new();
        for &byte in chunk {
            match self.guard.step(byte)? {
                Step::LineByte => self.line.push(byte),
                Step::LineEnd => self.end_line(),
                Step::EventEnd => {
                    if let Some(event) = self.end_event() {
                        events.push(event);
                    }
                }
                Step::Skip => {}
            }
        }
        Ok(events)
    }

    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&line);
        // A line that starts with a colon is a comment (servers use it as a
        // keep-alive). `split_once` gives it an empty field name.
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.name = value.to_string(),
            _ => {}
        }
    }

    fn end_event(&mut self) -> Option<SseEvent> {
        let name = std::mem::take(&mut self.name);
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseEvent { name, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent {
            name: name.to_string(),
            data: data.to_string(),
        }
    }

    /// Feed `stream` in two chunks, split at each possible offset, and
    /// collect all events.
    fn parse_at_every_split(stream: &[u8], cap: usize) -> Vec<(usize, Vec<SseEvent>)> {
        (0..=stream.len())
            .map(|at| {
                let mut parser = SseParser::new(cap);
                let mut events = parser.feed(&stream[..at]).expect("first chunk");
                events.extend(parser.feed(&stream[at..]).expect("second chunk"));
                (at, events)
            })
            .collect()
    }

    #[test]
    fn a_terminator_split_across_chunks_is_one_terminator() {
        // A CR at the end of one chunk and its LF at the start of the next
        // are one line end. A parser that sees two would end the event after
        // the first line and lose the rest.
        let streams: &[&[u8]] = &[
            b"event: message\ndata: one\n\ndata: two\n\n",
            b"event: message\r\ndata: one\r\n\r\ndata: two\r\n\r\n",
            b"event: message\rdata: one\r\rdata: two\r\r",
        ];
        let expected = vec![event("message", "one"), event("", "two")];
        for stream in streams {
            for (at, events) in parse_at_every_split(stream, 1024) {
                assert_eq!(
                    events,
                    expected,
                    "stream {:?} split at {at}",
                    String::from_utf8_lossy(stream)
                );
            }
        }
    }

    #[test]
    fn a_line_end_inside_an_event_is_not_a_boundary() {
        // Each data line ends with one CRLF. Only the empty line ends the
        // event, so the three lines arrive as one event.
        let stream = b"data: a\r\ndata: b\r\ndata: c\r\n\r\n";
        for (at, events) in parse_at_every_split(stream, 1024) {
            assert_eq!(events, vec![event("", "a\nb\nc")], "split at {at}");
        }
    }

    #[test]
    fn fields_follow_the_event_stream_rules() {
        // (name, stream, expected events)
        let cases: &[(&str, &[u8], Vec<SseEvent>)] = &[
            (
                "comment lines are ignored",
                b": keep-alive\ndata: x\n\n",
                vec![event("", "x")],
            ),
            (
                "only one space after the colon is removed",
                b"data:  x\ndata:y\n\n",
                vec![event("", " x\ny")],
            ),
            (
                "an event with no data is not dispatched",
                b"event: ping\n\ndata: x\n\n",
                vec![event("", "x")],
            ),
            (
                "the event name does not leak into the next event",
                b"event: endpoint\ndata: /p\n\ndata: x\n\n",
                vec![event("endpoint", "/p"), event("", "x")],
            ),
            (
                "an event without the final empty line is incomplete",
                b"data: x\n",
                vec![],
            ),
            (
                "unknown fields are ignored",
                b"id: 7\nretry: 100\ndata: x\n\n",
                vec![event("", "x")],
            ),
        ];
        for (name, stream, expected) in cases {
            let events = SseParser::new(1024).feed(stream).expect(name);
            assert_eq!(&events, expected, "case: {name}");
        }
    }

    #[test]
    fn only_unnamed_and_message_events_carry_json_rpc() {
        assert!(event("", "{}").is_message());
        assert!(event("message", "{}").is_message());
        assert!(!event("endpoint", "/p").is_message());
    }

    #[test]
    fn the_cap_applies_to_one_event_and_resets_at_each_boundary() {
        // Ten events of 9 bytes each are 90 bytes in total. With a cap of
        // 16 they all pass, because no single event is larger than the cap.
        let mut guard = EventSizeGuard::new(16);
        for _ in 0..10 {
            guard.observe(b"data: ab\n\n").expect("small event");
        }

        // One event that never ends is refused when it passes the cap, and
        // not before.
        let mut guard = EventSizeGuard::new(16);
        guard.observe(&[b'x'; 16]).expect("at the cap");
        assert_eq!(guard.observe(b"x"), Err(EventTooLarge { cap: 16 }));
    }

    #[test]
    fn a_boundary_split_across_chunks_resets_the_counter() {
        // Each event is 14 bytes, below the cap of 16. The blank line is
        // split across chunks at each offset. If the split hid the boundary,
        // the second event would add to the first and exceed the cap.
        for boundary in [&b"\n\n"[..], b"\r\n\r\n", b"\r\r"] {
            let mut stream = b"data: 12345678".to_vec();
            stream.extend_from_slice(boundary);
            stream.extend_from_slice(b"data: 12345678");
            for at in 0..=stream.len() {
                let mut guard = EventSizeGuard::new(16);
                guard.observe(&stream[..at]).expect("first chunk");
                guard.observe(&stream[at..]).unwrap_or_else(|e| {
                    panic!("boundary {boundary:?} split at {at} was not seen: {e:?}")
                });
            }
        }
    }

    #[test]
    fn many_single_line_ends_do_not_reset_the_counter() {
        // A hostile server must not get past the cap with single line ends
        // between the lines of one event.
        let mut guard = EventSizeGuard::new(32);
        let mut result = Ok(());
        for _ in 0..10 {
            result = result.and(guard.observe(b"data: abcdef\r\n"));
        }
        assert_eq!(result, Err(EventTooLarge { cap: 32 }));
    }
}
