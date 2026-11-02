//! Files and text shared to Whisker from other apps.
//!
//! On the desktop they arrive as command-line arguments (the system share
//! box, a file manager's "Open with", or a drop on the launcher). FerroOS
//! hands over a read-only copy that it deletes once read, so everything is
//! read into memory straight away. If Whisker is already open, the new copy
//! passes what it got to it over a socket in the data folder and quits.
//! On Android they come in a share intent (see [`android`]).
//!
//! Whatever arrives goes through [`deliver`] to the handler the app sets
//! with [`on_share`]; it's kept until then.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The largest file Whisker will send (the server would refuse more).
const MAX_FILE: u64 = whisker_core::timeline::MAX_UPLOAD;
/// A text share is a small `.txt` file; anything this size or under is sent
/// as a message instead of as a file.
const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Shared {
    File { name: String, data: Vec<u8> },
    /// Text or a link: sent as a message.
    Text(String),
    /// Over the upload limit (only its name is kept, to say so).
    TooBig(String),
}

impl Shared {
    /// A file, or its text if it's what a "share text" turns into.
    fn file(name: String, data: Vec<u8>) -> Self {
        if data.len() <= MAX_TEXT
            && is_shared_text(&name)
            && let Ok(t) = std::str::from_utf8(&data)
        {
            return Shared::Text(t.trim().to_owned());
        }
        Shared::File { name, data }
    }
}

/// FerroOS names text shares "Shared text.txt" (with a number when there's
/// more than one).
fn is_shared_text(name: &str) -> bool {
    name.starts_with("Shared text") && name.ends_with(".txt")
}

/// What to call a share in the "Share …" dialog's title.
pub fn summary(items: &[Shared]) -> String {
    let files = items.iter().filter(|i| !matches!(i, Shared::Text(_))).count();
    let texts = items.len() - files;
    match (files, texts, items.first()) {
        (1, 0, Some(Shared::File { name, .. } | Shared::TooBig(name))) => name.clone(),
        (0, 1, Some(Shared::Text(t))) if is_link(t) => "a link".into(),
        (0, 1, _) => "some text".into(),
        (f, 0, _) => count(f, "file"),
        (0, t, _) => count(t, "message"),
        (f, t, _) => format!("{} and {}", count(f, "file"), count(t, "message")),
    }
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn is_link(t: &str) -> bool {
    !t.contains(char::is_whitespace) && t.contains("://")
}

/// Reads what was passed on the command line: file paths, `file://` URIs,
/// and links. Options (`--…`) are skipped.
pub fn from_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<Shared> {
    let mut out = Vec::new();
    for a in args {
        let s = a.to_string_lossy();
        if s.starts_with("--") || s.is_empty() {
            continue;
        }
        let path = if let Some(p) = s.strip_prefix("file://") {
            // file://host/path: only local files are readable.
            let p = p.find('/').map_or("", |i| &p[i..]);
            PathBuf::from(unescape(p))
        } else if s.contains("://") || s.starts_with("matrix:") || s.starts_with("mailto:") {
            out.push(Shared::Text(s.into_owned()));
            continue;
        } else {
            PathBuf::from(&a)
        };
        if let Some(item) = read(&path) {
            out.push(item);
        }
    }
    out
}

fn read(path: &Path) -> Option<Shared> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let meta = std::fs::metadata(path).map_err(|e| tracing::warn!(path = %path.display(), "can't share: {e}")).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_FILE {
        return Some(Shared::TooBig(name));
    }
    let data = std::fs::read(path).map_err(|e| tracing::warn!(path = %path.display(), "can't share: {e}")).ok()?;
    Some(Shared::file(name, data))
}

/// `%20` and friends back to bytes (as in a `file://` URI).
fn unescape(s: &str) -> std::ffi::OsString {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStringExt::from_vec(out)
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(&out).into_owned().into()
    }
}

// ---- handing shares to the app ----

type Handler = Box<dyn Fn(Vec<Shared>) + Send>;

