//! Two-way clipboard text sync over the (lossy) UDP link: resend until acknowledged.
use crate::proto::Msg;

const MAX_TEXT: usize = 8000; // ponytail: keeps a clipboard message to a few IP fragments; chunk it if bigger text matters

#[derive(Default)]
pub struct Clip {
    last: Option<String>,
    seq: u32,
    pending: Option<(u32, String)>,
    last_recv: Option<u32>,
}

impl Clip {
    /// Call every ~400 ms. Returns a message to (re)send if the local clipboard has unsent text.
    pub fn poll(&mut self) -> Option<Msg> {
        let text = arboard::Clipboard::new().ok()?.get_text().ok();
        if let Some(t) = text {
            if self.last.as_ref() != Some(&t) {
                let first = self.last.is_none();
                self.last = Some(t.clone());
                // Don't push whatever was on the clipboard before the session started.
                if !first && t.len() <= MAX_TEXT {
                    self.seq += 1;
                    self.pending = Some((self.seq, t));
                }
            }
        }
        self.pending.as_ref().map(|(seq, text)| Msg::Clipboard { seq: *seq, text: text.clone() })
    }

    /// Applies text from the other side; returns the ack to send.
    pub fn on_remote(&mut self, seq: u32, text: String) -> Msg {
        if self.last_recv != Some(seq) {
            self.last_recv = Some(seq);
            if let Ok(mut c) = arboard::Clipboard::new() {
                if c.set_text(text.clone()).is_ok() {
                    self.last = Some(text); // so poll() doesn't echo it back
                }
            }
        }
        Msg::ClipboardAck { seq }
    }

    pub fn on_ack(&mut self, seq: u32) {
        if self.pending.as_ref().is_some_and(|(s, _)| *s == seq) {
            self.pending = None;
        }
    }
}
