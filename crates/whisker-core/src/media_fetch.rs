//! Fetches files for matrix-sdk, with time limits and fallbacks.
//!
//! In order, until one gives the file:
//! 1. the account's server, logged in (`/_matrix/client/v1/media/…`);
//! 2. the account's server, the older address (`/_matrix/media/v3/…`);
//! 3. only when turned on ([`set_ask_file_servers`]): the file's own server,
//!    the older address, without the login. Some servers don't answer other
//!    servers' newer media requests ("Remote media not found …
//!    M_UNRECOGNIZED"), so the account's server can't get the file, though
//!    the file's server still gives it out the old way. Off by default: that
//!    server then sees your IP address (unless the account uses a proxy) and
//!    which file you opened.
//! 4. only when turned on ([`set_ask_room_servers`]): a few other servers in
//!    the room the file was posted in, the older address, without the login,
//!    asking only for copies they already keep (`allow_remote=false`). When
//!    a server shuts down its files are gone from it, but a server whose
//!    users opened them keeps a copy, and some still give those out. Tried:
//!    the two with the most members in the room (likeliest to have opened
//!    it), then two others at random; never the account's server or the
//!    file's own. Off by default: each one asked sees your IP address
//!    (unless the account uses a proxy) and which file you wanted. The room
//!    comes from [`in_room`].
//!
//! The SDK's own fetcher retries a failing server for up to 15 minutes, with
//! no time limit on a stalled download, so a few slow previews held up all
//! the others. Here a request waits at most [`ANSWER`] for an answer and
//! [`STALL`] between pieces, and is retried once.

use matrix_sdk::media::{MediaFetcher, MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::{MxcUri, OwnedMxcUri, OwnedRoomId, OwnedServerName, ServerName};
use matrix_sdk::{BoxFuture, Client};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest wait for a server to start answering.
const ANSWER: Duration = Duration::from_secs(30);
/// Longest wait between two pieces of a file.
const STALL: Duration = Duration::from_secs(30);
/// Longest a whole download may take.
const WHOLE: Duration = Duration::from_secs(2 * 60 * 60);
/// Biggest file taken in memory.
const MAX_BYTES: usize = 512 * 1024 * 1024;

static ASK_FILE_SERVERS: AtomicBool = AtomicBool::new(false);
static ASK_ROOM_SERVERS: AtomicBool = AtomicBool::new(false);

/// How many of the room's other servers are asked for a copy.
const ROOM_TRIES: usize = 4;
/// After the room's servers had no copy of a file, how long before they're
/// asked again (previews retry on their own).
const ROOM_AGAIN: Duration = Duration::from_secs(600);

tokio::task_local! {
    static ROOM: OwnedRoomId;
}

/// Runs `f` with `room` as the room whose files it fetches, so a file whose
/// server is gone can be asked of the room's other servers.
pub async fn in_room<F: std::future::Future>(room: OwnedRoomId, f: F) -> F::Output {
    ROOM.scope(room, f).await
}

/// Whether files nobody else can give are asked of other servers in their
/// room (see the module docs).
pub fn set_ask_room_servers(on: bool) {
    ASK_ROOM_SERVERS.store(on, Ordering::Relaxed);
}

/// Whether files the account's server can't get are asked of their own
/// server directly (see the module docs).
pub fn set_ask_file_servers(on: bool) {
    ASK_FILE_SERVERS.store(on, Ordering::Relaxed);
}

#[derive(Debug, Default)]
pub struct WithFallback {
    /// For tests: overrides [`set_ask_file_servers`].
    ask_file_servers: Option<bool>,
    /// For tests: overrides [`set_ask_room_servers`].
    ask_room_servers: Option<bool>,
    /// For tests: the room's other servers, instead of reading its members.
    room_servers: Option<Vec<OwnedServerName>>,
    /// Files the room's servers had no copy of, and when they were asked.
    no_copy: Mutex<HashMap<(OwnedMxcUri, bool), Instant>>,
    /// Where each file server answers client requests (from its
    /// `.well-known/matrix/client`); `None`: it can't be reached that way.
    origins: Mutex<HashMap<OwnedServerName, Option<reqwest::Url>>>,
}

impl MediaFetcher for WithFallback {
    fn fetch_media_content<'a>(&'a self, client: &'a Client, request: &'a MediaRequestParameters) -> BoxFuture<'a, Result<Vec<u8>, matrix_sdk::Error>> {
        Box::pin(async move {
            let uri = match &request.source {
                MediaSource::Plain(u) => u,
                MediaSource::Encrypted(f) => &f.url,
            };
            let data = self
                .fetch(client, uri, &request.format)
                .await
                .map_err(|e| matrix_sdk::Error::UnknownError(e.to_string().into()))?;
            match &request.source {
                MediaSource::Plain(_) => Ok(data),
                MediaSource::Encrypted(file) => {
                    let mut cursor = std::io::Cursor::new(data);
                    let mut reader = matrix_sdk_base::crypto::AttachmentDecryptor::new(
                        &mut cursor,
                        file.as_ref().clone().into(),
                    )?;
                    let mut out = Vec::new();
                    reader.read_to_end(&mut out)?;
                    Ok(out)
                }
            }
        })
    }
}

