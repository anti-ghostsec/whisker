//! File links: a file already on a Matrix server, shared by its address
//! (`mxc://server/id`) instead of being uploaded again. Pasting one sends it
//! as a picture, video or file message; a message that is only a link shows
//! the file itself.

use anyhow::{Context, Result, bail};
use matrix_sdk::Room;
use matrix_sdk::ruma::events::relation::{InReplyTo, Reply};
use matrix_sdk::ruma::events::room::message::{
    AudioInfo, AudioMessageEventContent, FileInfo, FileMessageEventContent, ImageMessageEventContent, MessageType, Relation, RoomMessageEventContent,
    VideoInfo, VideoMessageEventContent,
};
use matrix_sdk::ruma::events::room::{EncryptedFileInfo, ImageInfo, MediaSource, ThumbnailInfo};
use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::{EventId, MatrixToUri, MatrixUri, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomOrAliasId, UInt};
use matrix_sdk::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::timeline::MediaKind;

/// What a pasted link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileLink {
    /// A file on a server.
    Media(OwnedMxcUri),
    /// A message in a room (`https://matrix.to/#/!room/$event`): the way to
    /// share an encrypted file, whose address alone can't open it.
    Event(OwnedRoomOrAliasId, OwnedEventId),
}

/// Reads `text` as a single file link: `mxc://server/id`, a server's media
/// download address, or a matrix.to / `matrix:` link to a message.
pub fn parse_link(text: &str) -> Option<FileLink> {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    if let Some(mxc) = media_address(t) {
        return Some(FileLink::Media(mxc));
    }
    let id = MatrixToUri::parse(t).map(|u| u.id().clone()).or_else(|_| MatrixUri::parse(t).map(|u| u.id().clone())).ok()?;
    match id {
        MatrixId::Event(room, event) => Some(FileLink::Event(room, event)),
        _ => None,
    }
}

/// `mxc://server/id`, or `https://…/_matrix/…/media/…/download/server/id[/name]`.
fn media_address(t: &str) -> Option<OwnedMxcUri> {
    let (server, id) = match t.strip_prefix("mxc://") {
        Some(rest) => rest.split_once('/')?,
        None => {
            let path = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://"))?;
            let (_, after) = path.split_once("/_matrix/")?;
            let rest = ["media/v3/download/", "media/r0/download/", "media/v1/download/", "client/v1/media/download/"].iter().find_map(|p| after.strip_prefix(p))?;
            let rest = rest.split(['?', '#']).next()?;
            let (server, tail) = rest.split_once('/')?;
            (server, tail.split('/').next()?)
        }
    };
    let mxc = OwnedMxcUri::from(format!("mxc://{server}/{id}"));
    (!server.is_empty() && !id.is_empty() && mxc.is_valid()).then_some(mxc)
}

/// The link to share for a file sent in `room` as message `event`: its
/// address, or for an encrypted file (which needs the message's key) a
/// link to the message. The flag says it's the second kind.
pub fn link_for(source: &MediaSource, room: &Room, event: &EventId) -> (String, bool) {
    match source {
        MediaSource::Plain(mxc) => (mxc.to_string(), false),
        MediaSource::Encrypted(_) => (room.room_id().matrix_to_event_uri(event.to_owned()).to_string(), true),
    }
}

/// What the server says about a linked file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkedFile {
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub name: Option<String>,
}

impl LinkedFile {
    pub fn kind(&self) -> MediaKind {
        kind_of(self.mime.as_deref())
    }
}

/// Pictures we can show, videos and sound by their type; the rest are files.
pub fn kind_of(mime: Option<&str>) -> MediaKind {
    match mime.unwrap_or_default() {
        "image/svg+xml" => MediaKind::File,
        m if m.starts_with("image/") => MediaKind::Image,
        m if m.starts_with("video/") => MediaKind::Video,
        m if m.starts_with("audio/") => MediaKind::Audio,
        _ => MediaKind::File,
    }
}

