//! A minimal, dependency-free Server-Sent-Events decoder for the streamable-
//! HTTP transport. A streamable-HTTP MCP server may answer a POST with either
//! `application/json` (one response) or `text/event-stream` (JSON-RPC
//! messages carried as `data:` lines); this decoder handles the latter.
//!
//! `stella-model` has its own SSE parser, but it is that crate's internal —
//! so, per the architecture's "one tiny local decoder is fine, keep it tested"
//! guidance for this crate, this is a self-contained copy of the same
//! `push`/`poll` shape, tested against the same chunk-boundary pathologies.

// (The copy tracks stella-model's decoder shape, including its
// `push_bytes` UTF-8 chunk-boundary handling — see that crate's sse.rs.)

/// One parsed SSE event: the newline-joined concatenation of its `data:`
/// lines (per the SSE spec), plus the `event:` name if present.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// The stream contained bytes that are not merely an incomplete trailing
/// UTF-8 sequence (which the decoder buffers) but an actually invalid one.
/// Terminal — a genuinely malformed body will not become valid by waiting
/// for more bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUtf8;

impl std::fmt::Display for InvalidUtf8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SSE stream contained invalid UTF-8")
    }
}

impl std::error::Error for InvalidUtf8 {}

/// Incremental decoder: `push_bytes` raw bytes as they arrive, `poll`
/// complete events. Partial lines, events split across chunk boundaries, and
/// multi-byte UTF-8 characters split across chunk boundaries are all held
/// over to the next push — a lossy per-chunk conversion would turn a
/// boundary-split character into U+FFFD and corrupt the JSON-RPC payload.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: String,
    /// At most 3 bytes: a UTF-8 sequence is 1–4 bytes, so a valid-but-
    /// incomplete tail can never exceed 3 buffered bytes.
    utf8_tail: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of already-decoded UTF-8 text. Retained for unit tests
    /// (and compiled only there — no production caller exists); transports
    /// reading a byte stream must use [`SseDecoder::push_bytes`] so a
    /// character split across two network chunks is reassembled rather than
    /// mangled.
    #[cfg(test)]
    pub fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
    }

    /// Feed a chunk of raw bytes straight off the wire. An incomplete
    /// trailing multi-byte sequence is buffered until the next chunk; only
    /// genuinely invalid UTF-8 is an error.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), InvalidUtf8> {
        self.utf8_tail.extend_from_slice(chunk);
        match std::str::from_utf8(&self.utf8_tail) {
            Ok(valid) => {
                self.buf.push_str(valid);
                self.utf8_tail.clear();
                Ok(())
            }
            Err(err) => {
                // `error_len().is_some()` ⇒ an actually invalid sequence
                // sits at `valid_up_to`; `None` ⇒ the buffer merely ends
                // mid-character and the rest is still to come.
                if err.error_len().is_some() {
                    return Err(InvalidUtf8);
                }
                let valid_up_to = err.valid_up_to();
                let valid = std::str::from_utf8(&self.utf8_tail[..valid_up_to])
                    .expect("valid_up_to marks a validated UTF-8 boundary");
                self.buf.push_str(valid);
                self.utf8_tail.drain(..valid_up_to);
                Ok(())
            }
        }
    }

    /// Drain every complete event currently buffered. An event terminates on
    /// a blank line — `\n\n` (LF) or `\r\n\r\n` (CRLF, which the SSE spec
    /// permits and proxies often produce); anything after the last blank line
    /// stays buffered.
    pub fn poll(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        loop {
            // Take whichever blank-line boundary appears first, and drain its
            // own separator length (2 for LF-LF, 4 for CRLF-CRLF).
            let (boundary, sep_len) = match (self.buf.find("\n\n"), self.buf.find("\r\n\r\n")) {
                (Some(lf), Some(crlf)) if crlf < lf => (crlf, 4),
                (Some(lf), _) => (lf, 2),
                (None, Some(crlf)) => (crlf, 4),
                (None, None) => break,
            };
            let raw = self.buf[..boundary].to_string();
            self.buf.drain(..boundary + sep_len);
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

    #[test]
    fn push_bytes_reassembles_a_multibyte_char_split_across_chunks() {
        // Regression: the transport used to do `from_utf8_lossy` per chunk,
        // so a multi-byte character straddling a network chunk boundary
        // became U+FFFD and corrupted the JSON-RPC payload.
        let full = "data: {\"msg\":\"caf\u{00e9}\"}\n\n";
        let bytes = full.as_bytes();
        let idx = full.find('é').unwrap() + 1; // split inside 'é'
        let (head, tail) = bytes.split_at(idx);

        let mut decoder = SseDecoder::new();
        decoder
            .push_bytes(head)
            .expect("incomplete char is not an error");
        assert!(decoder.poll().is_empty(), "event not terminated yet");
        decoder
            .push_bytes(tail)
            .expect("rest of the stream is valid");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"msg\":\"café\"}");
    }

    #[test]
    fn push_bytes_rejects_genuinely_invalid_utf8() {
        // 0xFF is never valid anywhere in UTF-8 — a real error, not a
        // truncation the next chunk could complete.
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push_bytes(&[0xFF]), Err(InvalidUtf8));
    }

    #[test]
    fn parses_crlf_framed_events() {
        // The SSE spec permits CRLF line endings, and HTTP proxies often
        // normalize to them — the boundary scan must match `\r\n\r\n`, not
        // only `\n\n`, or a CRLF server would stall until timeout.
        let mut decoder = SseDecoder::new();
        decoder.push("event: message\r\ndata: {\"id\":1}\r\n\r\ndata: two\r\n\r\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"id\":1}");
        assert_eq!(events[1].data, "two");
    }
}
