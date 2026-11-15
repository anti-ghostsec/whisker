//! Choosing files on Android, inside Whisker: a list of the phone's folders
//! and files. Android's own picker hands its answer to Java code, which
//! Whisker doesn't have, so Whisker asks for storage access instead (see
//! [`crate::droid`]) and lists the folders itself.
//!
//! With the usual access Android shows only photos, videos and music here;
//! other files (key files, documents) need "all files" access, which the
//! list offers to turn on.

use crate::{AppWindow, PickEntry};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The phone's shared storage (where Android says it is).
fn root() -> PathBuf {
    crate::droid::storage_root()
}
/// Places offered at the top: (label, folder under [`root`]).
const PLACES: [(&str, &str); 8] = [
    ("Download", "Download"),
    ("Camera", "DCIM/Camera"),
    ("Pictures", "Pictures"),
    ("Movies", "Movies"),
    ("Music", "Music"),
    ("Documents", "Documents"),
    ("Saved by Whisker", "Download/Whisker"),
    ("Phone storage", ""),
];

struct State {
    dir: PathBuf,
    multi: bool,
    /// Extensions shown (lower case); empty: everything.
    exts: Vec<String>,
    picked: Vec<PathBuf>,
    /// What's listed: (path, is a folder).
    entries: Vec<(PathBuf, bool)>,
    done: Option<tokio::sync::oneshot::Sender<Vec<PathBuf>>>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static UI: OnceLock<Weak<AppWindow>> = OnceLock::new();

/// Wires the list's buttons.
pub fn install(ui: &AppWindow) {
    let _ = UI.set(ui.as_weak());
    let places: Vec<slint::SharedString> = PLACES.iter().map(|(l, _)| (*l).into()).collect();
    ui.set_picker_places(ModelRc::new(VecModel::from(places)));
    let w = ui.as_weak();
    ui.on_picker_tap(move |i| {
        if let Some(u) = w.upgrade() {
            tap(&u, i as usize);
        }
    });
    let w = ui.as_weak();
    ui.on_picker_place(move |i| {
        let Some((_, sub)) = PLACES.get(i as usize) else { return };
        if let Some(st) = STATE.lock().unwrap().as_mut() {
            st.dir = root().join(sub);
        }
        if let Some(u) = w.upgrade() {
            show(&u, true);
        }
    });
    let w = ui.as_weak();
    ui.on_picker_refresh(move || {
        if let Some(u) = w.upgrade() {
            show(&u, false);
        }
    });
    let w = ui.as_weak();
    ui.on_picker_finish(move |ok| {
        let picked = STATE.lock().unwrap().take().map(|mut st| {
            let picked = if ok { std::mem::take(&mut st.picked) } else { Vec::new() };
            if let Some(tx) = st.done.take() {
                let _ = tx.send(picked);
            }
        });
        let _ = picked;
        if let Some(u) = w.upgrade() {
            u.set_picker_open(false);
        }
    });
    ui.on_picker_all_files(crate::droid::ask_all_files);
}

/// Lets the user choose files (one, or with `multi` several) with these
/// extensions (any, when empty). Empty when they cancel.
pub async fn pick(title: &str, exts: &[&str], multi: bool) -> Vec<PathBuf> {
    crate::droid::request_media_access();
    let Some(ui) = UI.get().cloned() else { return Vec::new() };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let start = root().join("Download");
    // One open at a time: a list still open is cancelled.
    if let Some(mut old) = STATE.lock().unwrap().replace(State {
        dir: if start.is_dir() { start } else { root() },
        multi,
        exts: exts.iter().map(|e| e.to_lowercase()).collect(),
        picked: Vec::new(),
        entries: Vec::new(),
        done: Some(tx),
    }) && let Some(tx) = old.done.take()
    {
        let _ = tx.send(Vec::new());
    }
    let title = title.to_owned();
    let _ = ui.upgrade_in_event_loop(move |u| {
        u.set_picker_title(title.into());
        u.set_picker_multi(multi);
        show(&u, true);
        u.set_picker_open(true);
    });
    rx.await.unwrap_or_default()
}

fn tap(u: &AppWindow, i: usize) {
    let finish = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else { return };
        let Some((path, dir)) = st.entries.get(i).cloned() else { return };
        if dir {
            st.dir = path;
            false
        } else if st.multi {
            match st.picked.iter().position(|p| p == &path) {
                Some(at) => {
                    st.picked.remove(at);
                }
                None => st.picked.push(path),
            }
            false
        } else {
            st.picked = vec![path];
            true
        }
    };
    if finish {
        u.invoke_picker_finish(true);
    } else {
        show(u, true);
    }
}

/// Lists the folder. Without `always`, only when what's in it changed
/// (it's looked at again every moment, for when access is granted).
fn show(u: &AppWindow, always: bool) {
    let mut guard = STATE.lock().unwrap();
    let Some(st) = guard.as_mut() else { return };
    let entries = list(&st.dir, &st.exts);
    if !always && entries.iter().map(|(p, d, _)| (p, d)).eq(st.entries.iter().map(|(p, d)| (p, d))) {
        return;
    }
    let mut rows: Vec<PickEntry> = Vec::new();
    let mut kept: Vec<(PathBuf, bool)> = Vec::new();
    let top = root();
    if st.dir != top
        && let Some(up) = st.dir.parent()
    {
        rows.push(PickEntry { name: "⬆  Up a folder".into(), info: "".into(), dir: true, picked: false });
        kept.push((up.to_owned(), true));
    }
    for (path, dir, info) in entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        rows.push(PickEntry { name: if dir { format!("📁  {name}") } else { name }.into(), info: info.into(), dir, picked: st.picked.contains(&path) });
        kept.push((path, dir));
    }
    st.entries = kept;
    let shown = st.dir.strip_prefix(&top).map(|p| format!("Phone storage/{}", p.display())).unwrap_or_else(|_| st.dir.display().to_string());
    u.set_picker_dir(shown.trim_end_matches('/').into());
    u.set_picker_count(st.picked.len() as i32);
    u.set_picker_entries(ModelRc::new(VecModel::from(rows)));
    u.set_picker_access(if crate::droid::has_all_files() { 2 } else if crate::droid::has_media_access() { 1 } else { 0 });
}

/// A folder's subfolders (by name) then files (newest first), skipping
/// hidden ones: (path, is a folder, size and date).
fn list(dir: &Path, exts: &[String]) -> Vec<(PathBuf, bool, String)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in read.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            dirs.push((name.to_lowercase(), e.path()));
        } else {
            let ext = Path::new(&name).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !exts.is_empty() && !exts.contains(&ext) {
                continue;
            }
            let modified = meta.modified().ok();
            files.push((modified, e.path(), meta.len()));
        }
    }
    dirs.sort();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<(PathBuf, bool, String)> = dirs.into_iter().map(|(_, p)| (p, true, String::new())).collect();
    out.extend(files.into_iter().map(|(m, p, len)| {
        let when = m.map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%-d %b %Y %H:%M").to_string()).unwrap_or_default();
        (p, false, format!("{}  ·  {when}", size(len)))
    }));
    out
}

fn size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / f64::from(1u32 << 30)),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / f64::from(1u32 << 20)),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} bytes"),
    }
}