/// Asks `mxc`'s server for the file's type, size and name, through the
/// account's own connection, without downloading it.
pub async fn probe(client: &Client, mxc: &MxcUri) -> Result<LinkedFile> {
    let (res, _) = open(client, mxc, None).await?;
    let header = |h: reqwest::header::HeaderName| res.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut mime = header(reqwest::header::CONTENT_TYPE).map(|m| m.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
    let size = header(reqwest::header::CONTENT_LENGTH).and_then(|l| l.parse().ok());
    let name = header(reqwest::header::CONTENT_DISPOSITION).and_then(|d| filename_of(&d));
    // A server that doesn't know the type: go by the name.
    if mime.as_deref().is_none_or(|m| m == "application/octet-stream")
        && let Some(guess) = name.as_deref().and_then(|n| mime_guess::from_path(n).first())
    {
        mime = Some(guess.essence_str().to_owned());
    }
    // Dropping the response stops the download.
    Ok(LinkedFile { mime, size, name })
}

/// Where a file can be downloaded from, and whether that needs the login.
#[derive(Clone)]
struct Address {
    url: reqwest::Url,
    auth: bool,
}

impl Address {
    fn get(&self, client: &Client) -> reqwest::RequestBuilder {
        let req = client.http_client().get(self.url.clone());
        match (self.auth, client.access_token()) {
            (true, Some(token)) => req.bearer_auth(token),
            _ => req,
        }
    }
}

/// Starts downloading `mxc` (the caller reads as much as it wants, from
/// `range` when given), and says from where.
async fn open(client: &Client, mxc: &MxcUri, range: Option<&str>) -> Result<(reqwest::Response, Address)> {
    let (server, id) = (mxc.server_name().context("that isn't a file address")?, mxc.media_id().context("that isn't a file address")?);
    let base = client.homeserver();
    let mut last = None;
    // Servers from before authenticated media only know the old address.
    for (path, auth) in [(format!("_matrix/client/v1/media/download/{server}/{id}"), true), (format!("_matrix/media/v3/download/{server}/{id}"), false)] {
        let at = Address { url: base.join(&path)?, auth };
        let mut req = at.get(client);
        if let Some(r) = range {
            req = req.header(reqwest::header::RANGE, r);
        }
        let res = tokio::time::timeout(std::time::Duration::from_secs(30), req.send())
            .await
            .context("the server took too long")?
            .context("the server didn't answer")?;
        let status = res.status();
        if status.is_success() {
            return Ok((res, at));
        }
        last = Some(status);
        if !matches!(status.as_u16(), 400 | 401 | 404 | 405) {
            break;
        }
    }
    match last {
        Some(s) if s == reqwest::StatusCode::NOT_FOUND => bail!("the server has no such file"),
        Some(s) => bail!("the server said {s}"),
        None => bail!("the server didn't answer"),
    }
}

/// Most bytes ffmpeg may read of one file through a [`Relay`] to look at it.
const RELAY_BUDGET: u64 = 64 * 1024 * 1024;

/// Most bytes fetched of one file while playing it: enough to watch it a
/// few times over and seek about, not endless reading.
fn playing_budget(size: Option<u64>) -> u64 {
    size.map_or(RELAY_BUDGET, |s| s.saturating_mul(4)).saturating_add(RELAY_BUDGET)
}

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// The key of an encrypted file (AES-256-CTR).
#[derive(Clone)]
struct Key {
    k: [u8; 32],
    iv: [u8; 16],
}

impl Key {
    /// The cipher at byte `offset` of the file: CTR needs nothing before it.
    fn cipher_at(&self, offset: u64) -> Aes256Ctr {
        use aes::cipher::{KeyIvInit, StreamCipherSeek};
        let mut c = Aes256Ctr::new((&self.k).into(), (&self.iv).into());
        c.seek(offset);
        c
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.k.zeroize();
    }
}

/// The file's address, and its key if it's encrypted.
fn address_and_key(source: &MediaSource) -> Result<(OwnedMxcUri, Option<Key>)> {
    match source {
        MediaSource::Plain(mxc) => Ok((mxc.clone(), None)),
        MediaSource::Encrypted(file) => {
            let EncryptedFileInfo::V2(info) = &file.info else { bail!("the file is encrypted in a way Whisker doesn't know") };
            Ok((file.url.clone(), Some(Key { k: *info.k.as_inner(), iv: *info.iv.as_inner() })))
        }
    }
}

/// `bytes 10-20/300` in a Content-Range header: the first byte, and the
/// whole file's size when the server says it.
fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let (span, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let first = span.split_once('-')?.0.trim().parse().ok()?;
    Some((first, total.trim().parse().ok()))
}

/// The first and (when given) last byte a `Range` header asks for; only
/// one range. `-n` (the last n bytes) needs the size.
fn asked_range(v: &str, size: Option<u64>) -> Option<(u64, Option<u64>)> {
    let (a, b) = v.trim().strip_prefix("bytes=")?.split_once('-')?;
    if b.contains(',') {
        return None;
    }
    match (a.trim(), b.trim()) {
        ("", n) => Some((size?.saturating_sub(n.parse().ok()?), None)),
        (a, "") => Some((a.parse().ok()?, None)),
        (a, b) => {
            let (a, b) = (a.parse().ok()?, b.parse().ok()?);
            (b >= a).then_some((a, Some(b)))
        }
    }
}

/// One download of part of a file, decrypted as it comes. A server that
/// sends the whole file instead of the part is read from the start, and
/// what comes before the part is skipped.
struct Piece {
    res: reqwest::Response,
    /// Where in the file the next byte from the server is.
    pos: u64,
    /// The first byte wanted.
    from: u64,
    /// Bytes still wanted, when the part has an end.
    left: Option<u64>,
    key: Option<Key>,
    cipher: Option<Aes256Ctr>,
}

impl Piece {
    fn new(res: reqwest::Response, from: u64, to: Option<u64>, key: Option<Key>) -> Result<Piece> {
        let pos = match res.status().as_u16() {
            206 => {
                let first = content_range(res.headers()).map_or(from, |(first, _)| first);
                if first > from {
                    bail!("the server sent the wrong part of the file");
                }
                first
            }
            200 => 0,
            _ => bail!("the server said {}", res.status()),
        };
        Ok(Piece { res, pos, from, left: to.map(|t| (t + 1).saturating_sub(from)), key, cipher: None })
    }

    /// The next bytes of the part; `None` at its end. Everything the server
    /// sends counts against `budget`.
    async fn next(&mut self, budget: &AtomicU64) -> Result<Option<Vec<u8>>> {
        use aes::cipher::StreamCipher;
        loop {
            if self.left == Some(0) {
                return Ok(None);
            }
            let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(30), self.res.chunk()).await.context("the server stopped sending")?? else {
                return Ok(None);
            };
            let n = chunk.len() as u64;
            // Past the budget: stop, rather than download a big file over and over.
            if budget.try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(n)).is_err() {
                budget.store(0, Ordering::Relaxed);
                bail!("read too much of the file");
            }
            let start = self.pos;
            self.pos += n;
            if self.pos <= self.from {
                continue;
            }
            let skip = self.from.saturating_sub(start) as usize;
            let mut take = chunk.len() - skip;
            if let Some(left) = &mut self.left {
                take = take.min(*left as usize);
                *left -= take as u64;
            }
            let mut data = chunk[skip..skip + take].to_vec();
            if let Some(key) = &self.key {
                let at = start + skip as u64;
                self.cipher.get_or_insert_with(|| key.cipher_at(at)).apply_keystream(&mut data);
            }
            return Ok(Some(data));
        }
    }
}

