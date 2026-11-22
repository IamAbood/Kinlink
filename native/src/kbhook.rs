//! Low-level keyboard hook: while streaming and focused, every key (incl. Win, Alt+Tab) is swallowed
//! locally and forwarded to the host as a scan code. Ctrl+Alt+Del can't be intercepted (Windows SAS).
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::*};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Debug, Clone, Copy)]
pub struct KeyEv {
    pub scan: u16,
    pub ext: bool,
    pub down: bool,
}

type Wake = Box<dyn Fn() + Send>;

/// Marks input this app injects itself (host side), so a hook on the same PC skips only that.
/// Keys other software injects (keyboard drivers like Razer Synapse, macro tools) are real input.
pub const OUR_INPUT: usize = 0x5043_4C4E; // "PCLN"

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ALLOW_INJECTED: AtomicBool = AtomicBool::new(false);
static DEBUG: AtomicBool = AtomicBool::new(false);
static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
static TX: Mutex<Option<(Sender<KeyEv>, Wake)>> = Mutex::new(None);

unsafe extern "system" fn proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if DEBUG.load(Relaxed) && code >= 0 {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            crate::plog!("HOOK scan={:#x} active={} fg_match={} injected={}", k.scanCode, ACTIVE.load(Relaxed), GetForegroundWindow().0 as isize == HWND_VAL.load(Relaxed), k.flags.0 & LLKHF_INJECTED.0 != 0);
        }
        if code >= 0 {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            CALLS.fetch_add(1, Relaxed);
            LAST_FG.store(GetForegroundWindow().0 as isize, Relaxed);
            LAST_EXTRA.store(k.dwExtraInfo, Relaxed);
        }
        if code >= 0 && ACTIVE.load(Relaxed) && ours_in_front() {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            // Skip only keys this app injected (host and client on one PC would loop). Keys other
            // software injects are real typing: some keyboards send every key that way.
            if k.dwExtraInfo != OUR_INPUT || ALLOW_INJECTED.load(Relaxed) {
                let ev = KeyEv { scan: k.scanCode as u16, ext: k.flags.0 & LLKHF_EXTENDED.0 != 0, down: k.flags.0 & LLKHF_UP.0 == 0 };
                if let Some((tx, wake)) = &*TX.lock().unwrap() {
                    let _ = tx.send(ev);
                    // The hook runs inside the message wait without producing a window event,
                    // so the UI loop must be woken explicitly or the key sits in the queue.
                    wake();
                }
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

// What the hook last saw, for the "key got past the hook" log line.
static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_FG: AtomicIsize = AtomicIsize::new(0);
static LAST_EXTRA: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The foreground window is ours: the session window, or any other window of this process
/// (a child/owned window can be what Windows reports as foreground).
fn ours_in_front() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0 as isize == HWND_VAL.load(Relaxed) {
            return true;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        pid == std::process::id()
    }
}

/// "explorer.exe" etc. for a window handle (diagnostics only).
fn window_exe(hwnd: isize) -> String {
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(HWND(hwnd as _), Some(&mut pid));
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return format!("pid {pid}") };
        let (mut buf, mut len) = ([0u16; 512], 512u32);
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return format!("pid {pid}");
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or_default().to_string()
    }
}

/// "Windows 10.0 build 19045 (22H2), elevated: false": what matters for who gets keyboard input.
fn system_info() -> String {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let reg = |name: &str| unsafe {
        let (mut buf, mut len) = ([0u16; 64], 128u32);
        let ok = RegGetValueW(HKEY_LOCAL_MACHINE, windows::core::w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"), &windows::core::HSTRING::from(name), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len)).is_ok();
        if ok { String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]) } else { "?".into() }
    };
    let elevated = unsafe {
        let mut tok = windows::Win32::Foundation::HANDLE::default();
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0;
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_ok() && GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut n).is_ok() && e.TokenIsElevated != 0
    };
    format!("Windows build {} ({}), elevated: {elevated}", reg("CurrentBuild"), reg("DisplayVersion"))
}

