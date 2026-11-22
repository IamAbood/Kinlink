//! The Microsoft Store (MSIX) build: the same exe, but Windows runs it with a package identity.
//! Then the Store does updates, the manifest declares startup, the firewall rule and kinlink://
//! links, and services / elevation aren't available.
use std::path::PathBuf;
use std::sync::OnceLock;
use windows::core::HSTRING;

/// Id of the startup task in the package manifest (installer/msix/AppxManifest.xml).
const STARTUP_TASK: &str = "KinlinkStartup";

pub fn packaged() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| {
        let mut len = 0u32;
        // APPMODEL_ERROR_NO_PACKAGE (15700) = a normal, unpackaged exe.
        unsafe { windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName(&mut len, None) }.0 != 15700
    })
}

/// What shortcuts should start. The package folder changes on every Store update, so they go
/// through the app execution alias (also in the manifest) instead.
pub fn launcher() -> Option<PathBuf> {
    if packaged() {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft\\WindowsApps\\Kinlink.exe"));
    }
    std::env::current_exe().ok()
}

/// Start with Windows for the Store build. None = just read. Returns whether it's on.
/// WinRT calls run on their own thread (the UI thread is single-threaded COM).
pub fn startup_task(set: Option<bool>) -> bool {
    std::thread::spawn(move || {
        let r = (|| -> windows::core::Result<bool> {
            use windows::ApplicationModel::{StartupTask, StartupTaskState};
            let t = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK))?.join()?;
            let state = match set {
                Some(true) => t.RequestEnableAsync()?.join()?,
                Some(false) => {
                    t.Disable()?;
                    StartupTaskState::Disabled
                }
                None => t.State()?,
            };
            Ok(state == StartupTaskState::Enabled || state == StartupTaskState::EnabledByPolicy)
        })();
        r.unwrap_or_else(|e| {
            crate::plog!("startup task: {e}");
            false
        })
    })
    .join()
    .unwrap_or(false)
}