/// Bytes fetched at a time by [`RemoteFile::read_at`].
const BLOCK: u64 = 512 * 1024;
/// Blocks kept (about 20 MB).
const KEEP_BLOCKS: usize = 40;
/// Blocks fetched ahead of what's read, so playing doesn't wait on them
/// (8 MB: several seconds of a phone video; 2 MB ran out every couple
/// of seconds, and the picture stopped and jumped).
const READ_AHEAD: u64 = 16;

#[derive(Default)]
struct Blocks {
    cells: std::collections::HashMap<u64, Arc<tokio::sync::OnceCell<Arc<[u8]>>>>,
    order: std::collections::VecDeque<u64>,
}

/// A file on a Matrix server read a part at a time, through the account's
/// own connection (and proxy), decrypted on the way when it's encrypted:
/// a video can play while it comes, instead of after all of it has.
///
/// An encrypted file's SHA-256 is of the whole file, so it can't be checked
/// on parts: what plays has not been checked against it (a server could
/// change the bytes, which then decrypt to noise). Only the formats in
/// [`crate::timeline::SAFE_FORMATS`] are read from it.
pub struct RemoteFile {
    client: Client,
    at: Address,
    key: Option<Key>,
    size: Option<u64>,
    /// The server sends parts of the file when asked.
    ranges: bool,
    /// The file's first bytes (up to 4 KiB), decrypted.
    head: Vec<u8>,
    /// Bytes still allowed to be fetched.
    budget: AtomicU64,
    blocks: std::sync::Mutex<Blocks>,
}

impl RemoteFile {
    /// For looking at the file: reads at most [`RELAY_BUDGET`].
    pub async fn open(client: &Client, source: &MediaSource) -> Result<Arc<RemoteFile>> {
        Self::open_with(client, source, |_| RELAY_BUDGET).await
    }

    /// For playing the file: reads up to a few times its size.
    pub async fn open_to_play(client: &Client, source: &MediaSource) -> Result<Arc<RemoteFile>> {
        Self::open_with(client, source, playing_budget).await
    }

