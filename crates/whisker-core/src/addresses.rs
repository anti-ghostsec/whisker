//! anoncat's signed address list: where its servers can be reached, so an
//! account finds its server again when a domain dies.
//!
//! The list is published in several places (see [`STARTING`] and the list's
//! own `lists`) as `{"v":1, "list":"…", "sig":"…", "key":"…"}`. `sig` is an
//! Ed25519 signature over the `list` string's bytes exactly as sent, checked
//! only against [`KEY`], built into Whisker (the file's own `key` is
//! ignored). A copy is used only while it hasn't expired, and never one
//! older than the newest already seen, so an old list can't be replayed to
//! send accounts to a server that's since been dropped.
//!
//! Off while [`KEY`] is empty.
//!
//! The addresses it names are trusted because of the signature, not their
//! hosts (an onion isn't under the server's own name); the account's token
//! still only goes to one that answers as this very account's server (see
//! [`crate::moves::confirm`]).

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// anoncat's address list key: 32 raw Ed25519 public key bytes, standard
/// base64. Empty: the feature is off.
pub const KEY: &str = "";

/// Where to look before any copy has been seen (the newest copy's own
/// `lists` are added to these).
pub const STARTING: &[&str] = &[
    "https://anoncat.cc/.well-known/anoncat/addresses.json",
    "https://anoncat.gleeze.com/.well-known/anoncat/addresses.json",
    "https://raw.githubusercontent.com/anti-ghostsec/anoncat/anoncat/ops/addresses/addresses.signed.json",
];

/// Server names the list is about before any copy has been seen: only
/// accounts on one of these (or a server a copy names) cause a fetch.
pub const KNOWN_SERVERS: &[&str] = &["anoncat.gleeze.com", "anoncat.cc"];

/// How often the list is fetched again, and the least time between fetches
/// that a failing server sets off.
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const AT_MOST: Duration = Duration::from_secs(10 * 60);

/// A copy bigger than this isn't one.
const MAX_BYTES: usize = 256 * 1024;

