//! Where accounts' local Matrix stores live, and how they're kept.
//!
//! A per-account folder on disk would show how many accounts (and so how
//! many hidden vaults) a device holds. Instead, while a vault is unlocked
//! its accounts' stores run from a RAM-backed folder, and the parts worth
//! keeping (the state and crypto databases) are packed into the vault file
//! itself as attachments. Caches (event cache, media) stay in RAM only.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The databases worth keeping: room state and encryption keys.
const KEPT: [&str; 2] = ["matrix-sdk-state.sqlite3", KEYS_DB];

/// The database with the account's encryption keys.
pub const KEYS_DB: &str = "matrix-sdk-crypto.sqlite3";

/// Root for running stores. On Linux a tmpfs (`$XDG_RUNTIME_DIR`, else
/// `/dev/shm`), so nothing reaches the disk. Elsewhere a folder in the data
/// directory, wiped on lock (Android keeps it app-private; on Windows it is
/// on disk while unlocked).
pub fn runtime_root(data_dir: &Path) -> PathBuf {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let user = std::env::var("USER").unwrap_or_default();
        let candidates = [
            std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("whisker")),
            Some(PathBuf::from("/dev/shm").join(format!("whisker-{user}"))),
        ];
        for c in candidates.into_iter().flatten() {
            if c.parent().is_some_and(Path::is_dir) && std::fs::create_dir_all(&c).is_ok() {
                // A link planted there by someone else isn't followed (nor
                // its target's permissions changed).
                if !std::fs::symlink_metadata(&c).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                crate::vault::owner_only(&c, true);
                // `/dev/shm` is shared: someone else may have made "our"
                // folder first. Then it isn't used (the next place is).
                if ours(&c) {
                    return c;
                }
            }
        }
    }
    data_dir.join("run")
}

/// Claims the running-stores folder for this process and empties it of
/// what an earlier run left behind (after a crash, or a kill): a folder per
/// account there would count the accounts of every vault that was open,
/// hidden ones included, for as long as it stayed. What's lost is what
/// changed since the last snapshot into the vault, as after a power cut.
///
/// The returned file holds a lock for as long as it's kept: a second
/// Whisker started meanwhile doesn't get it, and leaves the folder alone.
pub fn claim(root: &Path) -> Option<std::fs::File> {
    make_private_dir(root).ok()?;
    if !ours(root) {
        return None;
    }
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join(LOCK)).ok()?;
    lock.try_lock().ok()?;
    // Once per process: what's there at a later claim in the same process
    // (another manager, as tests start one each) is stores this process is
    // running, not an earlier run's leftovers. Wiping them zeroed databases
    // still in use.
    {
        static WIPED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
        let mut wiped = WIPED.lock().unwrap_or_else(|e| e.into_inner());
        if wiped.iter().any(|w| w == root) {
            return Some(lock);
        }
        wiped.push(root.to_path_buf());
    }
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == LOCK) {
            continue;
        }
        // Not through a symlink: only what's in the folder itself goes.
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            shred_dir(&p);
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
    Some(lock)
}

/// Overwrites every file in `dir` with zeros, then removes it. On a disk
/// (Windows, or a Linux without a RAM folder) a plain delete leaves the
/// data for undelete tools; this leaves zeros. (An SSD may still keep old
/// copies of blocks: full-disk encryption is the real answer there.)
pub fn shred_dir(dir: &Path) {
    use std::io::Write;
    fn walk(dir: &Path) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&p),
                Ok(t) if t.is_file() => {
                    if let Ok(len) = e.metadata().map(|m| m.len())
                        && let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&p)
                    {
                        let zeros = vec![0u8; 1 << 16];
                        let mut left = len;
                        while left > 0 {
                            let n = left.min(zeros.len() as u64) as usize;
                            if f.write_all(&zeros[..n]).is_err() {
                                break;
                            }
                            left -= n as u64;
                        }
                        let _ = f.sync_all();
                    }
                }
                _ => {}
            }
        }
    }
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        walk(dir);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Whether `dir` is a real folder (not a link to one) that belongs to this
/// user and that nobody else can get into. In a shared place another user
/// can create the folder we'd use before we do, and then watch or swap what
/// goes into it, or point it somewhere for the startup wipe to empty.
#[cfg(unix)]
fn ours(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::symlink_metadata(dir) else { return false };
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return false;
    }
    // A file made in it belongs to us: its owner must be the folder's too.
    // (`create_new` refuses a name that's already there, links included.)
    let probe = dir.join(format!(".owner-check-{}", std::process::id()));
    let Ok(file) = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) else { return false };
    let mine = file.metadata().map(|m| m.uid()).ok();
    drop(file);
    let _ = std::fs::remove_file(&probe);
    mine == Some(meta.uid())
}

