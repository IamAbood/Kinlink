//! Lock screen + admin prompts.
//! Normal apps can't see or click the secure desktop (lock screen, UAC) or elevated windows.
//! A small Windows service (LocalSystem) keeps a SYSTEM helper running in the user's session;
//! the app asks it, over a named pipe, to capture the current input desktop and send input there.
//! The helper only talks to the genuine Kinlink exe running in the same session.
use crate::capture::{monitors, Capturer};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, UOI_NAME};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const SERVICE: &str = "ParsecCloneHelper";

// ---------------------------------------------------------------- protocol

/// A raw SendInput event (already in absolute/scan-code form).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum RawIn {
    Mouse { dx: i32, dy: i32, data: i32, flags: u32 },
    Key { scan: u16, flags: u32 },
}

#[derive(Serialize, Deserialize, Debug)]
enum Req {
    /// Is the secure desktop (lock screen / UAC / Ctrl+Alt+Del) showing?
    Status,
    Frame { monitor: u8 },
    Input(Vec<RawIn>),
}

#[derive(Serialize, Deserialize, Debug)]
enum Resp {
    Status { secure: bool },
    Frame { w: u32, h: u32, bgra: Vec<u8> },
    NoFrame,
    Ok,
    Err(String),
}

fn session_id() -> u32 {
    let mut s = 0;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut s);
    }
    s
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\ParsecCloneHelper-{session}")
}

fn write_msg<T: Serialize>(w: &mut impl Write, m: &T) -> Result<()> {
    let b = bincode::serialize(m)?;
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(&b)?;
    w.flush()?;
    Ok(())
}

fn read_msg<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 128 << 20 {
        bail!("message too large");
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(bincode::deserialize(&b)?)
}

// ---------------------------------------------------------------- app side

/// Connection from the app to the SYSTEM helper in this session.
pub struct Link(std::fs::File);

impl Link {
    pub fn connect() -> Result<Link> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(pipe_name(session_id())).context("helper not running")?;
        Ok(Link(f))
    }
    fn call(&mut self, r: &Req) -> Result<Resp> {
        write_msg(&mut self.0, r)?;
        read_msg(&mut self.0)
    }
    pub fn secure(&mut self) -> Result<bool> {
        match self.call(&Req::Status)? {
            Resp::Status { secure } => Ok(secure),
            r => bail!("unexpected {r:?}"),
        }
    }
    /// Some((w, h, bgra)) if the picture changed.
    pub fn frame(&mut self, monitor: u8) -> Result<Option<(usize, usize, Vec<u8>)>> {
        match self.call(&Req::Frame { monitor })? {
            Resp::Frame { w, h, bgra } => Ok(Some((w as usize, h as usize, bgra))),
            Resp::NoFrame => Ok(None),
            Resp::Err(e) => bail!(e),
            r => bail!("unexpected {r:?}"),
        }
    }
    pub fn input(&mut self, ins: Vec<RawIn>) -> Result<()> {
        match self.call(&Req::Input(ins))? {
            Resp::Ok => Ok(()),
            Resp::Err(e) => bail!(e),
            r => bail!("unexpected {r:?}"),
        }
    }
}

/// Is the service installed (and running)?
pub fn service_state() -> (bool, bool) {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return (false, false) };
    match m.open_service(SERVICE, ServiceAccess::QUERY_STATUS) {
        Ok(s) => (true, s.query_status().is_ok_and(|st| st.current_state == windows_service::service::ServiceState::Running)),
        Err(_) => (false, false),
    }
}

/// The exe the installed helper service runs (it only serves that exact program).
pub fn service_exe() -> Option<std::path::PathBuf> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let s = m.open_service(SERVICE, ServiceAccess::QUERY_CONFIG).ok()?;
    Some(s.query_config().ok()?.executable_path)
}

/// The helper is installed for a different copy of the app (moved, renamed or an old version).
pub fn helper_is_stale() -> bool {
    let (Some(svc), Ok(me)) = (service_exe(), std::env::current_exe()) else { return false };
    let norm = |p: &std::path::Path| p.to_string_lossy().trim_matches('"').to_lowercase();
    norm(&svc) != norm(&me)
}

/// Runs `--install-service` / `--uninstall-service` elevated (UAC prompt).
pub fn run_elevated(arg: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    if let Ok(exe) = std::env::current_exe() {
        unsafe {
            ShellExecuteW(None, &HSTRING::from("runas"), &HSTRING::from(exe.as_os_str()), &HSTRING::from(arg), None, SW_HIDE);
        }
    }
}

