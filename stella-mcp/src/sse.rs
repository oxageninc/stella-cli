//! A minimal, dependency-free Server-Sent-Events decoder for the streamable-
//! HTTP transport. A streamable-HTTP MCP server may answer a POST with either
//! `application/json` (one response) or `text/event-stream` (JSON-RPC
//! messages carried as `data:` lines); this decoder handles the latter.
//!
//! `stella-model` has its own SSE parser, but it is that crate's internal —
//! so, per the architecture's "one tiny local decoder is fine, keep it tested"
//! guidance for this crate, this is a self-contained copy of the same
//! `push`/`poll` shape, tested against the same chunk-boundary pathologies.

/// One parsed SSE event: the newline-joined concatenation of its `data:`
/// lines (per the SSE spec), plus the `event:` name if present.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental decoder: `push` raw UTF-8 as it arrives, `poll` complete
/// events. Partial lines and events split across chunk boundaries are held
/// over to the next `push`.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of decoded UTF-8 text.
    pub fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
    }

    /// Drain every complete event currently buffered. An event terminates on
    /// a blank line; anything after the last blank line stays buffered.
    pub fn poll(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some(boundary) = self.buf.find("\n\n") {
            let raw = self.buf[..boundary].to_string();
            self.buf.drain(..boundary + 2);
            if raw.trim().is_empty() {
                continue;
            }
            events.push(parse_event(&raw));
        }
        events
    }
}

fn parse_event(raw: &str) -> SseEvent {
    let mut event = SseEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("event:") {
            event.event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        // `id:` / `retry:` are accepted-but-ignored — no replay resumption here.
    }
    event.data = data_lines.join("\n");
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_event_in_a_single_push() {
        let mut decoder = SseDecoder::new();
        decoder.push("event: message\ndata: {\"id\":1}\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"id\":1}");
    }

    #[test]
    fn handles_a_data_line_split_across_two_pushes() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: {\"partial\":");
        assert!(decoder.poll().is_empty());
        decoder.push("true}\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"partial\":true}");
    }

    #[test]
    fn handles_multiple_events_in_one_push() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: one\n\ndata: two\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
    }

    #[test]
    fn joins_multiline_data_with_newlines() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: a\ndata: b\n\n");
        let events = decoder.poll();
        assert_eq!(events[0].data, "a\nb");
    }
}