#[cfg(not(unix))]
fn ours(_dir: &Path) -> bool {
    true
}

/// The file in the running-stores folder whose lock says a Whisker is using it.
const LOCK: &str = "lock";

/// Takes a consistent copy of the kept databases in `store` (safe while
/// the client is running) and packs them, compressed.
pub fn snapshot(store: &Path) -> Result<Vec<u8>> {
    let mut parts = Vec::new();
    for name in KEPT {
        let db = store.join(name);
        if !db.exists() {
            continue;
        }
        let copy = store.join(format!("{name}.snapshot"));
        let _ = std::fs::remove_file(&copy);
        let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {name}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute("VACUUM INTO ?1", [copy.to_string_lossy()]).with_context(|| format!("copying {name}"))?;
        drop(conn);
        let bytes = std::fs::read(&copy);
        // A decrypted copy: overwritten, not just unlinked.
        wipe_file(&copy);
        parts.push((name, bytes?));
    }
    let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(3));
    z.write_all(&(parts.len() as u32).to_le_bytes())?;
    for (name, bytes) in &parts {
        z.write_all(&(name.len() as u16).to_le_bytes())?;
        z.write_all(name.as_bytes())?;
        z.write_all(&(bytes.len() as u64).to_le_bytes())?;
        z.write_all(bytes)?;
    }
    Ok(z.finish()?)
}

/// Unpacks a [`snapshot`] into `store`.
pub fn unpack(packed: &[u8], store: &Path) -> Result<()> {
    let mut raw = Vec::new();
    flate2::read::DeflateDecoder::new(packed).read_to_end(&mut raw).context("damaged store snapshot")?;
    let mut r = &raw[..];
    let mut take = |n: usize| -> Result<&[u8]> {
        if n > r.len() {
            bail!("damaged store snapshot");
        }
        let (a, b) = r.split_at(n);
        r = b;
        Ok(a)
    };
    make_private_dir(store)?;
    let count = u32::from_le_bytes(take(4)?.try_into()?);
    for _ in 0..count {
        let n = u16::from_le_bytes(take(2)?.try_into()?) as usize;
        let name = std::str::from_utf8(take(n)?)?.to_owned();
        let len = u64::from_le_bytes(take(8)?.try_into()?) as usize;
        let bytes = take(len)?;
        if !KEPT.contains(&name.as_str()) {
            bail!("unexpected file {name:?} in store snapshot");
        }
        write_private(&store.join(&name), bytes)?;
    }
    Ok(())
}

/// The media cache's database, in an account's cache folder.
pub const MEDIA_DB: &str = "matrix-sdk-media.sqlite3";

/// A consistent copy of the media cache in `cache` (its values are already
/// encrypted with the store passphrase, so it isn't compressed), or `None`
/// when there's none.
pub fn snapshot_media(cache: &Path) -> Result<Option<Vec<u8>>> {
    let db = cache.join(MEDIA_DB);
    if !db.exists() {
        return Ok(None);
    }
    let copy = cache.join(format!("{MEDIA_DB}.snapshot"));
    let _ = std::fs::remove_file(&copy);
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).context("opening the media cache")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute("VACUUM INTO ?1", [copy.to_string_lossy()]).context("copying the media cache")?;
    drop(conn);
    let bytes = std::fs::read(&copy);
    wipe_file(&copy);
    Ok(Some(bytes?))
}

/// Puts a [`snapshot_media`] back as the media cache in `cache`.
pub fn unpack_media(bytes: &[u8], cache: &Path) -> Result<()> {
    make_private_dir(cache)?;
    write_private(&cache.join(MEDIA_DB), bytes)?;
    Ok(())
}