/// Shares that came before the app was ready, and the app's handler.
static INBOX: Mutex<(Vec<Shared>, Option<Handler>)> = Mutex::new((Vec::new(), None));

/// Passes shares to the app (kept until it's ready). An empty list asks
/// the app to come to the front.
pub fn deliver(items: Vec<Shared>) {
    let mut inbox = INBOX.lock().unwrap();
    match &inbox.1 {
        Some(f) => f(items),
        None => inbox.0.extend(items),
    }
}

/// Sets what happens to shares, and hands it any that are waiting.
pub fn on_share(f: impl Fn(Vec<Shared>) + Send + 'static) {
    let mut inbox = INBOX.lock().unwrap();
    let waiting = std::mem::take(&mut inbox.0);
    if !waiting.is_empty() {
        f(waiting);
    }
    inbox.1 = Some(Box::new(f));
}

// ---- one Whisker at a time (desktop) ----

/// The socket for Whisker open on `dir`: in the private runtime folder
/// (socket paths are short, data folders can be long), one per data folder.
#[cfg(unix)]
fn socket(dir: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|r| r.is_dir()) {
        Some(run) => {
            let mut h = std::hash::DefaultHasher::new();
            dir.hash(&mut h);
            run.join(format!("whisker-{:016x}.sock", h.finish()))
        }
        None => dir.join("whisker.sock"),
    }
}

/// If Whisker is already open on `dir`, gives it `items` (or just brings it
/// forward) and returns true: this copy should then quit. An open Whisker
/// from an older build closes instead, and this one takes over (false).
#[cfg(unix)]
pub fn hand_off(dir: &Path, items: &[Shared]) -> bool {
    use std::io::{Read, Write};
    let path = socket(dir);
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(&path) else { return false };
    let mut msg = BUILD.to_vec();
    let me = build_id();
    msg.extend((me.len() as u32).to_le_bytes());
    msg.extend(me.as_bytes());
    msg.extend(encode(items));
    if let Err(e) = s.write_all(&msg) {
        tracing::warn!("couldn't pass the share to the open Whisker: {e}");
        return true;
    }
    let _ = s.shutdown(std::net::Shutdown::Write);
    // It answers "new" when it's an older build and is closing.
    let mut answer = Vec::new();
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = s.read_to_end(&mut answer);
    if answer != b"new" {
        return true;
    }
    // Wait for it to lock up and let go of the socket.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while path.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Which build of Whisker this is: the program file's size and time.
fn build_id() -> String {
    let meta = std::env::current_exe().and_then(std::fs::metadata);
    match meta {
        Ok(m) => format!("{}-{:?}", m.len(), m.modified().ok()),
        Err(_) => env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Before a share: who's sending it ([`build_id`]).
const BUILD: &[u8; 4] = b"WSKB";

/// The sender's build and the rest of the message.
fn split_build(b: &[u8]) -> (Option<String>, &[u8]) {
    let parse = || {
        let rest = b.strip_prefix(BUILD.as_slice())?;
        let n = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let id = std::str::from_utf8(rest.get(4..4 + n)?).ok()?.to_owned();
        Some((id, rest.get(4 + n..)?))
    };
    match parse() {
        Some((id, rest)) => (Some(id), rest),
        None => (None, b),
    }
}

#[cfg(not(unix))]
pub fn hand_off(_dir: &Path, _items: &[Shared]) -> bool {
    false
}

/// Listens for later copies of Whisker handing over shares. A copy from a
/// newer build gets told so, and `on_newer` runs (to close this one).
#[cfg(unix)]
pub fn listen(dir: &Path, on_newer: impl Fn() + Send + 'static) {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let path = socket(dir);
    if let Err(e) = whisker_core::stores::make_private_dir(dir) {
        return tracing::warn!("can't take shares from other apps: {e:#}");
    }
    // Left over from a copy that didn't quit cleanly (hand_off found no one).
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return tracing::warn!("can't take shares from other apps: {e}"),
    };
    whisker_core::vault::owner_only(&path, false);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut buf = Vec::new();
            // Room for a handful of files at the upload limit, no more.
            let mut conn = conn;
            if (&mut conn).take(8 * (MAX_FILE + 4096)).read_to_end(&mut buf).is_ok() {
                let (sender, rest) = split_build(&buf);
                if sender.is_some_and(|b| b != build_id()) {
                    use std::io::Write;
                    let _ = conn.write_all(b"new");
                    drop(conn);
                    on_newer();
                    return;
                }
                deliver(decode(rest).unwrap_or_default());
            }
        }
    });
}

