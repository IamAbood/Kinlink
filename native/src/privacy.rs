//! Privacy mode: while a guest with it is connected, black covers sit on every host monitor.
//! They're excluded from capture (guests still see the desktop) and click-through (guest input
//! reaches the apps below). Needs Windows 10 2004+ for WDA_EXCLUDEFROMCAPTURE.
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static WANT: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Called by the host: is a guest with privacy mode connected right now?
pub fn set(on: bool) {
    if WANT.swap(on, SeqCst) != on {
        update();
    }
}

fn wanted() -> bool {
    WANT.load(SeqCst)
}

fn update() {
    if wanted() && !RUNNING.swap(true, SeqCst) {
        std::thread::spawn(|| {
            cover_thread();
            RUNNING.store(false, SeqCst);
            if wanted() {
                update(); // switched back on while closing
            }
        });
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(h, m, w, l),
    }
}

/// Shows the covers until the switch goes off or guests leave.
pub fn cover_thread() {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("KinlinkPrivacy"), hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0), ..Default::default() };
        RegisterClassW(&wc);
        let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT;
        let mut wins = Vec::new();
        for m in crate::capture::monitors() {
            let r = m.rect;
            let Ok(h) = CreateWindowExW(ex, w!("KinlinkPrivacy"), w!("Kinlink privacy"), WS_POPUP, r.left, r.top, r.right - r.left, r.bottom - r.top, None, None, Some(inst.into()), None) else { continue };
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
            if SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE).is_err() {
                // Old Windows: the cover would hide the screen from guests too.
                crate::plog!("privacy: this Windows can't hide windows from capture");
                let _ = DestroyWindow(h);
                continue;
            }
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
            wins.push(h);
        }
        let mut msg = MSG::default();
        let mut n = 0u32;
        while wanted() && !wins.is_empty() {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            // ponytail: re-assert topmost every second; other topmost windows (Start, flyouts) can still pop over briefly.
            if n % 10 == 0 {
                for &h in &wins {
                    let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
            n += 1;
            std::thread::sleep(Duration::from_millis(100));
        }
        for h in wins {
            let _ = DestroyWindow(h);
        }
    }
}

/// Test: with the cover up, the captured screen must not be all black.
pub fn selftest() -> i32 {
    set(true);
    std::thread::sleep(Duration::from_millis(800));
    let up = RUNNING.load(SeqCst); // false = no cover could be made
    let m = crate::capture::monitors().into_iter().next().expect("monitor");
    let mut c = crate::capture::Capturer::new(m).expect("capture");
    let mut buf = Vec::new();
    for _ in 0..20 {
        let _ = c.next_frame(100, &mut buf);
    }
    set(false);
    let lit = buf.chunks(4).filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 30).count();
    let pct = lit * 100 / (buf.len() / 4).max(1);
    println!("cover up {up}, guests see {pct}% non-black");
    if up && pct > 5 { 0 } else { 1 }
}
