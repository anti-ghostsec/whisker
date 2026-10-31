//! What a vault holds: account records and opaque extras (agent configs).

use matrix_sdk::authentication::matrix::MatrixSession;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use uuid::Uuid;

/// Whisker's own id for an account. Stable across renames and re-logins,
/// and never sent to any server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub Uuid);

impl AccountId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for AccountId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// A person's account, driven from the UI.
    #[default]
    Human,
    /// An automated account driven by an AI agent.
    Agent,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: AccountId,
    pub kind: AccountKind,
    pub user_id: String,
    pub homeserver: String,
    /// Local nickname shown instead of the MXID.
    #[serde(default)]
    pub label: Option<String>,
    /// Local-only labels used to group and filter accounts ("main", "alt", "work").
    #[serde(default)]
    pub tags: BTreeSet<String>,
    /// Per-account proxy, e.g. `socks5h://127.0.0.1:9050`. Giving alts
    /// different proxies (or Tor circuits) stops a server linking them by IP.
    #[serde(default)]
    pub proxy: Option<String>,
    pub session: MatrixSession,
    /// Encrypts this account's local Matrix store (keys, room state).
    pub store_passphrase: String,
    /// Place in the account list (lower first).
    #[serde(default)]
    pub position: u32,
}

impl AccountRecord {
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.user_id)
    }
}

impl fmt::Debug for AccountRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the session token or store passphrase.
        f.debug_struct("AccountRecord")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("user_id", &self.user_id)
            .field("tags", &self.tags)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
    /// Data owned by other crates, keyed by crate (e.g. `"agents"`).
    #[serde(default)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// Normalises a tag: trimmed, lowercase, no spaces.
pub fn normalize_tag(tag: &str) -> Option<String> {
    let t: String = tag.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-");
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_normalize() {
        assert_eq!(normalize_tag("  Hidden Alt "), Some("hidden-alt".into()));
        assert_eq!(normalize_tag("   "), None);
    }
}
