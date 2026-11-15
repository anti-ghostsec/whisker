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
use std::path::{Path, PathBuf};
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

/// The phone's shared storage, as Android reports it: not always
/// `/storage/emulated/0` (a work profile or Samsung's Secure Folder is
/// another user, with its own number).
pub fn storage_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        with_env(|env, _| {
            let dir = env.call_static_method("android/os/Environment", "getExternalStorageDirectory", "()Ljava/io/File;", &[])?.l()?;
            let path = env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
            string(env, path)
        })
        .flatten()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/storage/emulated/0"))
    })
    .clone()
}

/// The permissions that let Whisker see the phone's photos, videos and
/// music (Android 13 split them by kind), and on Android 10 and older
/// write to its Download folder.
fn media_permissions() -> &'static [&'static str] {
    match sdk() {
        33.. => &["android.permission.READ_MEDIA_IMAGES", "android.permission.READ_MEDIA_VIDEO", "android.permission.READ_MEDIA_AUDIO"],
        30..=32 => &["android.permission.READ_EXTERNAL_STORAGE"],
        _ => &["android.permission.READ_EXTERNAL_STORAGE", "android.permission.WRITE_EXTERNAL_STORAGE"],
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

/// Puts Whisker in the background, as Home does (Back with nothing open):
/// it keeps running and stays unlocked, where closing it would lock it.
pub fn to_background() {
    with_env(|env, activity| {
        env.call_method(activity, "moveTaskToBack", "(Z)Z", &[JValue::Bool(1)])?;
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

/// A file saved to the phone's Download/Whisker folder.
pub struct Saved {
    /// Where to find it, for people ("Download/Whisker/a.jpg").
    pub shown: String,
    /// Android's address for it (Android 10 and later), for sharing.
    uri: Option<String>,
    /// Its path (older Android).
    path: Option<PathBuf>,
}

/// Moves a file Whisker wrote to its own folder (`staged`) into the phone's
/// Download/Whisker folder, keeping its name (or "name (2)" when taken).
///
/// Android 10 and later take it through their media store, which needs no
/// permission and finds the right folder wherever the storage is (Secure
/// Folder, work profiles); older Android gets it written there directly.
pub fn publish(staged: &Path, mime: &str) -> Result<Saved, String> {
    let name = staged.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let r = (|| -> Result<Saved, String> {
        if sdk() >= 29
            && let Some((uri, fd)) = media_store_new(&name, mime)
        {
            use std::os::fd::FromRawFd;
            // SAFETY: the descriptor was just detached from Java for us to own.
            let mut out = unsafe { std::fs::File::from_raw_fd(fd) };
            let copied = std::fs::File::open(staged).and_then(|mut src| std::io::copy(&mut src, &mut out)).and_then(|_| out.sync_all());
            drop(out);
            return match copied {
                Ok(()) => {
                    let shown = media_store_name(&uri).unwrap_or(name);
                    Ok(Saved { shown: format!("Download/Whisker/{shown}"), uri: Some(uri), path: None })
                }
                Err(e) => {
                    media_store_delete(&uri);
                    Err(format!("couldn't write it: {e}"))
                }
            };
        }
        let dest = download_path(&name).ok_or("the phone's Download folder can't be written to (allow Whisker to see the phone's files)")?;
        std::fs::copy(staged, &dest).map_err(|e| format!("couldn't write it: {e}"))?;
        let shown = dest.strip_prefix(storage_root()).map(|p| p.display().to_string()).unwrap_or_else(|_| dest.display().to_string());
        Ok(Saved { shown, uri: None, path: Some(dest) })
    })();
    let _ = std::fs::remove_file(staged);
    r
}

/// A free name for `name` in Download/Whisker ("a.mp4", else "a (2).mp4"…),
/// or `None` when that folder can't be made.
fn download_path(name: &str) -> Option<PathBuf> {
    let dir = storage_root().join("Download").join("Whisker");
    std::fs::create_dir_all(&dir).ok()?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((a, b)) if !a.is_empty() => (a.to_owned(), format!(".{b}")),
        _ => (name.to_owned(), String::new()),
    };
    (1..1000).map(|n| if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) }).find(|p| !p.exists())
}

/// A new, empty file in the media store's Download/Whisker: its address
/// and a descriptor to write it through.
fn media_store_new(name: &str, mime: &str) -> Option<(String, i32)> {
    with_env(|env, activity| {
        let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
        let values = env.new_object("android/content/ContentValues", "()V", &[])?;
        for (k, v) in [("_display_name", name), ("mime_type", mime), ("relative_path", "Download/Whisker")] {
            let (k, v) = (env.new_string(k)?, env.new_string(v)?);
            env.call_method(&values, "put", "(Ljava/lang/String;Ljava/lang/String;)V", &[JValue::Object(&k), JValue::Object(&v)])?;
        }
        let downloads = env.get_static_field("android/provider/MediaStore$Downloads", "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?.l()?;
        let uri = env.call_method(&resolver, "insert", "(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;", &[JValue::Object(&downloads), JValue::Object(&values)])?.l()?;
        if uri.is_null() {
            return Ok(None);
        }
        let text = env.call_method(&uri, "toString", "()Ljava/lang/String;", &[])?.l()?;
        let Some(text) = string(env, text)? else { return Ok(None) };
        let mode = env.new_string("w")?;
        let opened = env
            .call_method(&resolver, "openFileDescriptor", "(Landroid/net/Uri;Ljava/lang/String;)Landroid/os/ParcelFileDescriptor;", &[JValue::Object(&uri), JValue::Object(&mode)])
            .and_then(|v| v.l());
        let pfd = match opened {
            Ok(p) if !p.is_null() => p,
            _ => {
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
                let null = JObject::null();
                let _ = env.call_method(&resolver, "delete", "(Landroid/net/Uri;Ljava/lang/String;[Ljava/lang/String;)I", &[JValue::Object(&uri), JValue::Object(&null), JValue::Object(&null)]);
                return Ok(None);
            }
        };
        let fd = env.call_method(&pfd, "detachFd", "()I", &[])?.i()?;
        Ok(Some((text, fd)))
    })
    .flatten()
}

/// The name the media store gave a file (it adds " (1)" when one is taken).
fn media_store_name(uri: &str) -> Option<String> {
    with_env(|env, activity| {
        let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
        let text = env.new_string(uri)?;
        let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?.l()?;
        let column = env.new_string("_display_name")?;
        let columns = env.new_object_array(1, "java/lang/String", &column)?;
        let null = JObject::null();
        let cursor = env
            .call_method(
                &resolver,
                "query",
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
                &[JValue::Object(&uri), JValue::Object(&columns), JValue::Object(&null), JValue::Object(&null), JValue::Object(&null)],
            )?
            .l()?;
        if cursor.is_null() {
            return Ok(None);
        }
        let name = if env.call_method(&cursor, "moveToFirst", "()Z", &[])?.z()? {
            let s = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[JValue::Int(0)])?.l()?;
            string(env, s)?
        } else {
            None
        };
        env.call_method(&cursor, "close", "()V", &[])?;
        Ok(name)
    })
    .flatten()
}

fn media_store_delete(uri: &str) {
    with_env(|env, activity| {
        let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
        let text = env.new_string(uri)?;
        let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?.l()?;
        let null = JObject::null();
        env.call_method(&resolver, "delete", "(Landroid/net/Uri;Ljava/lang/String;[Ljava/lang/String;)I", &[JValue::Object(&uri), JValue::Object(&null), JValue::Object(&null)])?;
        Ok(())
    });
}

/// Opens Android's Share menu for a saved file, to send it to another app.
pub fn share(file: &Saved, mime: &str) -> bool {
    with_env(|env, activity| {
        let uri = if let Some(u) = &file.uri {
            let text = env.new_string(u)?;
            env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?.l()?
        } else if let Some(p) = &file.path {
            // Older Android: a plain file address, which Android only lets
            // apps hand on once its check for them is off.
            let builder = env.new_object("android/os/StrictMode$VmPolicy$Builder", "()V", &[])?;
            let policy = env.call_method(&builder, "build", "()Landroid/os/StrictMode$VmPolicy;", &[])?.l()?;
            env.call_static_method("android/os/StrictMode", "setVmPolicy", "(Landroid/os/StrictMode$VmPolicy;)V", &[JValue::Object(&policy)])?;
            let path = env.new_string(p.to_string_lossy())?;
            let f = env.new_object("java/io/File", "(Ljava/lang/String;)V", &[JValue::Object(&path)])?;
            env.call_static_method("android/net/Uri", "fromFile", "(Ljava/io/File;)Landroid/net/Uri;", &[JValue::Object(&f)])?.l()?
        } else {
            return Ok(false);
        };
        let action = env.new_string("android.intent.action.SEND")?;
        let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
        let kind = env.new_string(mime)?;
        env.call_method(&intent, "setType", "(Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&kind)])?;
        let extra = env.new_string("android.intent.extra.STREAM")?;
        env.call_method(&intent, "putExtra", "(Ljava/lang/String;Landroid/os/Parcelable;)Landroid/content/Intent;", &[JValue::Object(&extra), JValue::Object(&uri)])?;
        // Lets the app it goes to read it.
        let label = env.new_string("")?;
        let clip = env.call_static_method("android/content/ClipData", "newRawUri", "(Ljava/lang/CharSequence;Landroid/net/Uri;)Landroid/content/ClipData;", &[JValue::Object(&label), JValue::Object(&uri)])?.l()?;
        env.call_method(&intent, "setClipData", "(Landroid/content/ClipData;)V", &[JValue::Object(&clip)])?;
        env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(1)])?;
        let title = env.new_string("Share")?;
        let chooser = env.call_static_method("android/content/Intent", "createChooser", "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;", &[JValue::Object(&intent), JValue::Object(&title)])?.l()?;
        env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&chooser)])?;
        Ok(true)
    })
    .unwrap_or(false)
}

/// Where files being saved are written first (Whisker's own folder).
pub fn staging_dir() -> Option<PathBuf> {
    let dir = APP.get()?.internal_data_path()?.join("saving");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn string(env: &mut JNIEnv, o: JObject) -> R<Option<String>> {
    if o.is_null() {
        return Ok(None);
    }
    let s = JString::from(o);
    Ok(Some(env.get_string(&s)?.into()))
}