#[cfg(not(unix))]
pub fn listen(_dir: &Path, _on_newer: impl Fn() + Send + 'static) {}

/// Removes the socket when Whisker closes.
pub fn stop_listening(dir: &Path) {
    #[cfg(unix)]
    let _ = std::fs::remove_file(socket(dir));
    #[cfg(not(unix))]
    let _ = dir;
}

const MAGIC: &[u8; 4] = b"WSK1";

fn encode(items: &[Shared]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for i in items {
        let (kind, name, data): (u8, &str, &[u8]) = match i {
            Shared::File { name, data } => (0, name, data),
            Shared::Text(t) => (1, "", t.as_bytes()),
            Shared::TooBig(name) => (2, name, &[]),
        };
        out.push(kind);
        out.extend((name.len() as u32).to_le_bytes());
        out.extend(name.as_bytes());
        out.extend((data.len() as u64).to_le_bytes());
        out.extend(data);
    }
    out
}

fn decode(mut b: &[u8]) -> Option<Vec<Shared>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (b.get(..n)?, b.get(n..)?);
        *b = rest;
        Some(head)
    }
    if take(&mut b, 4)? != MAGIC {
        return None;
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let kind = take(&mut b, 1)?[0];
        let n = u32::from_le_bytes(take(&mut b, 4)?.try_into().ok()?) as usize;
        let name = String::from_utf8_lossy(take(&mut b, n)?).into_owned();
        let n = usize::try_from(u64::from_le_bytes(take(&mut b, 8)?.try_into().ok()?)).ok()?;
        let data = take(&mut b, n)?;
        out.push(match kind {
            0 => Shared::File { name, data: data.to_vec() },
            1 => Shared::Text(String::from_utf8_lossy(data).into_owned()),
            _ => Shared::TooBig(name),
        });
    }
    Some(out)
}

// ---- Android ----

/// Reading a share intent: `ACTION_SEND` / `ACTION_SEND_MULTIPLE` with
/// files (content:// URIs, read through the content resolver) or text.
#[cfg(target_os = "android")]
pub mod android {
    use super::Shared;
    use jni::JNIEnv;
    use jni::objects::{JObject, JString, JValue};

    type R<T> = jni::errors::Result<T>;

    /// What the activity's intent shares (nothing for a normal launch).
    pub fn intent_shares(app: &slint::android::AndroidApp) -> Vec<Shared> {
        with_env(app, |env, activity| {
            let intent = env.call_method(activity, "getIntent", "()Landroid/content/Intent;", &[])?.l()?;
            if intent.is_null() {
                return Ok(Vec::new());
            }
            let action = env.call_method(&intent, "getAction", "()Ljava/lang/String;", &[])?.l()?;
            let action = string(env, action)?;
            if !matches!(action.as_deref(), Some("android.intent.action.SEND" | "android.intent.action.SEND_MULTIPLE")) {
                return Ok(Vec::new());
            }
            read_intent(env, activity, &intent)
        })
        .unwrap_or_default()
    }

    /// The id of the task this activity is in.
    pub fn task_id(app: &slint::android::AndroidApp) -> Option<i32> {
        with_env(app, |env, activity| env.call_method(activity, "getTaskId", "()I", &[])?.i())
    }

