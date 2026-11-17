//! Fetching from the open web without being turned against the user's own
//! network: only public http(s) on the usual ports, every address checked
//! before connecting (and the connection pinned to it, so a second DNS
//! answer can't swap in a local one), every redirect checked the same way,
//! and a size cap. With a proxy, the proxy does the lookups (and onion
//! sites are allowed).

use anyhow::{Result, anyhow, bail};
use reqwest::Url;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

const MAX_REDIRECTS: usize = 5;

/// What to ask for.
pub struct Ask<'a> {
    pub accept: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    /// Bytes kept; the rest is cut off (`Got::truncated`).
    pub max_bytes: usize,
    pub user_agent: &'a str,
    pub timeout: Duration,
    /// Redirects must stay on the first URL's host (a privacy front-end
    /// mustn't hand the request on to the site it stands in for).
    pub same_host: bool,
    /// Told the bytes so far and the total (0: not known) as they come.
    pub progress: Option<&'a (dyn Fn(u64, u64) + Send + Sync)>,
    /// A server the user named themselves (their own Invidious, say): it
    /// may be on their own network or another port. Implies `same_host`;
    /// one on their own network is reached directly, not through the proxy.
    pub chosen: bool,
}

impl<'a> Ask<'a> {
    pub fn new(accept: &'a str, max_bytes: usize, user_agent: &'a str, timeout: Duration) -> Self {
        Self { accept, headers: &[], max_bytes, user_agent, timeout, same_host: false, progress: None, chosen: false }
    }
}

pub struct Got {
    /// Where it ended up, after redirects.
    pub url: Url,
    /// Lower case, may be empty.
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// GETs `url`, following safe redirects.
pub async fn get(mut url: Url, proxy: Option<&str>, ask: &Ask<'_>) -> Result<Got> {
    let first_host = url.host_str().unwrap_or_default().to_owned();
    let chosen_local = ask.chosen && check_url(&url, true).is_err();
    let proxy = if chosen_local { None } else { proxy };
    for _ in 0..=MAX_REDIRECTS {
        if ask.chosen {
            if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
                bail!("only plain http and https addresses");
            }
        } else {
            check_url(&url, proxy.is_some())?;
        }
        if (ask.same_host || ask.chosen) && url.host_str() != Some(first_host.as_str()) {
            bail!("{first_host} sent the request on to {}, which was refused", url.host_str().unwrap_or("another site"));
        }
        let host = url.host_str().unwrap_or_default().to_owned();
        let mut b = crate::tls::http_builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(ask.timeout)
            .connect_timeout(Duration::from_secs(8))
            .user_agent(ask.user_agent);
        match proxy {
            Some(p) => b = b.proxy(reqwest::Proxy::all(p).map_err(|e| anyhow!("bad proxy: {e}"))?),
            None if chosen_local => b = b.no_proxy(),
            None => {
                b = b.no_proxy();
                // Check every address now and connect only to those.
                if url.host().is_some_and(|h| matches!(h, url::Host::Domain(_))) {
                    let port = url.port_or_known_default().unwrap_or(443);
                    let addrs: Vec<SocketAddr> = tokio::time::timeout(Duration::from_secs(8), tokio::net::lookup_host((host.as_str(), port)))
                        .await
                        .map_err(|_| anyhow!("looking up {host} timed out"))?
                        .map_err(|e| anyhow!("couldn't look up {host}: {e}"))?
                        .collect();
                    if addrs.is_empty() {
                        bail!("{host} has no address");
                    }
                    if let Some(bad) = addrs.iter().find(|a| !is_public(a.ip())) {
                        bail!("refusing {host}: it points at a private or local address ({})", bad.ip());
                    }
                    b = b.resolve_to_addrs(&host, &addrs);
                }
            }
        }
        let http = b.build()?;
        let mut req = http.get(url.clone()).header(reqwest::header::ACCEPT, ask.accept);
        for (k, v) in ask.headers {
            req = req.header(*k, *v);
        }
        let mut resp = req.send().await.map_err(|e| anyhow!("couldn't fetch {url}: {}", short_err(&e)))?;
        let status = resp.status();
        if status.is_redirection() {
            let to = resp.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok()).ok_or_else(|| anyhow!("redirect without a location"))?;
            url = url.join(to)?;
            continue;
        }
        if !status.is_success() {
            bail!("{url} answered {status}");
        }
        let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let total = resp.content_length().unwrap_or(0);
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!("reading {url}: {}", short_err(&e)))? {
            let room = ask.max_bytes - body.len();
            if chunk.len() >= room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
            if let Some(p) = ask.progress {
                p(body.len() as u64, total);
            }
        }
        return Ok(Got { url, content_type, body, truncated });
    }
    bail!("too many redirects")
}

fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() { "timed out".into() } else if e.is_connect() { "couldn't connect".into() } else { e.to_string() }
}

/// Refuses addresses that aren't plain public http(s).
pub fn check_url(url: &Url, proxied: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http and https addresses");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("addresses with a user name or password aren't fetched");
    }
    if !matches!(url.port(), None | Some(80) | Some(443)) {
        bail!("only the usual web ports (80, 443)");
    }
    match url.host() {
        None => bail!("no host"),
        Some(url::Host::Ipv4(ip)) if !is_public(IpAddr::V4(ip)) => bail!("refusing a private or local address"),
        Some(url::Host::Ipv6(ip)) if !is_public(IpAddr::V6(ip)) => bail!("refusing a private or local address"),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            let local = d == "localhost" || !d.contains('.') || [".localhost", ".local", ".internal", ".lan", ".home", ".home.arpa", ".corp", ".intranet"].iter().any(|s| d.ends_with(s));
            // Onion sites only make sense (and are only reachable) through a Tor proxy.
            if local || (d.ends_with(".onion") && !proxied) {
                bail!("refusing local or unreachable host {d}");
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether `ip` is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = v6.segments();
            // NAT64 (64:ff9b::/96) carries an IPv4 address.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] & 0xffc0) == 0xfec0 // site local (old)
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x2002) // 6to4 can carry a private IPv4
                || (s[0] == 0x2001 && s[1] == 0) // Teredo
                || s[..6] == [0, 0, 0, 0, 0, 0] // IPv4-compatible and other ::/96
                || v6 == Ipv6Addr::UNSPECIFIED)
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18/15
        || o[0] >= 240)
}

