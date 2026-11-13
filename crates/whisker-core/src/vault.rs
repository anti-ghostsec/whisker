//! The vault: one encrypted file holding every account's session, tags and
//! agent API keys. Nothing about who you are is readable on disk without
//! the passphrase.
//!
//! Format: `MAGIC | salt(16) | m_cost(u32 le) | t_cost(u32 le) | nonce(24) | ciphertext`.
//! The header is authenticated as AAD, so the KDF cost can't be lowered.
//!
//! Plaintext: `0x00 | json_len(u32 le) | json | count(u32 le) | count ×
//! (name_len(u16 le) | name | len(u64 le) | bytes) | zero padding`. The
//! named parts ([`Attachments`]) carry each account's local Matrix store,
//! so no per-account files are left on disk. (Vaults from before this
//! start with `{`: plain JSON, space-padded.)
//!
//! Hidden vaults only stay hidden if nothing on disk tells vaults apart or
//! counts them. So:
//! - file sizes come in fixed steps (powers of two from 64 KiB), and the
//!   folder also holds decoys ([`write_decoy`]): random bytes shaped like a
//!   vault, in sizes from the same steps;
//! - a vault is rewritten in place (same file, same creation time), through
//!   a journal file so a crash can't corrupt it ([`recover`]);
//! - after any write every file gets the same timestamps ([`blend`]).

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"WHSKVLT1";
const HEADER_LEN: usize = 8 + 16 + 4 + 4 + 24;
/// 64 MiB, 3 passes: the same cost Enclave uses.
const M_COST: u32 = 64 * 1024;
const T_COST: u32 = 3;
/// The cost a vault file may ask for: 8 MiB and 1 pass up to 1 GiB and 16
/// passes. It's read from the file before anything proves the file is ours,
/// and every file in the folder is tried, so a planted one must neither
/// weaken the key nor make the app allocate gigabytes.
const M_COST_RANGE: std::ops::RangeInclusive<u32> = 8 * 1024..=1024 * 1024;
const T_COST_RANGE: std::ops::RangeInclusive<u32> = 1..=16;
/// File sizes are powers of two, from 2^16 (64 KiB). Decoys are 2^16..=2^24.
const MIN_SIZE_LOG2: u32 = 16;
const DECOY_MAX_LOG2: u32 = 24;
/// Every vault file's timestamps are set to this (2020-01-01 UTC).
const FIXED_TIME_SECS: u64 = 1_577_836_800;

/// Named binary parts stored inside a vault (each account's store).
pub type Attachments = std::collections::BTreeMap<String, Vec<u8>>;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("wrong passphrase or damaged vault")]
    Decrypt,
    #[error("not a Whisker vault")]
    Format,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("key derivation failed: {0}")]
    Kdf(String),
}

/// An unlocked vault. Holds the derived key, never the passphrase.
#[derive(Clone)]
pub struct Vault {
    path: PathBuf,
    salt: [u8; 16],
    m_cost: u32,
    t_cost: u32,
    key: Zeroizing<[u8; 32]>,
}

impl Vault {
    pub fn exists(path: &Path) -> bool {
        path.is_file()
    }

