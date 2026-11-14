//! What Whisker asks of Android itself, through its Java side (JNI): access
//! to the phone's files, and the certificate authorities the user added to
//! the phone.
//!
//! Whisker has no Java code of its own, so it can't get answers back from
//! other screens (Android's file picker hands its choice to Java code). It
//! asks for storage access instead and lists folders itself (see
//! [`crate::picker`]).

use jni::JNIEnv;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use std::sync::OnceLock;

type R<T> = jni::errors::Result<T>;

static APP: OnceLock<slint::android::AndroidApp> = OnceLock::new();

/// Remembers the running app, for the calls below.
pub fn set_app(app: &slint::android::AndroidApp) {
    let _ = APP.set(app.clone());
}

fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> R<T>) -> Option<T> {
    let app = APP.get()?;
    // SAFETY: both pointers come from the running activity and stay valid
    // while it runs; the activity is a global ref we don't own, and a plain
    // JObject never deletes it.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let r = env.with_local_frame(64, |env| f(env, &activity));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    r.map_err(|e| tracing::warn!("android call failed: {e}")).ok()
}

/// The Android version's API level (0 if it can't be read).
pub fn sdk() -> i32 {
    with_env(|env, _| env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()).unwrap_or(0)
}

/// The permissions that let Whisker see the phone's photos, videos and
/// music (Android 13 split them by kind).
fn media_permissions() -> &'static [&'static str] {
    if sdk() >= 33 {
        &["android.permission.READ_MEDIA_IMAGES", "android.permission.READ_MEDIA_VIDEO", "android.permission.READ_MEDIA_AUDIO"]
    } else {
        &["android.permission.READ_EXTERNAL_STORAGE"]
    }
}

/// Whether Whisker may read the phone's photos, videos and music.
pub fn has_media_access() -> bool {
    has_all_files()
        || with_env(|env, activity| {
            for p in media_permissions() {
                let name = env.new_string(p)?;
                let r = env.call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[JValue::Object(&name)])?.i()?;
                if r != 0 {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .unwrap_or(false)
}

/// Asks the user (Android's own question) to let Whisker read the phone's
/// photos, videos and music, unless it may already.
pub fn request_media_access() {
    if has_media_access() {
        return;
    }
    with_env(|env, activity| {
        let perms = media_permissions();
        let array: JObjectArray = env.new_object_array(perms.len() as i32, "java/lang/String", JObject::null())?;
        for (i, p) in perms.iter().enumerate() {
            let s = env.new_string(p)?;
            env.set_object_array_element(&array, i as i32, s)?;
        }
        env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&array), JValue::Int(7)])?;
        Ok(())
    });
}

/// Whether the user let Whisker see every file (Android 11 and later):
/// documents and key files too, not just photos, videos and music.
pub fn has_all_files() -> bool {
    sdk() >= 30 && with_env(|env, _| env.call_static_method("android/os/Environment", "isExternalStorageManager", "()Z", &[])?.z()).unwrap_or(false)
}

/// Opens Android's settings page where the user can let Whisker see every file.
pub fn ask_all_files() {
    with_env(|env, activity| {
        let pkg = env.call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?.l()?;
        let pkg = string(env, pkg)?.unwrap_or_default();
        let uri_text = env.new_string(format!("package:{pkg}"))?;
        let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&uri_text)])?.l()?;
        let action = env.new_string("android.settings.MANAGE_APP_ALL_FILES_ACCESS_PERMISSION")?;
        let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[JValue::Object(&action), JValue::Object(&uri)])?;
        env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
        Ok(())
    });
}

/// The certificate authorities the user added to the phone themself
/// (Settings → Security → Encryption & credentials), as DER bytes.
pub fn user_certificates() -> Vec<Vec<u8>> {
    with_env(|env, _| {
        let kind = env.new_string("AndroidCAStore")?;
        let store = env.call_static_method("java/security/KeyStore", "getInstance", "(Ljava/lang/String;)Ljava/security/KeyStore;", &[JValue::Object(&kind)])?.l()?;
        let null = JObject::null();
        env.call_method(&store, "load", "(Ljava/security/KeyStore$LoadStoreParameter;)V", &[JValue::Object(&null)])?;
        let aliases = env.call_method(&store, "aliases", "()Ljava/util/Enumeration;", &[])?.l()?;
        let mut out = Vec::new();
        while env.call_method(&aliases, "hasMoreElements", "()Z", &[])?.z()? {
            let a = env.call_method(&aliases, "nextElement", "()Ljava/lang/Object;", &[])?.l()?;
            let Some(alias) = string(env, a)? else { continue };
            // "system:…" are the phone's built-in ones; Whisker has its own list.
            if !alias.starts_with("user:") {
                continue;
            }
            let name = env.new_string(&alias)?;
            let cert = env.call_method(&store, "getCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;", &[JValue::Object(&name)])?.l()?;
            if cert.is_null() {
                continue;
            }
            let der = env.call_method(&cert, "getEncoded", "()[B", &[])?.l()?;
            out.push(env.convert_byte_array(jni::objects::JByteArray::from(der))?);
        }
        Ok(out)
    })
    .unwrap_or_default()
}

fn string(env: &mut JNIEnv, o: JObject) -> R<Option<String>> {
    if o.is_null() {
        return Ok(None);
    }
    let s = JString::from(o);
    Ok(Some(env.get_string(&s)?.into()))
}
