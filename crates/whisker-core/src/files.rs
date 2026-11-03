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
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource};
use matrix_sdk::ruma::matrix_uri::MatrixId;
use matrix_sdk::ruma::{EventId, MatrixToUri, MatrixUri, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomOrAliasId, UInt};
use matrix_sdk::Client;

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
    let (server, id) = (mxc.server_name().context("that isn't a file address")?, mxc.media_id().context("that isn't a file address")?);
    let base = client.homeserver();
    let mut last = None;
    // Servers from before authenticated media only know the old address.
    for (path, auth) in [(format!("_matrix/client/v1/media/download/{server}/{id}"), true), (format!("_matrix/media/v3/download/{server}/{id}"), false)] {
        let url = base.join(&path)?;
        let mut req = client.http_client().get(url).timeout(std::time::Duration::from_secs(30));
        if auth && let Some(token) = client.access_token() {
            req = req.bearer_auth(token);
        }
        let res = req.send().await.context("the server didn't answer")?;
        let status = res.status();
        if status.is_success() {
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
            return Ok(LinkedFile { mime, size, name });
        }
        last = Some(status);
        if status == reqwest::StatusCode::NOT_FOUND && auth {
            // Could be either "no such file" or "no such address": try the old one.
            continue;
        }
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

/// Posts the file at `mxc` as a message, nothing uploaded. `size` is a
/// picture's width and height, when known.
pub async fn send_linked(room: &Room, mxc: &MxcUri, file: &LinkedFile, size: Option<(u32, u32)>, reply_to: Option<&EventId>) -> Result<OwnedEventId> {
    let mut content = RoomMessageEventContent::new(message_for(mxc, file, size));
    if let Some(r) = reply_to {
        content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(r.to_owned()))));
    }
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
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
    let config = matrix_sdk::config::RequestConfig::default().retry_limit(3).max_retry_time(std::time::Duration::from_secs(20));
    Ok(Some(to.send(content).with_request_config(config).await?.response.event_id))
}

fn message_for(mxc: &MxcUri, file: &LinkedFile, size: Option<(u32, u32)>) -> MessageType {
    let name = name_for(mxc, file);
    let src = mxc.to_owned();
    let bytes = file.size.and_then(|s| UInt::try_from(s).ok());
    let mime = file.mime.clone();
    match file.kind() {
        MediaKind::Image => {
            let mut info = ImageInfo::new();
            info.mimetype = mime;
            info.size = bytes;
            if let Some((w, h)) = size {
                info.width = Some(UInt::from(w));
                info.height = Some(UInt::from(h));
            }
            MessageType::Image(ImageMessageEventContent::plain(name, src).info(Some(Box::new(info))))
        }
        MediaKind::Video => {
            let mut info = VideoInfo::new();
            info.mimetype = mime;
            info.size = bytes;
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
        match message_for(&mxc, &png, Some((800, 800))) {
            MessageType::Image(i) => {
                assert_eq!(i.body, "abc.png");
                assert_eq!(i.info.unwrap().width, Some(UInt::from(800u32)));
            }
            other => panic!("{other:?}"),
        }
    }
}
