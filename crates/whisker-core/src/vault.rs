//! The vault: one encrypted file holding every account's session, tags and
//! agent API keys. Nothing about who you are is readable on disk without
//! the passphrase.
//!
//! Format: `MAGIC | salt(16) | m_cost(u32 le) | t_cost(u32 le) | nonce(24) | ciphertext`.
//! The header is authenticated as AAD, so the KDF cost can't be lowered.

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
        let plain = Zeroizing::new(serde_json::to_vec(value)?);
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

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &self.path)?;
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