/// Why a try didn't give the file.
#[derive(Debug)]
enum Failed {
    /// The server answered no (it's up, it just won't give this file there).
    Refused(String),
    /// No usable answer (offline, too slow, broken connection).
    Unreachable(String),
    /// The server answered, but with an error of its own (5xx); a server
    /// says this when it couldn't reach the file's server, too.
    ServerError(String),
    /// Rate limited: try again after this long. Never leaves [`get`].
    Busy(String, Duration),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Refused(s) | Failed::Unreachable(s) | Failed::ServerError(s) | Failed::Busy(s, _) => f.write_str(s),
        }
    }
}

impl WithFallback {
    /// For tests: where `server`'s files can be fetched from.
    #[cfg(test)]
    fn set_origin(&self, server: &str, base: reqwest::Url) {
        self.origins
            .lock()
            .unwrap()
            .insert(server.try_into().unwrap(), Some(base));
    }

    fn asks_room_servers(&self) -> bool {
        self.ask_room_servers
            .unwrap_or_else(|| ASK_ROOM_SERVERS.load(Ordering::Relaxed))
    }

    fn asks_file_servers(&self) -> bool {
        self.ask_file_servers
            .unwrap_or_else(|| ASK_FILE_SERVERS.load(Ordering::Relaxed))
    }