    /// Creates a new, empty vault keyed by `passphrase`. Call [`Vault::save`] to write it.
    pub fn create(path: impl Into<PathBuf>, passphrase: &str) -> Result<Self, VaultError> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| VaultError::Kdf(e.to_string()))?;
        let key = derive(passphrase, &salt, M_COST, T_COST)?;
        Ok(Self { path: path.into(), salt, m_cost: M_COST, t_cost: T_COST, key })
    }

    /// Opens an existing vault and returns it with its decrypted contents.
    pub fn open<T: DeserializeOwned>(path: impl Into<PathBuf>, passphrase: &str) -> Result<(Self, T), VaultError> {
        Self::open_with(path, passphrase).map(|(v, t, _)| (v, t))
    }

    /// Like [`Vault::open`], also returning the vault's attachments.
    pub fn open_with<T: DeserializeOwned>(
        path: impl Into<PathBuf>,
        passphrase: &str,
    ) -> Result<(Self, T, Attachments), VaultError> {
        Self::open_inner(path.into(), passphrase, true)
    }

    fn open_inner<T: DeserializeOwned>(path: PathBuf, passphrase: &str, journals: bool) -> Result<(Self, T, Attachments), VaultError> {
        let bytes = std::fs::read(&path)?;
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err(VaultError::Format);
        }
        let salt: [u8; 16] = bytes[8..24].try_into().unwrap();
        let m_cost = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let t_cost = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
        let nonce: [u8; 24] = bytes[32..56].try_into().unwrap();
        // Refuse a header that asks for a trivially cheap KDF, or a huge one.
        if !M_COST_RANGE.contains(&m_cost) || !T_COST_RANGE.contains(&t_cost) {
            return Err(VaultError::Format);
        }
        let key = derive(passphrase, &salt, m_cost, t_cost)?;
        // A save cut off by a crash: its journal (which only this key opens)
        // holds the whole new file. Put it in place and read that instead.
        if journals && finish_journal(&path, &key) {
            return Self::open_inner(path, passphrase, false);
        }
        let cipher = XChaCha20Poly1305::new(&Key::from(*key));
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    &XNonce::from(nonce),
                    Payload { msg: &bytes[HEADER_LEN..], aad: &bytes[..HEADER_LEN] },
                )
                .map_err(|_| VaultError::Decrypt)?,
        );
        let (value, attachments) = decode(&plain)?;
        Ok((Self { path, salt, m_cost, t_cost, key }, value, attachments))
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), VaultError> {
        self.save_with(value, &Attachments::new())
    }

    /// Encrypts `value` and `attachments` and rewrites the vault file in place.
    pub fn save_with<T: Serialize>(&self, value: &T, attachments: &Attachments) -> Result<(), VaultError> {
        let out = self.encrypt(value, attachments)?;
        self.write(&out, &[&self.key])
    }

    /// The vault file's bytes for `value` and `attachments`.
    fn encrypt<T: Serialize>(&self, value: &T, attachments: &Attachments) -> Result<Vec<u8>, VaultError> {
        let mut plain = encode(value, attachments)?;
        let padded = file_size(plain.len()) - HEADER_LEN - 16;
        plain.resize(padded, 0);
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| VaultError::Kdf(e.to_string()))?;

        let mut out = Vec::with_capacity(HEADER_LEN + plain.len() + 16);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&self.m_cost.to_le_bytes());
        out.extend_from_slice(&self.t_cost.to_le_bytes());
        out.extend_from_slice(&nonce);

        let cipher = XChaCha20Poly1305::new(&Key::from(*self.key));
        let ct = cipher
            .encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: &out[..HEADER_LEN] })
            .map_err(|_| VaultError::Decrypt)?;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn write(&self, out: &[u8], keys: &[&Zeroizing<[u8; 32]>]) -> Result<(), VaultError> {
        write_in_place(&self.path, out, keys)?;
        if let Some(dir) = self.path.parent() {
            blend(dir)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-keys the vault under a new passphrase (fresh salt).
    pub fn change_passphrase<T: Serialize>(
        &mut self,
        new_passphrase: &str,
        value: &T,
        attachments: &Attachments,
    ) -> Result<(), VaultError> {
        let old_key = self.key.clone();
        let old_salt = self.salt;
        getrandom::fill(&mut self.salt).map_err(|e| VaultError::Kdf(e.to_string()))?;
        self.key = derive(new_passphrase, &self.salt, self.m_cost, self.t_cost)?;
        // The journal is written for both keys: a crash part-way leaves the
        // file's header old or new, and either passphrase must finish it.
        let r = self.encrypt(value, attachments).and_then(|out| self.write(&out, &[&self.key, &old_key]));
        if r.is_err() {
            self.salt = old_salt;
            self.key = old_key;
        }
        r
    }
}

/// The size step a file holding `plain_len` bytes of plaintext is padded to.
fn file_size(plain_len: usize) -> usize {
    (HEADER_LEN + plain_len + 16).next_power_of_two().max(1 << MIN_SIZE_LOG2)
}

/// Whether a vault file's size is one of the steps (older versions padded differently).
pub fn is_stepped(len: u64) -> bool {
    len.is_power_of_two() && len >= 1 << MIN_SIZE_LOG2
}

fn encode<T: Serialize>(value: &T, attachments: &Attachments) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let json = Zeroizing::new(serde_json::to_vec(value)?);
    let mut out = Zeroizing::new(Vec::with_capacity(json.len() + attachments.values().map(|a| a.len() + 64).sum::<usize>() + 16));
    out.push(0);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&(attachments.len() as u32).to_le_bytes());
    for (name, data) in attachments {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    Ok(out)
}