/// What's fetched: the signed list as sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signed {
    pub v: u32,
    pub list: String,
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct List {
    pub v: u32,
    pub issued: u64,
    pub expires: u64,
    #[serde(default)]
    pub servers: Vec<Server>,
    /// Other places this file is published.
    #[serde(default)]
    pub lists: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Server {
    pub server_name: String,
    #[serde(default)]
    pub homeservers: Vec<String>,
}

/// The built-in key, or `None` while the feature is off.
pub fn key() -> Option<[u8; 32]> {
    let raw = base64::engine::general_purpose::STANDARD.decode(KEY.trim()).ok()?;
    raw.try_into().ok()
}

pub fn enabled() -> bool {
    key().is_some()
}

/// `body` as a list signed with `key`, still good at `now` (unix seconds).
pub fn open(body: &[u8], key: &[u8; 32], now: u64) -> Option<(Signed, List)> {
    let signed: Signed = serde_json::from_slice(body).ok()?;
    let list = check(&signed, key)?;
    (list.expires > now).then_some((signed, list))
}

/// The list in `signed` if its signature is good (expired or not).
fn check(signed: &Signed, key: &[u8; 32]) -> Option<List> {
    if signed.v != 1 {
        return None;
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(key).ok()?;
    let sig = base64::engine::general_purpose::STANDARD.decode(signed.sig.trim()).ok()?;
    let sig = ed25519_dalek::Signature::from_slice(&sig).ok()?;
    vk.verify_strict(signed.list.as_bytes(), &sig).ok()?;
    let list: List = serde_json::from_str(&signed.list).ok()?;
    (list.v == 1).then_some(list)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether `url` may be fetched from or offered a token: https, or http
/// to an onion (only when there's a proxy to reach it through). No
/// user names or passwords in it.
pub fn usable(url: &str, proxied: bool) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    let Some(host) = u.host_str() else { return false };
    if !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let onion = host.trim_end_matches('.').to_ascii_lowercase().ends_with(".onion");
    if onion && !proxied {
        return false;
    }
    match u.scheme() {
        "https" => true,
        "http" => onion,
        _ => false,
    }
}

/// Where to fetch the list from: the built-in places, the ones `list`
/// names, and each onion homeserver's copy (only with a proxy).
pub fn sources(list: Option<&List>, proxied: bool) -> Vec<String> {
    let mut all: Vec<String> = STARTING.iter().map(|s| (*s).to_owned()).collect();
    if let Some(l) = list {
        all.extend(l.lists.iter().cloned());
        for s in &l.servers {
            for h in &s.homeservers {
                if usable(h, proxied) && url::Url::parse(h).ok().and_then(|u| u.host_str().map(|h| h.ends_with(".onion"))).unwrap_or(false) {
                    all.push(format!("{}/.well-known/anoncat/addresses.json", h.trim_end_matches('/')));
                }
            }
        }
    }
    let mut seen = Vec::new();
    all.retain(|u| usable(u, proxied) && !seen.contains(u) && {
        seen.push(u.clone());
        true
    });
    all
}

/// The addresses `list` names for `server`, in its order, that can be
/// tried from here; never `current`.
pub fn homeservers(list: &List, server: &str, current: &str, proxied: bool) -> Vec<String> {
    let current = current.trim_end_matches('/');
    let server = server.to_ascii_lowercase();
    list.servers
        .iter()
        .filter(|s| s.server_name.to_ascii_lowercase() == server)
        .flat_map(|s| s.homeservers.iter())
        .map(|h| h.trim_end_matches('/').to_owned())
        .filter(|h| h != current && usable(h, proxied))
        .collect()
}

/// The newest good copy seen: kept (in `addresses.json`, beside the
/// vaults) so a restart can't be sent an older one.
#[derive(Default)]
pub struct Store {
    pub signed: Option<Signed>,
    /// The kept copy's list, even when expired: its `issued` is the floor.
    pub list: Option<List>,
    /// When the list was last fetched, this run.
    pub fetched: Option<std::time::Instant>,
}

impl Store {
    pub fn load(path: &std::path::Path) -> Self {
        let Some(key) = key() else { return Self::default() };
        let Some(signed) = std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Signed>(&b).ok()) else { return Self::default() };
        match check(&signed, &key) {
            Some(list) => Self { signed: Some(signed), list: Some(list), fetched: None },
            None => Self::default(),
        }
    }

    /// The list, if it hasn't expired.
    pub fn current(&self) -> Option<&List> {
        self.list.as_ref().filter(|l| l.expires > now())
    }

    /// Takes `body` if it's a good copy newer than the one kept. Says
    /// whether it did.
    pub fn offer(&mut self, body: &[u8], key: &[u8; 32]) -> bool {
        let Some((signed, list)) = open(body, key, now()) else { return false };
        if self.list.as_ref().is_some_and(|l| list.issued <= l.issued) {
            return false;
        }
        self.signed = Some(signed);
        self.list = Some(list);
        true
    }

    pub fn save(&self, path: &std::path::Path) {
        let Some(signed) = &self.signed else { return };
        let Ok(json) = serde_json::to_vec(signed) else { return };
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, json).is_ok() {
            crate::vault::owner_only(&tmp, false);
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Whether `server` is one the list is about.
    pub fn covers(&self, server: &str) -> bool {
        let server = server.to_ascii_lowercase();
        KNOWN_SERVERS.contains(&server.as_str()) || self.list.as_ref().is_some_and(|l| l.servers.iter().any(|s| s.server_name.to_ascii_lowercase() == server))
    }
}

/// Fetches every place the list is published (through `proxy` if there is
/// one) and keeps the newest good copy. Says whether it changed.
pub async fn fetch(store: &tokio::sync::Mutex<Store>, proxy: Option<&str>, path: &std::path::Path) -> bool {
    let Some(key) = key() else { return false };
    let urls = {
        let mut st = store.lock().await;
        st.fetched = Some(std::time::Instant::now());
        sources(st.list.as_ref(), proxy.is_some())
    };
    // Nothing about who's asking: a common browser's name, nothing else.
    const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:128.0) Gecko/20100101 Firefox/128.0";
    let mut changed = false;
    for u in urls {
        let Ok(url) = reqwest::Url::parse(&u) else { continue };
        let ask = crate::safe_web::Ask::new("application/json", MAX_BYTES, AGENT, Duration::from_secs(30));
        let got = match crate::safe_web::get(url, proxy, &ask).await {
            Ok(g) if !g.truncated => g,
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!("address list from {u}: {e:#}");
                continue;
            }
        };
        let mut st = store.lock().await;
        if st.offer(&got.body, &key) {
            tracing::info!("address list from {u}: issued {}", st.list.as_ref().map_or(0, |l| l.issued));
            st.save(path);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn sign(sk: &ed25519_dalek::SigningKey, list: &str) -> Vec<u8> {
        let sig = base64::engine::general_purpose::STANDARD.encode(sk.sign(list.as_bytes()).to_bytes());
        serde_json::to_vec(&serde_json::json!({"v": 1, "list": list, "sig": sig, "key": "ignored"})).unwrap()
    }

    fn list(issued: u64, expires: u64) -> String {
        // Spacing and order as the server writes it: checked as sent.
        format!(
            r#"{{"v":1, "issued":{issued}, "expires":{expires}, "servers":[{{"server_name":"anoncat.gleeze.com", "homeservers":["https://anoncat.gleeze.com", "http://abc.onion", "https://new.example/"]}}], "lists":["https://mirror.example/a.json"]}}"#
        )
    }

    #[test]
    fn signed_lists() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let key = sk.verifying_key().to_bytes();
        let now = now();
        let good = sign(&sk, &list(100, now + 3600));
        let (_, l) = open(&good, &key, now).unwrap();
        assert_eq!(l.issued, 100);
        // Expired, signed by another key, or changed after signing: refused.
        assert!(open(&sign(&sk, &list(100, now - 1)), &key, now).is_none());
        let other = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        assert!(open(&sign(&other, &list(100, now + 3600)), &key, now).is_none());
        let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
        v["list"] = serde_json::Value::String(list(100, now + 3600).replace("new.example", "evil.example"));
        assert!(open(&serde_json::to_vec(&v).unwrap(), &key, now).is_none());

        // Newest only: never back to an older one.
        let mut st = Store::default();
        assert!(st.offer(&good, &key));
        assert!(!st.offer(&good, &key));
        assert!(!st.offer(&sign(&sk, &list(99, now + 9999)), &key));
        assert!(st.offer(&sign(&sk, &list(101, now + 3600)), &key));
        assert_eq!(st.current().unwrap().issued, 101);
        assert!(st.covers("Anoncat.Gleeze.com"));
        assert!(!st.covers("matrix.org"));
    }

    #[test]
    fn where_to_go() {
        let l: List = serde_json::from_str(&list(1, u64::MAX)).unwrap();
        // The onion only with a proxy to reach it; never the current one.
        assert_eq!(homeservers(&l, "anoncat.gleeze.com", "https://anoncat.gleeze.com/", false), ["https://new.example"]);
        assert_eq!(homeservers(&l, "anoncat.gleeze.com", "https://other", true), ["https://anoncat.gleeze.com", "http://abc.onion", "https://new.example"]);
        assert!(homeservers(&l, "matrix.org", "", true).is_empty());
        assert!(!usable("http://anoncat.gleeze.com", true));
        assert!(!usable("https://u:p@anoncat.gleeze.com", true));
        let s = sources(Some(&l), true);
        assert!(s.contains(&"https://mirror.example/a.json".to_owned()));
        assert!(s.contains(&"http://abc.onion/.well-known/anoncat/addresses.json".to_owned()));
        assert!(!sources(Some(&l), false).iter().any(|u| u.contains(".onion")));
    }

    #[test]
    fn off_without_a_key() {
        assert_eq!(enabled(), !KEY.is_empty());
    }
}
