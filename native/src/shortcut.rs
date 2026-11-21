//! .lnk shortcuts: the Start menu entry (which also lets Windows 10 show our toasts, via the
//! AppUserModelID stamped on it) and per-friend "connect" shortcuts on the desktop.
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use windows::core::{Interface, GUID, HSTRING, PWSTR};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = match SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) {
            Ok(p) => p,
            Err(e) => {
                crate::plog!("known folder: {e}");
                return None;
            }
        };
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

pub fn desktop() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

/// Creates/overwrites a shortcut to this exe. Runs on its own STA thread (COM requirement).
pub fn create(lnk: &Path, args: &str, description: &str, aumid: Option<&str>) -> Result<(), String> {
    let (lnk, args, description, aumid) = (lnk.to_path_buf(), args.to_string(), description.to_string(), aumid.map(str::to_string));
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        let exe_h = HSTRING::from(exe.as_os_str());
        link.SetPath(&exe_h).map_err(|e| e.to_string())?;
        link.SetArguments(&HSTRING::from(args)).map_err(|e| e.to_string())?;
        link.SetDescription(&HSTRING::from(description)).map_err(|e| e.to_string())?;
        let _ = link.SetIconLocation(&exe_h, 0);
        if let Some(id) = aumid {
            let store: IPropertyStore = link.cast().map_err(|e| e.to_string())?;
            let wide = HSTRING::from(id);
            // ManuallyDrop: the value borrows `wide`; letting it drop would free memory we don't own.
            let pv = ManuallyDrop::new(PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_LPWSTR,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 { pwszVal: PWSTR(wide.as_ptr() as *mut _) },
                    }),
                },
            });
            store.SetValue(&PKEY_AppUserModel_ID, &*pv).map_err(|e| e.to_string())?;
            store.Commit().map_err(|e| e.to_string())?;
        }
        let file: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true).map_err(|e| e.to_string())
    })
    .join()
    .map_err(|_| "shortcut thread panicked".to_string())?
}

/// Start menu entry "Kinlink" carrying our AppUserModelID (refreshed each launch so it
/// always points at the current exe).
pub fn ensure_start_menu(aumid: &str) {
    if let Some(dir) = known_folder(&FOLDERID_Programs) {
        let _ = std::fs::remove_file(dir.join("Parsec Clone.lnk")); // from before the rename
        if let Err(e) = create(&dir.join("Kinlink.lnk"), "", "Kinlink", Some(aumid)) {
            crate::plog!("start menu shortcut: {e}");
        }
    }
}

/// File-name-safe version of a friend's display name.
pub fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect()
}

/// Windows "Open" dialog on its own thread; `done` gets the chosen file (None = cancelled).
pub fn pick_file(done: impl FnOnce(Option<PathBuf>) + Send + 'static) {
    std::thread::spawn(move || unsafe {
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let pick = || -> Option<PathBuf> {
            let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let _ = d.SetTitle(&HSTRING::from("Send a file"));
            d.Show(None).ok()?;
            let p = d.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as _));
            s.map(PathBuf::from)
        };
        done(pick());
    });
}
