//! Following a server to a new address without signing in again.
//!
//! A server can move (a new port, or behind a relay now, or not any more):
//! the address saved at sign-in then goes dead, though the account, its
//! session and its encryption keys are all still good. When it can't be
//! reached for a while (and once when unlocking), Whisker asks the server
//! name where it lives now and checks the answer before using it.
//!
//! The account's access token is only ever sent to an address that:
//! - is https (http only for a server that is itself reached over http: a
//!   local one, say),
//! - is on the server name's own host or one under it (`example.org`,
//!   `matrix.example.org`), or on a host this account already used,
//! - answers as a Matrix server, and says the token is this very account's.
//!
//! So a tampered `.well-known` page can't send the token somewhere else.

use std::time::Duration;

/// How long a server has to be unreachable before its address is checked.
pub const CHECK_AFTER: Duration = Duration::from_secs(25);
/// And how often, after that, while it stays unreachable.
pub const CHECK_EVERY: Duration = Duration::from_secs(120);
/// Addresses an account has used, kept for when `.well-known` can't be had.
pub const REMEMBER: usize = 5;

/// The host of a server name or address, lowercased, without the port.
fn host_of(s: &str) -> String {
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.split(['/', '?', '#']).next().unwrap_or_default();
    let s = s.rsplit_once('@').map_or(s, |(_, h)| h);
    let host = if let Some(v6) = s.strip_prefix('[') { v6.split(']').next().unwrap_or_default() } else { s.split(':').next().unwrap_or_default() };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether the token may be offered to `candidate` for an account on
/// `server` (the part of its user id after the `:`). `used` are addresses
/// this account has already used (the one saved at sign-in among them).
pub fn trusted(candidate: &str, server: &str, used: &[String]) -> bool {
    let Ok(url) = url::Url::parse(candidate) else { return false };
    let plain_ok = crate::rooms::scheme_for(server) == "http";
    match url.scheme() {
        "https" => {}
        "http" if plain_ok => {}
        _ => return false,
    }
    if url.username() != "" || url.password().is_some() || url.host_str().is_none() {
        return false;
    }
    let host = host_of(candidate);
    let home = host_of(server);
    if host.is_empty() || home.is_empty() {
        return false;
    }
    host == home || host.ends_with(&format!(".{home}")) || used.iter().any(|u| host_of(u) == host)
}

/// Whether `base` is a Matrix server that knows `token` as `user_id`'s.
/// Asked through the account's proxy, following no redirects.
pub async fn confirm(base: &str, user_id: &str, token: &str, proxy: Option<&str>) -> bool {
    let mut b = crate::tls::http_builder().timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none());
    if let Some(p) = proxy {
        let Ok(p) = reqwest::Proxy::all(p) else { return false };
        b = b.proxy(p);
    }
    let Ok(http) = b.build() else { return false };
    let base = base.trim_end_matches('/');
    let versions: Option<serde_json::Value> = async {
        http.get(format!("{base}/_matrix/client/versions")).send().await.ok()?.error_for_status().ok()?.json().await.ok()
    }
    .await;
    if !versions.is_some_and(|v| v.get("versions").is_some_and(|v| v.is_array())) {
        return false;
    }
    let me: Option<serde_json::Value> = async {
        http.get(format!("{base}/_matrix/client/v3/account/whoami")).bearer_auth(token).send().await.ok()?.error_for_status().ok()?.json().await.ok()
    }
    .await;
    me.and_then(|v| v.get("user_id")?.as_str().map(|s| s == user_id)).unwrap_or(false)
}

/// Where to look, in order: what `.well-known` names (`named`, if it could
/// be had: then only those, the faster first), else the server name itself
/// and the addresses the account used before. Never `current`, and only
/// trusted ones.
pub fn candidates(named: Option<&[String]>, server: &str, current: &str, used: &[String]) -> Vec<String> {
    let current = current.trim_end_matches('/');
    let mut all: Vec<String> = match named {
        Some(n) => n.to_vec(),
        None => std::iter::once(format!("{}://{server}", crate::rooms::scheme_for(server))).chain(used.iter().cloned()).collect(),
    };
    let mut trusted_list = used.to_vec();
    trusted_list.push(current.to_owned());
    all = all.into_iter().map(|c| c.trim_end_matches('/').to_owned()).filter(|c| c != current && trusted(c, server, &trusted_list)).collect();
    all.dedup();
    let mut seen = Vec::new();
    all.retain(|c| {
        let new = !seen.contains(c);
        seen.push(c.clone());
        new
    });
    all
}

