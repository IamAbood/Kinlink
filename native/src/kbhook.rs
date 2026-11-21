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
        if code >= 0 && ACTIVE.load(Relaxed) && GetForegroundWindow().0 as isize == HWND_VAL.load(Relaxed) {
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
        let module = GetModuleHandleW(None).unwrap_or_default();
        if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), Some(module.into()), 0) {
            return crate::plog!("keyboard hook failed: {e}");
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
    });
    rx
}

pub fn set_active(on: bool) {
    ACTIVE.store(on, Relaxed);
}

