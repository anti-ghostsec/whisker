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
                crate::vault::owner_only(&c, true);
                return c;
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
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join(LOCK)).ok()?;
    lock.try_lock().ok()?;
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        if p.file_name().is_some_and(|n| n == LOCK) {
            continue;
        }
        // Not through a symlink: only what's in the folder itself goes.
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
    Some(lock)
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
