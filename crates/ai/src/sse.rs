//! Minimal Server-Sent Events decoder for streaming LLM responses.

use futures::{Stream, StreamExt};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Append `chunk` to `out` as UTF-8, keeping a character split at the end
/// of the chunk in `carry` until the next chunk completes it (decoding
/// each chunk alone turns it into two replacement characters). Bytes that
/// can never be UTF-8 become U+FFFD.
pub fn push_utf8(carry: &mut Vec<u8>, chunk: &[u8], out: &mut String) {
    carry.extend_from_slice(chunk);
    loop {
        match std::str::from_utf8(carry) {
            Ok(s) => {
                out.push_str(s);
                carry.clear();
                return;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&carry[..valid]).expect("valid prefix"));
                match e.error_len() {
                    // Incomplete at the end: wait for the rest.
                    None => {
                        carry.drain(..valid);
                        return;
                    }
                    Some(n) => {
                        out.push('\u{FFFD}');
                        carry.drain(..valid + n);
                    }
                }
            }
        }
    }
}

/// Longest line, and longest event (its `data` lines together), a stream
/// may send. A provider's events are deltas of a few KB, a whole message at
/// most; past this the server (a misbehaving local or compatible one) isn't
/// sending events, and holding the rest would grow without end.
pub const MAX_EVENT_BYTES: usize = 32 << 20;

/// Most bytes one stream may send in all: an answer is text and tool calls
/// of a few MB; past this a server that keeps sending (endless small
/// deltas) would grow what the caller collects without end.
pub const MAX_STREAM_BYTES: usize = 128 << 20;

/// Why a stream of events ended early.
#[derive(Debug, Clone, PartialEq)]
pub enum SseError {
    /// The connection failed (reading the body).
    Transport(String),
    /// The server sent what isn't a stream of events of a sane size.
    TooLarge(String),
}

impl From<SseError> for crate::AiError {
    fn from(e: SseError) -> Self {
        match e {
            SseError::Transport(m) => crate::AiError::Network(m),
            // Not a broken connection: the provider (or a server posing as one) misbehaved.
            SseError::TooLarge(m) => crate::AiError::Provider(m),
        }
    }
}

/// Incremental decoder: feed bytes, get complete events.
#[derive(Default)]
pub struct SseDecoder {
    buf: String,
    /// How much of `buf` is known to hold no newline (a long line isn't
    /// scanned again from its start at every chunk).
    scanned: usize,
    /// The start of a character split across chunks.
    carry: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    /// Bytes in `data` so far.
    data_len: usize,
    /// A line or an event went past [`MAX_EVENT_BYTES`]: the decoder reads
    /// nothing more, and the stream ends with this error.
    failed: Option<String>,
}

impl SseDecoder {
    /// The complete events in what has arrived so far. Past
    /// [`MAX_EVENT_BYTES`] in a line or an event it stops reading (what it
    /// held is dropped) and [`SseDecoder::failed`] says why.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if self.failed.is_some() {
            return out;
        }
        push_utf8(&mut self.carry, chunk, &mut self.buf);
        loop {
            let Some(at) = self.buf[self.scanned..].find('\n') else {
                self.scanned = self.buf.len();
                if self.buf.len() > MAX_EVENT_BYTES {
                    self.fail(format!("the stream sent a line longer than {} MB without ending it", MAX_EVENT_BYTES >> 20));
                }
                break;
            };
            let pos = self.scanned + at;
            self.scanned = 0;
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.data.is_empty() || self.event.is_some() {
                    out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
                    self.data.clear();
                    self.data_len = 0;
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line.as_str(), ""),
            };
            match field {
                "event" => self.event = Some(value.to_string()),
                "data" => {
                    self.data_len += value.len() + 1;
                    if self.data_len > MAX_EVENT_BYTES {
                        self.fail(format!("the stream sent an event larger than {} MB", MAX_EVENT_BYTES >> 20));
                        break;
                    }
                    self.data.push(value.to_string())
                }
                _ => {}
            }
        }
        out
    }

    /// Why the decoder stopped reading (a line or an event too large).
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn fail(&mut self, why: String) {
        self.failed = Some(why);
        self.buf = String::new();
        self.scanned = 0;
        self.carry.clear();
        self.data.clear();
        self.data_len = 0;
        self.event = None;
    }

    /// Flush a trailing event without a final blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.failed.is_some() {
            return None;
        }
        // A character the stream never completed.
        if !self.carry.is_empty() {
            let rest = std::mem::take(&mut self.carry);
            self.buf.push_str(&String::from_utf8_lossy(&rest));
        }
        if !self.buf.trim().is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.scanned = 0;
            let _ = self.feed(format!("{rest}\n").as_bytes());
        }
        (!self.data.is_empty()).then(|| SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") })
    }
}