    async fn fetch(
        &self,
        client: &Client,
        uri: &MxcUri,
        format: &MediaFormat,
    ) -> Result<Vec<u8>, Failed> {
        let (Ok(server), Ok(id)) = (uri.server_name(), uri.media_id()) else {
            return Err(Failed::Refused(format!("{uri} isn't a file address")));
        };
        let tail = match format {
            MediaFormat::File => format!("download/{server}/{id}"),
            MediaFormat::Thumbnail(s) => format!(
                "thumbnail/{server}/{id}?width={}&height={}&method={}&animated={}",
                s.width,
                s.height,
                s.method.as_str(),
                s.animated
            ),
        };
        let home = client.homeserver();
        let token = client.access_token();
        let mut first = None;
        let at = |base: &reqwest::Url, path: String| {
            base.join(&path).map_err(|e| Failed::Refused(e.to_string()))
        };
        if let Some(t) = token.as_deref() {
            match get(
                client,
                at(&home, format!("_matrix/client/v1/media/{tail}"))?,
                Some(t),
            )
            .await
            {
                Ok(d) => return Ok(d),
                Err(e @ Failed::Unreachable(_)) => return Err(e),
                Err(e) => first = Some(e),
            }
        }
        match get(client, at(&home, format!("_matrix/media/v3/{tail}"))?, None).await {
            Ok(d) => return Ok(d),
            Err(e @ Failed::Unreachable(_)) => return Err(first.unwrap_or(e)),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
        let first = first.expect("set above");
        let sep = if tail.contains('?') { '&' } else { '?' };
        let cached = format!("_matrix/media/v3/{tail}{sep}allow_remote=false");
        // The file's own server, if allowed and not the one we just asked.
        if self.asks_file_servers() && home.host_str() != Some(server.as_str())
            && let Some(base) = self.origin(client, server).await
            && base.origin() != home.origin()
        {
            match get(client, at(&base, cached.clone())?, None).await {
                Ok(d) => {
                    tracing::debug!("{uri}: fetched from its own server, as the account's server couldn't ({first})");
                    return Ok(d);
                }
                Err(again) => tracing::debug!("{uri}: its own server didn't give it either: {again}"),
            }
        }
        if !self.asks_room_servers() {
            return Err(first);
        }
        let key = (uri.to_owned(), matches!(format, MediaFormat::File));
        if self.no_copy.lock().unwrap().get(&key).is_some_and(|t| t.elapsed() < ROOM_AGAIN) {
            return Err(first);
        }
        for other in self.room_servers(client, server).await {
            let Some(base) = self.origin(client, &other).await else { continue };
            if base.origin() == home.origin() {
                continue;
            }
            // One try each: a server without a copy says so at once, and a
            // dead one shouldn't hold up the rest.
            match get_once(client, &at(&base, cached.clone())?, None).await {
                Ok(d) => {
                    tracing::debug!("{uri}: fetched a copy from {other}, as the account's server couldn't ({first})");
                    return Ok(d);
                }
                Err(e) => tracing::debug!("{uri}: {other} had no copy: {e}"),
            }
        }
        let mut no_copy = self.no_copy.lock().unwrap();
        if no_copy.len() > 2000 {
            no_copy.retain(|_, t| t.elapsed() < ROOM_AGAIN);
        }
        no_copy.insert(key, Instant::now());
        Err(first)
    }

    /// Other servers in the room the file is being fetched for (see
    /// [`in_room`]), to ask for a copy: the two with the most members there,
    /// then others at random. Never the account's server or `file_server`.
    async fn room_servers(&self, client: &Client, file_server: &ServerName) -> Vec<OwnedServerName> {
        if let Some(s) = &self.room_servers {
            return s.clone();
        }
        let Some(room) = ROOM.try_with(Clone::clone).ok().and_then(|r| client.get_room(&r)) else {
            return Vec::new();
        };
        let Ok(members) = room.members_no_sync(matrix_sdk::RoomMemberships::JOIN).await else {
            return Vec::new();
        };
        let own = client.user_id().map(|u| u.server_name().to_owned());
        let mut counts: HashMap<OwnedServerName, usize> = HashMap::new();
        for m in &members {
            let s = m.user_id().server_name();
            if s != file_server && own.as_deref() != Some(s) {
                *counts.entry(s.to_owned()).or_default() += 1;
            }
        }
        pick_servers(counts.into_iter().collect())
    }

    /// Where `server` answers client requests, asked once.
    async fn origin(
        &self,
        client: &Client,
        server: &matrix_sdk::ruma::ServerName,
    ) -> Option<reqwest::Url> {
        if let Some(known) = self.origins.lock().unwrap().get(server) {
            return known.clone();
        }
        let fallback = reqwest::Url::parse(&format!("https://{server}/")).ok();
        let found = async {
            let url = format!("https://{server}/.well-known/matrix/client");
            let res = tokio::time::timeout(
                Duration::from_secs(10),
                client.http_client().get(url).send(),
            )
            .await
            .ok()?
            .ok()?;
            if !res.status().is_success() {
                return None;
            }
            let body: serde_json::Value = tokio::time::timeout(Duration::from_secs(10), res.json())
                .await
                .ok()?
                .ok()?;
            let base = body.get("m.homeserver")?.get("base_url")?.as_str()?;
            let mut url = reqwest::Url::parse(base.trim())
                .ok()
                .filter(|u| u.scheme() == "https" && u.host_str().is_some())?;
            if !url.path().ends_with('/') {
                url.set_path(&format!("{}/", url.path()));
            }
            Some(url)
        }
        .await
        .or(fallback);
        self.origins
            .lock()
            .unwrap()
            .insert(server.to_owned(), found.clone());
        found
    }
}

/// From servers and how many room members each has: the two biggest, then
/// [`ROOM_TRIES`] in all, the rest picked at random.
fn pick_servers(mut counts: Vec<(OwnedServerName, usize)>) -> Vec<OwnedServerName> {
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut rest = counts.split_off(counts.len().min(2));
    let mut out: Vec<_> = counts.into_iter().map(|(s, _)| s).collect();
    while out.len() < ROOM_TRIES && !rest.is_empty() {
        let mut b = [0u8; 4];
        let i = if getrandom::fill(&mut b).is_ok() { u32::from_le_bytes(b) as usize % rest.len() } else { 0 };
        out.push(rest.swap_remove(i).0);
    }
    out
}

/// Longest wait a rate-limited request does before trying again.
const MAX_WAIT: Duration = Duration::from_secs(20);

/// One GET of a whole file, retried when the server is busy or the
/// connection breaks: once, or (rate limited) up to three times, waiting as
/// long as the server asks.
async fn get(client: &Client, url: reqwest::Url, token: Option<&str>) -> Result<Vec<u8>, Failed> {
    let mut tries = 0;
    loop {
        tries += 1;
        match get_once(client, &url, token).await {
            Err(Failed::Unreachable(e) | Failed::ServerError(e)) if tries < 2 => {
                tracing::debug!("{}: {e}; trying again", url.path());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(Failed::Busy(e, wait)) if tries < 4 => {
                tracing::debug!("{}: {e}; trying again in {wait:?}", url.path());
                tokio::time::sleep(wait.min(MAX_WAIT)).await;
            }
            Err(Failed::Busy(e, _)) => return Err(Failed::Unreachable(e)),
            other => return other,
        }
    }
}

async fn get_once(
    client: &Client,
    url: &reqwest::Url,
    token: Option<&str>,
) -> Result<Vec<u8>, Failed> {
    // The SDK's connection gives every request 30 seconds in all, which cut
    // off any file that took longer (videos: "error decoding response
    // body"). The waits below already catch a dead or stalled server.
    let mut req = client.http_client().get(url.clone()).timeout(WHOLE);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let mut res = match tokio::time::timeout(ANSWER, req.send()).await {
        Err(_) => {
            return Err(Failed::Unreachable(
                "the server took too long to answer".into(),
            ));
        }
        Ok(Err(e)) => {
            return Err(Failed::Unreachable(format!(
                "the server didn't answer: {e}"
            )));
        }
        Ok(Ok(r)) => r,
    };
    let status = res.status();
    if !status.is_success() {
        let header_wait = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = tokio::time::timeout(Duration::from_secs(10), res.text())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let said = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .map(|v| {
                format!(
                    "{} {}",
                    v["errcode"].as_str().unwrap_or(""),
                    v["error"].as_str().unwrap_or("")
                )
                .trim()
                .to_owned()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_owned());
        let what = format!("[{} / {said}]", status.as_u16());
        if status.as_u16() == 429 {
            let asked = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["retry_after_ms"].as_u64())
                .map(Duration::from_millis);
            return Err(Failed::Busy(
                what,
                asked.or(header_wait).unwrap_or(Duration::from_secs(3)),
            ));
        }
        // Busy: worth one more try. Anything else is a no.
        return Err(
            if status.is_server_error() && !body.contains("M_UNRECOGNIZED") {
                Failed::ServerError(what)
            } else {
                Failed::Refused(what)
            },
        );
    }
    let mut out =
        Vec::with_capacity(res.content_length().unwrap_or(0).min(MAX_BYTES as u64) as usize);
    loop {
        match tokio::time::timeout(STALL, res.chunk()).await {
            Err(_) => return Err(Failed::Unreachable("the download stalled".into())),
            Ok(Err(e)) => return Err(Failed::Unreachable(format!("the download broke off: {e}"))),
            Ok(Ok(None)) => return Ok(out),
            Ok(Ok(Some(chunk))) => {
                if out.len() + chunk.len() > MAX_BYTES {
                    return Err(Failed::Refused("the file is too big".into()));
                }
                out.extend_from_slice(&chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedMxcUri;
    use wiremock::matchers::{path, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    fn unrecognized() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_json(
            serde_json::json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}),
        )
    }

    fn file(name: &str) -> MediaRequestParameters {
        MediaRequestParameters {
            source: MediaSource::Plain(OwnedMxcUri::from(format!("mxc://other.org/{name}"))),
            format: MediaFormat::File,
        }
    }

    /// A server that answers "Unrecognized" on the new address still gives
    /// the file on the old one.
    #[tokio::test]
    async fn falls_back_to_the_old_address() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        Mock::given(path_regex("^/_matrix/client/v1/media/"))
            .respond_with(unrecognized())
            .mount(mock.server())
            .await;
        Mock::given(path("/_matrix/media/v3/download/other.org/cat"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"meow".to_vec(), "image/png"))
            .mount(mock.server())
            .await;
        let client = mock.client_builder().build().await;
        client
            .set_media_fetcher(std::sync::Arc::new(WithFallback::default()))
            .await;
        assert_eq!(
            client
                .media()
                .get_media_content(&file("cat"), false)
                .await
                .unwrap(),
            b"meow"
        );
    }

    /// When the account's server can't get another server's file at all,
    /// it comes from the file's own server, without the login.
    #[tokio::test]
    async fn asks_the_files_own_server() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        let not_found = ResponseTemplate::new(404).set_body_json(
            serde_json::json!({"errcode": "M_NOT_FOUND", "error": "Remote media not found"}),
        );
        Mock::given(path_regex("^/_matrix/"))
            .respond_with(not_found)
            .mount(mock.server())
            .await;
        let origin = wiremock::MockServer::start().await;
        Mock::given(path("/_matrix/media/v3/download/other.org/dog"))
            .and(wiremock::matchers::query_param("allow_remote", "false"))
            .and(|r: &wiremock::Request| !r.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"woof".to_vec(), "image/png"))
            .mount(&origin)
            .await;
        let client = mock.client_builder().build().await;
        let fetcher = WithFallback {
            ask_file_servers: Some(true),
            ..Default::default()
        };
        fetcher.set_origin(
            "other.org",
            reqwest::Url::parse(&format!("{}/", origin.uri())).unwrap(),
        );
        client.set_media_fetcher(std::sync::Arc::new(fetcher)).await;
        assert_eq!(
            client
                .media()
                .get_media_content(&file("dog"), false)
                .await
                .unwrap(),
            b"woof"
        );
    }

    /// With the setting off, the file's own server is never contacted.
    #[tokio::test]
    async fn leaves_the_files_own_server_alone_when_off() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        let not_found = ResponseTemplate::new(404).set_body_json(
            serde_json::json!({"errcode": "M_NOT_FOUND", "error": "Remote media not found"}),
        );
        Mock::given(path_regex("^/_matrix/"))
            .respond_with(not_found)
            .mount(mock.server())
            .await;
        let origin = wiremock::MockServer::start().await;
        Mock::given(path_regex("^/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"woof".to_vec(), "image/png"))
            .expect(0)
            .mount(&origin)
            .await;
        let client = mock.client_builder().build().await;
        let fetcher = WithFallback {
            ask_file_servers: Some(false),
            ..Default::default()
        };
        fetcher.set_origin(
            "other.org",
            reqwest::Url::parse(&format!("{}/", origin.uri())).unwrap(),
        );
        client.set_media_fetcher(std::sync::Arc::new(fetcher)).await;
        assert!(
            client
                .media()
                .get_media_content(&file("dog"), false)
                .await
                .is_err()
        );
        origin.verify().await;
    }

    /// When the file's server is gone, a copy comes from another server in
    /// the room: one without a copy is skipped, and none get the login.
    #[tokio::test]
    async fn asks_the_rooms_other_servers() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        Mock::given(path_regex("^/_matrix/"))
            .respond_with(ResponseTemplate::new(502).set_body_json(serde_json::json!({"errcode": "M_UNKNOWN", "error": "Failed to fetch remote media"})))
            .mount(mock.server())
            .await;
        let empty = wiremock::MockServer::start().await;
        Mock::given(path_regex("^/"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({"errcode": "M_NOT_FOUND", "error": "Not found"})))
            .expect(1)
            .mount(&empty)
            .await;
        let keeper = wiremock::MockServer::start().await;
        Mock::given(path("/_matrix/media/v3/download/other.org/owl"))
            .and(wiremock::matchers::query_param("allow_remote", "false"))
            .and(|r: &wiremock::Request| !r.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"hoot".to_vec(), "image/png"))
            .mount(&keeper)
            .await;
        let client = mock.client_builder().build().await;
        let fetcher = WithFallback {
            ask_room_servers: Some(true),
            room_servers: Some(vec!["empty.org".try_into().unwrap(), "keeper.org".try_into().unwrap()]),
            ..Default::default()
        };
        fetcher.set_origin("empty.org", reqwest::Url::parse(&format!("{}/", empty.uri())).unwrap());
        fetcher.set_origin("keeper.org", reqwest::Url::parse(&format!("{}/", keeper.uri())).unwrap());
        client.set_media_fetcher(std::sync::Arc::new(fetcher)).await;
        assert_eq!(client.media().get_media_content(&file("owl"), false).await.unwrap(), b"hoot");
        empty.verify().await;
    }