/// Copies the kept databases (with their write-ahead logs) from a store
/// folder made by an older version.
pub fn copy_legacy(legacy: &Path, store: &Path) -> Result<()> {
    make_private_dir(store)?;
    for name in KEPT {
        for suffix in ["", "-wal"] {
            let from = legacy.join(format!("{name}{suffix}"));
            if from.exists() {
                write_private(&store.join(format!("{name}{suffix}")), &std::fs::read(&from)?)?;
            }
        }
    }
    Ok(())
}

/// Whether `store` holds a running store.
pub fn has_store(store: &Path) -> bool {
    KEPT.iter().any(|n| store.join(n).exists())
}

/// Where Whisker keeps its data unless told otherwise. On Windows that's
/// the *local* app data folder: the roaming one is copied to other
/// computers and servers by domain roaming profiles. Data an older version
/// left in the roaming folder is moved over once.
pub fn default_data_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "anti-ghostsec", "Whisker")?;
    let (local, roaming) = (dirs.data_local_dir().to_owned(), dirs.data_dir().to_owned());
    if local != roaming && roaming.is_dir() && !local.exists() {
        if let Some(parent) = local.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&roaming, &local) {
            tracing::warn!("couldn't move Whisker's data out of the roaming folder: {e}");
            return Some(roaming);
        }
    }
    Some(local)
}

static SCRATCH_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Sets where [`scratch`] folders go: in the running-stores root, so on
/// Linux in RAM. Called once, by the account manager.
pub fn set_scratch_root(data_dir: &Path) {
    SCRATCH_ROOT.get_or_init(|| runtime_root(data_dir).join("scratch"));
}

/// A folder only this user can open, with a random name, for a file that
/// ffmpeg has to read from disk (a decrypted video, say). It's in RAM on
/// Linux, never in the shared temp folder, and removed when dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `bytes` as `name` inside, readable by this user only.
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.0.join(name);
        write_private(&path, bytes)?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Overwritten first: off Linux this folder is on a disk.
        shred_dir(&self.0);
    }
}

/// Older versions left decrypted videos in the shared temp folder (a
/// preview frame's source, an upload being cleaned) if they were stopped
/// midway. Each one that's this user's is overwritten and removed. Links
/// aren't followed, and nothing else there is touched.
pub fn wipe_old_temp_files() {
    wipe_old_temp_files_in(&std::env::temp_dir());
}

fn wipe_old_temp_files_in(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if !is_mine(&meta) {
            continue;
        }
        if old_poster_name(name) && meta.is_file() {
            wipe_file(&e.path());
        } else if old_upload_name(name) && meta.is_dir() {
            if let Ok(inner) = std::fs::read_dir(e.path()) {
                for f in inner.flatten() {
                    if std::fs::symlink_metadata(f.path()).is_ok_and(|m| m.is_file() && is_mine(&m)) {
                        wipe_file(&f.path());
                    }
                }
            }
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// `whisker-poster-<pid>-<n>`.
fn old_poster_name(name: &str) -> bool {
    name.strip_prefix("whisker-poster-")
        .and_then(|r| r.split_once('-'))
        .is_some_and(|(a, b)| [a, b].iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())))
}

/// `whisker-up-<16 letters and digits>`.
fn old_upload_name(name: &str) -> bool {
    name.strip_prefix("whisker-up-").is_some_and(|r| r.len() == 16 && r.bytes().all(|c| c.is_ascii_alphanumeric()))
}

