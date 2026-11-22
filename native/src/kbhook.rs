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
static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
static TX: Mutex<Option<(Sender<KeyEv>, Wake)>> = Mutex::new(None);

unsafe extern "system" fn proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if std::env::var_os("PCLONE_DEBUG").is_some() && code >= 0 {
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

/// Installs the hook on its own thread with a plain GetMessage loop. Windows only calls a
/// low-level hook while its thread pumps messages, and silently removes hooks that don't
/// answer in time; the UI thread's wait doesn't reliably service them.
pub fn install(hwnd: HWND, wake: impl Fn() + Send + 'static) -> Receiver<KeyEv> {
    let (tx, rx) = channel();
    *TX.lock().unwrap() = Some((tx, Box::new(wake)));
    HWND_VAL.store(hwnd.0 as isize, Relaxed);
    // Test switch: lets simulated key presses through so the path can be tested automatically.
    ALLOW_INJECTED.store(std::env::var_os("PCLONE_HOOK_INJECTED").is_some(), Relaxed);
    std::thread::spawn(|| unsafe {
        HOOK_THREAD.store(windows::Win32::System::Threading::GetCurrentThreadId(), Relaxed);
        let module = GetModuleHandleW(None).unwrap_or_default();
        let mut hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), Some(module.into()), 0) {
            Ok(h) => h,
            Err(e) => return crate::plog!("keyboard hook failed: {e}"),
        };
        crate::plog!("keyboard: hook installed");
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
    crate::plog!(
        "keyboard: a key got past the hook during the session; reinstalling it (hook saw {} keys, last with foreground {:#x} extra {:#x}; ours {:#x}, now in front {:#x})",
        CALLS.load(Relaxed),
        LAST_FG.load(Relaxed),
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