/// What `.well-known` names that's better than `current` (named earlier:
/// the server's direct address while using its relay, say), best first.
/// Empty when `current` is already the best.
pub fn better(named: &[String], current: &str) -> Vec<String> {
    let current = current.trim_end_matches('/');
    named.iter().map(|n| n.trim_end_matches('/').to_owned()).take_while(|n| n != current).collect()
}

/// `used` with `old` added (newest first, at most [`REMEMBER`]).
pub fn remember(used: &mut Vec<String>, old: &str) {
    let old = old.trim_end_matches('/').to_owned();
    used.retain(|u| *u != old);
    used.insert(0, old);
    used.truncate(REMEMBER);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_servers_own_hosts() {
        let none: Vec<String> = Vec::new();
        assert!(trusted("https://anoncat.example", "anoncat.example", &none));
        assert!(trusted("https://hs.anoncat.example:65123", "anoncat.example", &none));
        assert!(trusted("https://HS.Anoncat.Example./", "anoncat.example", &none));
        // Not someone else's server, or one that merely ends the same way.
        assert!(!trusted("https://evil.example", "anoncat.example", &none));
        assert!(!trusted("https://xanoncat.example", "anoncat.example", &none));
        assert!(!trusted("https://anoncat.example.evil.example", "anoncat.example", &none));
        assert!(!trusted("https://anoncat.example@evil.example", "anoncat.example", &none));
        // Never down to http.
        assert!(!trusted("http://anoncat.example", "anoncat.example", &none));
        assert!(trusted("http://localhost:8008", "localhost:8008", &none));
        // A host the account already used (delegated elsewhere at sign-in).
        let used = vec!["https://matrix.host.example".to_owned()];
        assert!(trusted("https://matrix.host.example:8448", "example.org", &used));
        assert!(!trusted("https://other.example", "example.org", &used));
    }

    #[test]
    fn where_to_look() {
        let used = vec!["https://hs.cat.example:53383".to_owned()];
        let one = |s: &str| vec![s.to_owned()];
        // `.well-known` answered: only what it names, if trusted.
        assert_eq!(candidates(Some(&one("https://hs.cat.example:65123/")), "cat.example", "https://hs.cat.example:53383", &used), ["https://hs.cat.example:65123"]);
        assert!(candidates(Some(&one("https://evil.example")), "cat.example", "https://hs.cat.example:53383", &used).is_empty());
        assert!(candidates(Some(&one("https://hs.cat.example:53383")), "cat.example", "https://hs.cat.example:53383/", &used).is_empty());
        // The direct port, then the relay: on the relay, the direct one is
        // better; on the direct one, nothing is.
        let both = vec!["https://hs.cat.example:65123".to_owned(), "https://cat.example".to_owned()];
        assert_eq!(better(&both, "https://cat.example/"), ["https://hs.cat.example:65123"]);
        assert!(better(&both, "https://hs.cat.example:65123").is_empty());
        assert_eq!(better(&both, "https://old.cat.example"), both);
        // The direct port down: the relay.
        assert_eq!(candidates(Some(&both), "cat.example", "https://hs.cat.example:65123", &[]), ["https://cat.example"]);
        // It didn't: the server name, then the addresses used before.
        assert_eq!(candidates(None, "cat.example", "https://hs.cat.example:65123", &used), ["https://cat.example", "https://hs.cat.example:53383"]);
        let mut u = used.clone();
        remember(&mut u, "https://hs.cat.example:65123/");
        remember(&mut u, "https://hs.cat.example:53383");
        assert_eq!(u, ["https://hs.cat.example:53383", "https://hs.cat.example:65123"]);
    }
}