/// At session start with our window in front: press F24 (no real keyboard has it) and log
/// whether the hook saw it. Tells "hook blocked for our window" apart from everything else.
pub fn self_test() {
    std::thread::spawn(|| unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F24};
        std::thread::sleep(std::time::Duration::from_millis(800)); // the fresh hook is in place
        if !ours_in_front() {
            return crate::plog!("keyboard: self-test skipped (Kinlink not in front); {}", system_info());
        }
        let before = CALLS.load(Relaxed);
        let key = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F24, wScan: 0x76, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
        SendInput(&[key(Default::default()), key(KEYEVENTF_KEYUP)], std::mem::size_of::<INPUT>() as i32);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let seen = CALLS.load(Relaxed) - before;
        crate::plog!("keyboard: self-test with Kinlink in front: hook saw {seen} of 2 test keys ({}); {}", if seen >= 2 { "OK" } else { "BLOCKED" }, system_info());
    });
}

/// Logs the installed antivirus / security products once: they are what usually blocks the hook.
pub fn log_security_software() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            use std::os::windows::process::CommandExt;
            let out = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "(Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct -ErrorAction SilentlyContinue).displayName -join ', '"])
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .output();
            let names = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            crate::plog!("keyboard: security software on this PC: {}", if names.is_empty() { "(none reported)" } else { &names });
        });
    });
}

/// Installs the hook on its own thread with a plain GetMessage loop. Windows only calls a
/// low-level hook while its thread pumps messages, and silently removes hooks that don't
/// answer in time; the UI thread's wait doesn't reliably service them.
pub fn install(hwnd: HWND, wake: impl Fn() + Send + 'static) -> Receiver<KeyEv> {
    let (tx, rx) = channel();
    *TX.lock().unwrap() = Some((tx, Box::new(wake)));
    HWND_VAL.store(hwnd.0 as isize, Relaxed);
    // Test switch: lets simulated key presses through so the path can be tested automatically.
    ALLOW_INJECTED.store(std::env::var_os("PCLONE_HOOK_INJECTED").is_some(), Relaxed);
    DEBUG.store(std::env::var_os("PCLONE_DEBUG").is_some(), Relaxed);
    std::thread::spawn(|| unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
        HOOK_THREAD.store(GetCurrentThreadId(), Relaxed);
        // Windows skips (then silently removes) a hook that answers late. On a slow laptop busy
        // decoding the stream, a normal-priority thread was too late every time Kinlink was in
        // front, so Alt+Tab went local. The hook does almost nothing, so top priority is safe.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let module = GetModuleHandleW(None).unwrap_or_default();
        let mut hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), Some(module.into()), 0) {
            Ok(h) => h,
            Err(e) => return crate::plog!("keyboard hook failed: {e}"),
        };
        crate::plog!("keyboard: hook installed ({})", system_info());
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_APP_REHOOK {
                // Windows silently drops a hook that was slow to answer once; put it back.
                let _ = UnhookWindowsHookEx(hook);
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), Some(module.into()), 0) {
                    Ok(h) => hook = h,
                    Err(e) => crate::plog!("keyboard hook reinstall failed: {e}"),
                }
            }
        }
    });
    rx
}

const WM_APP_REHOOK: u32 = WM_APP + 7;
static HOOK_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LAST_REHOOK: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// A key reached the window during a session, so the hook didn't catch it (Windows removed
/// it, or something else is in the way): log it and reinstall the hook (at most every 3 s).
pub fn missed_key() {
    let mut last = LAST_REHOOK.lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3)) {
        return;
    }
    *last = Some(std::time::Instant::now());
    let fg = unsafe { GetForegroundWindow() }.0 as isize;
    let last = LAST_FG.load(Relaxed);
    crate::plog!(
        "keyboard: a key got past the hook during the session; reinstalling it (hook saw {} keys, last with foreground {:#x} [{}] extra {:#x}; ours {:#x}, now in front {:#x})",
        CALLS.load(Relaxed),
        last,
        window_exe(last),
        LAST_EXTRA.load(Relaxed),
        HWND_VAL.load(Relaxed),
        fg
    );
    rehook();
}

/// Puts the hook back fresh. Windows drops a hook without telling us, and Alt+Tab / Win never
/// reach our window, so missed_key can't notice: also done at session start and on focus loss.
pub fn rehook() {
    let t = HOOK_THREAD.load(Relaxed);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, WM_APP_REHOOK, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn is_active() -> bool {
    ACTIVE.load(Relaxed)
}

pub fn set_active(on: bool) {
    ACTIVE.store(on, Relaxed);
}

