#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// COM/DXGI calls live inside unsafe fns on purpose; per-call unsafe blocks would only add noise.
#![allow(unsafe_op_in_unsafe_fn)]

mod app;
mod applog;
mod audio;
mod capture;
mod client;
mod clip;
mod codec;
mod files;
mod crypto;
mod host;
mod hotkey;
mod input;
mod kbhook;
mod mf;
mod net;
mod padout;
mod nettest;
mod notify;
mod proto;
mod rec;
mod rtl;
mod i18n;
mod i18n_ar;
mod render;
mod scale;
mod settings;
mod voice;
mod shortcut;
mod signal;
mod stun;
mod theme;
mod syshelper;
mod pkg;
mod updater;
mod web;
mod games;

use std::time::{Duration, Instant};

fn main() {
    // Two TLS crypto backends get compiled in (MQTT client + HTTP client); pick one explicitly,
    // otherwise rustls refuses to choose and the friend network can't connect.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    // Lock-screen helper modes (no window).
    if args.iter().any(|a| a == "--service") {
        let _ = syshelper::run_service();
        return;
    }
    if args.iter().any(|a| a == "--system-helper") {
        let _ = syshelper::run_helper();
        return;
    }
    if args.iter().any(|a| a == "--install-service") {
        let r = syshelper::install();
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--uninstall-service") {
        let r = syshelper::uninstall();
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    updater::after_update(&args);
    if args.iter().any(|a| a == "--version") {
        println!("{}", updater::current());
        return;
    }
    if args.iter().any(|a| a == "--update-swap") {
        // Elevated half of an update when the app lives in Program Files (see updater::install).
        let ok = matches!(updater::check(), Ok(Some(r)) if updater::swap(&r, |_| {}).is_ok());
        std::process::exit(if ok { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--update-now") {
        // Headless update (tests / scripts).
        match updater::check() {
            Ok(Some(r)) => match updater::install(&r, |_| {}) {
                Ok(()) => println!("updated to {}", r.version),
                Err(e) => println!("update failed: {e:#}"),
            },
            Ok(None) => println!("up to date ({})", updater::current()),
            Err(e) => println!("check failed: {e:#}"),
        }
        return;
    }
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(selftest(args.iter().any(|a| a == "--software")));
    }
    if args.iter().any(|a| a == "--codecs") {
        for line in mf::list_codecs() {
            println!("{line}");
        }
        return;
    }
    if args.iter().any(|a| a == "--helpertest") {
        std::process::exit(helpertest());
    }
    if args.iter().any(|a| a == "--audiotest") {
        std::process::exit(audiotest());
    }
    if args.iter().any(|a| a == "--nettest") {
        let r = nettest::run(false);
        for l in &r.lines {
            println!("{} {}", if l.ok { "ok  " } else { "note" }, l.text);
        }
        println!("suggested max bitrate: {:?} Mbps", r.suggest_mbps);
        return;
    }
    if args.iter().any(|a| a == "--toasttest") {
        notify::register();
        { notify::toast("tester_b#1234 is online", "Click Connect in Kinlink to join."); }
        std::thread::sleep(Duration::from_secs(2));
        return;
    }
    if args.iter().any(|a| a == "--startuptest") {
        // Store build: the startup task must switch on and off (results go to the log).
        let (a, b, c) = (pkg::startup_task(None), pkg::startup_task(Some(true)), pkg::startup_task(Some(false)));
        let r = format!("packaged {} before {a} on {b} off {c} -> {}", pkg::packaged(), if pkg::packaged() && b && !c { "PASS" } else { "FAIL" });
        if let Some(f) = std::env::var_os("PCLONE_OUT") {
            let _ = std::fs::write(f, &r);
        }
        println!("{r}");
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--alttabtest") {
        std::process::exit(alttabtest());
    }
    if args.iter().any(|a| a == "--padtest") {
        std::process::exit(padtest());
    }
    if args.iter().any(|a| a == "--appaudiotest") {
        // Lists apps playing sound, then counts audio chunks for 4 s with PCLONE_MUTE (one exe) muted.
        let apps = audio::playing_apps();
        println!("playing: {apps:?}");
        if std::env::var_os("PCLONE_EACH").is_some() {
            // Which app is actually making sound: each one alone for 2 s.
            let names: Vec<String> = apps.iter().map(|a| a.1.clone()).collect();
            for one in &names {
                let others: Vec<String> = names.iter().filter(|n| *n != one).cloned().collect();
                let n = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
                let n2 = n.clone();
                let (h, _) = audio::start_app_mix(others, move |c| {
                    let peak = c.iter().map(|x| (*x as i32).unsigned_abs()).max().unwrap_or(0);
                    n2.fetch_max(peak, std::sync::atomic::Ordering::SeqCst);
                });
                std::thread::sleep(Duration::from_secs(2));
                drop(h);
                println!("{one} alone: loudest sample {} (of 32767)", n.load(std::sync::atomic::Ordering::SeqCst));
            }
            std::process::exit(0);
        }
        let muted: Vec<String> = std::env::var("PCLONE_MUTE").map(|m| vec![m]).unwrap_or_default();
        let n = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let n2 = n.clone();
        let (h, _) = audio::start_app_mix(muted.clone(), move |_| {
            n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_secs(5));
        drop(h);
        println!("muted {muted:?}: {} sound chunks in 5 s", n.load(std::sync::atomic::Ordering::SeqCst));
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--toolstest") {
        // In-process host + guest: games list, all-screens picture.
        let cfg = host::HostConfig { name: "host".into(), port: 47895, max_mbps: 20, fps: 30, hardware: true, upnp: false, record: false, muted_apps: Vec::new() };
        let h = host::Host::start(cfg).unwrap();
        let (ce, he) = (crypto::Ephemeral::new(), crypto::Ephemeral::new());
        h.handle().prepare(he.cipher(&ce.public, 79, false), host::Perms { mouse: true, keyboard: true, gamepad: false }, "g#0001".into());
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let c = client::Client::start(sock, vec!["127.0.0.1:47895".parse().unwrap()], ce.cipher(&he.public, 79, true), "host".into(), || {});
        std::thread::sleep(Duration::from_secs(3));
        c.list_games();
        std::thread::sleep(Duration::from_secs(3));
        println!("games: {:?}", c.games());
        let mons = capture::monitors().len();
        c.select_monitor(proto::ALL_SCREENS);
        let t = Instant::now();
        let mut size = (0, 0);
        while t.elapsed() < Duration::from_secs(5) {
            if let Some(f) = c.take_frame() {
                size = (f.w, f.h);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!("monitors: {mons}, all-screens picture {}x{}; host: {}", size.0, size.1, h.status().encoder);
        let ok = size.0 > 0;
        println!("{}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--webtest") {
        // View-only host + a browser link; prints who's connected for 3 minutes.
        let cfg = host::HostConfig { name: "host".into(), port: 47896, max_mbps: 8, fps: 30, hardware: true, upnp: false, record: std::env::var_os("PCLONE_RECORD").is_some(), muted_apps: std::env::var("PCLONE_MUTE").map(|m| vec![m]).unwrap_or_default() };
        let h = host::Host::start(cfg).unwrap();
        let link = h.handle().web_link(host::Perms { mouse: false, keyboard: false, gamepad: false }, std::env::var_os("PCLONE_PARTY").is_some());
        let local = link.replacen(crate::web::PAGE, "http://localhost:8765/", 1);
        println!("LINK {link}
LOCAL {local}");
        for _ in 0..90 {
            std::thread::sleep(Duration::from_secs(2));
            let st = h.status();
            println!("clients {:?} | {} | {}", st.clients, st.encoder, st.stats);
        }
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--signaltest") {
        std::process::exit(signaltest());
    }
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if let Some(p) = arg("--profile") {
        unsafe { std::env::set_var("PCLONE_PROFILE", p) };
    }
    if args.iter().any(|a| a == "--pair-test-profiles") {
        return pair_test_profiles();
    }
    if !single_instance(arg("--connect-friend")) {
        return;
    }
    applog::init();
    crate::plog!("Kinlink {} starting", updater::current());
    // Like Parsec: stay smooth while a game hogs the CPU. Costs nothing while idle.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
    let el = winit::event_loop::EventLoop::<()>::with_user_event().build().expect("event loop");
    let mut app = app::App::new(el.create_proxy(), args.iter().any(|a| a == "--minimized") || pkg::started_at_login(), arg("--connect-friend"));
    el.run_app(&mut app).expect("run");
}

/// Returns false (after showing the running copy) if the app is already open.
fn single_instance(connect: Option<String>) -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    unsafe {
        let name = windows::core::HSTRING::from(format!("ParsecCloneNative.SingleInstance.{}", settings::profile().unwrap_or_default()));
        let m = CreateMutexW(None, true, &name);
        if GetLastError() != ERROR_ALREADY_EXISTS {
            std::mem::forget(m); // hold it for the process lifetime
            return true;
        }
        use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
        // A desktop "connect" shortcut while the app is already running: hand the request over.
        if let (Some(id), Some(p)) = (connect, settings::connect_request_path()) {
            let _ = std::fs::write(p, id);
        }
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, &settings::show_event_name()) {
            let _ = SetEvent(h);
        }
        false
    }
}

/// Test helper: profiles testA/testB as friends; B lets A connect without asking.
fn pair_test_profiles() {
    let (a, b, c) = (crypto::Identity::new("tester_a"), crypto::Identity::new("tester_b"), crypto::Identity::new("tester_c"));
    let fr = |me: &crypto::Identity, auto: bool| signal::Friend { name: me.name.clone(), tag: me.tag(), public: me.public, allow_mouse: true, allow_keyboard: true, allow_gamepad: true, auto_connect: auto, nickname: String::new(), group: String::new() };
    // B is the host; A and C are both B's friends and may connect without asking.
    for (p, me, friends) in [("testA", &a, vec![fr(&b, false)]), ("testB", &b, vec![fr(&a, true), fr(&c, true)]), ("testC", &c, vec![fr(&b, false)])] {
        unsafe { std::env::set_var("PCLONE_PROFILE", p) };
        me.save();
        println!("{p}: {} (friends: {})", me.id(), friends.iter().map(|f| f.id()).collect::<Vec<_>>().join(", "));
        signal::write_friends(&settings::data_dir().unwrap().join("friends.json"), friends);
    }
    // testA as a phone: push identity-phone.json as identity.json into the app's files dir.
    let _ = std::fs::write(settings::data_dir().unwrap().parent().unwrap().join("ParsecCloneNative-testA").join("identity-phone.json"), a.phone_file());
}

/// Needs the helper service. Normal desktop -> secure=false + frame; then (while a UAC prompt is up)
/// secure=true, capture it, press Esc through the helper to cancel it.
fn helpertest() -> i32 {
    let mut l = match syshelper::Link::connect() {
        Ok(l) => l,
        Err(e) => {
            println!("FAIL can't reach helper: {e:#}");
            return 1;
        }
    };
    println!("normal desktop: secure={:?}", l.secure().ok());
    let mut grab = |l: &mut syshelper::Link, name: &str| {
        for _ in 0..20 {
            if let Ok(Some((w, h, bgra))) = l.frame(0) {
                let path = std::env::temp_dir().join(name);
                let f = std::fs::File::create(&path).unwrap();
                let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                e.set_color(png::ColorType::Rgba);
                let rgba: Vec<u8> = bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                e.write_header().unwrap().write_image_data(&rgba).unwrap();
                println!("captured {w}x{h} -> {}", path.display());
                return true;
            }
        }
        println!("no frame");
        false
    };
    grab(&mut l, "pclone-helper-normal.png");
    if args_has("--stream") {
        // Session whose video comes from the SYSTEM helper (PCLONE_FORCE_HELPER), no UAC needed.
        unsafe { std::env::set_var("PCLONE_FORCE_HELPER", "1") };
        let cfg = host::HostConfig { name: "host".into(), port: 47897, max_mbps: 20, fps: 30, hardware: true, upnp: false, record: false, muted_apps: Vec::new() };
        let h = host::Host::start(cfg).unwrap();
        let (ce, he) = (crypto::Ephemeral::new(), crypto::Ephemeral::new());
        h.handle().prepare(he.cipher(&ce.public, 78, false), host::Perms { mouse: false, keyboard: false, gamepad: false }, "g#0001".into());
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let c = client::Client::start(sock, vec!["127.0.0.1:47897".parse().unwrap()], ce.cipher(&he.public, 78, true), "host".into(), || {});
        let (mut frames, t) = (0, Instant::now());
        while t.elapsed() < Duration::from_secs(6) {
            if c.take_frame().is_some() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let shot = c.screenshot();
        println!("guest got {frames} frames streamed via the helper; screenshot: {shot:?}");
        println!("host: {}", h.status().encoder);
        return if frames > 0 { 0 } else { 1 };
    }
    if !args_has("--with-uac") {
        return 0;
    }
    // Optionally run a real encrypted session so the guest's screenshot shows what it streamed.
    let session = args_has("--session").then(|| {
        let cfg = host::HostConfig { name: "host".into(), port: 47898, max_mbps: 20, fps: 30, hardware: true, upnp: false, record: false, muted_apps: Vec::new() };
        let h = host::Host::start(cfg).unwrap();
        let (ce, he) = (crypto::Ephemeral::new(), crypto::Ephemeral::new());
        h.handle().prepare(he.cipher(&ce.public, 77, false), host::Perms { mouse: true, keyboard: true, gamepad: false }, "g#0001".into());
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let c = client::Client::start(sock, vec!["127.0.0.1:47898".parse().unwrap()], ce.cipher(&he.public, 77, true), "host".into(), || {});
        std::thread::sleep(Duration::from_secs(3));
        (h, c)
    });
    // Trigger a UAC prompt (cancelled below, nothing gets elevated).
    let _ = std::process::Command::new("powershell").args(["-NoProfile", "-Command", "Start-Process cmd -Verb RunAs"]).spawn();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) && !l.secure().unwrap_or(false) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let up = l.secure().unwrap_or(false);
    println!("UAC prompt showing on the secure desktop: {up}");
    if !up {
        return 1;
    }
    std::thread::sleep(Duration::from_millis(800));
    let shot = grab(&mut l, "pclone-helper-uac.png");
    if let Some((_h, c)) = &session {
        std::thread::sleep(Duration::from_millis(2500)); // host notices the secure desktop and streams it
        match c.screenshot() {
            Ok(p) => println!("guest screenshot during UAC: {}", p.display()),
            Err(e) => println!("guest screenshot failed: {e}"),
        }
    }
    // Esc = "No" on the UAC prompt, sent through the SYSTEM helper.
    let _ = l.input(vec![syshelper::RawIn::Key { scan: 0x01, flags: 0x0008 }, syshelper::RawIn::Key { scan: 0x01, flags: 0x0008 | 0x0002 }]);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) && l.secure().unwrap_or(true) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let gone = !l.secure().unwrap_or(true);
    println!("UAC prompt cancelled by helper input: {gone}");
    if shot && gone { 0 } else { 1 }
}

fn args_has(a: &str) -> bool {
    std::env::args().any(|x| x == a)
}

/// Our own tone must be heard by normal loopback but NOT by the self-excluding capture.
fn audiotest() -> i32 {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    use std::sync::Arc;
    let dev = cpal::default_host().default_output_device().unwrap();
    let cfg = dev.default_output_config().unwrap();
    let (rate, ch) = (cfg.sample_rate() as f32, cfg.channels() as usize);
    let mut phase = 0f32;
    let tone = dev
        .build_output_stream(cfg.config(), move |out: &mut [f32], _| {
            for f in out.chunks_mut(ch) {
                phase = (phase + 440.0 / rate) % 1.0;
                let v = (phase * std::f32::consts::TAU).sin() * 0.08;
                f.iter_mut().for_each(|s| *s = v);
            }
        }, |e| crate::plog!("{e}"), None)
        .unwrap();
    tone.play().unwrap();
    let measure = |exclude: bool| -> (u64, f64) {
        let (chunks, energy) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (c2, e2) = (chunks.clone(), energy.clone());
        let cb = move |pcm: Vec<i16>| {
            c2.fetch_add(1, SeqCst);
            e2.fetch_add(pcm.iter().map(|s| (*s as i64).unsigned_abs()).sum::<u64>() / pcm.len().max(1) as u64, SeqCst);
        };
        if exclude {
            let _h = audio::start_loopback_excluding_self(cb).unwrap();
            std::thread::sleep(Duration::from_secs(2));
        } else {
            let _h = audio::start_loopback(cb).unwrap();
            std::thread::sleep(Duration::from_secs(2));
        }
        let n = chunks.load(SeqCst);
        (n, energy.load(SeqCst) as f64 / n.max(1) as f64)
    };
    std::thread::sleep(Duration::from_millis(300));
    let normal = measure(false);
    let excl = measure(true);
    println!("normal loopback (hears everything):  {} chunks, avg level {:.0}", normal.0, normal.1);
    println!("self-excluding loopback:             {} chunks, avg level {:.0}", excl.0, excl.1);
    let ok = normal.0 > 100 && normal.1 > 500.0 && (excl.0 < 20 || excl.1 < normal.1 / 4.0);
    println!("{}", if ok { "PASS: our own sound is excluded" } else { "FAIL" });
    if ok { 0 } else { 1 }
}

/// Real virtual-controller test (needs ViGEmBus): Xbox pads per guest, rumble routing,
/// player-slot reordering, and a PlayStation pad.
fn padtest() -> i32 {
    use std::sync::mpsc::channel;
    use windows::Win32::UI::Input::XboxController::{XInputGetState, XInputSetState, XINPUT_STATE, XINPUT_VIBRATION};
    let mut inj = input::Injector::default();
    let (tx, rx) = channel();
    inj.rumble_tx = Some(tx);
    let xb = |buttons| proto::PadState { buttons, ..Default::default() };
    let read = || -> Vec<(u32, u16)> {
        (0..4u32).filter_map(|i| { let mut s = XINPUT_STATE::default(); (unsafe { XInputGetState(i, &mut s) } == 0).then(|| (i, s.Gamepad.wButtons.0)) }).collect()
    };
    let before = read();
    println!("XInput pads before: {before:?}");
    // Like the real client: resend state every 100 ms (Windows takes a moment to register new pads).
    for _ in 0..10 {
        inj.apply(111, proto::Input::Pad { id: 0, state: xb(0x1000) }); // guest 111 holds A
        inj.apply(222, proto::Input::Pad { id: 0, state: xb(0x2000) }); // guest 222 holds B
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300)); // player LEDs arrive via notifications
    let pads = inj.pads();
    let slots: Vec<(u64, Option<u32>)> = pads.iter().map(|p| (p.guest, p.player)).collect();
    println!("virtual pads (guest, player): {slots:?}");
    let after = read();
    println!("XInput pads after: {after:?}");
    let a_ok = after.iter().any(|(_, b)| *b == 0x1000) && after.iter().any(|(_, b)| *b == 0x2000);
    println!("{} each guest's button reaches its own pad", if a_ok { "ok  " } else { "FAIL" });
    // A game vibrates guest 222's pad.
    let p222 = pads.iter().find(|p| p.guest == 222).and_then(|p| p.player).unwrap_or(0);
    unsafe { XInputSetState(p222 - 1, &XINPUT_VIBRATION { wLeftMotorSpeed: 65535, wRightMotorSpeed: 32896 }) };
    // Skip the "motors off" notifications sent when pads are created.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut r = None;
    while Instant::now() < deadline {
        if let Ok(n) = rx.recv_timeout(Duration::from_millis(200)) {
            if n.2 > 0 || n.3 > 0 {
                r = Some(n);
                break;
            }
        }
    }
    println!("rumble notification: {r:?}");
    let r_ok = matches!(r, Some((222, 0, 255, 128)));
    println!("{} rumble routed to guest 222", if r_ok { "ok  " } else { "FAIL" });
    let first = inj.pads().first().map(|p| p.guest);
    inj.move_pad(1, true); // second pad becomes first
    std::thread::sleep(Duration::from_millis(800));
    let swapped: Vec<(u64, Option<u32>)> = inj.pads().iter().map(|p| (p.guest, p.player)).collect();
    println!("after moving the second pad up: {swapped:?}");
    let s_ok = swapped.len() == 2 && Some(swapped[1].0) == first && swapped[0].1 < swapped[1].1;
    println!("{} player slots reordered", if s_ok { "ok  " } else { "FAIL" });
    // PlayStation pad (keep a controller watcher running so Windows can announce it)
    let mut gil = gilrs::Gilrs::new().unwrap();
    inj.apply(333, proto::Input::Pad { id: 4, state: proto::PadState { buttons: 0x1000, kind: proto::PAD_PLAYSTATION, ..Default::default() } });
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        while gil.next_event().is_some() {}
        if gil.gamepads().any(|(_, p)| p.vendor_id() == Some(0x054C)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let sony = gil.gamepads().any(|(_, p)| p.vendor_id() == Some(0x054C));
    let names: Vec<String> = gil.gamepads().map(|(_, p)| format!("{} ({:04x?})", p.name(), p.vendor_id())).collect();
    println!("controllers Windows sees: {names:?}");
    println!("{} virtual PlayStation pad visible", if sony { "ok  " } else { "FAIL" });
    // Guest side: the same reader the client uses must see Cross (= A / South) and the stick.
    let mut read_ok = false;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) && !read_ok {
        inj.apply(333, proto::Input::Pad { id: 4, state: proto::PadState { buttons: 0x1000, lx: 32767, kind: proto::PAD_PLAYSTATION, ..Default::default() } });
        while gil.next_event().is_some() {}
        read_ok = gil.gamepads().any(|(_, p)| p.vendor_id() == Some(0x054C) && p.is_pressed(gilrs::Button::South) && p.value(gilrs::Axis::LeftStickX) > 0.9);
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("{} PlayStation Cross + stick read back correctly", if read_ok { "ok  " } else { "FAIL" });
    // Write a DualShock 4 rumble report to the virtual pad (what the guest does to a real one);
    // the host's notification must deliver it to guest 333.
    let mut ps_rumble = None;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) && ps_rumble.is_none() {
        if let Err(e) = padout::rumble(0x05C4, 0, 200, 100) {
            println!("hid write: {e}");
        }
        while let Ok(n) = rx.recv_timeout(Duration::from_millis(200)) {
            if n.0 == 333 && (n.2 > 0 || n.3 > 0) {
                ps_rumble = Some(n);
                break;
            }
        }
    }
    let ps_ok = matches!(ps_rumble, Some((333, 4, 200, 100)));
    println!("{} PlayStation rumble: {ps_rumble:?}", if ps_ok { "ok  " } else { "FAIL" });
    inj.release_everyone();
    std::thread::sleep(Duration::from_millis(300));
    println!("XInput pads after release: {:?}", read());
    if a_ok && r_ok && s_ok && sony && read_ok && ps_ok { println!("PASS"); 0 } else { 1 }
}

/// Encrypted multi-guest loopback: two guests with different permissions, kick one mid-session.
/// Run with PCLONE_DRY_INPUT=1 (host logs input instead of injecting).
fn selftest(software: bool) -> i32 {
    let cfg = host::HostConfig { name: "host".into(), port: 47899, max_mbps: 20, fps: 60, hardware: !software, upnp: false, record: false, muted_apps: Vec::new() };
    let h = match host::Host::start(cfg) {
        Ok(h) => h,
        Err(e) => {
            println!("FAIL host: {e:#}");
            return 1;
        }
    };
    let join = |sid: u64, name: &str, perms: host::Perms| {
        let (ce, he) = (crypto::Ephemeral::new(), crypto::Ephemeral::new());
        h.handle().prepare(he.cipher(&ce.public, sid, false), perms, name.into());
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        client::Client::start(sock, vec!["127.0.0.1:47899".parse().unwrap()], ce.cipher(&he.public, sid, true), "host".into(), || {})
    };
    let tmp = std::env::temp_dir().join(format!("pclone-selftest-{}", rand::random::<u32>()));
    let _ = std::fs::create_dir_all(tmp.join("recv"));
    unsafe {
        std::env::set_var("PCLONE_SAVE_DIR", tmp.join("recv"));
        std::env::set_var("USERPROFILE", &tmp); // screenshots/recordings land in the temp folder
    }
    let guest_file: Vec<u8> = (0..3_000_000u32).map(|i| (i ^ (i >> 7)) as u8).collect();
    let host_file: Vec<u8> = (0..1_000_000u32).map(|i| (i * 31 + 7) as u8).collect();
    std::fs::write(tmp.join("from_guest.bin"), &guest_file).unwrap();
    std::fs::write(tmp.join("from_host.bin"), &host_file).unwrap();
    let g1 = join(1001, "guest1#0001", host::Perms { mouse: true, keyboard: false, gamepad: true });
    let g2 = join(2002, "guest2#0002", host::Perms { mouse: false, keyboard: true, gamepad: true });
    let start = Instant::now();
    let (mut f1, mut f2, mut f2_after_kick) = (0, 0, 0);
    let (mut sent, mut kicked, mut last2) = (false, false, (0, 0));
    let mut clip_set = false;
    let saved_clip = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok());
    let (mut rec_started, mut rec_stopped) = (false, false);
    let (mut heard_host, mut heard_g1) = (false, false);
    while start.elapsed() < Duration::from_secs(10) {
        if g1.take_frame().is_some() {
            f1 += 1;
        }
        if let Some(f) = g2.take_frame() {
            f2 += 1;
            last2 = (f.w, f.h);
            if kicked {
                f2_after_kick += 1;
            }
        }
        if !sent && start.elapsed() > Duration::from_secs(2) {
            sent = true;
            println!("host guests: {:?}", h.status().clients);
            for g in [&g1, &g2] {
                g.input(proto::Input::MouseAbs { x: 0.5, y: 0.5 });
                g.input(proto::Input::Key { scan: 0x1E, ext: false, down: true });
                g.input(proto::Input::Key { scan: 0x1E, ext: false, down: false });
            }
            g2.set_quality(720, 10);
            g2.send_file(&tmp.join("from_guest.bin")).unwrap();
            println!("host sending file to {} guests", h.send_file(&tmp.join("from_host.bin")).unwrap());
            g2.say("hello from guest2");
            g2.set_mic(true);
            h.say("hi everyone");
        }
        if !rec_started && start.elapsed() > Duration::from_millis(2500) {
            rec_started = true;
            match g2.screenshot() {
                Ok(p) => println!("screenshot: {}", p.display()),
                Err(e) => println!("screenshot failed: {e}"),
            }
            g2.start_recording();
        }
        if !rec_stopped && start.elapsed() > Duration::from_millis(8500) {
            rec_stopped = true;
            g2.stop_recording();
        }
        if start.elapsed() > Duration::from_millis(3000) && start.elapsed() < Duration::from_millis(4500) {
            // ~1.5 s of "voice" from guest2 (a loud tone), 20 ms at a time
            let pcm: Vec<i16> = (0..480).map(|i| ((i as f32 * 0.12).sin() * 8000.0) as i16).collect();
            g2.send_voice(pcm);
            let (host_hears, _) = h.voice_status();
            let (g1_hears, _) = g1.voice_status();
            heard_host |= host_hears.iter().any(|n| n == "guest2#0002");
            heard_g1 |= g1_hears.iter().any(|n| n == "guest2#0002");
            std::thread::sleep(Duration::from_millis(19));
        }
        if !clip_set && start.elapsed() > Duration::from_secs(2) {
            // Copy a picture: it should reach the other side as clipboard content.
            clip_set = true;
            let px: Vec<u8> = (0..64 * 64).flat_map(|i| [(i % 256) as u8, 40, 200, 255]).collect();
            let _ = arboard::Clipboard::new().and_then(|mut c| c.set_image(arboard::ImageData { width: 64, height: 64, bytes: px.into() }));
        }
        if !kicked && start.elapsed() > Duration::from_secs(5) {
            kicked = true;
            h.kick("guest1#0001");
            println!("kicked guest1; host guests now: {:?}", h.status().clients);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let hs = h.status();
    // Files: every received copy must match its source byte for byte.
    let recv: Vec<Vec<u8>> = std::fs::read_dir(tmp.join("recv")).unwrap().flatten().filter(|e| e.path().is_file()).map(|e| std::fs::read(e.path()).unwrap()).collect();
    let clip_pngs = std::fs::read_dir(tmp.join("recv").join("Clipboard")).map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "png")).count()).unwrap_or(0);
    println!("clipboard: copied picture arrived on the other side x{clip_pngs}");
    // ...and a received picture goes onto the clipboard, ready to paste.
    let pasted = std::fs::read_dir(tmp.join("recv").join("Clipboard")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| {
        let ok = clip::set_received(&[e.path()], true).is_ok();
        ok && arboard::Clipboard::new().ok().and_then(|mut c| c.get_image().ok()).is_some_and(|i| i.width == 64 && i.height == 64)
    }) == Some(true);
    println!("clipboard: received picture is pasteable={pasted}");
    if let Some(t) = &saved_clip {
        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(t.clone()));
    }
    let got_guest = recv.iter().filter(|d| **d == guest_file).count();
    let got_host = recv.iter().filter(|d| **d == host_file).count();
    println!("files: host got guest2's 3 MB file x{got_guest}; guests got the host's 1 MB file x{got_host} (of {} files)", recv.len());
    let chat_host = hs.chat.iter().any(|(f, t)| f == "guest2#0002" && t == "hello from guest2");
    let chat_g1 = g1.chat_log().iter().any(|(f, t)| f == "guest2#0002" && t == "hello from guest2") && g1.chat_log().iter().any(|(f, t)| f == "host" && t == "hi everyone");
    let chat_g2 = g2.chat_log().iter().any(|(f, t)| f == "host" && t == "hi everyone");
    println!("chat: host got guest2's message={chat_host}, guest1 got relay+host={chat_g1}, guest2 got host={chat_g2}");
    let mic_err = g2.voice_status().1;
    println!("voice: host hears guest2={heard_host}, guest1 hears guest2 via relay={heard_g1}, guest2 mic error={mic_err:?}");
    std::thread::sleep(Duration::from_millis(500)); // recorder finalizes on the receive thread
    let notices = g2.take_notices();
    println!("recording: {notices:?}");
    let png = std::fs::read_dir(tmp.join("Pictures").join("Kinlink")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| std::fs::read(e.path()).unwrap_or_default()).unwrap_or_default();
    let png_ok = png.starts_with(b"\x89PNG") && png.len() > 10_000;
    let mp4 = std::fs::read_dir(tmp.join("Videos").join("Kinlink")).ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.path());
    let probe = mp4.as_ref().map(|p| rec::probe(p));
    println!("screenshot png: {} bytes, valid={png_ok}; recording read back: {probe:?}", png.len());
    let rec_ok = matches!(probe, Some(Ok((v, a, secs))) if v >= 5 && secs > 4.0); // video only arrives when the screen changes; audio only while something plays
    let _ = std::fs::remove_dir_all(&tmp);
    let files_ok = heard_host && heard_g1 && png_ok && rec_ok && got_guest == 1 && got_host == 2 && chat_host && chat_g1 && chat_g2 && clip_pngs >= 1 && pasted;
    println!("host: encoder={} | {}", hs.encoder, hs.stats);
    println!("guest1: frames={f1} state={:?}", g1.state());
    println!("guest2: frames={f2} (after kick {f2_after_kick}) last_frame={}x{} state={:?}", last2.0, last2.1, matches!(g2.state(), client::State::Connected { .. }));
    let st = g2.stats();
    println!("guest2 stats: capture->decoded={:.1}ms lost={}", st.e2e_ms, st.lost);
    let g1_kicked = matches!(g1.state(), client::State::Failed(_));
    g2.disconnect();
    drop(h);
    if f1 > 0 && f2 > 0 && f2_after_kick > 0 && g1_kicked && last2 == (1280, 720) && files_ok { 0 } else { 1 }
}

