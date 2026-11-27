#![allow(dead_code)] // shared engine files carry desktop-only helpers
#![allow(deprecated)] // egui::Id::new, same as the desktop app
//! Kinlink for Android: control your PC from your phone.
//! Reuses the Windows app's engine files (crypto, friends, protocol, client, decoder, renderer).
#[path = "../../native/src/applog.rs"]
mod applog;
#[path = "../../native/src/audio.rs"]
mod audio;
#[path = "../../native/src/client.rs"]
mod client;
#[path = "../../native/src/codec.rs"]
mod codec;
#[path = "../../native/src/crypto.rs"]
mod crypto;
#[path = "../../native/src/files.rs"]
mod files;
#[path = "../../native/src/net.rs"]
mod net;
#[path = "../../native/src/proto.rs"]
mod proto;
#[path = "../../native/src/render.rs"]
mod render;
#[path = "../../native/src/i18n.rs"]
mod i18n;
#[path = "../../native/src/i18n_ar.rs"]
mod i18n_ar;
#[path = "../../native/src/rtl.rs"]
mod rtl;
#[path = "../../native/src/signal.rs"]
mod signal;
#[path = "../../native/src/theme.rs"]
mod theme;
#[path = "../../native/src/stun.rs"]
mod stun;

// Android-side versions of desktop-only modules.
mod clip;
mod rec;
mod settings;
#[path = "../../native/src/voice.rs"]
mod voice;
mod mic;
mod gamepad;
mod scan;

mod app;

use winit::platform::android::activity::AndroidApp;
use winit::platform::android::EventLoopBuilderExtAndroid;

#[unsafe(no_mangle)]
fn android_main(android: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("pclone"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Some(dir) = android.internal_data_path() {
        let _ = settings::DATA_DIR.set(dir);
    }
    applog::init();
    app::load_language();
    let handle = android.clone();
    let el = winit::event_loop::EventLoop::<()>::with_user_event().with_android_app(android).build().expect("event loop");
    let mut app = app::App::new(handle, el.create_proxy());
    el.run_app(&mut app).expect("run");
}
