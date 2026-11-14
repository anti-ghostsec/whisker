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
        let bytes = std::fs::read(&copy)?;
        std::fs::remove_file(&copy)?;
        parts.push((name, bytes));
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
    let bytes = std::fs::read(&copy)?;
    std::fs::remove_file(&copy)?;
    Ok(Some(bytes))
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
}