fn decode<T: DeserializeOwned>(plain: &[u8]) -> Result<(T, Attachments), VaultError> {
    if plain.first() != Some(&0) {
        // Older vault: space-padded JSON.
        return Ok((serde_json::from_slice(plain)?, Attachments::new()));
    }
    let mut r = Reader(&plain[1..]);
    let json_len = u32::from_le_bytes(r.take(4)?.try_into().unwrap()) as usize;
    let value = serde_json::from_slice(r.take(json_len)?)?;
    let count = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
    let mut attachments = Attachments::new();
    for _ in 0..count {
        let n = u16::from_le_bytes(r.take(2)?.try_into().unwrap()) as usize;
        let name = String::from_utf8(r.take(n)?.to_vec()).map_err(|_| VaultError::Format)?;
        let len = u64::from_le_bytes(r.take(8)?.try_into().unwrap()) as usize;
        attachments.insert(name, r.take(len)?.to_vec());
    }
    Ok((value, attachments))
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], VaultError> {
        if n > self.0.len() {
            return Err(VaultError::Format);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
}

/// Writes a decoy: a file shaped exactly like a vault (same header and KDF
/// cost, a size from the same steps) whose contents are random, so no
/// passphrase opens it and nothing distinguishes it from a real vault.
pub fn write_decoy(path: &Path) -> Result<(), VaultError> {
    let mut pick = [0u8; 1];
    getrandom::fill(&mut pick).map_err(|e| VaultError::Kdf(e.to_string()))?;
    let size = 1usize << (MIN_SIZE_LOG2 + pick[0] as u32 % (DECOY_MAX_LOG2 - MIN_SIZE_LOG2 + 1));
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    let mut body = vec![0u8; size - HEADER_LEN];
    for b in [&mut salt[..], &mut nonce[..], &mut body[..]] {
        getrandom::fill(b).map_err(|e| VaultError::Kdf(e.to_string()))?;
    }
    out.extend_from_slice(&salt);
    out.extend_from_slice(&M_COST.to_le_bytes());
    out.extend_from_slice(&T_COST.to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    // Written like a real save (journal and all), under a throwaway key.
    let mut throwaway = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut throwaway[..]).map_err(|e| VaultError::Kdf(e.to_string()))?;
    write_in_place(path, &out, &[&throwaway])?;
    if let Some(dir) = path.parent() {
        blend(dir)?;
    }
    Ok(())
}

/// Gives every vault file in `dir` the same fixed timestamps and owner-only
/// access (which also moves every file's change time together), so nothing
/// shows which file was written.
pub fn blend(dir: &Path) -> Result<(), VaultError> {
    let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(FIXED_TIME_SECS);
    let times = std::fs::FileTimes::new().set_accessed(fixed).set_modified(fixed);
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "vault") {
            owner_only(&p, false);
            std::fs::OpenOptions::new().write(true).open(&p)?.set_times(times)?;
        }
    }
    owner_only(dir, true);
    let _ = std::fs::File::open(dir).and_then(|d| d.set_times(times));
    Ok(())
}

