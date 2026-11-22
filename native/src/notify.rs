//! Windows toast notifications for an unpackaged app: register an AppUserModelID (name + icon)
//! under HKCU, then show toasts with that id.
use windows::core::{w, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

const AUMID: &str = "ParsecClone.Native";
const ICON_PNG: &[u8] = include_bytes!("../../src-tauri/icon-source.png");

fn reg_sz(key: &HSTRING, name: PCWSTR, value: &str) {
    let v = HSTRING::from(value);
    unsafe {
        let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, (v.len() + 1) * 2);
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_SZ.0, Some(bytes.as_ptr() as _), bytes.len() as u32);
    }
}

/// Registers the display name + icon toasts show. Safe to call on every launch.
pub fn register() {
    if crate::pkg::packaged() {
        return; // the package gives us a name, icon and Start menu tile
    }
    let key = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
    reg_sz(&key, w!("DisplayName"), "Kinlink");
    if let Some(dir) = crate::settings::data_dir() {
        let icon = dir.join("icon.png");
        if !icon.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&icon, ICON_PNG);
        }
        reg_sz(&key, w!("IconUri"), &icon.display().to_string());
    }
    // Windows 10 only shows toasts for an app id that a Start menu shortcut carries.
    if crate::settings::profile().is_none() {
        crate::shortcut::ensure_start_menu(AUMID);
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Shows a toast (on a background thread; failures are ignored, e.g. Focus Assist).
pub fn toast(title: &str, body: &str) {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    std::thread::spawn(move || {
        let r = (|| -> windows::core::Result<()> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let doc = XmlDocument::new()?;
            doc.LoadXml(&HSTRING::from(xml))?;
            let t = ToastNotification::CreateToastNotification(&doc)?;
            if crate::pkg::packaged() {
                ToastNotificationManager::CreateToastNotifier()?.Show(&t)
            } else {
                ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&t)
            }
        })();
        if let Err(e) = r {
            crate::plog!("toast failed: {e}");
        }
    });
}