/// Runs this exe elevated (UAC prompt) and waits. None if the user said no.
pub fn run_elevated_wait(arg: &str) -> Option<u32> {
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let exe = HSTRING::from(std::env::current_exe().ok()?.as_os_str());
    let (verb, params) = (HSTRING::from("runas"), HSTRING::from(arg));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(exe.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: windows::Win32::UI::WindowsAndMessaging::SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).ok()?;
        if info.hProcess.is_invalid() {
            return None;
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = windows::Win32::Foundation::CloseHandle(info.hProcess);
        Some(code)
    }
}

// ---------------------------------------------------------------- install (admin)

pub fn install() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let _ = uninstall(); // re-install cleanly (e.g. the exe moved)
    let info = ServiceInfo {
        name: OsString::from(SERVICE),
        display_name: OsString::from("Kinlink helper (lock screen & admin prompts)"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let s = m.create_service(&info, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    s.start::<&str>(&[])?;
    Ok(())
}

pub fn uninstall() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(SERVICE, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)?;
    let _ = s.stop();
    for _ in 0..20 {
        if s.query_status().is_ok_and(|st| st.current_state == windows_service::service::ServiceState::Stopped) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    s.delete()?;
    Ok(())
}

// ---------------------------------------------------------------- service (LocalSystem)

windows_service::define_windows_service!(ffi_service_main, service_main);

pub fn run_service() -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE, ffi_service_main)?;
    Ok(())
}

/// Starts `exe --system-helper` as SYSTEM in `session` (on the user's desktop).
/// `exe` is the path recorded when the service started (the registered install path): after an
/// update replaced the file there, the new helper comes from the new version.
fn launch_helper(session: u32, exe: &std::path::Path) -> Result<HANDLE> {
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut tok)?;
        let mut dup = HANDLE::default();
        DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut dup)?;
        let _ = CloseHandle(tok);
        SetTokenInformation(dup, TokenSessionId, &session as *const u32 as _, 4)?;
        let mut cmd: Vec<u16> = format!("\"{}\" --system-helper", exe.display()).encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(Some(dup), PCWSTR::null(), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, CREATE_NO_WINDOW, None, None, &si, &mut pi)?;
        let _ = CloseHandle(dup);
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}

fn service_main(_args: Vec<OsString>) {
    use windows_service::service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType};
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    let stop = Arc::new(AtomicBool::new(false));
    let session_changed = Arc::new(AtomicBool::new(false));
    let (s2, c2) = (stop.clone(), session_changed.clone());
    let Ok(handle) = service_control_handler::register(SERVICE, move |ev| match ev {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            s2.store(true, SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            c2.store(true, SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    }) else {
        return;
    };
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let _ = handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE));

    // Keep exactly one helper alive in the active console session.
    let exe = std::env::current_exe().unwrap_or_default();
    let mut helper: Option<(HANDLE, u32)> = None;
    while !stop.load(SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let alive = helper.is_some_and(|(h, _)| unsafe { WaitForSingleObject(h, 0) }.0 != 0);
        let moved = helper.is_some_and(|(_, s)| s != session) || session_changed.swap(false, SeqCst);
        if helper.is_some() && (!alive || moved) {
            if let Some((h, _)) = helper.take() {
                unsafe {
                    let _ = TerminateProcess(h, 0);
                    let _ = CloseHandle(h);
                }
            }
        }
        if helper.is_none() && session != 0xFFFF_FFFF {
            if let Ok(h) = launch_helper(session, &exe) {
                helper = Some((h, session));
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    if let Some((h, _)) = helper {
        unsafe {
            let _ = TerminateProcess(h, 0);
            let _ = CloseHandle(h);
        }
    }
    let _ = handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()));
}

// ---------------------------------------------------------------- helper (SYSTEM, user session)

/// Moves the calling thread onto whatever desktop currently receives input. Returns its name.
fn follow_input_desktop() -> Option<String> {
    unsafe {
        let d = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x10000000 /* GENERIC_ALL */)).ok()?;
        let mut name = [0u16; 64];
        let mut needed = 0;
        let _ = GetUserObjectInformationW(HANDLE(d.0), UOI_NAME, Some(name.as_mut_ptr() as _), (name.len() * 2) as u32, Some(&mut needed));
        let _ = SetThreadDesktop(d);
        // The old desktop handle (if any) stays referenced by the thread until it switches again.
        let n = String::from_utf16_lossy(&name).trim_end_matches('\0').to_string();
        let _ = CloseDesktop(d);
        Some(n)
    }
}

fn exe_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
    }
}

