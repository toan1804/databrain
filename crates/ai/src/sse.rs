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

/// Incremental decoder: feed bytes, get complete events.
#[derive(Default)]
pub struct SseDecoder {
    buf: String,
    /// The start of a character split across chunks.
    carry: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        push_utf8(&mut self.carry, chunk, &mut self.buf);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.data.is_empty() || self.event.is_some() {
                    out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
                    self.data.clear();
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
                "data" => self.data.push(value.to_string()),
                _ => {}
            }
        }
        out
    }

    /// Flush a trailing event without a final blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        // A character the stream never completed.
        if !self.carry.is_empty() {
            let rest = std::mem::take(&mut self.carry);
            self.buf.push_str(&String::from_utf8_lossy(&rest));
        }
        if !self.buf.trim().is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let _ = self.feed(format!("{rest}\n").as_bytes());
        }
        (!self.data.is_empty()).then(|| SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") })
    }
}

/// Turn a byte stream into SSE events.
pub fn events<S, E>(bytes: S) -> impl Stream<Item = Result<SseEvent, String>>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    futures::stream::unfold((bytes, SseDecoder::default(), std::collections::VecDeque::new(), false), |(mut s, mut d, mut q, mut done)| async move {
        loop {
            if let Some(e) = q.pop_front() {
                return Some((Ok(e), (s, d, q, done)));
            }
            if done {
                return None;
            }
            match s.next().await {
                Some(Ok(chunk)) => q.extend(d.feed(&chunk)),
                Some(Err(e)) => {
                    done = true;
                    return Some((Err(e.to_string()), (s, d, q, done)));
                }
                None => {
                    done = true;
                    if let Some(e) = d.finish() {
                        q.push_back(e);
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
}