    async fn open_with(client: &Client, source: &MediaSource, budget: fn(Option<u64>) -> u64) -> Result<Arc<RemoteFile>> {
        let (mxc, key) = address_and_key(source)?;
        let (res, at) = open(client, &mxc, Some("bytes=0-4095")).await?;
        let ranges = res.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let size = if ranges { content_range(res.headers()).and_then(|(_, total)| total) } else { res.content_length() };
        let allowed = AtomicU64::new(budget(size));
        let mut piece = Piece::new(res, 0, Some(4095), key.clone())?;
        let mut head = Vec::new();
        while let Some(data) = piece.next(&allowed).await? {
            head.extend_from_slice(&data);
        }
        Ok(Arc::new(RemoteFile { client: client.clone(), at, key, size, ranges, head, budget: allowed, blocks: Default::default() }))
    }

    /// The file's size, when the server says it.
    pub fn size(&self) -> Option<u64> {
        self.size
    }

    /// The file's first bytes (up to 4 KiB), decrypted: enough to tell what it is.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Whether any part can be read without reading what comes before it:
    /// the server sends parts and says the size.
    pub fn seekable(&self) -> bool {
        self.ranges && self.size.is_some()
    }

    /// Starts downloading the file from byte `from`, to `to` when given.
    async fn fetch(&self, from: u64, to: Option<u64>) -> Result<Piece> {
        let mut req = self.at.get(&self.client);
        if from > 0 || to.is_some() {
            let to = to.map(|t| t.to_string()).unwrap_or_default();
            req = req.header(reqwest::header::RANGE, format!("bytes={from}-{to}"));
        }
        let res = tokio::time::timeout(std::time::Duration::from_secs(30), req.send())
            .await
            .context("the server took too long")?
            .context("the server didn't answer")?;
        Piece::new(res, from, to, self.key.clone())
    }

    /// Up to `len` bytes from `offset` (fewer only at the end of the file).
    /// Fetched a block at a time, a few blocks ahead, and kept a while:
    /// readers ask for little pieces, and often for the same ones again.
    pub async fn read_at(self: &Arc<Self>, offset: u64, len: usize) -> Result<Vec<u8>> {
        let size = self.size.context("the server didn't say how big the file is")?;
        let end = offset.saturating_add(len as u64).min(size);
        if offset >= end {
            return Ok(Vec::new());
        }
        let (first, last) = (offset / BLOCK, (end - 1) / BLOCK);
        for i in last + 1..=last + READ_AHEAD {
            if i * BLOCK < size && !self.blocks.lock().unwrap().cells.contains_key(&i) {
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.block(i).await;
                });
            }
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        for i in first..=last {
            let block = self.block(i).await?;
            let (s, e) = (offset.max(i * BLOCK) - i * BLOCK, end.min((i + 1) * BLOCK) - i * BLOCK);
            out.extend_from_slice(block.get(s as usize..e as usize).context("the server sent too little")?);
        }
        Ok(out)
    }

    /// Block `i`, fetched once however many ask for it at the same time.
    async fn block(&self, i: u64) -> Result<Arc<[u8]>> {
        let cell = {
            let mut b = self.blocks.lock().unwrap();
            let cell = b.cells.entry(i).or_default().clone();
            if !b.order.contains(&i) {
                b.order.push_back(i);
            }
            while b.order.len() > KEEP_BLOCKS {
                if let Some(old) = b.order.pop_front() {
                    b.cells.remove(&old);
                }
            }
            cell
        };
        cell.get_or_try_init(|| async {
            let size = self.size.context("the server didn't say how big the file is")?;
            let from = i * BLOCK;
            let to = (from + BLOCK).min(size) - 1;
            let mut piece = self.fetch(from, Some(to)).await?;
            let mut data = Vec::with_capacity((to + 1 - from) as usize);
            while let Some(d) = piece.next(&self.budget).await? {
                data.extend_from_slice(&d);
            }
            if data.len() as u64 != to + 1 - from {
                bail!("the server sent too little");
            }
            Ok::<Arc<[u8]>, anyhow::Error>(Arc::from(data))
        })
        .await
        .cloned()
    }
}

/// Lets ffprobe and ffmpeg read a file on a Matrix server, a piece at a
/// time, without the login ever leaving Whisker: they ask a private address
/// on this computer, and Whisker fetches those pieces through the account's
/// own connection (and proxy), decrypting them when the file is encrypted.
/// It stops when dropped.
pub struct Relay {
    /// What to give ffmpeg.
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relay {
    /// For looking at a file: reads at most [`RELAY_BUDGET`] of it.
    pub async fn start(client: &Client, mxc: &MxcUri) -> Result<Relay> {
        Self::serve(RemoteFile::open(client, &MediaSource::Plain(mxc.to_owned())).await?).await
    }

    /// Serves `file`.
    pub async fn serve(file: Arc<RemoteFile>) -> Result<Relay> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let secret = crate::vault::random_secret()[..32].to_owned();
        let url = format!("http://{}/{secret}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            while let Ok((sock, _)) = listener.accept().await {
                let (file, secret) = (file.clone(), secret.clone());
                conns.spawn(async move {
                    if let Err(e) = relay_one(sock, &file, &secret).await {
                        tracing::debug!("relay: {e:#}");
                    }
                });
            }
        });
        Ok(Relay { url, task })
    }
}

