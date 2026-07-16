//! Minimal, dependency-free Server-Sent-Events line parser shared by every
//! streaming adapter. Retiring "raw SSE parsing quality" as a Phase 0 risk
//! (`03-plan.md`) means this parser is unit-tested against the exact
//! chunking pathologies a real HTTP stream produces: a `data:` line split
//! across two reads, multiple events in one read, and a trailing partial
//! line held over to the next chunk.

/// One parsed SSE event: the concatenation of every `data:` line in the
/// event, newline-joined (per the SSE spec), plus the event name if the
/// stream sent one via `event:`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// The stream contained bytes that are not merely an incomplete trailing
/// UTF-8 sequence (which [`Utf8Decoder`] buffers) but an actually invalid
/// one. Terminal — a genuinely malformed body will not become valid by
/// waiting for more bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidUtf8;

impl std::fmt::Display for InvalidUtf8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stream contained invalid UTF-8")
    }
}

impl std::error::Error for InvalidUtf8 {}

/// Incremental UTF-8 decoder for a byte stream whose chunk boundaries can
/// fall in the middle of a multi-byte character. Feed raw bytes; get back the
/// longest valid UTF-8 prefix, with any incomplete trailing sequence buffered
/// for the next `push`. This is the fix for the "split multi-byte char across
/// two network chunks aborts the turn" defect: a naive `str::from_utf8` per
/// chunk fails on a boundary-split character even though the stream is
/// perfectly valid UTF-8 once reassembled.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    /// At most 3 bytes: a UTF-8 sequence is 1–4 bytes, so a valid-but-
    /// incomplete tail can never exceed 3 buffered bytes.
    tail: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode as much of (`tail` ++ `chunk`) as forms complete UTF-8,
    /// returning that prefix as an owned `String` and buffering any
    /// incomplete trailing multi-byte sequence for the next call. Errors only
    /// on bytes that are genuinely invalid — never on a merely truncated
    /// trailing sequence, which is exactly the boundary-split case that must
    /// succeed once the rest arrives.
    pub fn push(&mut self, chunk: &[u8]) -> Result<String, InvalidUtf8> {
        self.tail.extend_from_slice(chunk);
        match std::str::from_utf8(&self.tail) {
            Ok(valid) => {
                let out = valid.to_string();
                self.tail.clear();
                Ok(out)
            }
            Err(err) => {
                // `error_len().is_some()` ⇒ an actually invalid sequence
                // sits at `valid_up_to`; `None` ⇒ the buffer merely ends
                // mid-character and the rest is still to come.
                if err.error_len().is_some() {
                    return Err(InvalidUtf8);
                }
                let valid_up_to = err.valid_up_to();
                // SAFETY-by-construction: `valid_up_to` is a validated
                // boundary, so this slice is guaranteed valid UTF-8.
                let out = std::str::from_utf8(&self.tail[..valid_up_to])
                    .expect("valid_up_to marks a validated UTF-8 boundary")
                    .to_string();
                self.tail.drain(..valid_up_to);
                Ok(out)
            }
        }
    }
}

/// Incremental SSE decoder: feed it raw bytes as they arrive over the wire
/// (`push_bytes`), drain complete events (`poll`). Handles partial lines,
/// partial events, and multi-byte characters split across arbitrary chunk
/// boundaries.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: String,
    utf8: Utf8Decoder,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of already-decoded UTF-8 text. Retained for callers (and
    /// unit tests) that hand over `&str` directly; adapters reading a byte
    /// stream must use [`SseDecoder::push_bytes`] so a character split across
    /// two network chunks is reassembled rather than rejected.
    pub fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
    }

    /// Feed a chunk of raw bytes straight off the wire. Incomplete trailing
    /// multi-byte sequences are buffered until the next chunk; only genuinely
    /// invalid UTF-8 is an error.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), InvalidUtf8> {
        let decoded = self.utf8.push(chunk)?;
        self.buf.push_str(&decoded);
        Ok(())
    }

    /// Drain every complete event currently buffered. An event is complete
    /// once a blank line terminates it — `\n\n` or, per the SSE spec's line
    /// endings, `\r\n\r\n`; anything after the last blank line stays buffered
    /// for the next `push`/`push_bytes`.
    pub fn poll(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some((boundary, delim_len)) = next_event_boundary(&self.buf) {
            let raw_event = self.buf[..boundary].to_string();
            self.buf.drain(..boundary + delim_len);
            if raw_event.trim().is_empty() {
                continue;
            }
            events.push(parse_event(&raw_event));
        }
        events
    }
}