const JOURNAL_MAGIC: &[u8; 8] = b"WHSKJRN1";
const JOURNAL_EXT: &str = "jrnl";

/// The key a vault's journals are sealed with, made from the vault's key
/// (so a journal can't be matched to a vault by anything in it).
fn journal_key(key: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    let ks = cipher.encrypt(&XNonce::from(*b"whisker vault journal ke"), [0u8; 32].as_slice()).expect("fixed-size encrypt");
    let mut k = Zeroizing::new([0u8; 32]);
    k.copy_from_slice(&ks[..32]);
    k
}

/// Rewrites `path` with `bytes` in place, so the file keeps its identity
/// and creation time. The new bytes are first sealed into a journal with
/// a random name, under each of `keys`: if we crash while copying, opening
/// the vault (with its passphrase) finishes the copy. Nothing in a journal,
/// left over or not, says which vault it's for, and two saves never share one.
fn write_in_place(path: &Path, bytes: &[u8], keys: &[&Zeroizing<[u8; 32]>]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().map(Path::to_owned).unwrap_or_default();
    std::fs::create_dir_all(&dir)?;
    owner_only(&dir, true);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut plain = Zeroizing::new(Vec::with_capacity(bytes.len() + name.len() + 2));
    plain.extend_from_slice(&(name.len() as u16).to_le_bytes());
    plain.extend_from_slice(name.as_bytes());
    plain.extend_from_slice(bytes);
    let mut journals = Vec::new();
    for key in keys {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
        let jk = journal_key(key);
        let ct = XChaCha20Poly1305::new(&Key::from(*jk))
            .encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: JOURNAL_MAGIC })
            .map_err(|_| std::io::Error::other("couldn't seal the journal"))?;
        let j = dir.join(format!("{}.{JOURNAL_EXT}", &random_secret()[..16]));
        let mut f = private_file(&j)?;
        f.write_all(JOURNAL_MAGIC)?;
        f.write_all(&nonce)?;
        f.write_all(&ct)?;
        f.sync_all()?;
        journals.push(j);
    }
    copy_over_bytes(bytes, path)?;
    for j in journals {
        let _ = std::fs::remove_file(j);
    }
    Ok(())
}

/// Puts a journal left by a crashed save of `path` in place, if `key`
/// opens one. Journals of other vaults (other keys) are left alone.
fn finish_journal(path: &Path, key: &[u8; 32]) -> bool {
    let Some(dir) = path.parent() else { return false };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let jk = journal_key(key);
    let cipher = XChaCha20Poly1305::new(&Key::from(*jk));
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let j = e.path();
        if j.extension().is_none_or(|x| x != JOURNAL_EXT) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&j) else { continue };
        if bytes.len() < 32 || &bytes[..8] != JOURNAL_MAGIC {
            continue;
        }
        let nonce: [u8; 24] = bytes[8..32].try_into().unwrap();
        let Ok(plain) = cipher.decrypt(&XNonce::from(nonce), Payload { msg: &bytes[32..], aad: JOURNAL_MAGIC }) else { continue };
        let plain = Zeroizing::new(plain);
        let Some(len) = plain.get(..2).map(|l| u16::from_le_bytes([l[0], l[1]]) as usize) else { continue };
        if plain.get(2..2 + len) != Some(name.as_bytes()) {
            continue;
        }
        if copy_over_bytes(&plain[2 + len..], path).is_ok() {
            let _ = std::fs::remove_file(&j);
            return true;
        }
    }
    false
}

fn copy_over_bytes(bytes: &[u8], to: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(to)?;
    owner_only(to, false);
    f.write_all(bytes)?;
    f.set_len(bytes.len() as u64)?;
    f.sync_all()
}

