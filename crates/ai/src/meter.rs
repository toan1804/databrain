//! Tokens one model call took, for the provider usage page. Providers report
//! usage at the end of the stream, so a call cut off before that (inline
//! completion overtaken by typing, test data stopped) reports nothing; the
//! provider still bills the prompt it read and the text it wrote. Such a
//! call is counted by estimate: about 4 characters a token.

use std::sync::{Arc, Mutex};

use crate::types::ChatRequest;

const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Default)]
struct State {
    /// The request reached the provider (the stream opened).
    sent: bool,
    prompt_chars: usize,
    text_chars: usize,
    /// What the provider reported.
    reported: Option<(u32, u32)>,
    /// The stream ended (a call that ends without a report counts 0).
    finished: bool,
}

/// One call's tokens, shared between the call and whoever may cancel it.
#[derive(Debug, Clone, Default)]
pub struct Meter(Arc<Mutex<State>>);

impl Meter {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The request went out: its prompt is billed from here on.
    pub fn sent(&self, chat: &ChatRequest) {
        let chars = chat.system.chars().count() + chat.messages.iter().map(|m| m.text.chars().count()).sum::<usize>();
        let mut s = self.state();
        s.sent = true;
        s.prompt_chars = chars;
    }

    pub fn text(&self, t: &str) {
        self.state().text_chars += t.chars().count();
    }

    pub fn reported(&self, input: u32, output: u32) {
        self.state().reported = Some((input, output));
    }

    pub fn finished(&self) {
        self.state().finished = true;
    }

    /// Input and output tokens: as reported, else (for a call cut off after
    /// it was sent: cancelled or timed out, not one the provider ended with
    /// an error) estimated.
    pub fn tokens(&self) -> (u32, u32) {
        let s = self.state();
        match s.reported {
            Some(r) => r,
            None if s.sent && !s.finished => {
                let t = |chars: usize| u32::try_from(chars.div_ceil(CHARS_PER_TOKEN)).unwrap_or(u32::MAX);
                (t(s.prompt_chars), t(s.text_chars))
            }
            None => (0, 0),
        }
    }
}

/// The tokens of several calls.
pub fn total<'a>(meters: impl IntoIterator<Item = &'a Meter>) -> (u32, u32) {
    meters.into_iter().map(Meter::tokens).fold((0, 0), |(i, o), (a, b)| (i.saturating_add(a), o.saturating_add(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn chat() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            system: "x".repeat(400),
            messages: vec![Message::user("y".repeat(400))],
            tools: vec![],
            temperature: None,
            max_tokens: None,
        }
    }

    #[test]
    fn calls_cut_off_are_estimated_and_reports_win() {
        // Never sent: nothing.
        assert_eq!(Meter::default().tokens(), (0, 0));
        // Cut off after 41 characters came back: about 200 in, 11 out.
        let m = Meter::default();
        m.sent(&chat());
        m.text(&"z".repeat(41));
        assert_eq!(m.tokens(), (200, 11));
        // The provider's report replaces the estimate.
        m.reported(180, 9);
        assert_eq!(m.tokens(), (180, 9));
        // A call that ended without a report (a provider that reports none) counts 0.
        let done = Meter::default();
        done.sent(&chat());
        done.text("abc");
        done.finished();
        assert_eq!(done.tokens(), (0, 0));
        assert_eq!(total([&m, &done, &Meter::default()]), (180, 9));
    }
}
