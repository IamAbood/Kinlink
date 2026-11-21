//! Android v1: no screenshots/recording (same API as the desktop module).
use crate::codec::Frame;
use anyhow::{bail, Result};
use std::path::PathBuf;

pub fn save_png(_f: &Frame) -> Result<PathBuf> {
    bail!("Screenshots aren't available on the phone yet")
}

pub struct Recorder {
    pub size: (usize, usize),
}

impl Recorder {
    pub fn start(_key: &[u8], _w: usize, _h: usize, _audio_rate: u32) -> Result<Recorder> {
        bail!("Recording isn't available on the phone yet")
    }
    pub fn video(&mut self, _d: &[u8], _key: bool) -> Result<()> {
        Ok(())
    }
    pub fn audio(&mut self, _pcm: &[i16]) -> Result<()> {
        Ok(())
    }
    pub fn finish(self) -> Result<PathBuf> {
        bail!("not recording")
    }
}