fn copy_over(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(from)?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(to)?;
    owner_only(to, false);
    f.write_all(&bytes)?;
    f.set_len(bytes.len() as u64)?;
    f.sync_all()
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let f = opts.open(path)?;
    // `mode` only applies to new files; a leftover keeps its old bits.
    owner_only(path, false);
    Ok(f)
}

/// Finishes or discards writes interrupted by a crash: a complete
/// `.tmp` is copied over its vault, an incomplete `.part` is dropped.
pub fn recover(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        match p.extension().and_then(|x| x.to_str()) {
            Some("tmp") => {
                if copy_over(&p, &p.with_extension("vault")).is_ok() {
                    let _ = std::fs::remove_file(&p);
                }
            }
            Some("part") => {
                let _ = std::fs::remove_file(&p);
            }
            _ => {}
        }
    }
}

/// Unix: 0700 for folders, 0600 for files. Elsewhere the per-user profile
/// folder's own access control applies.
pub fn owner_only(path: &Path, dir: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(if dir { 0o700 } else { 0o600 }));
    }
    #[cfg(not(unix))]
    let _ = (path, dir);
}

fn derive(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let params = Params::new(m_cost, t_cost, 1, Some(32)).map_err(|e| VaultError::Kdf(e.to_string()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|e| VaultError::Kdf(e.to_string()))?;
    Ok(key)
}

/// Random secret for things like per-account store passphrases.
pub fn random_secret() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS RNG unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crashed_save_is_finished_by_its_own_passphrase_only() {
        let dir = std::env::temp_dir().join(format!("whisker-journal-test-{}", random_secret()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("aaaa.vault");
        let b = dir.join("bbbb.vault");
        let va = Vault::create(&a, "first passphrase").unwrap();
        va.save(&serde_json::json!({"n": 1})).unwrap();
        let vb = Vault::create(&b, "second passphrase").unwrap();
        vb.save(&serde_json::json!({"m": 1})).unwrap();
        // A save of A that crashed mid-copy: the journal is there, A is torn.
        let new = va.encrypt(&serde_json::json!({"n": 2}), &Attachments::new()).unwrap();
        let name = "aaaa.vault";
        let mut plain = (name.len() as u16).to_le_bytes().to_vec();
        plain.extend_from_slice(name.as_bytes());
        plain.extend_from_slice(&new);
        let nonce = [7u8; 24];
        let ct = XChaCha20Poly1305::new(&Key::from(*journal_key(&va.key))).encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: JOURNAL_MAGIC }).unwrap();
        let j = dir.join("0123456789abcdef.jrnl");
        std::fs::write(&j, [JOURNAL_MAGIC.as_slice(), &nonce, &ct].concat()).unwrap();
        let mut torn = std::fs::read(&a).unwrap();
        torn.truncate(torn.len() / 2);
        std::fs::write(&a, &torn).unwrap();
        // Nothing in the journal names A.
        let jb = std::fs::read(&j).unwrap();
        assert!(!jb.windows(4).any(|w| w == b"aaaa"));
        assert!(!jb.windows(16).any(|w| w == va.salt));
        // B's passphrase leaves it alone; A's finishes it.
        let _: (Vault, serde_json::Value) = Vault::open(&b, "second passphrase").unwrap();
        assert!(j.exists());
        let (_, v): (Vault, serde_json::Value) = Vault::open(&a, "first passphrase").unwrap();
        assert_eq!(v["n"], 2);
        assert!(!j.exists());
        // Normal saves leave no journal behind.
        va.save(&serde_json::json!({"n": 3})).unwrap();
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| e.path().extension().is_none_or(|x| x != JOURNAL_EXT)));
        std::fs::remove_dir_all(dir).ok();
    }

    use super::*;

    #[test]
    fn round_trip_and_wrong_passphrase() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let path = dir.join("vault.bin");
        let v = Vault::create(&path, "correct horse").unwrap();
        v.save(&vec!["hello".to_string()]).unwrap();

        let (_, got): (_, Vec<String>) = Vault::open(&path, "correct horse").unwrap();
        assert_eq!(got, vec!["hello"]);
        assert!(matches!(
            Vault::open::<Vec<String>>(&path, "wrong").err().unwrap(),
            VaultError::Decrypt
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn vaults_and_decoys_look_alike() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let real = dir.join("a.vault");
        Vault::create(&real, "pw").unwrap().save(&vec!["one account".to_string()]).unwrap();
        write_decoy(&dir.join("b.vault")).unwrap();
        Vault::create(dir.join("c.vault"), "other").unwrap().save(&Vec::<String>::new()).unwrap();

        let meta: Vec<_> = ["a", "b", "c"].iter().map(|n| std::fs::metadata(dir.join(format!("{n}.vault"))).unwrap()).collect();
        for m in &meta {
            assert!(is_stepped(m.len()), "size {} isn't a step", m.len());
            assert_eq!(m.modified().unwrap(), meta[0].modified().unwrap(), "mtimes differ");
            #[cfg(unix)]
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777, 0o600);
        }
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&dir).unwrap().permissions()) & 0o777, 0o700);
        assert_eq!(meta[0].len(), meta[2].len(), "small vaults share a size");
        // Blending didn't change the other files.
        assert_eq!(Vault::open::<Vec<String>>(&real, "pw").unwrap().1, vec!["one account"]);
        assert!(matches!(Vault::open::<Vec<String>>(dir.join("b.vault"), "pw").err().unwrap(), VaultError::Decrypt));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn attachments_round_trip_and_old_vaults_open() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let path = dir.join("a.vault");
        let v = Vault::create(&path, "pw").unwrap();
        let mut att = Attachments::new();
        att.insert("store/x".into(), vec![7u8; 200_000]);
        v.save_with(&vec!["acct".to_string()], &att).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 256 * 1024);
        let (_, got, back): (_, Vec<String>, _) = Vault::open_with(&path, "pw").unwrap();
        assert_eq!(got, vec!["acct"]);
        assert_eq!(back, att);
        // The previous format (space-padded JSON) still opens.
        let (old, _): (Vec<String>, _) = decode(b"[\"acct\"]   ").unwrap();
        assert_eq!(old, vec!["acct"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn interrupted_write_is_finished_on_recover() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let path = dir.join("a.vault");
        let v = Vault::create(&path, "pw").unwrap();
        v.save(&1u32).unwrap();
        // Crash after the journal was complete but before the copy.
        let good = std::fs::read(&path).unwrap();
        v.save(&2u32).unwrap();
        std::fs::write(path.with_extension("tmp"), &good).unwrap();
        std::fs::write(path.with_extension("part"), b"half").unwrap();
        recover(&dir);
        assert_eq!(Vault::open::<u32>(&path, "pw").unwrap().1, 1);
        assert!(!path.with_extension("tmp").exists() && !path.with_extension("part").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A planted file can't make unlocking allocate gigabytes or run for
    /// hours: its cost is refused before any key is derived.
    #[test]
    fn a_huge_kdf_cost_is_refused() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let path = dir.join("a.vault");
        Vault::create(&path, "pw").unwrap().save(&1u32).unwrap();
        let good = std::fs::read(&path).unwrap();
        for (at, value) in [(24, u32::MAX), (24, 1024 * 1024 + 1), (28, 17u32), (24, 1024u32)] {
            let mut bytes = good.clone();
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            std::fs::write(&path, bytes).unwrap();
            assert!(matches!(Vault::open::<u32>(&path, "pw").err().unwrap(), VaultError::Format));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tampered_header_is_rejected() {
        let dir = std::env::temp_dir().join(format!("whisker-vault-{}", random_secret()));
        let path = dir.join("vault.bin");
        Vault::create(&path, "pw").unwrap().save(&1u32).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[24] ^= 1; // m_cost
        std::fs::write(&path, bytes).unwrap();
        assert!(Vault::open::<u32>(&path, "pw").is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
