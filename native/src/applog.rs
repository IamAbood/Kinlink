//! Timestamped log of what the app is doing (Settings → Open log). One file per run:
//! pclone.log (this run) and pclone.old.log (the run before). Also echoed to stderr.
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static RECENT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

pub fn path() -> Option<PathBuf> {
    Some(crate::settings::data_dir()?.join("pclone.log"))
}

/// Starts this run's log file (the app's main process only; test modes just print).
pub fn init() {
    let Some(p) = path() else { return };
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::rename(&p, p.with_file_name("pclone.old.log"));
    *FILE.lock().unwrap() = std::fs::File::create(&p).ok();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write(&format!("CRASH: {info}"));
        prev(info);
    }));
}

#[cfg(windows)]
fn stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

#[cfg(not(windows))]
fn stamp() -> String {
    let ms = crate::net::now_us() / 1000;
    format!("{:02}:{:02}:{:02}.{:03} UTC", ms / 3_600_000 % 24, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

pub fn write(msg: &str) {
    let line = format!("{} {msg}", stamp());
    eprintln!("{line}");
    #[cfg(target_os = "android")]
    log::info!("{msg}");
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{line}");
    }
    let mut r = RECENT.lock().unwrap();
    if r.len() >= 200 {
        r.pop_front();
    }
    r.push_back(line);
}

/// Last `n` lines (for the "connecting" screen).
pub fn recent(n: usize) -> Vec<String> {
    let r = RECENT.lock().unwrap();
    r.iter().skip(r.len().saturating_sub(n)).cloned().collect()
}

#[macro_export]
macro_rules! plog {
    ($($t:tt)*) => { $crate::applog::write(&format!($($t)*)) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn recent_keeps_last() {
        for i in 0..205 {
            super::write(&format!("line {i}"));
        }
        let r = super::recent(2);
        assert!(r[0].ends_with("line 203") && r[1].ends_with("line 204"));
    }
}