/// Two throwaway identities talk through the real public brokers: friend request, accept,
/// presence, and a connect request/answer. Nothing is written to disk.
fn signaltest() -> i32 {
    use signal::{Event, Sig, Signal};
    let a_ident = crypto::Identity::new("alpha_test");
    let a_store = std::env::temp_dir().join(format!("pclone-signaltest-{}.json", std::process::id()));
    let a = Signal::start(a_ident.clone(), Some(a_store.clone()), || {});
    let b = Signal::start(crypto::Identity::new("bravo_test"), None, || {});
    let (aid, bid) = (a.id(), b.id());
    println!("A = {aid}, B = {bid}");
    let wait = |what: &str, secs: u64, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            if f() {
                println!("ok  {what} ({:.1}s)", t.elapsed().as_secs_f32());
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("FAIL {what}");
        false
    };
    if !wait("both online on a broker", 20, &|| a.online() && b.online()) {
        return 1;
    }
    a.add_friend(&bid).unwrap();
    if !wait("B receives A's friend request", 30, &|| b.incoming().iter().any(|(id, _)| *id == aid)) {
        return 1;
    }
    let (_, apub) = b.incoming()[0].clone();
    b.accept(apub);
    if !wait("A sees B as friend", 20, &|| a.friends().iter().any(|(f, _)| f.id() == bid)) {
        return 1;
    }
    if !wait("A sees B online", 20, &|| a.friends().iter().any(|(_, on)| *on)) {
        return 1;
    }
    a.set_status("Busy");
    if !wait("B sees A's status", 20, &|| b.status_of(&apub) == "Busy") {
        return 1;
    }
    let bpub = a.friends()[0].0.public;
    a.send_message(&bpub, "hello from A");
    if !wait("B receives A's chat message", 20, &|| b.messages(&apub).iter().any(|x| !x.mine && x.text == "hello from A")) {
        return 1;
    }
    if !wait("A sees it delivered", 20, &|| a.messages(&bpub).iter().any(|x| x.mine && x.delivered)) {
        return 1;
    }
    a.ask_help(&bpub);
    if !wait("B gets A's help request", 20, &|| b.take_events().iter().any(|e| matches!(e, Event::HelpRequest(p) if *p == apub))) {
        return 1;
    }
    a.send(&bpub, Sig::Connect { req: 9, eph: [1; 32], addrs: vec!["203.0.113.1:5000".parse().unwrap()] });
    let got = std::sync::Mutex::new(false);
    if !wait("B gets connect request", 20, &|| {
        for e in b.take_events() {
            if let Event::IncomingConnect { req: 9, addrs, .. } = e {
                b.send(&apub, Sig::ConnectOk { req: 9, session: 5, eph: [2; 32], addrs });
                *got.lock().unwrap() = true;
            }
        }
        *got.lock().unwrap()
    }) {
        return 1;
    }
    if !wait("A gets the answer", 20, &|| a.take_events().iter().any(|e| matches!(e, Event::ConnectReply { sig: Sig::ConnectOk { req: 9, .. }, .. }))) {
        return 1;
    }
    // Rename A (same key): B keeps A as a friend under the new name and can still reach it.
    let a2_id = a_ident.renamed("alpha2test", 1);
    let new_id = a2_id.id();
    a.tell_renamed("alpha2test");
    a.shutdown();
    let a2 = Signal::start(a2_id, Some(a_store.clone()), || {}); // same friends file, like the app
    if !wait("B sees A's new name", 30, &|| b.friends().iter().any(|(f, _)| f.id() == new_id)) {
        return 1;
    }
    if !wait("renamed A comes online for B", 40, &|| a2.online() && b.friends().iter().any(|(f, on)| f.id() == new_id && *on)) {
        return 1;
    }
    // Invites: a stranger joins with a one-use code (no friend request).
    let e = Signal::start(crypto::Identity::new("echo_test"), None, || {});
    if !wait("E online", 20, &|| e.online()) {
        return 1;
    }
    let link = a2.create_invite(true, false, true);
    let (hid, code) = signal::parse_invite(&link).expect("invite link parses");
    e.join(&hid);
    let host = std::sync::Mutex::new(None);
    if !wait("E finds the host by its invite", 20, &|| {
        for ev in e.take_events() {
            if let Event::JoinReady { host: h, .. } = ev {
                *host.lock().unwrap() = Some(h);
            }
        }
        host.lock().unwrap().is_some()
    }) {
        return 1;
    }
    let host = host.lock().unwrap().unwrap();
    let ask = |req: u64| e.send_to(&hid, &host, Sig::GuestConnect { code: code.clone(), name: e.id(), req, eph: [3; 32], addrs: vec!["203.0.113.9:5000".parse().unwrap()] });
    ask(41);
    let guest = std::sync::Mutex::new(None);
    if !wait("host accepts the invite with its permissions", 20, &|| {
        for ev in a2.take_events() {
            if let Event::GuestConnect { from, name, req: 41, mouse: true, keyboard: false, gamepad: true, .. } = ev {
                a2.send_to(&name, &from, Sig::ConnectOk { req: 41, session: 9, eph: [4; 32], addrs: vec![] });
                *guest.lock().unwrap() = Some(from);
            }
        }
        guest.lock().unwrap().is_some()
    }) {
        return 1;
    }
    if !wait("guest gets the host's answer", 20, &|| e.take_events().iter().any(|ev| matches!(ev, Event::ConnectReply { sig: Sig::ConnectOk { req: 41, .. }, .. }))) {
        return 1;
    }
    ask(42);
    if !wait("the same code can't be used twice", 20, &|| e.take_events().iter().any(|ev| matches!(ev, Event::ConnectReply { sig: Sig::ConnectNo { req: 42, .. }, .. }))) {
        return 1;
    }
    e.shutdown();
    // Old short ids still work (friends on older versions share those).
    let c = Signal::start(crypto::Identity::new("charlie_test"), None, || {});
    if !wait("C online", 20, &|| c.online()) {
        return 1;
    }
    c.add_friend(&crypto::legacy_id(&new_id)).unwrap();
    if !wait("adding by the old short id reaches A", 40, &|| a2.incoming().iter().any(|(id, _)| id.starts_with("charlie_test#"))) {
        return 1;
    }
    // A faker brute-forces a key whose SHORT tag matches A's (seconds of work): whoever adds
    // A by the short id must be warned. (The long id can't be faked like this.)
    let short = crypto::legacy_id(&new_id);
    let t0 = Instant::now();
    let fake = loop {
        let f = crypto::Identity::new("alpha2test");
        if crypto::legacy_id(&f.id()).eq_ignore_ascii_case(&short) {
            break f;
        }
    };
    println!("faker found a matching short id in {:.1}s (long ids differ: {} vs {})", t0.elapsed().as_secs_f32(), fake.id(), new_id);
    let faker = Signal::start(fake, None, || {});
    let d = Signal::start(crypto::Identity::new("delta_test"), None, || {});
    if !wait("faker and D online", 20, &|| faker.online() && d.online()) {
        return 1;
    }
    d.add_friend(&short).unwrap();
    if !wait("D is warned that two PCs claim the id", 70, &|| d.outgoing_info().iter().any(|o| o.conflict)) {
        return 1;
    }
    for s in [&c, &d, &faker] {
        s.shutdown();
    }
    a2.shutdown();
    b.shutdown();
    let _ = std::fs::remove_file(&a_store);
    println!("PASS");
    0
}

