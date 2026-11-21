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
    pub fn poll_rich(&mut self) -> Option<Rich> {
        None
    }
}

pub const CLIP_PREFIX: &str = "kinlink-clip-";
pub const MAX_FILES: u64 = 200_000_000;

pub enum Rich {
    Image(std::path::PathBuf),
    Files(Vec<std::path::PathBuf>),
}

pub fn marked_names(_r: &Rich) -> Vec<(std::path::PathBuf, String)> {
    Vec::new()
}

/// (batch, count, is_image, real name) for clipboard transfer names.
pub fn parse_marked(name: &str) -> Option<(String, usize, bool, String)> {
    let rest = name.strip_prefix(CLIP_PREFIX)?;
    let mut it = rest.splitn(4, '-');
    let (batch, count, kind, real) = (it.next()?, it.next()?.parse().ok()?, it.next()?, it.next()?);
    Some((batch.to_string(), count, kind == "i", real.to_string()))
}