/// Find the earliest blank-line event boundary and its delimiter length.
/// Handles both `\n\n` (LF) and `\r\n\r\n` (CRLF) terminators — a stream may
/// use either, and the CRLF form is what the SSE spec actually mandates, so a
/// decoder that only split on `\n\n` would buffer a CRLF stream forever.
fn next_event_boundary(buf: &str) -> Option<(usize, usize)> {
    // CRLF and LF forms cannot overlap-alias (`\r\n\r\n` contains no `\n\n`),
    // so taking the minimum index across both patterns is well-defined.
    [("\r\n\r\n", 4usize), ("\n\n", 2usize)]
        .into_iter()
        .filter_map(|(pat, len)| buf.find(pat).map(|idx| (idx, len)))
        .min_by_key(|(idx, _)| *idx)
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
        // `id:` and `retry:` fields are accepted-but-ignored: no adapter
        // here needs replay-id resumption yet.
    }
    event.data = data_lines.join("\n");
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_complete_event_in_a_single_push() {
        let mut decoder = SseDecoder::new();
        decoder.push("event: message\ndata: {\"hello\":true}\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("message".into()));
        assert_eq!(events[0].data, "{\"hello\":true}");
    }

    #[test]
    fn handles_a_data_line_split_across_two_pushes() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: {\"partial\":");
        assert!(decoder.poll().is_empty(), "no complete event yet");
        decoder.push("true}\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"partial\":true}");
    }

    #[test]
    fn handles_multiple_events_in_one_push() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: one\n\ndata: two\n\ndata: three\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        assert_eq!(events[2].data, "three");
    }

    #[test]
    fn holds_a_trailing_partial_event_for_the_next_poll() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: complete\n\ndata: incomplete");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "complete");
        decoder.push(" now\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "incomplete now");
    }

    #[test]
    fn joins_multiline_data_fields_with_newlines_per_sse_spec() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: line one\ndata: line two\n\n");
        let events = decoder.poll();
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn ignores_blank_events_between_boundaries() {
        let mut decoder = SseDecoder::new();
        decoder.push("\n\ndata: real\n\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    // ---- CRLF event delimiters (SSE spec line endings) -------------------

    #[test]
    fn handles_crlf_event_delimiters() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: hello\r\n\r\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn handles_a_crlf_event_split_across_two_pushes() {
        let mut decoder = SseDecoder::new();
        decoder.push("data: split\r\n");
        assert!(decoder.poll().is_empty(), "no complete event yet");
        decoder.push("\r\n");
        let events = decoder.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "split");
    }

    // ---- Utf8Decoder: multi-byte characters split across chunks ----------

    #[test]
    fn utf8_decoder_reassembles_a_multibyte_char_split_across_two_pushes() {
        // "é" is U+00E9 → two bytes 0xC3 0xA9. Split them across two pushes:
        // the first push must NOT error (it's incomplete, not invalid) and
        // must buffer the lead byte; the second completes the character.
        let bytes = "café".as_bytes();
        let split = bytes.len() - 1; // between the two bytes of 'é'
        let (head, tail) = bytes.split_at(split);

        let mut decoder = Utf8Decoder::new();
        let first = decoder.push(head).expect("incomplete tail is not an error");
        assert_eq!(first, "caf"); // the trailing lead byte is buffered
        let second = decoder.push(tail).expect("second half completes the char");
        assert_eq!(second, "é");
    }

    #[test]
    fn utf8_decoder_handles_a_four_byte_emoji_split_byte_by_byte() {
        // "🦀" is U+1F980 → four bytes; feed one byte at a time. Only the
        // final byte should yield output; none should error.
        let bytes = "🦀".as_bytes();
        assert_eq!(bytes.len(), 4);
        let mut decoder = Utf8Decoder::new();
        let mut assembled = String::new();
        for (i, b) in bytes.iter().enumerate() {
            let out = decoder.push(&[*b]).expect("no partial byte is invalid");
            if i < 3 {
                assert!(out.is_empty(), "byte {i} should not complete the char");
            }
            assembled.push_str(&out);
        }
        assert_eq!(assembled, "🦀");
    }

    #[test]
    fn utf8_decoder_rejects_genuinely_invalid_bytes() {
        // 0xFF is never valid anywhere in UTF-8 — this is a real error, not a
        // truncation the next chunk could complete.
        let mut decoder = Utf8Decoder::new();
        assert_eq!(decoder.push(&[0xFF]), Err(InvalidUtf8));
    }

    #[test]
    fn large_multi_fragment_tool_payload_reassembles_when_fed_one_byte_at_a_time() {
        // Build a large tool-call JSON, fragment it into many SSE events, and
        // feed the whole wire stream ONE BYTE AT A TIME — the worst possible
        // chunk boundary alignment. Then reassemble the `partial_json`
        // fragments and assert the JSON round-trips. This exercises the SSE
        // decoder's boundary handling far past what a loopback mock server
        // (which delivers large chunks) can.
        let mut content = String::new();
        for i in 0..300 {
            content.push_str(&format!("line {i}: \"quoted\" \\ and more\n"));
        }
        let full = serde_json::json!({"path": "README.md", "content": content});
        let full_json = serde_json::to_string(&full).unwrap();

        let chars: Vec<char> = full_json.chars().collect();
        let mut wire = String::new();
        for piece in chars.chunks(13) {
            let frag: String = piece.iter().collect();
            let escaped = serde_json::to_string(&frag).unwrap();
            wire.push_str(&format!(
                "event: content_block_delta\ndata: {{\"partial_json\":{escaped}}}\n\n"
            ));
        }

        let mut decoder = SseDecoder::new();
        let mut assembled = String::new();
        for b in wire.as_bytes() {
            decoder.push_bytes(&[*b]).expect("valid utf8");
            for ev in decoder.poll() {
                let v: serde_json::Value = serde_json::from_str(&ev.data).unwrap();
                assembled.push_str(v["partial_json"].as_str().unwrap());
            }
        }
        let reparsed: serde_json::Value = serde_json::from_str(&assembled).unwrap();
        assert_eq!(reparsed, full);
    }

    #[test]
    fn sse_decoder_push_bytes_survives_a_multibyte_split_across_chunks() {
        // The end-to-end fix: an SSE data line carrying a multi-byte char
        // split across two network chunks must parse cleanly.
        let full = "data: caf\u{00e9}\n\n";
        let bytes = full.as_bytes();
        // Split inside the two bytes of 'é'.
        let idx = full.find('é').unwrap() + 1;
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
        assert_eq!(events[0].data, "café");
    }
}