/// Presses Alt+Tab through SendInput (like keyboard software such as Razer Synapse does) while a
/// session is "active" on the current window: the hook must forward it and Windows must not switch.
fn alttabtest() -> i32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU, VK_TAB};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    // PCLONE_OWN_WINDOW: put a window of this process in front first, like a real session.
    if std::env::var_os("PCLONE_OWN_WINDOW").is_some() {
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, SetForegroundWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE};
        unsafe {
            let w = CreateWindowExW(WINDOW_EX_STYLE(0), windows::core::w!("STATIC"), windows::core::w!("kbtest"), WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 300, 200, None, None, None, None).unwrap();
            let _ = SetForegroundWindow(w);
        }
        std::thread::sleep(Duration::from_millis(300));
        println!("own window in front: {}", unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(GetForegroundWindow(), None) } == unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });
    }
    let fg = unsafe { GetForegroundWindow() };
    let rx = kbhook::install(fg, || {});
    kbhook::set_active(true);
    std::thread::sleep(Duration::from_millis(300)); // hook thread up
    unsafe {
        keybd_event(VK_MENU.0 as u8, 0x38, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(Duration::from_millis(40));
        keybd_event(VK_TAB.0 as u8, 0x0F, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(Duration::from_millis(40));
        keybd_event(VK_TAB.0 as u8, 0x0F, KEYEVENTF_KEYUP, 0);
        std::thread::sleep(Duration::from_millis(40));
        keybd_event(VK_MENU.0 as u8, 0x38, KEYEVENTF_KEYUP, 0);
    }
    std::thread::sleep(Duration::from_millis(400));
    let got: Vec<_> = rx.try_iter().collect();
    let still = unsafe { GetForegroundWindow() } == fg;
    kbhook::set_active(false);
    std::thread::sleep(Duration::from_millis(200));
    println!("forwarded: {got:?}");
    println!("window stayed in front: {still}");
    let ok = got.iter().any(|k| k.scan == 0x0F && k.down) && still;
    println!("{}", if ok { "PASS" } else { "FAIL" });
    if ok { 0 } else { 1 }
}