/// Turn a byte stream into SSE events. A line or an event past
/// [`MAX_EVENT_BYTES`], or a stream past [`MAX_STREAM_BYTES`], ends the
/// stream with [`SseError::TooLarge`], after the events before it.
pub fn events<S, E>(bytes: S) -> impl Stream<Item = Result<SseEvent, SseError>>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let queue: std::collections::VecDeque<Result<SseEvent, SseError>> = std::collections::VecDeque::new();
    futures::stream::unfold((bytes, SseDecoder::default(), queue, false, 0usize), |(mut s, mut d, mut q, mut done, mut total)| async move {
        loop {
            if let Some(e) = q.pop_front() {
                return Some((e, (s, d, q, done, total)));
            }
            if done {
                return None;
            }
            match s.next().await {
                Some(Ok(chunk)) => {
                    total += chunk.len();
                    if total > MAX_STREAM_BYTES {
                        q.push_back(Err(SseError::TooLarge(format!("the stream sent more than {} MB", MAX_STREAM_BYTES >> 20))));
                        done = true;
                        continue;
                    }
                    q.extend(d.feed(&chunk).into_iter().map(Ok));
                    if let Some(why) = d.failed() {
                        q.push_back(Err(SseError::TooLarge(why.to_string())));
                        done = true;
                    }
                }
                Some(Err(e)) => {
                    q.push_back(Err(SseError::Transport(e.to_string())));
                    done = true;
                }
                None => {
                    done = true;
                    if let Some(e) = d.finish() {
                        q.push_back(Ok(e));
                    }
                    // The last, unterminated event was too large: said, not dropped silently.
                    if let Some(why) = d.failed() {
                        q.push_back(Err(SseError::TooLarge(why.to_string())));
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multibyte character split across chunks (Vietnamese text, tool-call
    /// JSON) arrives whole, at every split point.
    #[test]
    fn characters_split_across_chunks() {
        let text = "data: {\"t\": \"Xin chào, tiếng Việt 🇻🇳\"}\n\n";
        let bytes = text.as_bytes();
        for cut in 1..bytes.len() {
            let mut d = SseDecoder::default();
            let mut ev = d.feed(&bytes[..cut]);
            ev.extend(d.feed(&bytes[cut..]));
            assert_eq!(ev.len(), 1, "cut at {cut}");
            assert_eq!(ev[0].data, "{\"t\": \"Xin chào, tiếng Việt 🇻🇳\"}", "cut at {cut}");
        }
        // One byte at a time.
        let mut d = SseDecoder::default();
        let ev: Vec<SseEvent> = bytes.iter().flat_map(|b| d.feed(&[*b])).collect();
        assert_eq!(ev[0].data, "{\"t\": \"Xin chào, tiếng Việt 🇻🇳\"}");
        // Bytes that can never be UTF-8: one replacement each, the rest kept.
        let mut out = String::new();
        let mut carry = vec![];
        push_utf8(&mut carry, b"a\xffb\xe1", &mut out);
        assert_eq!((out.as_str(), carry.len()), ("a\u{FFFD}b", 1));
        push_utf8(&mut carry, b"\xba\xbfc", &mut out);
        assert_eq!(out, "a\u{FFFD}bếc");
    }

    #[test]
    fn decodes_split_events() {
        let mut d = SseDecoder::default();
        assert!(d.feed(b"event: message_start\ndata: {\"a\"").is_empty());
        let ev = d.feed(b":1}\n\n: comment\ndata: x\ndata: y\n\n");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].event.as_deref(), Some("message_start"));
        assert_eq!(ev[0].data, "{\"a\":1}");
        assert_eq!(ev[1].data, "x\ny");
        assert!(d.feed(b"data: [DONE]").is_empty());
        assert_eq!(d.finish().unwrap().data, "[DONE]");
    }

    /// A line or an event past the cap stops the decoder (it doesn't grow
    /// without end); the stream gives the events before it, then an error.
    /// A long line arriving in many chunks isn't scanned again from its start.
    #[tokio::test]
    async fn too_long_lines_and_events_end_the_stream() {
        let mut d = SseDecoder::default();
        assert_eq!(d.feed(b"data: ok\n\n").len(), 1);
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..=(MAX_EVENT_BYTES >> 20) {
            assert!(d.feed(&chunk).is_empty());
        }
        assert!(d.failed().unwrap().contains("line longer"), "{:?}", d.failed());
        assert!(d.buf.is_empty(), "what it held is dropped");
        assert!(d.feed(b"\ndata: later\n\n").is_empty() && d.finish().is_none(), "nothing more is read");

        // An event of many data lines, each short.
        let mut d = SseDecoder::default();
        let line = format!("data: {}\n", "y".repeat(1 << 20));
        for _ in 0..=(MAX_EVENT_BYTES >> 20) {
            d.feed(line.as_bytes());
        }
        assert!(d.failed().unwrap().contains("event larger"), "{:?}", d.failed());

        // Through the stream: the event before it, then the error, then the end.
        let mut parts: Vec<Result<bytes::Bytes, String>> = vec![Ok(bytes::Bytes::from_static(b"data: first\n\n"))];
        for _ in 0..=(MAX_EVENT_BYTES >> 20) {
            parts.push(Ok(bytes::Bytes::from(chunk.clone())));
        }
        parts.push(Ok(bytes::Bytes::from_static(b"\ndata: never\n\n")));
        let got: Vec<Result<SseEvent, SseError>> = events(futures::stream::iter(parts)).collect().await;
        assert_eq!(got.len(), 2, "{:?}", got.iter().map(|r| r.as_ref().map(|e| e.data.len())).collect::<Vec<_>>());
        assert_eq!(got[0].as_ref().unwrap().data, "first");
        assert!(matches!(&got[1], Err(SseError::TooLarge(m)) if m.contains("line longer")), "{:?}", got[1]);
        assert!(matches!(crate::AiError::from(SseError::TooLarge("x".into())), crate::AiError::Provider(_)));

        // A last, unterminated event too large: an error, not a silent end.
        // (Lines under the cap until the last, which has no newline: only the
        // end of the stream, `finish`, goes past it.)
        let mut parts: Vec<Result<bytes::Bytes, String>> = vec![Ok(bytes::Bytes::from_static(b"data: first\n\n"))];
        for _ in 0..(MAX_EVENT_BYTES >> 20) - 1 {
            parts.push(Ok(bytes::Bytes::from(format!("data: {}\n", "z".repeat((1 << 20) - 8)))));
        }
        parts.push(Ok(bytes::Bytes::from(format!("data: {}", "z".repeat(2 << 20)))));
        let got: Vec<Result<SseEvent, SseError>> = events(futures::stream::iter(parts)).collect().await;
        assert!(matches!(got.last(), Some(Err(SseError::TooLarge(_)))), "{:?}", got.iter().map(|r| r.is_ok()).collect::<Vec<_>>());

        // A stream past its total: small events without end.
        let small = bytes::Bytes::from(format!("data: {}\n\n", "d".repeat(1 << 16)));
        let parts = std::iter::repeat_n(small, (MAX_STREAM_BYTES >> 16) + 4).map(Ok::<_, String>);
        let got: Vec<Result<SseEvent, SseError>> = events(futures::stream::iter(parts)).collect().await;
        assert!(matches!(got.last(), Some(Err(SseError::TooLarge(m))) if m.contains("more than")));
        assert!(got.len() <= (MAX_STREAM_BYTES >> 16) + 1);
    }

    /// A long line arriving in chunks is scanned once: the search resumes
    /// where it stopped (not from the line's start at every chunk); CRLF
    /// split across chunks still ends the line.
    #[test]
    fn a_long_line_is_scanned_once() {
        let mut d = SseDecoder::default();
        d.feed(b"data: ");
        for _ in 0..10 {
            d.feed(&[b'q'; 4096]);
            assert_eq!(d.scanned, d.buf.len(), "the search resumes at the end of what was scanned");
        }
        d.feed(b"\r");
        let ev = d.feed(b"\n\r\n");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data.len(), 10 * 4096);
        assert_eq!((d.scanned, d.buf.as_str()), (0, ""));
    }
}
