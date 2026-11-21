//! Android v1: no voice chat (same API as the desktop module, does nothing).
pub struct Voice;

impl Voice {
    pub fn start(_send: impl Fn(Vec<i16>) + Send + Sync + 'static) -> Voice {
        Voice
    }
    pub fn set_mic(&self, _on: bool) {}
    pub fn mic_on(&self) -> bool {
        false
    }
    pub fn play(&self, _from: &str, _pcm: &[i16]) {}
    pub fn speaking(&self) -> Vec<String> {
        Vec::new()
    }
    pub fn error(&self) -> Option<String> {
        None
    }
}