/// Answers one request from ffmpeg with that piece of the file.
async fn relay_one(mut sock: tokio::net::TcpStream, file: &Arc<RemoteFile>, secret: &str) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), sock.read(&mut buf)).await??;
        if n == 0 || head.len() > 16 * 1024 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
    if path.trim_start_matches('/') != secret || !matches!(method, "GET" | "HEAD") || file.budget.load(Ordering::Relaxed) == 0 {
        sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let range = lines.find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("range")).map(|(_, v)| v.trim().to_owned()));
    // A server that only sends whole files: so does the relay, which
    // tells ffmpeg not to jump about.
    let asked = range.as_deref().filter(|_| file.ranges).and_then(|r| asked_range(r, file.size));
    // Not the sender's content type: ffmpeg would take it as a hint (a
    // playlist type makes it read the file as a playlist).
    let mut out = String::new();
    let (from, to) = match (asked, file.size) {
        (Some((a, _)), Some(size)) if a >= size => {
            let answer = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            sock.write_all(answer.as_bytes()).await?;
            return Ok(());
        }
        (Some((a, b)), Some(size)) => {
            let end = b.map_or(size - 1, |b| b.min(size - 1));
            out.push_str(&format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{end}/{size}\r\nContent-Length: {}\r\n", end + 1 - a));
            (a, Some(end))
        }
        (_, size) => {
            out.push_str("HTTP/1.1 200 OK\r\n");
            if let Some(size) = size {
                out.push_str(&format!("Content-Length: {size}\r\n"));
            }
            (0, None)
        }
    };
    out.push_str("Content-Type: application/octet-stream\r\nConnection: close\r\n");
    if file.ranges {
        out.push_str("Accept-Ranges: bytes\r\n");
    }
    out.push_str("\r\n");
    if method == "HEAD" {
        sock.write_all(out.as_bytes()).await?;
        return Ok(());
    }
    // A file read in parts goes through the kept blocks, fetched ahead:
    // ffmpeg jumps between a video's picture and sound parts, and asking
    // the server afresh each time kept it waiting.
    if file.seekable() {
        let size = file.size.unwrap_or(0);
        let end = to.map_or(size, |t| (t + 1).min(size));
        // The first block before the answer, so a failure can still say so.
        let first = (BLOCK - from % BLOCK).min(end.saturating_sub(from)) as usize;
        let data = match file.read_at(from, first).await {
            Ok(d) => d,
            Err(e) => {
                sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                return Err(e);
            }
        };
        sock.write_all(out.as_bytes()).await?;
        sock.write_all(&data).await?;
        let mut at = from + data.len() as u64;
        while at < end {
            let data = file.read_at(at, (BLOCK - at % BLOCK).min(end - at) as usize).await?;
            if data.is_empty() {
                break;
            }
            sock.write_all(&data).await?;
            at += data.len() as u64;
        }
        return Ok(());
    }
    let mut piece = match file.fetch(from, to).await {
        Ok(p) => p,
        Err(e) => {
            sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Err(e);
        }
    };
    sock.write_all(out.as_bytes()).await?;
    while let Some(data) = piece.next(&file.budget).await? {
        sock.write_all(&data).await?;
    }
    Ok(())
}

/// A linked video's size, length and sound, and when `poster` a frame
/// from it, read through a [`Relay`]: usually only a small part of the
/// file is fetched.
pub async fn look_at_video(client: &Client, mxc: &MxcUri, poster: bool) -> Option<(crate::timeline::VideoInfo, Option<crate::timeline::Poster>)> {
    let relay = Relay::start(client, mxc).await.inspect_err(|e| tracing::warn!("couldn't read {mxc}: {e:#}")).ok()?;
    let url = relay.url.clone();
    let work = tokio::task::spawn_blocking(move || {
        let info = crate::timeline::probe_video(&url)?;
        Some((info, if poster { crate::timeline::video_poster(&url) } else { None }))
    });
    let out = tokio::time::timeout(std::time::Duration::from_secs(90), work).await.ok()?.ok()?;
    // Dropping the relay ends any reading still going on.
    drop(relay);
    out
}