    /// Brings Whisker's task (where its window is) to the front.
    pub fn bring_to_front(app: &slint::android::AndroidApp, task: i32) {
        with_env(app, |env, activity| {
            let name = env.new_string("activity")?;
            let am = env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?.l()?;
            env.call_method(&am, "moveTaskToFront", "(II)V", &[JValue::Int(task), JValue::Int(0)])?;
            Ok(())
        });
    }

    fn with_env<T>(app: &slint::android::AndroidApp, f: impl FnOnce(&mut JNIEnv, &JObject) -> R<T>) -> Option<T> {
        // SAFETY: both pointers come from the running activity and stay
        // valid while it runs; the activity is a global ref we don't own,
        // and a plain JObject never deletes it.
        let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
        let mut env = vm.attach_current_thread().ok()?;
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let r = env.with_local_frame(64, |env| f(env, &activity));
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        r.map_err(|e| tracing::warn!("reading the share failed: {e}")).ok()
    }

    fn read_intent(env: &mut JNIEnv, activity: &JObject, intent: &JObject) -> R<Vec<Shared>> {
        let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
        let mut out = Vec::new();
        // Android copies the shared URIs and text into the clip data.
        let clip = env.call_method(intent, "getClipData", "()Landroid/content/ClipData;", &[])?.l()?;
        if !clip.is_null() {
            let n = env.call_method(&clip, "getItemCount", "()I", &[])?.i()?;
            for i in 0..n {
                let item = env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[JValue::Int(i)])?.l()?;
                let uri = env.call_method(&item, "getUri", "()Landroid/net/Uri;", &[])?.l()?;
                if !uri.is_null() {
                    out.extend(read_uri(env, &resolver, &uri));
                    continue;
                }
                let text = env.call_method(&item, "getText", "()Ljava/lang/CharSequence;", &[])?.l()?;
                if !text.is_null() {
                    let t = env.call_method(&text, "toString", "()Ljava/lang/String;", &[])?.l()?;
                    out.extend(string(env, t)?.filter(|t| !t.trim().is_empty()).map(Shared::Text));
                }
            }
        }
        if out.is_empty() {
            let key = env.new_string("android.intent.extra.STREAM")?;
            let uri = env.call_method(intent, "getParcelableExtra", "(Ljava/lang/String;)Landroid/os/Parcelable;", &[JValue::Object(&key)])?.l()?;
            if !uri.is_null() {
                out.extend(read_uri(env, &resolver, &uri));
            }
            let key = env.new_string("android.intent.extra.TEXT")?;
            let text = env.call_method(intent, "getStringExtra", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&key)])?.l()?;
            out.extend(string(env, text)?.filter(|t| !t.trim().is_empty()).map(Shared::Text));
        }
        Ok(out)
    }

    /// One shared file, read in full. A file that can't be read is skipped.
    fn read_uri(env: &mut JNIEnv, resolver: &JObject, uri: &JObject) -> Option<Shared> {
        let r = (|| -> R<Shared> {
            let name = display_name(env, resolver, uri)?.unwrap_or_else(|| "file".into());
            let stream = env.call_method(resolver, "openInputStream", "(Landroid/net/Uri;)Ljava/io/InputStream;", &[JValue::Object(uri)])?.l()?;
            let buf = env.new_byte_array(256 * 1024)?;
            let mut data = Vec::new();
            loop {
                let n = env.call_method(&stream, "read", "([B)I", &[JValue::Object(&buf)])?.i()?;
                if n < 0 {
                    break;
                }
                if data.len() as u64 + n as u64 > super::MAX_FILE {
                    let _ = env.call_method(&stream, "close", "()V", &[]);
                    return Ok(Shared::TooBig(name));
                }
                let start = data.len();
                data.resize(start + n as usize, 0);
                let dst: &mut [i8] = bytemuck_i8(&mut data[start..]);
                env.get_byte_array_region(&buf, 0, dst)?;
            }
            env.call_method(&stream, "close", "()V", &[])?;
            Ok(Shared::file(name, data))
        })();
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        r.map_err(|e| tracing::warn!("couldn't read a shared file: {e}")).ok()
    }

    /// The file's name as the sharing app gives it.
    fn display_name(env: &mut JNIEnv, resolver: &JObject, uri: &JObject) -> R<Option<String>> {
        let null = JObject::null();
        let cursor = env
            .call_method(
                resolver,
                "query",
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
                &[JValue::Object(uri), JValue::Object(&null), JValue::Object(&null), JValue::Object(&null), JValue::Object(&null)],
            )?
            .l()?;
        let mut name = None;
        if !cursor.is_null() {
            let col = env.new_string("_display_name")?;
            let i = env.call_method(&cursor, "getColumnIndex", "(Ljava/lang/String;)I", &[JValue::Object(&col)])?.i()?;
            if i >= 0 && env.call_method(&cursor, "moveToFirst", "()Z", &[])?.z()? {
                let s = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[JValue::Int(i)])?.l()?;
                name = string(env, s)?;
            }
            env.call_method(&cursor, "close", "()V", &[])?;
        }
        if name.is_none() {
            let s = env.call_method(uri, "getLastPathSegment", "()Ljava/lang/String;", &[])?.l()?;
            name = string(env, s)?;
        }
        Ok(name.filter(|n| !n.is_empty()))
    }

    fn string(env: &mut JNIEnv, o: JObject) -> R<Option<String>> {
        if o.is_null() {
            return Ok(None);
        }
        let s = JString::from(o);
        Ok(Some(env.get_string(&s)?.into()))
    }

    fn bytemuck_i8(b: &mut [u8]) -> &mut [i8] {
        // SAFETY: u8 and i8 have the same size and alignment.
        unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr().cast(), b.len()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_cross_the_socket() {
        let items = vec![
            Shared::File { name: "photo.jpg".into(), data: vec![1, 2, 3] },
            Shared::Text("https://example.com".into()),
            Shared::TooBig("huge.iso".into()),
            Shared::File { name: "empty".into(), data: Vec::new() },
        ];
        assert_eq!(decode(&encode(&items)), Some(items));
        assert_eq!(decode(&encode(&[])), Some(Vec::new()));
        assert_eq!(decode(b"WSK1\0\x09"), None);
        assert_eq!(decode(b"nope"), None);
        assert_eq!(split_build(b"WSKB\x03\0\0\0abcWSK1"), (Some("abc".to_owned()), b"WSK1".as_slice()));
        assert_eq!(split_build(b"WSK1"), (None, b"WSK1".as_slice()));
    }

    #[test]
    fn reads_the_command_line() {
        let dir = std::env::temp_dir().join(format!("whisker-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.png");
        std::fs::write(&file, b"png").unwrap();
        let text = dir.join("Shared text.txt");
        std::fs::write(&text, "hello there\n").unwrap();
        let uri = format!("file://{}", dir.join("a%20b.png").display());
        let got = from_args(["--version".into(), file.clone().into(), uri.into(), text.into(), "https://x.org/a".into(), dir.join("missing").into()]);
        std::fs::remove_dir_all(&dir).unwrap();
        let png = Shared::File { name: "a b.png".into(), data: b"png".to_vec() };
        assert_eq!(got, [png.clone(), png, Shared::Text("hello there".into()), Shared::Text("https://x.org/a".into())]);
    }

    #[test]
    fn titles() {
        let f = |n: &str| Shared::File { name: n.into(), data: Vec::new() };
        assert_eq!(summary(&[f("cat.gif")]), "cat.gif");
        assert_eq!(summary(&[f("a"), f("b")]), "2 files");
        assert_eq!(summary(&[Shared::Text("https://a.b/c".into())]), "a link");
        assert_eq!(summary(&[Shared::Text("hi there".into())]), "some text");
        assert_eq!(summary(&[f("a"), Shared::Text("x".into())]), "1 file and 1 message");
    }
}
