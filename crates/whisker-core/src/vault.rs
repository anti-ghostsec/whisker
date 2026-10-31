//! The vault: one encrypted file holding every account's session, tags and
//! agent API keys. Nothing about who you are is readable on disk without
//! the passphrase.
//!
//! Format: `MAGIC | salt(16) | m_cost(u32 le) | t_cost(u32 le) | nonce(24) | ciphertext`.
//! The header is authenticated as AAD, so the KDF cost can't be lowered.
//!
//! Hidden vaults only stay hidden if nothing on disk tells vaults apart or
//! counts them. So every vault is padded to the same size, the folder also
//! holds decoy files ([`write_decoy`]) that look exactly like vaults, and
//! after any write every file in the folder is rewritten (same bytes) with
//! the same timestamps ([`blend`]), so nothing shows which one changed.

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
/// Plaintext is padded to a multiple of this, so vaults (and decoys) are
/// all the same size unless one holds an unusual amount of data.
const PAD: usize = 64 * 1024;
/// Every vault file's timestamps are set to this (2020-01-01 UTC).
const FIXED_TIME_SECS: u64 = 1_577_836_800;

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
    pub fn open<T: DeserializeOwned>(
        path: impl Into<PathBuf>,
        passphrase: &str,
    ) -> Result<(Self, T), VaultError> {
        let path = path.into();
        let bytes = std::fs::read(&path)?;
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err(VaultError::Format);
        }
        let salt: [u8; 16] = bytes[8..24].try_into().unwrap();
        let m_cost = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let t_cost = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
        let nonce: [u8; 24] = bytes[32..56].try_into().unwrap();
        // Refuse a header that asks for a trivially cheap KDF.
        if m_cost < 8 * 1024 || t_cost < 1 {
            return Err(VaultError::Format);
        }
        let key = derive(passphrase, &salt, m_cost, t_cost)?;
        let cipher = XChaCha20Poly1305::new(&Key::from(*key));
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    &XNonce::from(nonce),
                    Payload { msg: &bytes[HEADER_LEN..], aad: &bytes[..HEADER_LEN] },
                )
                .map_err(|_| VaultError::Decrypt)?,
        );
        let value = serde_json::from_slice(&plain)?;
        Ok((Self { path, salt, m_cost, t_cost, key }, value))
    }

    /// Encrypts `value` and atomically replaces the vault file.
    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), VaultError> {
        let mut plain = Zeroizing::new(serde_json::to_vec(value)?);
        // Trailing spaces are valid JSON whitespace, so padding needs no format change.
        let padded = plain.len().div_ceil(PAD).max(1) * PAD;
        plain.resize(padded, b' ');
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

        replace(&self.path, &out)?;
        if let Some(dir) = self.path.parent() {
            blend(dir)?;
        }
        Ok(())
    }

    /// Re-keys the vault under a new passphrase (fresh salt).
    pub fn change_passphrase<T: Serialize>(
        &mut self,
        new_passphrase: &str,
        value: &T,
    ) -> Result<(), VaultError> {
        getrandom::fill(&mut self.salt).map_err(|e| VaultError::Kdf(e.to_string()))?;
        self.key = derive(new_passphrase, &self.salt, self.m_cost, self.t_cost)?;
        self.save(value)
    }
}

/// Size of a vault file whose JSON is `plain_len` bytes.
pub fn padded_len(plain_len: usize) -> usize {
    HEADER_LEN + plain_len.div_ceil(PAD).max(1) * PAD + 16
}

/// Writes a decoy: a file shaped exactly like a vault (same header, KDF
/// cost and padded size) whose contents are random, so no passphrase opens
/// it and nothing distinguishes it from a real vault.
pub fn write_decoy(path: &Path) -> Result<(), VaultError> {
    let mut out = Vec::with_capacity(HEADER_LEN + PAD + 16);
    out.extend_from_slice(MAGIC);
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    let mut body = vec![0u8; PAD + 16];
    for b in [&mut salt[..], &mut nonce[..], &mut body[..]] {
        getrandom::fill(b).map_err(|e| VaultError::Kdf(e.to_string()))?;
    }
    out.extend_from_slice(&salt);
    out.extend_from_slice(&M_COST.to_le_bytes());
    out.extend_from_slice(&T_COST.to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    replace(path, &out)?;
    if let Some(dir) = path.parent() {
        blend(dir)?;
    }
    Ok(())
}

/// Makes every vault file in `dir` look equally fresh: each is rewritten
/// with its own bytes (new inode and creation time, like a saved vault),
/// made owner-only, and given the same fixed timestamps. Locked vaults are
/// only copied, never changed.
pub fn blend(dir: &Path) -> Result<(), VaultError> {
    let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(FIXED_TIME_SECS);
    let times = std::fs::FileTimes::new().set_accessed(fixed).set_modified(fixed);
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "vault") {
            let bytes = std::fs::read(&p)?;
            replace(&p, &bytes)?;
            std::fs::OpenOptions::new().write(true).open(&p)?.set_times(times)?;
        }
    }
    owner_only(dir, true);
    Ok(())
}

/// Atomically replaces `path` with `bytes`, readable only by the owner.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        owner_only(dir, true);
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp)?;
    // `mode` only applies when the file is new; a leftover tmp keeps its old bits.
    owner_only(&tmp, false);
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
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
            assert_eq!(m.len(), meta[0].len(), "sizes differ");
            assert_eq!(m.modified().unwrap(), meta[0].modified().unwrap(), "mtimes differ");
            #[cfg(unix)]
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777, 0o600);
        }
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&dir).unwrap().permissions()) & 0o777, 0o700);
        // Blending rewrote the other files without changing them.
        assert_eq!(Vault::open::<Vec<String>>(&real, "pw").unwrap().1, vec!["one account"]);
        assert!(matches!(Vault::open::<Vec<String>>(dir.join("b.vault"), "pw").err().unwrap(), VaultError::Decrypt));
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
