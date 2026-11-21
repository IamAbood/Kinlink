//! Android v1: no clipboard sync (same API as the desktop module, does nothing).
use crate::proto::Msg;

#[derive(Default)]
pub struct Clip;

impl Clip {
    pub fn poll(&mut self) -> Option<Msg> {
        None
    }
    pub fn on_remote(&mut self, seq: u32, _text: String) -> Msg {
        Msg::ClipboardAck { seq }
    }
    pub fn on_ack(&mut self, _seq: u32) {}
}