/// The file name in a Content-Disposition header.
fn filename_of(header: &str) -> Option<String> {
    let mut plain = None;
    for part in header.split(';').map(str::trim) {
        if let Some(v) = part.strip_prefix("filename*=") {
            // RFC 5987: charset'lang'percent-encoded.
            let v = v.rsplit('\'').next().unwrap_or(v);
            return Some(percent_decode(v)).filter(|n| !n.is_empty());
        }
        if let Some(v) = part.strip_prefix("filename=") {
            plain = Some(v.trim_matches('"').to_owned());
        }
    }
    plain.filter(|n| !n.is_empty())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fills in a linked file's kind, type, size and name from its server.
pub fn fill(media: &mut crate::timeline::Media, file: &LinkedFile) {
    let MediaSource::Plain(mxc) = &media.source else { return };
    media.name = name_for(mxc, file);
    media.kind = file.kind();
    media.mime.clone_from(&file.mime);
    media.size = file.size;
}

/// A name for a linked file: the server's, else its id with a fitting ending.
pub fn name_for(mxc: &MxcUri, file: &LinkedFile) -> String {
    if let Some(n) = &file.name {
        return n.clone();
    }
    let id = mxc.media_id().unwrap_or("file").to_owned();
    let ext = file.mime.as_deref().and_then(|m| mime_guess::get_mime_extensions_str(m)).and_then(|e| e.first());
    match ext {
        Some(e) => format!("{id}.{e}"),
        None => id,
    }
}

/// What else is known about a linked file when it's sent.
#[derive(Debug, Clone, Default)]
pub struct Details {
    /// A picture's or video's width and height.
    pub size: Option<(u32, u32)>,
    /// A video's length.
    pub duration: Option<std::time::Duration>,
    /// A video's preview frame, uploaded: (address, width, height, bytes).
    pub thumbnail: Option<(OwnedMxcUri, u32, u32, u64)>,
}

/// Uploads a video's preview frame (a small JPEG), for [`Details::thumbnail`].
pub async fn upload_poster(client: &Client, poster: &crate::timeline::Poster) -> Result<(OwnedMxcUri, u32, u32, u64)> {
    let res = client.media().upload(&mime_guess::mime::IMAGE_JPEG, poster.jpeg.clone(), None).await?;
    Ok((res.content_uri, poster.size.0, poster.size.1, poster.jpeg.len() as u64))
}

/// Posts the file at `mxc` as a message, the file itself not uploaded.
pub async fn send_linked(room: &Room, mxc: &MxcUri, file: &LinkedFile, details: &Details, reply_to: Option<&EventId>) -> Result<OwnedEventId> {
    let mut content = RoomMessageEventContent::new(message_for(mxc, file, details));
    if let Some(r) = reply_to {
        content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(r.to_owned()))));
    }
    let config = crate::timeline::send_config();
    Ok(room.send(content).with_request_config(config).await?.response.event_id)
}

/// Re-posts the file of message `event` in `from` into `to`, as the same
/// file (an encrypted one keeps its key). `None` when that message has no
/// file, or isn't one this device can read.
pub async fn share_file_of(from: &Room, event: &EventId, to: &Room, reply_to: Option<&EventId>) -> Result<Option<OwnedEventId>> {
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
    let ev = from.event(event, None).await.context("couldn't fetch that message")?;
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(m)))) = ev.raw().deserialize() else {
        return Ok(None);
    };
    if !matches!(m.content.msgtype, MessageType::Image(_) | MessageType::Video(_) | MessageType::Audio(_) | MessageType::File(_)) {
        return Ok(None);
    }
    let mut content = RoomMessageEventContent::new(m.content.msgtype);
    if let Some(r) = reply_to {
        content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(r.to_owned()))));
    }
    let config = crate::timeline::send_config();
    Ok(Some(to.send(content).with_request_config(config).await?.response.event_id))
}

