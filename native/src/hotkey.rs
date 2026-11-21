//! Host panic hotkey (Ctrl+Alt+Shift+F12): kicks every guest. Uses RegisterHotKey, which keeps
//! working where low-level hooks are blocked (e.g. by anti-cheat drivers).
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, VK_F12};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

static PANIC: AtomicBool = AtomicBool::new(false);

pub fn install(wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || unsafe {
        if RegisterHotKey(None, 1, MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, VK_F12.0 as u32).is_err() {
            return crate::plog!("panic hotkey unavailable (another app owns Ctrl+Alt+Shift+F12)");
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                PANIC.store(true, SeqCst);
                wake();
            }
        }
    });
}

/// True once per press.
pub fn take() -> bool {
    PANIC.swap(false, SeqCst)
}
