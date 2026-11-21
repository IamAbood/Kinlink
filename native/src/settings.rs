//! Persistent settings (%APPDATA%\ParsecCloneNative\settings.json) and Start-with-Windows.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use windows::core::{w, HSTRING};
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// Accept incoming connections from friends.
    pub hosting: bool,
    pub port: u16,
    pub max_mbps: u32,
    pub fps: u32,
    pub hardware: bool,
    pub upnp: bool,
    /// Start-with-Windows has been offered once (it's on by default for new installs).
    pub autostart_init: bool,
    /// Go fullscreen when a session starts.
    pub fullscreen_connect: bool,
    /// The in-session tip has been shown once.
    pub hint_seen: bool,
    /// Windows notification when a friend comes online.
    pub notify_online: bool,
    /// Interface language: "en", "ar", or "" = follow Windows.
    pub language: String,
    /// My status line shown to friends.
    pub status: String,
    /// Ask before accepting incoming files.
    pub ask_files: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { hosting: true, port: crate::proto::DEFAULT_PORT, max_mbps: 30, fps: 60, hardware: true, upnp: true, autostart_init: false, fullscreen_connect: true, hint_seen: false, notify_online: true, language: String::new(), status: String::new(), ask_files: true }
    }
}

/// Test profiles (PCLONE_PROFILE) get their own identity/friends/settings so two copies can run on one PC.
pub fn profile() -> Option<String> {
    std::env::var("PCLONE_PROFILE").ok().filter(|p| !p.is_empty())
}

/// Named event a second launch sets to make the running copy show its window.
pub fn show_event_name() -> HSTRING {
    HSTRING::from(format!("ParsecCloneNative.Show.{}", profile().unwrap_or_default()))
}

/// File a second launch leaves for the running copy: "connect to this friend".
pub fn connect_request_path() -> Option<PathBuf> {
    Some(data_dir()?.join("connect_request.txt"))
}

pub fn data_dir() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var_os("APPDATA")?);
    Some(match profile() {
        Some(p) => base.join(format!("ParsecCloneNative-{p}")),
        None => base.join("ParsecCloneNative"),
    })
}

fn path() -> Option<PathBuf> {
    Some(data_dir()?.join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let s = path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok());
        s.unwrap_or_else(|| {
            let s = Settings::default();
            s.save();
            s
        })
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap());
        }
    }
}

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_NAME: windows::core::PCWSTR = w!("ParsecCloneNative");

/// Keeps "Start with Windows" pointing at this copy of the app (it may have been moved,
/// renamed or updated) and switched on in Task Manager > Startup apps.
pub fn refresh_autostart() {
    set_autostart(true);
    // Task Manager's on/off switch lives here; 02 00.. = enabled.
    let enabled: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run"),
            RUN_NAME,
            windows::Win32::System::Registry::REG_BINARY.0,
            Some(enabled.as_ptr() as _),
            enabled.len() as u32,
        );
    }
}

pub fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

/// Registers the exe to start minimized and hosting at login.
/// Lets kinlink:// invite links open this app (per user, no admin needed).
pub fn register_url_scheme() {
    let Ok(exe) = std::env::current_exe() else { return };
    let set = |key: &str, name: Option<&str>, value: &str| unsafe {
        let v = HSTRING::from(value);
        let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, (v.len() + 1) * 2);
        let name = name.map(HSTRING::from);
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), name.as_ref().map(|n| windows::core::PCWSTR(n.as_ptr())).unwrap_or(windows::core::PCWSTR::null()), REG_SZ.0, Some(bytes.as_ptr() as _), bytes.len() as u32);
    };
    set("Software\\Classes\\kinlink", None, "URL:Kinlink invite");
    set("Software\\Classes\\kinlink", Some("URL Protocol"), "");
    set("Software\\Classes\\kinlink\\shell\\open\\command", None, &format!("\"{}\" --connect-friend \"%1\"", exe.display()));
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = HSTRING::from(format!("\"{}\" --minimized", exe.display()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, (cmd.len() + 1) * 2);
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME, REG_SZ.0, Some(bytes.as_ptr() as _), bytes.len() as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME);
        }
    }
}