fn message_for(mxc: &MxcUri, file: &LinkedFile, details: &Details) -> MessageType {
    let name = name_for(mxc, file);
    let src = mxc.to_owned();
    let bytes = file.size.and_then(|s| UInt::try_from(s).ok());
    let mime = file.mime.clone();
    match file.kind() {
        MediaKind::Image => {
            let mut info = ImageInfo::new();
            info.mimetype = mime;
            info.size = bytes;
            if let Some((w, h)) = details.size {
                info.width = Some(UInt::from(w));
                info.height = Some(UInt::from(h));
            }
            MessageType::Image(ImageMessageEventContent::plain(name, src).info(Some(Box::new(info))))
        }
        MediaKind::Video => {
            let mut info = VideoInfo::new();
            info.mimetype = mime;
            info.size = bytes;
            info.duration = details.duration;
            if let Some((w, h)) = details.size {
                info.width = Some(UInt::from(w));
                info.height = Some(UInt::from(h));
            }
            if let Some((thumb, w, h, n)) = &details.thumbnail {
                let mut t = ThumbnailInfo::new();
                t.width = Some(UInt::from(*w));
                t.height = Some(UInt::from(*h));
                t.size = UInt::try_from(*n).ok();
                t.mimetype = Some("image/jpeg".into());
                info.thumbnail_source = Some(MediaSource::Plain(thumb.clone()));
                info.thumbnail_info = Some(Box::new(t));
            }
            MessageType::Video(VideoMessageEventContent::plain(name, src).info(Some(Box::new(info))))
        }
        MediaKind::Audio => {
            let mut info = AudioInfo::new();
            info.mimetype = mime;
            info.size = bytes;
            MessageType::Audio(AudioMessageEventContent::plain(name, src).info(Some(Box::new(info))))
        }
        MediaKind::File => {
            let mut info = FileInfo::new();
            info.mimetype = mime;
            info.size = bytes;
            MessageType::File(FileMessageEventContent::plain(name, src).info(Some(Box::new(info))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_file_links() {
        let cat = || Some(FileLink::Media(OwnedMxcUri::from("mxc://matrix.org/tpHtIjWpKHwjJafivrWRdBoA")));
        assert_eq!(parse_link(" mxc://matrix.org/tpHtIjWpKHwjJafivrWRdBoA "), cat());
        assert_eq!(parse_link("https://matrix-client.matrix.org/_matrix/client/v1/media/download/matrix.org/tpHtIjWpKHwjJafivrWRdBoA"), cat());
        assert_eq!(parse_link("https://matrix.org/_matrix/media/v3/download/matrix.org/tpHtIjWpKHwjJafivrWRdBoA/cat.png?allow_redirect=true"), cat());
        assert_eq!(parse_link("look mxc://matrix.org/tpHtIjWpKHwjJafivrWRdBoA"), None);
        assert_eq!(parse_link("mxc://matrix.org/"), None);
        assert_eq!(parse_link("https://example.org/cat.png"), None);
        assert_eq!(parse_link("https://matrix.to/#/!tpHtIjWpKHwjJafivrWRdBoA:matrix.org"), None);
        match parse_link("https://matrix.to/#/!room:example.org/$ev123?via=example.org") {
            Some(FileLink::Event(room, ev)) => {
                assert_eq!(room.as_str(), "!room:example.org");
                assert_eq!(ev.as_str(), "$ev123");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Asks matrix.org about the cat picture (needs the network).
    #[tokio::test]
    #[ignore]
    async fn probes_a_real_file() {
        let client = Client::builder().homeserver_url("https://matrix-client.matrix.org").build().await.unwrap();
        let file = probe(&client, &OwnedMxcUri::from("mxc://matrix.org/tpHtIjWpKHwjJafivrWRdBoA")).await.unwrap();
        assert_eq!(file.mime.as_deref(), Some("image/png"));
        assert_eq!(file.size, Some(266_919));
    }

    /// ffprobe and ffmpeg read a video on a (pretend) server through the relay.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_videos_through_the_relay() {
        let dir = std::env::temp_dir().join(format!("whisker-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mkv");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=10:duration=3", "-pix_fmt", "yuv420p"])
            .arg(&video)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("no ffmpeg; skipping");
            return;
        }
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path_regex("/download/localhost/vid$"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(std::fs::read(&video).unwrap(), "video/x-matroska"))
            .mount(&server)
            .await;
        let client = Client::builder().homeserver_url(server.uri()).build().await.unwrap();
        let (info, poster) = look_at_video(&client, &OwnedMxcUri::from("mxc://localhost/vid"), true).await.unwrap();
        assert_eq!((info.width, info.height), (1280, 720));
        assert!(info.duration.is_some_and(|d| d.as_secs_f64() > 2.5));
        let poster = poster.unwrap();
        assert_eq!(poster.size, (640, 360));
        assert!(poster.jpeg.starts_with(&[0xff, 0xd8]));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A server that sends parts of a file when asked, as most do.
    fn in_parts(body: Vec<u8>) -> impl Fn(&wiremock::Request) -> wiremock::ResponseTemplate {
        move |req| {
            let size = body.len() as u64;
            match req.headers.get("range").and_then(|v| v.to_str().ok()).and_then(|r| asked_range(r, Some(size))) {
                Some((a, b)) => {
                    let end = b.map_or(size - 1, |b| b.min(size - 1));
                    wiremock::ResponseTemplate::new(206)
                        .insert_header("content-range", format!("bytes {a}-{end}/{size}").as_str())
                        .set_body_bytes(body[a as usize..=end as usize].to_vec())
                }
                None => wiremock::ResponseTemplate::new(200).set_body_bytes(body.clone()),
            }
        }
    }

    /// An encrypted file is read in parts, each decrypted on its own, and
    /// the relay hands ffmpeg the part it asks for.
    #[tokio::test(flavor = "multi_thread")]
    async fn streams_encrypted_files_in_parts() {
        use aes::cipher::{KeyIvInit, StreamCipher};
        use matrix_sdk::ruma::events::room::{EncryptedFile, EncryptedFileHashes, V2EncryptedFileInfo};
        let plain: Vec<u8> = (0..1_300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let (k, iv) = ([7u8; 32], [9u8, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut sealed = plain.clone();
        Aes256Ctr::new((&k).into(), (&iv).into()).apply_keystream(&mut sealed);
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path_regex("/download/localhost/enc$")).respond_with(in_parts(sealed)).mount(&server).await;
        let client = Client::builder().homeserver_url(server.uri()).build().await.unwrap();
        let info = EncryptedFileInfo::V2(V2EncryptedFileInfo::encode(k, iv));
        let source = MediaSource::Encrypted(Box::new(EncryptedFile::new(OwnedMxcUri::from("mxc://localhost/enc"), info, EncryptedFileHashes::new())));
        let file = RemoteFile::open_to_play(&client, &source).await.unwrap();
        assert!(file.seekable());
        assert_eq!(file.size(), Some(plain.len() as u64));
        assert_eq!(file.head(), &plain[..4096]);
        for (at, len) in [(0, 10), (1_234_567, 50_000), (BLOCK as usize - 3, 7), (plain.len() - 5, 100)] {
            assert_eq!(file.read_at(at as u64, len).await.unwrap(), plain[at..(at + len).min(plain.len())]);
        }
        let relay = Relay::serve(file).await.unwrap();
        let got = reqwest::Client::new().get(&relay.url).header("range", "bytes=100000-").send().await.unwrap();
        assert_eq!(got.status().as_u16(), 206);
        assert_eq!(&got.bytes().await.unwrap()[..], &plain[100_000..]);
    }

    #[test]
    fn reads_range_headers() {
        assert_eq!(asked_range("bytes=0-", None), Some((0, None)));
        assert_eq!(asked_range("bytes=10-20", None), Some((10, Some(20))));
        assert_eq!(asked_range("bytes=-100", Some(1000)), Some((900, None)));
        assert_eq!(asked_range("bytes=-100", None), None);
        assert_eq!(asked_range("bytes=20-10", None), None);
        assert_eq!(asked_range("bytes=0-1,5-6", None), None);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::CONTENT_RANGE, "bytes 4096-8191/123456".parse().unwrap());
        assert_eq!(content_range(&h), Some((4096, Some(123_456))));
        assert_eq!(playing_budget(Some(10)), 40 + RELAY_BUDGET);
    }

    #[test]
    fn names_and_kinds() {
        assert_eq!(filename_of("inline; filename=\"cat.png\""), Some("cat.png".into()));
        assert_eq!(filename_of("attachment; filename*=utf-8''black%20cat.png"), Some("black cat.png".into()));
        assert_eq!(filename_of("inline"), None);
        let mxc = OwnedMxcUri::from("mxc://matrix.org/abc");
        let png = LinkedFile { mime: Some("image/png".into()), size: Some(5), name: None };
        assert_eq!(name_for(&mxc, &png), "abc.png");
        assert_eq!(png.kind(), MediaKind::Image);
        assert_eq!(kind_of(Some("video/mp4")), MediaKind::Video);
        assert_eq!(kind_of(Some("image/svg+xml")), MediaKind::File);
        assert_eq!(kind_of(None), MediaKind::File);
        match message_for(&mxc, &png, &Details { size: Some((800, 800)), ..Default::default() }) {
            MessageType::Image(i) => {
                assert_eq!(i.body, "abc.png");
                assert_eq!(i.info.unwrap().width, Some(UInt::from(800u32)));
            }
            other => panic!("{other:?}"),
        }
        let mp4 = LinkedFile { mime: Some("video/mp4".into()), size: Some(9000), name: Some("clip.mp4".into()) };
        let thumb = OwnedMxcUri::from("mxc://matrix.org/poster");
        let d = Details { size: Some((1280, 720)), duration: Some(std::time::Duration::from_secs(8)), thumbnail: Some((thumb.clone(), 640, 360, 4000)) };
        match message_for(&mxc, &mp4, &d) {
            MessageType::Video(v) => {
                let info = v.info.unwrap();
                assert_eq!((info.width, info.height), (Some(UInt::from(1280u32)), Some(UInt::from(720u32))));
                assert_eq!(info.duration, Some(std::time::Duration::from_secs(8)));
                assert!(matches!(info.thumbnail_source, Some(MediaSource::Plain(t)) if t == thumb));
            }
            other => panic!("{other:?}"),
        }
    }
}
