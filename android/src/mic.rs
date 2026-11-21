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