    /// With the setting off, the room's other servers are never contacted.
    #[tokio::test]
    async fn leaves_the_rooms_servers_alone_when_off() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        Mock::given(path_regex("^/_matrix/"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({"errcode": "M_NOT_FOUND", "error": "Remote media not found"})))
            .mount(mock.server())
            .await;
        let keeper = wiremock::MockServer::start().await;
        Mock::given(path_regex("^/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"hoot".to_vec(), "image/png"))
            .expect(0)
            .mount(&keeper)
            .await;
        let client = mock.client_builder().build().await;
        let fetcher = WithFallback {
            ask_room_servers: Some(false),
            room_servers: Some(vec!["keeper.org".try_into().unwrap()]),
            ..Default::default()
        };
        fetcher.set_origin("keeper.org", reqwest::Url::parse(&format!("{}/", keeper.uri())).unwrap());
        client.set_media_fetcher(std::sync::Arc::new(fetcher)).await;
        assert!(client.media().get_media_content(&file("owl"), false).await.is_err());
        keeper.verify().await;
    }

    /// The two biggest servers first, then others up to the limit, each once.
    #[test]
    fn picks_the_biggest_then_random() {
        let counts: Vec<(OwnedServerName, usize)> = [("a.org", 1), ("big.org", 50), ("b.org", 2), ("mid.org", 9), ("c.org", 1), ("d.org", 3)]
            .iter()
            .map(|(s, n)| ((*s).try_into().unwrap(), *n))
            .collect();
        let picked = pick_servers(counts);
        assert_eq!(picked.len(), ROOM_TRIES);
        assert_eq!(picked[0].as_str(), "big.org");
        assert_eq!(picked[1].as_str(), "mid.org");
        let mut unique = picked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), picked.len());
        assert_eq!(pick_servers(vec![("only.org".try_into().unwrap(), 1)]).len(), 1);
        assert!(pick_servers(Vec::new()).is_empty());
    }

    /// A server that keeps failing gives up quickly instead of being retried
    /// for minutes (which held up every other preview).
    #[tokio::test]
    async fn gives_up_on_a_failing_server() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        Mock::given(path_regex("^/_matrix/client/v1/media/"))
            .respond_with(ResponseTemplate::new(502))
            .mount(mock.server())
            .await;
        let client = mock.client_builder().build().await;
        client
            .set_media_fetcher(std::sync::Arc::new(WithFallback::default()))
            .await;
        let started = std::time::Instant::now();
        assert!(
            client
                .media()
                .get_media_content(&file("slow"), false)
                .await
                .is_err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    /// A rate-limited request waits as long as the server asks, then gets
    /// the file, instead of failing the preview.
    #[tokio::test]
    async fn waits_out_a_rate_limit() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        let limited = ResponseTemplate::new(429).set_body_json(serde_json::json!({"errcode": "M_LIMIT_EXCEEDED", "error": "Too many requests", "retry_after_ms": 300}));
        Mock::given(path_regex(
            "^/_matrix/client/v1/media/download/other.org/fox",
        ))
        .respond_with(limited)
        .up_to_n_times(2)
        .mount(mock.server())
        .await;
        Mock::given(path_regex(
            "^/_matrix/client/v1/media/download/other.org/fox",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"yip".to_vec(), "image/png"))
        .mount(mock.server())
        .await;
        let client = mock.client_builder().build().await;
        client
            .set_media_fetcher(std::sync::Arc::new(WithFallback::default()))
            .await;
        assert_eq!(
            client
                .media()
                .get_media_content(&file("fox"), false)
                .await
                .unwrap(),
            b"yip"
        );
    }
}
