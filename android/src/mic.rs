//! Android asks the user before any app can use the microphone (runtime permission).
use jni::objects::{JObject, JValue};
use jni::{jni_sig, jni_str, JavaVM};

/// True if the microphone is already allowed. Otherwise shows Android's "Allow Kinlink to record
/// audio?" prompt and returns false (tap Mic again after allowing).
pub fn ensure_mic() -> bool {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let r: Result<bool, jni::errors::Error> = vm.attach_current_thread(|env| {
        // The activity is a global reference owned by the app; it's only borrowed here.
        let activity = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        let perm = env.new_string("android.permission.RECORD_AUDIO")?;
        let granted = env
            .call_method(&activity, jni_str!("checkSelfPermission"), jni_sig!("(Ljava/lang/String;)I"), &[JValue::Object(&perm)])?
            .i()?
            == 0;
        if !granted {
            let arr = env.new_object_array(1, jni_str!("java/lang/String"), &perm)?;
            env.call_method(&activity, jni_str!("requestPermissions"), jni_sig!("([Ljava/lang/String;I)V"), &[JValue::Object(&arr), JValue::Int(1)])?;
        }
        Ok(granted)
    });
    r.unwrap_or_else(|e| {
        log::error!("microphone permission: {e}");
        false
    })
}

/// Opens a web link in the phone's browser (Android Intent.ACTION_VIEW). Err = why it failed.
pub fn open_url(url: &str) -> Result<(), String> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let r: Result<(), jni::errors::Error> = vm.attach_current_thread(|env| {
        let activity = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        let jurl = env.new_string(url)?;
        let uri = env.call_static_method(jni_str!("android/net/Uri"), jni_str!("parse"), jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"), &[JValue::Object(&jurl)])?.l()?;
        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object(jni_str!("android/content/Intent"), jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"), &[JValue::Object(&action), JValue::Object(&uri)])?;
        // FLAG_ACTIVITY_NEW_TASK: required when Android doesn't treat the caller as an activity.
        env.call_method(&intent, jni_str!("addFlags"), jni_sig!("(I)Landroid/content/Intent;"), &[JValue::Int(0x1000_0000)])?;
        env.call_method(&activity, jni_str!("startActivity"), jni_sig!("(Landroid/content/Intent;)V"), &[JValue::Object(&intent)])?;
        Ok(())
    });
    r.map_err(|e| {
        log::error!("open url: {e}");
        e.to_string()
    })
}

/// Downloads the APK with Android's download manager (shows its own progress notification), then
/// opens the installer. `report` gets status lines for the UI. Err = couldn't even start.
pub fn download_and_install(url: &str, file: &str, report: impl Fn(String) + Send + 'static) -> Result<(), String> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let id: Result<i64, jni::errors::Error> = vm.attach_current_thread(|env| {
        let activity = unsafe { JObject::from_raw(env, ctx.context().cast()) };
        let svc = env.new_string("download")?;
        let dm = env.call_method(&activity, jni_str!("getSystemService"), jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"), &[JValue::Object(&svc)])?.l()?;
        let jurl = env.new_string(url)?;
        let uri = env.call_static_method(jni_str!("android/net/Uri"), jni_str!("parse"), jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"), &[JValue::Object(&jurl)])?.l()?;
        let req = env.new_object(jni_str!("android/app/DownloadManager$Request"), jni_sig!("(Landroid/net/Uri;)V"), &[JValue::Object(&uri)])?;
        let title = env.new_string("Kinlink update")?;
        env.call_method(&req, jni_str!("setTitle"), jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/DownloadManager$Request;"), &[JValue::Object(&title)])?;
        let mime = env.new_string("application/vnd.android.package-archive")?;
        env.call_method(&req, jni_str!("setMimeType"), jni_sig!("(Ljava/lang/String;)Landroid/app/DownloadManager$Request;"), &[JValue::Object(&mime)])?;
        // VISIBILITY_VISIBLE_NOTIFY_COMPLETED: tapping the finished notification also installs.
        env.call_method(&req, jni_str!("setNotificationVisibility"), jni_sig!("(I)Landroid/app/DownloadManager$Request;"), &[JValue::Int(1)])?;
        let (dir, name) = (env.new_string("Download")?, env.new_string(file)?);
        env.call_method(&req, jni_str!("setDestinationInExternalPublicDir"), jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/app/DownloadManager$Request;"), &[JValue::Object(&dir), JValue::Object(&name)])?;
        env.call_method(&dm, jni_str!("enqueue"), jni_sig!("(Landroid/app/DownloadManager$Request;)J"), &[JValue::Object(&req)])?.j()
    });
    let id = id.map_err(|e| {
        log::error!("download: {e}");
        e.to_string()
    })?;
    std::thread::spawn(move || {
        // getUriForDownloadedFile stays null until the download has finished successfully.
        for _ in 0..1200 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let ctx = ndk_context::android_context();
            let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
            let done: Result<bool, jni::errors::Error> = vm.attach_current_thread(|env| {
                let activity = unsafe { JObject::from_raw(env, ctx.context().cast()) };
                let svc = env.new_string("download")?;
                let dm = env.call_method(&activity, jni_str!("getSystemService"), jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"), &[JValue::Object(&svc)])?.l()?;
                let uri = env.call_method(&dm, jni_str!("getUriForDownloadedFile"), jni_sig!("(J)Landroid/net/Uri;"), &[JValue::Long(id)])?.l()?;
                if uri.is_null() {
                    return Ok(false);
                }
                let action = env.new_string("android.intent.action.VIEW")?;
                let intent = env.new_object(jni_str!("android/content/Intent"), jni_sig!("(Ljava/lang/String;)V"), &[JValue::Object(&action)])?;
                let mime = env.new_string("application/vnd.android.package-archive")?;
                env.call_method(&intent, jni_str!("setDataAndType"), jni_sig!("(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;"), &[JValue::Object(&uri), JValue::Object(&mime)])?;
                // NEW_TASK | GRANT_READ_URI_PERMISSION
                env.call_method(&intent, jni_str!("addFlags"), jni_sig!("(I)Landroid/content/Intent;"), &[JValue::Int(0x1000_0001)])?;
                env.call_method(&activity, jni_str!("startActivity"), jni_sig!("(Landroid/content/Intent;)V"), &[JValue::Object(&intent)])?;
                Ok(true)
            });
            match done {
                Ok(true) => return report(String::new()),
                Ok(false) => {}
                Err(e) => {
                    log::error!("install: {e}");
                    return report(format!("Downloaded. Open the notification or your Downloads folder to install it. ({e})"));
                }
            }
        }
        report("The download didn't finish. Check the notification.".into());
    });
    Ok(())
}


/// Window.addFlags/clearFlags on the activity (`activity` = AndroidApp::activity_as_ptr). Must run on
/// the Java UI thread. android-activity's own set_window_flags deadlocks against its event loop.
pub fn window_flags(activity: usize, add: i32, clear: i32) {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let r: Result<(), jni::errors::Error> = vm.attach_current_thread(|env| {
        let activity = unsafe { JObject::from_raw(env, activity as jni::sys::jobject) };
        let window = env.call_method(&activity, jni_str!("getWindow"), jni_sig!("()Landroid/view/Window;"), &[])?.l()?;
        env.call_method(&window, jni_str!("clearFlags"), jni_sig!("(I)V"), &[JValue::Int(clear)])?;
        env.call_method(&window, jni_str!("addFlags"), jni_sig!("(I)V"), &[JValue::Int(add)])?;
        Ok(())
    });
    if let Err(e) = r {
        log::error!("window flags: {e}");
    }
}