/// Only the same exe, in the same session, may drive the helper.
fn client_allowed(pipe: HANDLE) -> bool {
    unsafe {
        let mut pid = 0;
        if GetNamedPipeClientProcessId(pipe, &mut pid).is_err() {
            return false;
        }
        let mut s = 0;
        let same_session = ProcessIdToSessionId(pid, &mut s).is_ok() && s == session_id();
        let me = std::env::current_exe().map(|p| p.display().to_string().to_lowercase()).unwrap_or_default();
        same_session && exe_of(pid).is_some_and(|e| e == me)
    }
}

pub fn run_helper() -> Result<()> {
    // Exit when our exe gets replaced (auto-update / rebuild); the service starts the new one.
    let me = std::env::current_exe()?;
    let stamp = std::fs::metadata(&me).and_then(|m| m.modified()).ok();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if std::fs::metadata(&me).and_then(|m| m.modified()).ok() != stamp {
            std::process::exit(0);
        }
    });
    let name = HSTRING::from(pipe_name(session_id()));
    // SYSTEM: full access; interactive users: read/write (the exe check below does the real gatekeeping).
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from("D:(A;;GA;;;SY)(A;;GRGW;;;IU)"), SDDL_REVISION_1, &mut sd, None)? };
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
    loop {
        let pipe = unsafe { CreateNamedPipeW(&name, PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES, 16 << 20, 1 << 20, 0, Some(&sa)) };
        if pipe.is_invalid() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let _ = unsafe { ConnectNamedPipe(pipe, None) }; // ERROR_PIPE_CONNECTED is fine too
        if !client_allowed(pipe) {
            unsafe {
                let _ = CloseHandle(pipe);
            }
            continue;
        }
        // One thread per client; it owns the pipe and follows the input desktop.
        let raw = pipe.0 as isize;
        std::thread::spawn(move || {
            let mut f = unsafe { std::fs::File::from_raw_handle(raw as _) };
            serve(&mut f);
        });
    }
}

/// One capturer for the whole helper: Windows allows a single desktop duplication per screen
/// per process, so every connection shares it. (desktop name, monitor) tell when to recreate.
static CAP: std::sync::Mutex<Option<(Capturer, String, u8)>> = std::sync::Mutex::new(None);

fn serve(f: &mut std::fs::File) {
    let mut desk = String::new();
    while let Ok(req) = read_msg::<Req>(f) {
        let resp = match req {
            Req::Status => {
                desk = follow_input_desktop().unwrap_or_default();
                Resp::Status { secure: !desk.eq_ignore_ascii_case("Default") }
            }
            Req::Frame { monitor } => {
                let now = follow_input_desktop().unwrap_or_default();
                let mut cap = CAP.lock().unwrap();
                if cap.as_ref().is_none_or(|(_, d, m)| *d != now || *m != monitor) {
                    *cap = None; // release the old duplication before making a new one
                    let mons = monitors();
                    *cap = mons.get(monitor as usize).or(mons.first()).cloned().and_then(|m| Capturer::new(m).ok()).map(|c| (c, now.clone(), monitor));
                }
                match cap.as_mut() {
                    Some((c, _, _)) => {
                        let mut buf = Vec::new();
                        match c.next_frame(100, &mut buf) {
                            Ok(ch) if ch.image => Resp::Frame { w: c.width as u32, h: c.height as u32, bgra: buf },
                            Ok(_) => Resp::NoFrame,
                            Err(e) => {
                                *cap = None;
                                Resp::Err(format!("{e:#}"))
                            }
                        }
                    }
                    None => Resp::Err("can't capture this desktop".into()),
                }
            }
            Req::Input(list) => {
                if follow_input_desktop().is_some() {
                    let ins: Vec<INPUT> = list
                        .iter()
                        .map(|r| match *r {
                            RawIn::Mouse { dx, dy, data, flags } => INPUT {
                                r#type: INPUT_MOUSE,
                                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: MOUSE_EVENT_FLAGS(flags), time: 0, dwExtraInfo: crate::kbhook::OUR_INPUT } },
                            },
                            RawIn::Key { scan, flags } => INPUT {
                                r#type: INPUT_KEYBOARD,
                                Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: KEYBD_EVENT_FLAGS(flags), time: 0, dwExtraInfo: crate::kbhook::OUR_INPUT } },
                            },
                        })
                        .collect();
                    unsafe { SendInput(&ins, std::mem::size_of::<INPUT>() as i32) };
                }
                Resp::Ok
            }
        };
        if write_msg(f, &resp).is_err() {
            break;
        }
        let _ = &desk;
    }
}
