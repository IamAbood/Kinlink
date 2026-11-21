//! Two-way clipboard sync. Text goes in small acked messages; copied images and files go
//! through the reliable file transfer (names marked with CLIP_PREFIX) and land on the other
//! side's clipboard.
use crate::proto::Msg;

const MAX_TEXT: usize = 8000; // ponytail: keeps a clipboard message to a few IP fragments; chunk it if bigger text matters

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// File-transfer names starting with this are clipboard content, not ordinary files:
/// "kinlink-clip-<batch>-<count>-<i|f>-<real name>".
pub const CLIP_PREFIX: &str = "kinlink-clip-";
const MAX_IMAGE: u64 = 20_000_000;
pub const MAX_FILES: u64 = 200_000_000;

/// Clipboard change counter right after we set it ourselves (so it isn't sent back).
static OURS: AtomicU32 = AtomicU32::new(0);

fn clipboard_seq() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// Something copied that travels as files.
pub enum Rich {
    /// A PNG of a copied picture.
    Image(PathBuf),
    /// Copied files (not folders).
    Files(Vec<PathBuf>),
}

/// Marked transfer names for one copy: (path, name to send).
pub fn marked_names(r: &Rich) -> Vec<(PathBuf, String)> {
    let batch: u32 = rand::random::<u32>() & 0xFFFFFF;
    let (kind, paths) = match r {
        Rich::Image(p) => ("i", vec![p.clone()]),
        Rich::Files(v) => ("f", v.clone()),
    };
    let n = paths.len();
    paths
        .into_iter()
        .map(|p| {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            (p, format!("{CLIP_PREFIX}{batch:06x}-{n}-{kind}-{name}"))
        })
        .collect()
}

/// Splits a marked name: (batch, count, is_image, real name).
pub fn parse_marked(name: &str) -> Option<(String, usize, bool, String)> {
    let rest = name.strip_prefix(CLIP_PREFIX)?;
    let mut it = rest.splitn(4, '-');
    let (batch, count, kind, real) = (it.next()?, it.next()?.parse().ok()?, it.next()?, it.next()?);
    Some((batch.to_string(), count, kind == "i", real.to_string()))
}

/// Puts received clipboard content on this PC's clipboard.
pub fn set_received(paths: &[PathBuf], image: bool) -> Result<(), String> {
    let mut c = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    if image {
        let file = std::fs::File::open(&paths[0]).map_err(|e| e.to_string())?;
        let mut dec = png::Decoder::new(std::io::BufReader::new(file));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut r = dec.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; r.output_buffer_size().ok_or("image too big")?];
        let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            _ => buf.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        };
        c.set_image(arboard::ImageData { width: info.width as usize, height: info.height as usize, bytes: rgba.into() }).map_err(|e| e.to_string())?;
    } else {
        c.set().file_list(paths).map_err(|e| e.to_string())?;
    }
    OURS.store(clipboard_seq(), Relaxed);
    Ok(())
}

#[derive(Default)]
pub struct Clip {
    last: Option<String>,
    seq: u32,
    pending: Option<(u32, String)>,
    last_recv: Option<u32>,
    /// Clipboard change counter last looked at (images/files).
    rich_seen: Option<u32>,
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

    /// Call every ~400 ms: a newly copied picture or files, if any (not what was on the
    /// clipboard before the session, and not what we put there ourselves).
    pub fn poll_rich(&mut self) -> Option<Rich> {
        let seq = clipboard_seq();
        if self.rich_seen == Some(seq) {
            return None;
        }
        let first = self.rich_seen.is_none();
        self.rich_seen = Some(seq);
        if first || seq == OURS.load(Relaxed) {
            return None;
        }
        let mut c = arboard::Clipboard::new().ok()?;
        if let Ok(files) = c.get().file_list() {
            let files: Vec<PathBuf> = files.into_iter().filter(|p| p.is_file()).collect();
            let total: u64 = files.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
            if files.is_empty() || total > MAX_FILES || files.len() > 50 {
                return None;
            }
            return Some(Rich::Files(files));
        }
        let img = c.get_image().ok()?;
        let dir = std::env::temp_dir().join("kinlink-clip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("Clipboard image {seq}.png"));
        let file = std::fs::File::create(&path).ok()?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width as u32, img.height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().ok()?.write_image_data(&img.bytes).ok()?;
        (path.metadata().ok()?.len() <= MAX_IMAGE).then_some(Rich::Image(path))
    }

    pub fn on_ack(&mut self, seq: u32) {
        if self.pending.as_ref().is_some_and(|(s, _)| *s == seq) {
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_names_round_trip() {
        let names = marked_names(&Rich::Files(vec![PathBuf::from("C:/x/report-final.pdf"), PathBuf::from("C:/x/b.txt")]));
        let (batch, n, img, real) = parse_marked(&names[0].1).unwrap();
        assert_eq!((n, img, real.as_str()), (2, false, "report-final.pdf"));
        assert_eq!(parse_marked(&names[1].1).unwrap().0, batch);
        assert!(parse_marked("report.pdf").is_none());
    }
}
