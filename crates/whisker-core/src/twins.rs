//! Twin accounts: the same name on two servers (a main account and its
//! backup, say), both signed in here in the same vault. Room keys are copied
//! between them on this device, so a twin can read what was said in a room
//! before it first signed in (keys only ever go to devices that exist when a
//! message is sent; seeing a room's history isn't enough to read it).
//! Nothing goes through a server: the keys are exported from one account's
//! store and imported into the other's.

use crate::accounts::{AccountManager, AccountSummary};
use crate::profile::AccountId;
use anyhow::{Context, Result};
use matrix_sdk::ruma::{OwnedRoomId, UserId};
use std::collections::HashSet;

/// The part of `@name:server` before the server.
fn localpart(user: &str) -> Option<&str> {
    UserId::parse(user).ok()?;
    user.strip_prefix('@')?.split(':').next()
}

/// Whether two accounts are twins: the same name on different servers, in
/// the same vault (accounts kept apart in vaults are never tied together).
pub fn are_twins(a: &AccountSummary, b: &AccountSummary) -> bool {
    a.id != b.id
        && a.vault == b.vault
        && localpart(&a.user_id).is_some_and(|l| Some(l) == localpart(&b.user_id))
        && a.user_id != b.user_id
}

/// The signed-in twins of `account`.
pub async fn twins_of(mgr: &AccountManager, account: AccountId) -> Vec<AccountId> {
    let all = mgr.accounts().await;
    let Some(me) = all.iter().find(|a| a.id == account) else { return Vec::new() };
    all.iter().filter(|a| are_twins(me, a)).map(|a| a.id).collect()
}

/// Copies `from`'s room keys into `to`, for the rooms `to` has joined
/// (only `rooms` of them, when given). Returns how many keys were new to
/// `to`, and the rooms they were for.
pub async fn copy_keys(mgr: &AccountManager, from: AccountId, to: AccountId, rooms: Option<&[OwnedRoomId]>) -> Result<(usize, Vec<OwnedRoomId>)> {
    let src = mgr.client(from).await.context("that account isn't signed in")?;
    let dst = mgr.client(to).await.context("that account isn't signed in")?;
    let mut wanted: HashSet<OwnedRoomId> = dst.joined_rooms().into_iter().map(|r| r.room_id().to_owned()).collect();
    if let Some(only) = rooms {
        let only: HashSet<&OwnedRoomId> = only.iter().collect();
        wanted.retain(|r| only.contains(r));
    }
    // Only rooms both are in.
    let theirs: HashSet<OwnedRoomId> = src.joined_rooms().into_iter().map(|r| r.room_id().to_owned()).collect();
    wanted.retain(|r| theirs.contains(r));
    if wanted.is_empty() {
        return Ok((0, Vec::new()));
    }
    // The SDK moves keys through a passphrase-locked file: one in this
    // app's own folder, locked with a random passphrase that never leaves
    // memory, and removed as soon as it's read.
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("no random numbers: {e}"))?;
    let pass = zeroize::Zeroizing::new(secret.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut name = [0u8; 8];
    let _ = getrandom::fill(&mut name);
    let path = mgr.data_dir().join(format!("twin-keys-{}.tmp", name.iter().map(|b| format!("{b:02x}")).collect::<String>()));
    let result = async {
        let mut found = HashSet::new();
        src.encryption()
            .export_room_keys(path.clone(), &pass, |s| {
                let hit = wanted.contains(s.room_id());
                if hit {
                    found.insert(s.room_id().to_owned());
                }
                hit
            })
            .await
            .context("couldn't read the room keys")?;
        if found.is_empty() {
            return Ok((0, Vec::new()));
        }
        let got = dst.encryption().import_room_keys(path.clone(), &pass).await.context("couldn't store the room keys")?;
        Ok((got.imported_count, found.into_iter().collect()))
    }
    .await;
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(test)]
mod tests {
    use super::localpart;

    #[test]
    fn localparts() {
        assert_eq!(localpart("@nully:anoncat.gleeze.com"), Some("nully"));
        assert_eq!(localpart("@nully:anoncat.cc"), Some("nully"));
        assert_eq!(localpart("nully"), None);
    }
}