#[cfg(unix)]
fn is_mine(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and can't fail.
    meta.uid() == unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn is_mine(_: &std::fs::Metadata) -> bool {
    // The user's own temp folder.
    true
}

/// Overwrites a file with zeros (flushed to disk), then removes it.
fn wipe_file(path: &Path) {
    use std::io::Write;
    if let Ok(len) = std::fs::metadata(path).map(|m| m.len())
        && let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let zeros = vec![0u8; 64 * 1024];
        let mut left = len;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            if f.write_all(&zeros[..n]).is_err() {
                break;
            }
            left -= n as u64;
        }
        let _ = f.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

/// A new [`Scratch`] folder; `None` before [`set_scratch_root`], or if it
/// can't be made.
pub fn scratch() -> Option<Scratch> {
    let root = SCRATCH_ROOT.get()?;
    make_private_dir(root).ok()?;
    let dir = root.join(&crate::vault::random_secret()[..16]);
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    // A fresh name: an existing folder (planted, or a clash) isn't used.
    b.create(&dir).ok()?;
    crate::vault::owner_only(&dir, true);
    Some(Scratch(dir))
}

pub fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    crate::vault::owner_only(dir, true);
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    crate::vault::owner_only(path, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A media cache packed for the vault comes back as it was; no cache,
    /// nothing packed.
    #[test]
    fn media_cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("whisker-media-{}", crate::vault::random_secret()));
        make_private_dir(&dir).unwrap();
        assert!(snapshot_media(&dir).unwrap().is_none());
        let conn = rusqlite::Connection::open(dir.join(MEDIA_DB)).unwrap();
        conn.execute_batch("CREATE TABLE media (uri TEXT, data BLOB); INSERT INTO media VALUES ('mxc://a/b', x'0102');").unwrap();
        drop(conn);
        let packed = snapshot_media(&dir).unwrap().unwrap();
        let back = dir.join("back");
        unpack_media(&packed, &back).unwrap();
        let conn = rusqlite::Connection::open(back.join(MEDIA_DB)).unwrap();
        let uri: String = conn.query_row("SELECT uri FROM media", [], |r| r.get(0)).unwrap();
        assert_eq!(uri, "mxc://a/b");
        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Our own private folder passes; a link to it, or one others can
    /// enter, doesn't.
    #[cfg(unix)]
    #[test]
    fn only_our_own_private_folder_is_used() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("whisker-ours-test-{}", std::process::id()));
        let dir = base.join("real");
        make_private_dir(&dir).unwrap();
        assert!(ours(&dir));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "the check leaves nothing behind");
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(!ours(&link));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!ours(&dir));
        assert!(!ours(&base.join("missing")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn claiming_empties_what_a_crash_left() {
        let root = std::env::temp_dir().join(format!("whisker-run-{}", crate::vault::random_secret()));
        let left = root.join("some-account").join("store");
        make_private_dir(&left).unwrap();
        std::fs::write(left.join(KEYS_DB), b"x").unwrap();
        std::fs::write(root.join("stray"), b"x").unwrap();
        let lock = claim(&root).expect("the folder is free");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "only the lock file stays");
        // A second Whisker, while the first runs, takes nothing away.
        make_private_dir(&left).unwrap();
        assert!(claim(&root).is_none());
        assert!(left.exists());
        drop(lock);
        assert!(claim(&root).is_some());
        assert!(!left.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn snapshot_round_trip() {
        let dir = std::env::temp_dir().join(format!("whisker-store-{}", crate::vault::random_secret()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        make_private_dir(&a).unwrap();
        let conn = rusqlite::Connection::open(a.join(KEPT[1])).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE k(v TEXT); INSERT INTO k VALUES ('olm');").unwrap();
        // Taken while the writer still has the database open.
        let packed = snapshot(&a).unwrap();
        drop(conn);
        unpack(&packed, &b).unwrap();
        let got: String = rusqlite::Connection::open(b.join(KEPT[1])).unwrap().query_row("SELECT v FROM k", [], |r| r.get(0)).unwrap();
        assert_eq!(got, "olm");
        assert!(!b.join(KEPT[0]).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn scratch_folders_are_private_and_go_away() {
        let data = std::env::temp_dir().join(format!("whisker-scratch-{}", crate::vault::random_secret()));
        set_scratch_root(&data);
        let s = scratch().expect("a scratch folder");
        let root = SCRATCH_ROOT.get().unwrap();
        assert!(s.path().starts_with(root));
        let f = s.write("video", b"secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o077, 0);
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o077, 0);
        }
        let dir = s.path().to_owned();
        drop(s);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn old_temp_files_are_wiped() {
        let tmp = std::env::temp_dir().join(format!("whisker-oldtmp-{}", crate::vault::random_secret()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("whisker-poster-123-4"), b"video").unwrap();
        let up = tmp.join("whisker-up-abcdEF0123456789");
        std::fs::create_dir_all(&up).unwrap();
        std::fs::write(up.join("in.mp4"), b"video").unwrap();
        // Not ours to touch.
        std::fs::write(tmp.join("whisker-poster-test-1"), b"x").unwrap();
        std::fs::write(tmp.join("other"), b"x").unwrap();
        wipe_old_temp_files_in(&tmp);
        assert!(!tmp.join("whisker-poster-123-4").exists());
        assert!(!up.exists());
        assert!(tmp.join("whisker-poster-test-1").exists() && tmp.join("other").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
